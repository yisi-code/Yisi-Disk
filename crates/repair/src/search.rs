//! 丢失分区搜索：从头到尾读一遍设备，逐个 512 字节找 FAT12/16/32、exFAT、NTFS 的引导扇区，
//! 每个引导扇区既当作主引导扇区、也当作备份引导扇区推算分区起点，再核对文件系统的结构，
//! 合并成候选分区，并与现有分区表比对。只读，不改分区表。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use yisi_fs::boot::{exfat_boot_checksum, exfat_stored_checksum};
use yisi_fs::format::{FatWidth, fat_entry_raw, fat_media_ok};
use yisi_fs::{BootCopy, BootLayout, BootSector, FsKind, Volume, inspect_boot, mft_self_record_ok};
use yisi_io::BlockDevice;
use yisi_part::{PartitionKind, PartitionTable, Role};

/// 每次顺序读 1 MiB。
const CHUNK: usize = 1 << 20;
/// exFAT 的 FAT 第 0 项固定记介质描述符 `F8`（exFAT 的引导扇区里没有这一项）。
const EXFAT_MEDIA: u8 = 0xF8;
/// 引导扇区按 512 字节对齐找：4 KiB 扇区的盘上分区同样落在 512 字节的边界上。
const SECTOR: usize = 512;
/// 最多记这么多个候选分区，挡住装满了磁盘镜像文件的盘。
pub const MAX_CANDIDATES: usize = 1000;

/// 搜索过程中查询取消、报告进度。
pub trait SearchControl {
    /// 每读一段查一次。实现可以在这里阻塞（暂停），这时不读盘。
    fn is_cancelled(&self) -> bool;
    fn progress(&self, done: u64, total: u64);
}

/// 候选分区是靠哪个引导扇区找到的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoundBy {
    /// 只有分区开头的主引导扇区对得上（FAT12/16 没有备份，FAT32 / NTFS 的备份可能已被覆盖）。
    Primary,
    /// 主引导扇区已坏，靠备份引导扇区找到。
    Backup,
    Both,
}

/// 候选分区与现有分区表的关系。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// 分区表里有起点相同的分区。
    InTable { number: u32 },
    /// 与分区表里的分区部分重叠、起点不同：可能是那个分区里的镜像文件，也可能是表记错了。
    Overlaps { number: u32 },
    /// 分区表里没有，也不与任何分区重叠。
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub offset: u64,
    /// 卷的字节数，已截到设备末尾。
    pub length: u64,
    pub kind: FsKind,
    pub label: Option<String>,
    pub found_by: FoundBy,
    /// 浏览、复制时按哪一份引导扇区打开：只靠备份找到的（主引导扇区已坏）用备份，否则用主引导扇区。
    pub boot: BootCopy,
    /// 卷超出了设备末尾，长度已截短。
    pub truncated: bool,
    pub relation: Relation,
    /// 与别的候选分区重叠：其中至少一个不是真的分区。
    pub overlaps_candidate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReport {
    pub size: u64,
    /// 现有的分区表；读不出时为空。
    pub table: Option<PartitionTable>,
    /// 按起点排序。
    pub candidates: Vec<Candidate>,
    /// 候选分区超过 [`MAX_CANDIDATES`]，后面的没有记。
    pub candidates_truncated: bool,
    /// 读不出、跳过的字节数（整段 1 MiB 跳过，其中的引导扇区找不到）。
    pub unreadable_bytes: u64,
    pub elapsed_ms: u64,
}

/// 搜索整个设备；被取消时返回 `None`。
pub fn search(device: &Arc<dyn BlockDevice>, control: &dyn SearchControl) -> Option<SearchReport> {
    let started = Instant::now();
    let size = device.size();
    let mut finder = Finder::default();
    let mut buffer = vec![0u8; CHUNK];
    let mut unreadable_bytes = 0u64;
    let mut offset = 0u64;
    control.progress(0, size);
    while offset < size {
        if control.is_cancelled() {
            return None;
        }
        let length = (size - offset).min(CHUNK as u64);
        let block = buffer.get_mut(..length as usize)?;
        match device.read_exact_at(offset, block) {
            Ok(()) => {
                for (index, sector) in block.as_chunks::<SECTOR>().0.iter().enumerate() {
                    let at = offset + (index * SECTOR) as u64;
                    finder.examine(device.as_ref(), at, sector);
                }
            }
            Err(error) => {
                tracing::warn!(offset, length, %error, "读不出，跳过");
                unreadable_bytes += length;
            }
        }
        offset += length;
        control.progress(offset, size);
    }

    let table = yisi_part::read_table(device.as_ref())
        .inspect_err(|error| tracing::warn!(%error, "读不出分区表，候选分区都按丢失处理"))
        .ok();
    let mut candidates = Vec::with_capacity(finder.found.len());
    for (key, found) in &finder.found {
        if control.is_cancelled() {
            return None;
        }
        candidates.push(finder.candidate(device, key, found, table.as_ref()));
    }
    mark_overlapping(&mut candidates);
    Some(SearchReport {
        size,
        table,
        candidates,
        candidates_truncated: finder.truncated,
        unreadable_bytes,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// 一个假设的分区：起点、卷长度、文件系统。按起点排序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    offset: u64,
    volume_bytes: u64,
    kind: u8,
}

struct Found {
    kind: FsKind,
    primary: bool,
    /// 有备份引导扇区核对通过。
    backup: bool,
}

#[derive(Default)]
struct Finder {
    found: BTreeMap<Key, Found>,
    truncated: bool,
}

impl Finder {
    /// 看 `at` 处的一个扇区：是引导扇区的话，分别按主引导扇区、备份引导扇区推算分区起点，核对后记下。
    fn examine(&mut self, device: &dyn BlockDevice, at: u64, sector: &[u8]) {
        let jump = matches!(sector.first(), Some(0xEB | 0xE9));
        if !jump || sector.get(510..512) != Some([0x55, 0xAA].as_slice()) {
            return;
        }
        let Some(boot) = inspect_boot(sector) else {
            return;
        };
        self.consider(device, at, &boot, None);
        if let Some(start) = boot
            .backup_at
            .and_then(|backup_at| at.checked_sub(backup_at))
        {
            self.consider(device, start, &boot, Some(at));
        }
    }

    fn consider(
        &mut self,
        device: &dyn BlockDevice,
        start: u64,
        boot: &BootSector,
        backup: Option<u64>,
    ) {
        let key = Key {
            offset: start,
            volume_bytes: boot.volume_bytes,
            kind: kind_rank(boot.kind),
        };
        let known = self.found.contains_key(&key);
        if !known && self.found.len() >= MAX_CANDIDATES {
            self.truncated = true;
            return;
        }
        if !confirms(device, start, boot, backup.unwrap_or(start)) {
            return;
        }
        let found = self.found.entry(key).or_insert(Found {
            kind: boot.kind,
            primary: false,
            backup: false,
        });
        match backup {
            Some(_) => found.backup = true,
            None => found.primary = true,
        }
    }

    fn candidate(
        &self,
        device: &Arc<dyn BlockDevice>,
        key: &Key,
        found: &Found,
        table: Option<&PartitionTable>,
    ) -> Candidate {
        let room = device.size().saturating_sub(key.offset);
        let length = key.volume_bytes.min(room);
        let found_by = match (found.primary, found.backup) {
            (true, true) => FoundBy::Both,
            (false, true) => FoundBy::Backup,
            _ => FoundBy::Primary,
        };
        let boot = if found_by == FoundBy::Backup {
            BootCopy::Backup
        } else {
            BootCopy::Primary
        };
        Candidate {
            offset: key.offset,
            length,
            kind: found.kind,
            label: Volume::open_with(Arc::clone(device), key.offset, length, boot)
                .ok()
                .and_then(|volume| volume.label().map(str::to_owned)),
            found_by,
            boot,
            truncated: key.volume_bytes > room,
            relation: relation(table, key.offset, length),
            overlaps_candidate: false,
        }
    }
}

fn kind_rank(kind: FsKind) -> u8 {
    match kind {
        FsKind::Fat12 => 0,
        FsKind::Fat16 => 1,
        FsKind::Fat32 => 2,
        FsKind::Exfat => 3,
        FsKind::Ntfs => 4,
        _ => 5,
    }
}

/// 引导扇区之外的结构也对得上，才认作分区：FAT 的第 0 项是介质描述符、其余位全为 1（exFAT 的固定是 `F8`），
/// exFAT 的引导区校验和还要对；NTFS 的 `$MFT` 或 `$MFTMirr` 的第一条记录是可用的 `$MFT` 自身记录。
/// 判断规则与打开卷、文件系统检查共用（[`fat_media_ok`]、[`mft_self_record_ok`]）。
/// 单是引导扇区自洽不够：引导扇区的副本（备份、镜像文件里的、格式化前留下的）随处都有。
/// `region` 是这次用的引导区（主引导区或 exFAT 的备份引导区）在设备上的位置。
fn confirms(device: &dyn BlockDevice, start: u64, boot: &BootSector, region: u64) -> bool {
    let at = |relative: u64| start.checked_add(relative);
    let media_ok = |fat: u64, width: FatWidth, media: u8| {
        at(fat)
            .and_then(|fat| read(device, fat, 4))
            .and_then(|bytes| fat_entry_raw(width, 0, &bytes))
            .is_some_and(|entry0| fat_media_ok(width, media, entry0))
    };
    match &boot.layout {
        BootLayout::Fat { width, media, fats } => {
            fats.iter().any(|&fat| media_ok(fat, *width, *media))
        }
        BootLayout::Exfat { fat_offset } => {
            exfat_checksum_ok(device, region, boot.bytes_per_sector)
                && media_ok(*fat_offset, FatWidth::Exfat, EXFAT_MEDIA)
        }
        BootLayout::Ntfs {
            mft_offset,
            mirror_offset,
            record_size,
        } => {
            let Ok(record_size) = usize::try_from(*record_size) else {
                return false;
            };
            [*mft_offset, *mirror_offset].iter().any(|&record| {
                at(record)
                    .and_then(|record| read(device, record, record_size))
                    .is_some_and(|bytes| mft_self_record_ok(&bytes))
            })
        }
    }
}

fn read(device: &dyn BlockDevice, at: u64, len: usize) -> Option<Vec<u8>> {
    let mut bytes = vec![0u8; len];
    device.read_exact_at(at, &mut bytes).ok().map(|()| bytes)
}

/// exFAT 引导区（12 个扇区）的第 11 个扇区填满了前 11 个扇区的校验和，计算时跳过卷标志与使用百分比。
fn exfat_checksum_ok(device: &dyn BlockDevice, region: u64, bytes_per_sector: u64) -> bool {
    let Ok(sector) = usize::try_from(bytes_per_sector) else {
        return false;
    };
    let mut bytes = vec![0u8; sector * 12];
    if device.read_exact_at(region, &mut bytes).is_err() {
        return false;
    }
    let (covered, stored) = bytes.split_at(sector * 11);
    exfat_stored_checksum(stored) == Some(exfat_boot_checksum(covered))
}

/// 与分区表比对：起点相同算表里已有；否则与哪个分区有重叠就算重叠。
/// 整块设备（无分区表）与 GPT 的保护项覆盖整块盘，不拿来判断重叠。
fn relation(table: Option<&PartitionTable>, offset: u64, length: u64) -> Relation {
    let Some(table) = table else {
        return Relation::Lost;
    };
    if let Some(partition) = table
        .partitions
        .iter()
        .find(|partition| partition.offset == offset)
    {
        return Relation::InTable {
            number: partition.number,
        };
    }
    table
        .partitions
        .iter()
        .filter(|partition| {
            !matches!(partition.kind, PartitionKind::Whole)
                && partition.role() != Some(Role::GptProtective)
        })
        .find(|partition| overlaps(partition.offset, partition.length, offset, length))
        .map_or(Relation::Lost, |partition| Relation::Overlaps {
            number: partition.number,
        })
}

fn overlaps(a_offset: u64, a_length: u64, b_offset: u64, b_length: u64) -> bool {
    a_offset < b_offset.saturating_add(b_length) && b_offset < a_offset.saturating_add(a_length)
}

/// 候选分区按起点排好序：后一个的起点落在前面某个的范围里就是重叠。
fn mark_overlapping(candidates: &mut [Candidate]) {
    let ranges: Vec<(u64, u64)> = candidates
        .iter()
        .map(|candidate| (candidate.offset, candidate.length))
        .collect();
    for (index, candidate) in candidates.iter_mut().enumerate() {
        candidate.overlaps_candidate =
            ranges.iter().enumerate().any(|(other, &(offset, length))| {
                other != index && overlaps(offset, length, candidate.offset, candidate.length)
            });
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io;

    use yisi_fs::samples::MemoryDisk;
    use yisi_io::IoError;

    use super::*;
    use crate::samples::{SearchDisk, search_disk};

    struct Control {
        cancel_after: Option<usize>,
        checks: Cell<usize>,
        last: Cell<(u64, u64)>,
    }

    impl Control {
        fn new() -> Self {
            Self {
                cancel_after: None,
                checks: Cell::new(0),
                last: Cell::new((0, 0)),
            }
        }
    }

    impl SearchControl for Control {
        fn is_cancelled(&self) -> bool {
            self.checks.set(self.checks.get() + 1);
            self.cancel_after
                .is_some_and(|limit| self.checks.get() > limit)
        }

        fn progress(&self, done: u64, total: u64) {
            self.last.set((done, total));
        }
    }

    fn memory(disk: &SearchDisk) -> Arc<dyn BlockDevice> {
        Arc::new(MemoryDisk::new(disk.image.clone(), 512))
    }

    fn run(disk: &SearchDisk) -> SearchReport {
        let control = Control::new();
        let report = search(&memory(disk), &control).unwrap();
        assert_eq!(control.last.get(), (report.size, report.size));
        report
    }

    fn find(report: &SearchReport, offset: u64) -> &Candidate {
        report
            .candidates
            .iter()
            .find(|candidate| candidate.offset == offset)
            .unwrap_or_else(|| panic!("没有找到 {offset} 处的分区：{:?}", report.candidates))
    }

    /// 按候选分区打开卷（靠备份找到的用备份引导扇区），读出根目录下的一个文件。
    fn file_in_root(device: Arc<dyn BlockDevice>, candidate: &Candidate, name: &str) -> Vec<u8> {
        let volume =
            Volume::open_with(device, candidate.offset, candidate.length, candidate.boot).unwrap();
        let listing = volume.list(&volume.root()).unwrap();
        let entry = listing
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("根目录里没有 {name}"));
        let mut reader = volume.open_file(&entry.file).unwrap();
        let mut data = Vec::new();
        io::Read::read_to_end(&mut reader, &mut data).unwrap();
        data
    }

    #[test]
    fn partitions_in_an_intact_table_are_reported_as_listed() {
        let disk = search_disk();
        let report = run(&disk);
        let offsets: Vec<u64> = report.candidates.iter().map(|c| c.offset).collect();
        // 孤立的引导扇区、各种把备份当作主引导扇区的推算都核对不过，不出现
        assert_eq!(
            offsets,
            vec![disk.fat32.0, disk.ntfs.0, disk.nested_fat12.0, disk.exfat.0]
        );
        assert!(!report.candidates_truncated);
        assert_eq!(report.unreadable_bytes, 0);

        let fat32 = find(&report, disk.fat32.0);
        assert_eq!(fat32.kind, FsKind::Fat32);
        assert_eq!(fat32.length, disk.fat32.1);
        assert_eq!(fat32.found_by, FoundBy::Both);
        assert_eq!(fat32.label.as_deref(), Some("DATA32"));
        assert_eq!(fat32.relation, Relation::InTable { number: 1 });
        assert!(!fat32.overlaps_candidate);

        let ntfs = find(&report, disk.ntfs.0);
        assert_eq!((ntfs.kind, ntfs.length), (FsKind::Ntfs, disk.ntfs.1));
        assert_eq!(ntfs.found_by, FoundBy::Both);
        assert_eq!(ntfs.relation, Relation::InTable { number: 2 });
        assert!(ntfs.overlaps_candidate, "里面有 FAT12 镜像文件");

        let exfat = find(&report, disk.exfat.0);
        assert_eq!((exfat.kind, exfat.length), (FsKind::Exfat, disk.exfat.1));
        assert_eq!(exfat.found_by, FoundBy::Both);
        assert_eq!(exfat.label.as_deref(), Some("EXDATA"));
        assert_eq!(exfat.relation, Relation::InTable { number: 3 });
        assert_eq!(exfat.boot, BootCopy::Primary);
    }

    #[test]
    fn a_volume_inside_a_partition_is_flagged_as_overlapping() {
        let disk = search_disk();
        let report = run(&disk);
        let nested = find(&report, disk.nested_fat12.0);
        assert_eq!(nested.kind, FsKind::Fat12);
        assert_eq!(nested.length, disk.nested_fat12.1);
        assert_eq!(nested.found_by, FoundBy::Primary);
        assert_eq!(nested.relation, Relation::Overlaps { number: 2 });
        assert!(nested.overlaps_candidate);
        assert_eq!(
            file_in_root(memory(&disk), nested, "nested.txt"),
            disk.nested
        );
    }

    #[test]
    fn cleared_table_leaves_every_partition_lost() {
        let mut disk = search_disk();
        disk.clear_table();
        let report = run(&disk);
        assert_eq!(report.candidates.len(), 4);
        for offset in [disk.fat32.0, disk.ntfs.0, disk.nested_fat12.0, disk.exfat.0] {
            assert_eq!(find(&report, offset).relation, Relation::Lost);
        }
        let fat32 = find(&report, disk.fat32.0);
        assert_eq!(file_in_root(memory(&disk), fat32, "notes.txt"), disk.notes);
        let ntfs = find(&report, disk.ntfs.0);
        assert_eq!(file_in_root(memory(&disk), ntfs, "report.bin"), disk.report);
    }

    #[test]
    fn partitions_with_only_a_backup_boot_sector_are_found_and_readable() {
        let mut disk = search_disk();
        disk.clear_table();
        for offset in [disk.fat32.0, disk.ntfs.0, disk.exfat.0] {
            disk.wipe_primary_boot(offset);
        }
        let report = run(&disk);

        let fat32 = find(&report, disk.fat32.0);
        assert_eq!(fat32.found_by, FoundBy::Backup);
        assert_eq!(fat32.boot, BootCopy::Backup);
        assert_eq!(fat32.label.as_deref(), Some("DATA32"));
        assert_eq!(file_in_root(memory(&disk), fat32, "notes.txt"), disk.notes);

        let ntfs = find(&report, disk.ntfs.0);
        assert_eq!(ntfs.found_by, FoundBy::Backup);
        assert_eq!(ntfs.length, disk.ntfs.1);
        assert_eq!(ntfs.boot, BootCopy::Backup);
        assert_eq!(file_in_root(memory(&disk), ntfs, "report.bin"), disk.report);

        let exfat = find(&report, disk.exfat.0);
        assert_eq!(exfat.found_by, FoundBy::Backup);
        assert_eq!(exfat.boot, BootCopy::Backup);
        assert_eq!(exfat.label.as_deref(), Some("EXDATA"));
        assert_eq!(file_in_root(memory(&disk), exfat, "photo.dat"), disk.photo);
    }

    #[test]
    fn boot_sector_without_matching_structures_is_rejected() {
        let mut disk = search_disk();
        // FAT32 的两份 FAT 开头都坏了：只剩引导扇区自洽，不认
        let at = disk.fat32.0 as usize;
        let fat_sectors = u64::from(u32::from_le_bytes(
            disk.image[at + 36..at + 40].try_into().unwrap(),
        ));
        for copy in 0..2u64 {
            let fat = disk.fat32.0 + (32 + copy * fat_sectors) * 512;
            disk.image[fat as usize] = 0;
        }
        let report = run(&disk);
        let offsets: Vec<u64> = report.candidates.iter().map(|c| c.offset).collect();
        assert!(!offsets.contains(&disk.fat32.0), "{offsets:?}");
        assert!(!offsets.contains(&disk.stray_boot), "{offsets:?}");
    }

    #[test]
    fn volume_past_the_end_is_truncated() {
        let mut disk = search_disk();
        // 镜像截在 exFAT 中间
        let end = disk.exfat.0 as usize + (4 << 20);
        disk.image.truncate(end);
        let report = run(&disk);
        let exfat = find(&report, disk.exfat.0);
        assert!(exfat.truncated);
        assert_eq!(exfat.length, end as u64 - disk.exfat.0);
    }

    /// 落在坏区里的读取报错的设备。
    struct FailingDisk {
        inner: MemoryDisk,
        bad: (u64, u64),
    }

    impl BlockDevice for FailingDisk {
        fn size(&self) -> u64 {
            self.inner.size()
        }

        fn sector_size(&self) -> u32 {
            512
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
            let end = offset + buf.len() as u64;
            if offset < self.bad.1 && self.bad.0 < end {
                return Err(IoError::Read {
                    offset,
                    len: buf.len(),
                    source: io::Error::other("模拟的介质错误"),
                });
            }
            self.inner.read_at(offset, buf)
        }
    }

    #[test]
    fn unreadable_chunks_are_skipped_and_counted() {
        let disk = search_disk();
        // exFAT 开头的扇区读不出：整段 1 MiB 跳过，主备引导区都在里面，找不到它
        let device: Arc<dyn BlockDevice> = Arc::new(FailingDisk {
            inner: MemoryDisk::new(disk.image.clone(), 512),
            bad: (disk.exfat.0, disk.exfat.0 + 512),
        });
        let report = search(&device, &Control::new()).unwrap();
        assert_eq!(report.unreadable_bytes, 1 << 20);
        let offsets: Vec<u64> = report.candidates.iter().map(|c| c.offset).collect();
        assert!(!offsets.contains(&disk.exfat.0), "{offsets:?}");
        assert!(offsets.contains(&disk.ntfs.0), "{offsets:?}");
    }

    #[test]
    fn cancellation_stops_the_search() {
        let disk = search_disk();
        let control = Control {
            cancel_after: Some(3),
            ..Control::new()
        };
        assert!(search(&memory(&disk), &control).is_none());
        assert_eq!(control.last.get().0, 3 << 20);
    }
}
