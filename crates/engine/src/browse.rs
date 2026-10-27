//! 浏览分区：读分区表、按需打开卷、扫描已删除的项（NTFS 扫 MFT）、列目录（排好序缓存、分页给出）、
//! 预览文件。不在分区表里的分区（丢失分区搜索找到的）可以按位置打开，之后同样浏览。只读源盘。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use yisi_engine::protocol::{
    self, BrowseOpened, FileListPage, FilePreview, FsEntry, ListFilesParams, MAX_PAGE,
    OpenRangeParams, PartitionInfo, PartitionRef, RANGE_PARTITION_BASE, ScanResult,
};
use yisi_fs::{BootCopy, FileRef, FsKind, State, Unsupported, Volume};
use yisi_io::BlockDevice;
use yisi_part::{Note, Partition, PartitionKind, PartitionTable, Role, Scheme};
use yisi_rpc::{ErrorObject, error_code};

use crate::boot_check;
use crate::job::ProgressSender;
use crate::preview;
use crate::runtime::error_chain;

/// 一页答复里目录项编码后的总字节数上限，留出余量，保证整条消息不超过 1 MiB。
const PAGE_BYTES: usize = 768 * 1024;
/// 最多缓存这么多个目录的排序结果，满了整个清空。
const MAX_CACHED_DIRS: usize = 64;
/// 扫描进度里的阶段名。
pub const STAGE_SCAN_MFT: &str = "scanMft";
/// 一个会话里最多按位置打开这么多个分区。
const MAX_RANGES: usize = 1000;

pub struct Session {
    device: Arc<dyn BlockDevice>,
    table: PartitionTable,
    /// 各分区按哪一份引导扇区打开（「修复打不开的分区」里用备份浏览）。
    boot: BootCopy,
    volumes: Mutex<HashMap<u32, Arc<Volume>>>,
    listings: Mutex<HashMap<(u32, String), Arc<SortedDir>>>,
    progress: ProgressSender,
    /// 正在扫描的分区各自的停止标记。
    scans: Mutex<HashMap<u32, Arc<AtomicBool>>>,
    /// 浏览任务已取消：正在进行的扫描都停下。
    closing: AtomicBool,
    /// 按位置打开的分区，编号是 [`RANGE_PARTITION_BASE`] 加下标。
    ranges: Mutex<Vec<Range>>,
}

/// 按位置打开的分区，以及按哪一份引导扇区打开（靠备份找到的用备份）。
struct Range {
    offset: u64,
    length: u64,
    boot: BootCopy,
}

struct SortedDir {
    entries: Vec<FsEntry>,
    truncated: bool,
    overwritten: bool,
}

impl Session {
    /// 读分区表，识别每个分区的文件系统；能浏览的分区顺便打开，取出卷标。扫描的进度经 `progress` 发出。
    /// 各分区按 `boot` 指定的那一份引导扇区识别和打开。
    pub fn open(
        device: Arc<dyn BlockDevice>,
        progress: ProgressSender,
        boot: BootCopy,
    ) -> Result<(Self, BrowseOpened), ErrorObject> {
        let table = yisi_part::read_table(device.as_ref()).map_err(|error| {
            ErrorObject::new(
                protocol::error_code::SOURCE_UNAVAILABLE,
                error_chain(&error),
            )
        })?;
        let session = Self {
            device,
            table,
            boot,
            volumes: Mutex::new(HashMap::new()),
            listings: Mutex::new(HashMap::new()),
            progress,
            scans: Mutex::new(HashMap::new()),
            closing: AtomicBool::new(false),
            ranges: Mutex::new(Vec::new()),
        };
        let partitions = session
            .table
            .partitions
            .iter()
            .map(|partition| session.describe(partition))
            .collect();
        let opened = BrowseOpened {
            size: session.device.size(),
            sector_size: session.device.sector_size(),
            scheme: match session.table.scheme {
                Scheme::Mbr => "mbr",
                Scheme::Gpt => "gpt",
                Scheme::None => "none",
            }
            .to_owned(),
            notes: session.table.notes.iter().map(note_key).collect(),
            partitions,
        };
        Ok((session, opened))
    }

    fn describe(&self, partition: &Partition) -> PartitionInfo {
        let (type_id, name, bootable, logical) = table_kind(partition);
        PartitionInfo {
            type_id,
            role: partition.role().map(|role| role_key(role).to_owned()),
            name,
            bootable,
            logical,
            ..self.describe_volume(
                partition.number,
                self.device.as_ref(),
                partition.offset,
                partition.length,
                self.boot,
            )
        }
    }

    /// 分区上的文件系统与卷（按 `boot` 那一份引导扇区识别）；分区表里的信息（类型、用途、名字等）留空。
    fn describe_volume(
        &self,
        number: u32,
        device: &dyn BlockDevice,
        offset: u64,
        length: u64,
        boot: BootCopy,
    ) -> PartitionInfo {
        let detected = yisi_fs::detect_with(device, offset, length, boot);
        let volume = match detected {
            Ok(kind) if kind.is_browsable() => self.volume(number).ok(),
            _ => None,
        };
        PartitionInfo {
            number,
            offset,
            length,
            type_id: String::new(),
            role: None,
            name: String::new(),
            bootable: false,
            logical: false,
            filesystem: match detected {
                Ok(kind) => fs_key(kind),
                Err(_) => "unreadable",
            }
            .to_owned(),
            label: volume
                .as_ref()
                .and_then(|volume| volume.label().map(str::to_owned)),
            browsable: volume.is_some(),
            needs_scan: volume.as_ref().is_some_and(|volume| volume.needs_scan()),
            times_utc: volume.as_ref().is_some_and(|volume| volume.times_utc()),
        }
    }

    /// 找出分区里已删除的项：NTFS 扫一遍 MFT，期间发进度；被停下时答复 `cancelled`。
    /// 扫完清掉这个分区已缓存的目录，之后列目录才带上已删除的项。
    pub fn scan(&self, partition: u32) -> Result<ScanResult, ErrorObject> {
        let volume = self.volume(partition)?;
        let stop = Arc::clone(
            lock(&self.scans)
                .entry(partition)
                .or_insert_with(|| Arc::new(AtomicBool::new(false))),
        );
        let started = Instant::now();
        let outcome = volume.scan(&mut |done, total| {
            self.progress.send(Some(STAGE_SCAN_MFT), done, total);
            !stop.load(Ordering::Relaxed) && !self.closing.load(Ordering::Relaxed)
        });
        {
            let mut scans = lock(&self.scans);
            if scans
                .get(&partition)
                .is_some_and(|current| Arc::ptr_eq(current, &stop))
            {
                scans.remove(&partition);
            }
        }
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let summary = outcome.map_err(|error| fs_error(error_chain(&error)))?;
        let Some(summary) = summary else {
            return Ok(ScanResult {
                cancelled: true,
                records: 0,
                in_use: 0,
                deleted: 0,
                deleted_names: 0,
                orphans: 0,
                unreadable: 0,
                elapsed_ms,
            });
        };
        lock(&self.listings).retain(|(number, _), _| *number != partition);
        Ok(ScanResult {
            cancelled: false,
            records: summary.records,
            in_use: summary.in_use,
            deleted: summary.deleted,
            deleted_names: summary.deleted_names,
            orphans: summary.orphans,
            unreadable: summary.unreadable,
            elapsed_ms,
        })
    }

    /// 停下分区 `partition` 正在进行的扫描；没有在扫时什么也不做。
    pub fn cancel_scan(&self, partition: u32) {
        if let Some(stop) = lock(&self.scans).get(&partition) {
            stop.store(true, Ordering::Relaxed);
        }
    }

    /// 浏览任务结束：停下全部扫描。
    pub fn close(&self) {
        self.closing.store(true, Ordering::Relaxed);
    }

    /// 在会话里按位置打开一个分区（不在分区表里，例如搜索找到的），之后用答复里的编号浏览。
    /// 同一位置再打开时给出同一个编号。
    pub fn open_range(&self, params: &OpenRangeParams) -> Result<PartitionInfo, ErrorObject> {
        check_range(self.device.as_ref(), params.offset, params.length)?;
        let boot = boot_check::fs_copy(params.boot);
        let number = {
            let mut ranges = lock(&self.ranges);
            let existing = ranges.iter().position(|range| {
                range.offset == params.offset && range.length == params.length && range.boot == boot
            });
            let index = match existing {
                Some(index) => index,
                None => {
                    if ranges.len() >= MAX_RANGES {
                        return Err(ErrorObject::new(
                            error_code::INVALID_PARAMS,
                            format!("一次浏览最多按位置打开 {MAX_RANGES} 个分区"),
                        ));
                    }
                    ranges.push(Range {
                        offset: params.offset,
                        length: params.length,
                        boot,
                    });
                    ranges.len() - 1
                }
            };
            u32::try_from(index)
                .ok()
                .and_then(|index| RANGE_PARTITION_BASE.checked_add(index))
                .ok_or_else(|| fs_error("分区编号溢出".to_owned()))?
        };
        Ok(self.describe_volume(
            number,
            self.device.as_ref(),
            params.offset,
            params.length,
            boot,
        ))
    }

    /// 分区 `number` 的偏移、长度，以及按哪一份引导扇区打开：表里的分区按会话指定的那一份，
    /// 按位置打开的分区按打开时给的那一份。
    fn locate(&self, number: u32) -> Option<(u64, u64, BootCopy)> {
        if let Some(index) = number.checked_sub(RANGE_PARTITION_BASE) {
            let ranges = lock(&self.ranges);
            let range = ranges.get(usize::try_from(index).ok()?)?;
            return Some((range.offset, range.length, range.boot));
        }
        self.table
            .partitions
            .iter()
            .find(|partition| partition.number == number)
            .map(|partition| (partition.offset, partition.length, self.boot))
    }

    /// 打开（或取出已打开的）分区 `number` 上的卷。
    pub fn volume(&self, number: u32) -> Result<Arc<Volume>, ErrorObject> {
        if let Some(volume) = lock(&self.volumes).get(&number) {
            return Ok(Arc::clone(volume));
        }
        let (offset, length, boot) = self
            .locate(number)
            .ok_or_else(|| fs_error(format!("没有分区 {number}")))?;
        let volume = Volume::open_with(Arc::clone(&self.device), offset, length, boot)
            .map_err(|error| fs_error(error_chain(&error)))?;
        let volume = Arc::new(volume);
        lock(&self.volumes).insert(number, Arc::clone(&volume));
        Ok(volume)
    }

    fn partition_offset(&self, number: u32) -> u64 {
        self.locate(number).map_or(0, |(offset, _, _)| offset)
    }

    pub fn list(&self, params: &ListFilesParams) -> Result<FileListPage, ErrorObject> {
        let volume = self.volume(params.partition)?;
        let dir = match &params.dir {
            Some(token) => decode(token)?,
            None => volume.root(),
        };
        let key = (params.partition, dir.to_token());
        let cached = lock(&self.listings).get(&key).cloned();
        let sorted = match cached {
            Some(sorted) => sorted,
            None => {
                let sorted = Arc::new(self.read_dir(&volume, params.partition, &dir)?);
                let mut listings = lock(&self.listings);
                if listings.len() >= MAX_CACHED_DIRS {
                    listings.clear();
                }
                listings.insert(key, Arc::clone(&sorted));
                sorted
            }
        };
        let limit = params.limit.clamp(1, MAX_PAGE) as usize;
        let mut entries = Vec::new();
        let mut budget = PAGE_BYTES;
        for entry in sorted
            .entries
            .iter()
            .skip(params.offset as usize)
            .take(limit)
        {
            let cost = serde_json::to_vec(entry).map_or(PAGE_BYTES, |bytes| bytes.len() + 1);
            // 至少给一项，否则界面翻不了页
            if cost > budget && !entries.is_empty() {
                break;
            }
            budget = budget.saturating_sub(cost);
            entries.push(entry.clone());
        }
        Ok(FileListPage {
            total: u32::try_from(sorted.entries.len()).unwrap_or(u32::MAX),
            entries,
            truncated: sorted.truncated,
            overwritten: sorted.overwritten,
        })
    }

    fn read_dir(
        &self,
        volume: &Volume,
        partition: u32,
        dir: &FileRef,
    ) -> Result<SortedDir, ErrorObject> {
        let listing = volume
            .list(dir)
            .map_err(|error| fs_error(error_chain(&error)))?;
        let base = self.partition_offset(partition);
        let mut entries: Vec<FsEntry> = listing
            .entries
            .into_iter()
            .map(|entry| FsEntry {
                position: entry.position.and_then(|offset| offset.checked_add(base)),
                unsupported: entry
                    .unsupported
                    .map(|reason| unsupported_key(reason).to_owned()),
                record: entry.file.record(),
                virtual_dir: entry.file.is_orphans().then(|| "orphans".to_owned()),
                name: entry.name,
                short_name: entry.short_name,
                name_uncertain: entry.name_uncertain,
                is_dir: entry.file.is_dir,
                size: entry.size,
                modified: entry.modified,
                created: entry.created,
                deleted: entry.file.deleted,
                state: state_key(entry.state).to_owned(),
                file: entry.file.to_token(),
            })
            .collect();
        // 目录在前；名字不分大小写；同名时未删除的在前
        entries
            .sort_by_cached_key(|entry| (!entry.is_dir, entry.name.to_lowercase(), entry.deleted));
        Ok(SortedDir {
            entries,
            truncated: listing.truncated,
            overwritten: listing.overwritten,
        })
    }

    pub fn preview(&self, partition: u32, token: &str) -> Result<FilePreview, ErrorObject> {
        let volume = self.volume(partition)?;
        let file = decode(token)?;
        let unavailable =
            |message: String| ErrorObject::new(protocol::error_code::PREVIEW_UNAVAILABLE, message);
        let reader = volume
            .open_file(&file)
            .map_err(|error| unavailable(error_chain(&error)))?;
        let length = reader.size();
        preview::preview_from(reader, length).map_err(|error| unavailable(error_chain(&error)))
    }
}

/// 核对复制、检查之前浏览时看到的分区还在原处：表里的分区重新读分区表核对编号、偏移与长度；
/// 按位置打开的分区（搜索找到的）不在表里，只核对范围在设备之内。
pub fn verify_partition(
    device: &dyn BlockDevice,
    partition: PartitionRef,
) -> Result<(), ErrorObject> {
    if partition.outside_table {
        return check_range(device, partition.offset, partition.length);
    }
    let table = yisi_part::read_table(device).map_err(|error| fs_error(error_chain(&error)))?;
    let found = table.partitions.iter().any(|candidate| {
        candidate.number == partition.number
            && candidate.offset == partition.offset
            && candidate.length == partition.length
    });
    if found {
        Ok(())
    } else {
        Err(fs_error(format!(
            "分区 {} 已不在浏览时的位置",
            partition.number
        )))
    }
}

/// 按位置给出的分区要在设备之内，长度不能为 0。
fn check_range(device: &dyn BlockDevice, offset: u64, length: u64) -> Result<(), ErrorObject> {
    let size = device.size();
    if length > 0 && offset.checked_add(length).is_some_and(|end| end <= size) {
        Ok(())
    } else {
        Err(ErrorObject::new(
            error_code::INVALID_PARAMS,
            format!("偏移 {offset} 起 {length} 字节超出了设备范围（{size} 字节）"),
        ))
    }
}

pub fn decode(token: &str) -> Result<FileRef, ErrorObject> {
    FileRef::from_token(token).ok_or_else(|| {
        ErrorObject::new(
            error_code::INVALID_PARAMS,
            format!("定位信息不合法：{token}"),
        )
    })
}

fn fs_error(message: String) -> ErrorObject {
    ErrorObject::new(protocol::error_code::FS_UNAVAILABLE, message)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn state_key(state: State) -> &'static str {
    match state {
        State::Live => "live",
        State::Intact => "intact",
        State::Overwritten => "overwritten",
        State::Unknown => "unknown",
    }
}

fn unsupported_key(reason: Unsupported) -> &'static str {
    match reason {
        Unsupported::Compressed => "compressed",
        Unsupported::Encrypted => "encrypted",
        Unsupported::SystemCompressed => "systemCompressed",
        Unsupported::Reparse => "reparse",
    }
}

/// 分区表里的类型（MBR 类型字节或 GPT 类型 GUID）、GPT 分区名、是否活动分区、是否逻辑分区。
pub(crate) fn table_kind(partition: &Partition) -> (String, String, bool, bool) {
    match &partition.kind {
        PartitionKind::Mbr {
            type_id,
            bootable,
            logical,
        } => (
            format!("0x{type_id:02X}"),
            String::new(),
            *bootable,
            *logical,
        ),
        PartitionKind::Gpt {
            type_guid, name, ..
        } => (type_guid.to_string(), name.clone(), false, false),
        PartitionKind::Whole => (String::new(), String::new(), false, false),
    }
}

pub(crate) fn fs_key(kind: FsKind) -> &'static str {
    match kind {
        FsKind::Fat12 => "fat12",
        FsKind::Fat16 => "fat16",
        FsKind::Fat32 => "fat32",
        FsKind::Exfat => "exfat",
        FsKind::Ntfs => "ntfs",
        FsKind::BitLocker => "bitlocker",
        FsKind::Ext => "ext",
        FsKind::Unknown => "unknown",
    }
}

pub(crate) fn note_key(note: &Note) -> String {
    match note {
        Note::PrimaryGptInvalid => "primaryGptInvalid",
        Note::GptUnreadable => "gptUnreadable",
        Note::SectorSizeMismatch { .. } => "sectorSizeMismatch",
        Note::ExtendedChainBroken => "extendedChainBroken",
        Note::PartitionTruncated { .. } => "partitionTruncated",
        Note::PartitionOutside { .. } => "partitionOutside",
    }
    .to_owned()
}

pub(crate) fn role_key(role: Role) -> &'static str {
    match role {
        Role::EfiSystem => "efiSystem",
        Role::MicrosoftReserved => "microsoftReserved",
        Role::BasicData => "basicData",
        Role::WindowsRecovery => "windowsRecovery",
        Role::LdmMetadata => "ldmMetadata",
        Role::LdmData => "ldmData",
        Role::StorageSpaces => "storageSpaces",
        Role::Linux => "linux",
        Role::LinuxSwap => "linuxSwap",
        Role::LinuxLvm => "linuxLvm",
        Role::Apfs => "apfs",
        Role::HfsPlus => "hfsPlus",
        Role::Fat12 => "fat12",
        Role::Fat16 => "fat16",
        Role::Fat32 => "fat32",
        Role::NtfsOrExfat => "ntfsOrExfat",
        Role::Hidden => "hidden",
        Role::GptProtective => "gptProtective",
        Role::Whole => "whole",
    }
}
