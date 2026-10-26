//! 浏览分区：读分区表、按需打开卷、列目录（排好序缓存、分页给出）、预览文件。只读源盘。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use yisi_engine::protocol::{
    self, BrowseOpened, FileListPage, FilePreview, FsEntry, ListFilesParams, MAX_PAGE,
    PartitionInfo,
};
use yisi_fs::{FileRef, FsKind, State, Volume};
use yisi_io::BlockDevice;
use yisi_part::{Note, Partition, PartitionKind, PartitionTable, Role, Scheme};
use yisi_rpc::{ErrorObject, error_code};

use crate::preview;
use crate::runtime::error_chain;

/// 一页答复里目录项编码后的总字节数上限，留出余量，保证整条消息不超过 1 MiB。
const PAGE_BYTES: usize = 768 * 1024;
/// 最多缓存这么多个目录的排序结果，满了整个清空。
const MAX_CACHED_DIRS: usize = 64;

pub struct Session {
    device: Arc<dyn BlockDevice>,
    table: PartitionTable,
    volumes: Mutex<HashMap<u32, Arc<Volume>>>,
    listings: Mutex<HashMap<(u32, String), Arc<SortedDir>>>,
}

struct SortedDir {
    entries: Vec<FsEntry>,
    truncated: bool,
    overwritten: bool,
}

impl Session {
    /// 读分区表，识别每个分区的文件系统；能浏览的分区顺便打开，取出卷标。
    pub fn open(device: Arc<dyn BlockDevice>) -> Result<(Self, BrowseOpened), ErrorObject> {
        let table = yisi_part::read_table(device.as_ref()).map_err(|error| {
            ErrorObject::new(
                protocol::error_code::SOURCE_UNAVAILABLE,
                error_chain(&error),
            )
        })?;
        let session = Self {
            device,
            table,
            volumes: Mutex::new(HashMap::new()),
            listings: Mutex::new(HashMap::new()),
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
        let detected = yisi_fs::detect(self.device.as_ref(), partition.offset, partition.length);
        let label = match detected {
            Ok(kind) if kind.is_browsable() => self
                .volume(partition.number)
                .ok()
                .and_then(|volume| volume.label().map(str::to_owned)),
            _ => None,
        };
        let (type_id, name, bootable, logical) = match &partition.kind {
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
        };
        PartitionInfo {
            number: partition.number,
            offset: partition.offset,
            length: partition.length,
            type_id,
            role: partition.role().map(|role| role_key(role).to_owned()),
            name,
            bootable,
            logical,
            filesystem: match detected {
                Ok(kind) => fs_key(kind),
                Err(_) => "unreadable",
            }
            .to_owned(),
            label,
            browsable: detected.is_ok_and(FsKind::is_browsable)
                && self.volume(partition.number).is_ok(),
        }
    }

    /// 打开（或取出已打开的）分区 `number` 上的卷。
    pub fn volume(&self, number: u32) -> Result<Arc<Volume>, ErrorObject> {
        if let Some(volume) = lock(&self.volumes).get(&number) {
            return Ok(Arc::clone(volume));
        }
        let partition = self
            .table
            .partitions
            .iter()
            .find(|partition| partition.number == number)
            .ok_or_else(|| fs_error(format!("没有分区 {number}")))?;
        let volume = Volume::open(Arc::clone(&self.device), partition.offset, partition.length)
            .map_err(|error| fs_error(error_chain(&error)))?;
        let volume = Arc::new(volume);
        lock(&self.volumes).insert(number, Arc::clone(&volume));
        Ok(volume)
    }

    fn partition_offset(&self, number: u32) -> u64 {
        self.table
            .partitions
            .iter()
            .find(|partition| partition.number == number)
            .map_or(0, |partition| partition.offset)
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
                position: (entry.is_dir() || entry.size > 0)
                    .then(|| volume.first_offset(&entry.file))
                    .flatten()
                    .and_then(|offset| offset.checked_add(base)),
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

fn fs_key(kind: FsKind) -> &'static str {
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

fn note_key(note: &Note) -> String {
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

fn role_key(role: Role) -> &'static str {
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
