//! 丢失分区搜索：找 FAT12/16/32、exFAT、NTFS 的引导扇区，每个引导扇区既当作主引导扇区、
//! 也当作备份引导扇区推算分区起点，再核对文件系统的结构，合并成候选分区，并与现有分区表比对。只读，不改分区表。
//!
//! 两种搜法只是读哪些位置不同，认分区、比对分区表用同一套代码（[`Finder`]、[`Search::candidates`]）：
//! - 深度搜索从头到尾读一遍设备，逐个 512 字节看；
//! - 快速搜索只看分区通常开始的位置（[`next_start`]），找到一个分区就跳到它的末尾接着找。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Instant;

use yisi_fs::boot::{exfat_boot_checksum, exfat_stored_checksum};
use yisi_fs::format::{FatWidth, fat_entry_raw, fat_media_ok};
use yisi_fs::{BootCopy, BootLayout, BootSector, FsKind, Volume, inspect_boot, mft_self_record_ok};
use yisi_io::{BlockDevice, ScanControl, scan_device};
use yisi_part::{PartitionKind, PartitionTable, Role};

/// exFAT 的 FAT 第 0 项固定记介质描述符 `F8`（exFAT 的引导扇区里没有这一项）。
const EXFAT_MEDIA: u8 = 0xF8;
/// 引导扇区按 512 字节对齐找：4 KiB 扇区的盘上分区同样落在 512 字节的边界上。
const SECTOR: usize = 512;
/// 最多记这么多个候选分区，挡住装满了磁盘镜像文件的盘。
pub const MAX_CANDIDATES: usize = 1000;
/// 快速搜索看的起点：Windows Vista 以后与常见分区工具按 1 MiB 对齐。
const ALIGNMENT: u64 = 1 << 20;
/// 按柱面分区的旧盘（Windows XP 年代）：分区从柱面边界开始，第一个分区与逻辑分区在柱面之后 63 个扇区。
const CYLINDER: u64 = 255 * 63 * 512;
const TRACK: u64 = 63 * 512;
/// 小容量盘按 64 KiB 排分区（实测 diskpart 在 400 MB 的盘上）：第一个分区从 64 KiB 开始，逻辑分区在 EBR 之后 64 KiB。
const SMALL_ALIGNMENT: u64 = 64 << 10;
/// 一个分区结束之后，下一个分区常在这几处开始：紧接着、隔一个磁道（旧式的 EBR 与空隙）、隔 64 KiB、隔 1 MiB。
const FOLLOW_GAPS: [u64; 4] = [0, TRACK, SMALL_ALIGNMENT, ALIGNMENT];
/// 快速搜索在每个起点读的范围（按扇区数）：起点前一个扇区（到这里结束的 NTFS 卷的备份）、起点上的主引导扇区、
/// FAT32 第 6 扇区的备份、exFAT 第 12 扇区起的备份引导区。
const WINDOW_BEFORE: u64 = 1;
const WINDOW_AFTER: u64 = 13;

/// 怎么搜。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// 只看分区通常开始的位置，找到一个分区就跳到它的末尾。起点不对齐、又不紧跟在别的分区之后，两份引导扇区所在的位置
    /// 都不在这些地方的分区找不到。
    Quick,
    /// 从头到尾读一遍。
    Deep,
}

/// 搜索的控制：读盘的取消、暂停与进度之外，找到的分区有变化时交出当前的全部候选分区（按起点排序）。
pub trait SearchControl: ScanControl {
    fn found(&self, candidates: &[Candidate], truncated: bool);
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
    pub mode: SearchMode,
    /// 现有的分区表；读不出时为空。
    pub table: Option<PartitionTable>,
    /// 按起点排序。
    pub candidates: Vec<Candidate>,
    /// 候选分区超过 [`MAX_CANDIDATES`]，后面的没有记。
    pub candidates_truncated: bool,
    /// 读不出、跳过的字节数（深度搜索整段 1 MiB 跳过，快速搜索跳过那一个起点），其中的引导扇区找不到。
    pub unreadable_bytes: u64,
    /// 被停下时搜到的位置；停下之前找到的分区照样列出。搜完时为空。
    pub stopped_at: Option<u64>,
    pub elapsed_ms: u64,
}

/// 搜索设备。被停下（`control` 取消）时也给出结果：停下之前找到的分区与停在哪里。
pub fn search(
    device: &Arc<dyn BlockDevice>,
    mode: SearchMode,
    control: &dyn SearchControl,
) -> SearchReport {
    let started = Instant::now();
    let table = yisi_part::read_table(device.as_ref())
        .inspect_err(|error| tracing::warn!(%error, "读不出分区表，候选分区都按丢失处理"))
        .ok();
    let mut search = Search {
        device: Arc::clone(device),
        table,
        finder: Finder::default(),
        labels: HashMap::new(),
        unreadable_bytes: 0,
    };
    let stopped_at = match mode {
        SearchMode::Quick => search.quick(control),
        SearchMode::Deep => search.deep(control),
    };
    let candidates = search.candidates();
    SearchReport {
        size: device.size(),
        mode,
        table: search.table,
        candidates,
        candidates_truncated: search.finder.truncated,
        unreadable_bytes: search.unreadable_bytes,
        stopped_at,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

struct Search {
    device: Arc<dyn BlockDevice>,
    table: Option<PartitionTable>,
    finder: Finder,
    /// 已经取过的卷标，按候选分区与是否按备份引导扇区打开：每次交出候选分区时不再重新打开卷。
    labels: HashMap<(Key, bool), Option<String>>,
    unreadable_bytes: u64,
}

impl Search {
    /// 从头到尾读一遍；被停下时返回读到的位置。
    fn deep(&mut self, control: &dyn SearchControl) -> Option<u64> {
        let device = Arc::clone(&self.device);
        let mut reached = 0u64;
        let finished = scan_device(device.as_ref(), control, |offset, block| {
            let length = match block {
                Ok(bytes) => {
                    self.examine(offset, bytes);
                    bytes.len() as u64
                }
                Err((length, error)) => {
                    tracing::warn!(offset, length, %error, "读不出，跳过");
                    self.unreadable_bytes += length;
                    length
                }
            };
            reached = offset + length;
            self.report_changes(control);
        });
        (!finished).then_some(reached)
    }

    /// 只看可能的起点（[`next_start`]），每处读起点附近的几个扇区；找到的分区跳过，最后看一眼设备的最后一个扇区
    /// （到设备末尾结束的 NTFS 卷的备份）。被停下时返回停在的起点。
    ///
    /// 对齐的起点之外另看：分区表里各分区的起点、64 KiB 处（小容量盘的第一个分区），以及分区表里的分区与找到的分区
    /// 结束之后的几处（[`FOLLOW_GAPS`]）：不按 1 MiB 对齐的盘上，后面的分区与逻辑分区就从那里接着排。
    fn quick(&mut self, control: &dyn SearchControl) -> Option<u64> {
        let size = self.device.size();
        let sector = u64::from(self.device.sector_size()).max(SECTOR as u64);
        let mut starts = BTreeSet::from([SMALL_ALIGNMENT]);
        for partition in self.table.iter().flat_map(|table| &table.partitions) {
            starts.insert(partition.offset);
            follow(&mut starts, partition.offset.saturating_add(partition.length));
        }
        let mut followed = 0;
        control.progress(0, size);
        let mut from = 0u64;
        while let Some(at) = next_start(from, size, &starts) {
            if control.is_cancelled() {
                return Some(at);
            }
            let start = at.saturating_sub(WINDOW_BEFORE * sector);
            self.examine_range(start, at.saturating_add(WINDOW_AFTER * sector).min(size));
            if self.finder.found.len() != followed {
                followed = self.finder.found.len();
                for key in self.finder.found.keys() {
                    follow(&mut starts, key.offset.saturating_add(key.volume_bytes));
                }
            }
            from = self.end_of_found_at(at).unwrap_or(0).max(at + 1);
            self.report_changes(control);
            control.progress(from.min(size), size);
        }
        if size >= sector {
            self.examine_range(size - sector, size);
            self.report_changes(control);
        }
        control.progress(size, size);
        None
    }

    fn examine(&mut self, offset: u64, bytes: &[u8]) {
        for (index, sector) in bytes.as_chunks::<SECTOR>().0.iter().enumerate() {
            let at = offset + (index * SECTOR) as u64;
            self.finder.examine(self.device.as_ref(), at, sector);
        }
    }

    fn examine_range(&mut self, start: u64, end: u64) {
        let Ok(length) = usize::try_from(end.saturating_sub(start)) else {
            return;
        };
        let mut bytes = vec![0u8; length];
        match self.device.read_exact_at(start, &mut bytes) {
            Ok(()) => self.examine(start, &bytes),
            Err(error) => {
                tracing::warn!(offset = start, length, %error, "读不出，跳过");
                self.unreadable_bytes += length as u64;
            }
        }
    }

    /// 起点正好在 `at` 的候选分区里最靠后的末尾：快速搜索从那里接着找。
    fn end_of_found_at(&self, at: u64) -> Option<u64> {
        let first = Key {
            offset: at,
            volume_bytes: 0,
            kind: 0,
        };
        let last = Key {
            offset: at,
            volume_bytes: u64::MAX,
            kind: u8::MAX,
        };
        self.finder
            .found
            .range(first..=last)
            .map(|(key, _)| key.offset.saturating_add(key.volume_bytes))
            .max()
    }

    fn report_changes(&mut self, control: &dyn SearchControl) {
        if std::mem::take(&mut self.finder.changed) {
            let candidates = self.candidates();
            control.found(&candidates, self.finder.truncated);
        }
    }

    /// 当前找到的全部候选分区，按起点排序，标好与分区表、与彼此的关系。
    fn candidates(&mut self) -> Vec<Candidate> {
        let size = self.device.size();
        let mut candidates = Vec::with_capacity(self.finder.found.len());
        for (key, found) in &self.finder.found {
            let room = size.saturating_sub(key.offset);
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
            let device = &self.device;
            let label = self
                .labels
                .entry((*key, boot == BootCopy::Backup))
                .or_insert_with(|| {
                    Volume::open_with(Arc::clone(device), key.offset, length, boot)
                        .ok()
                        .and_then(|volume| volume.label().map(str::to_owned))
                })
                .clone();
            candidates.push(Candidate {
                offset: key.offset,
                length,
                kind: found.kind,
                label,
                found_by,
                boot,
                truncated: key.volume_bytes > room,
                relation: relation(self.table.as_ref(), key.offset, length),
                overlaps_candidate: false,
            });
        }
        mark_overlapping(&mut candidates);
        candidates
    }
}

/// 在 `end`（一个分区的末尾）之后的几处接着看。
fn follow(starts: &mut BTreeSet<u64>, end: u64) {
    starts.extend(FOLLOW_GAPS.iter().filter_map(|&gap| end.checked_add(gap)));
}

/// 快速搜索的下一个起点：不小于 `from`、在设备之内的最小一个，取自 1 MiB 边界、柱面边界与柱面之后 63 个扇区
/// （含第 63 扇区）与另外记下的起点 `starts`（分区表里的起点、分区结束之后的几处等，见 [`Search::quick`]）。
/// 第 0 扇区是 1 MiB 边界之一（不分区直接格式化的 U 盘）。
fn next_start(from: u64, size: u64, starts: &BTreeSet<u64>) -> Option<u64> {
    // 不小于 from 的 base + k * unit
    let up = |unit: u64, base: u64| {
        if from <= base {
            Some(base)
        } else {
            (from - base)
                .div_ceil(unit)
                .checked_mul(unit)
                .and_then(|step| step.checked_add(base))
        }
    };
    [
        up(ALIGNMENT, 0),
        up(CYLINDER, 0),
        up(CYLINDER, TRACK),
        starts.range(from..).next().copied(),
    ]
    .into_iter()
    .flatten()
    .filter(|&at| at < size)
    .min()
}

/// 一个假设的分区：起点、卷长度、文件系统。按起点排序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    /// 上次交出候选分区之后，有新的候选分区或靠哪份引导扇区找到变了。
    changed: bool,
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
        let flag = match backup {
            Some(_) => &mut found.backup,
            None => &mut found.primary,
        };
        self.changed |= !*flag;
        *flag = true;
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
    use std::cell::{Cell, RefCell};
    use std::io;
    use std::sync::atomic::{AtomicU64, Ordering};

    use yisi_fs::samples::{ExfatImage, MemoryDisk};
    use yisi_io::IoError;

    use super::*;
    use crate::samples::{SearchDisk, search_disk};

    struct Control {
        cancel_after: Option<usize>,
        checks: Cell<usize>,
        last: Cell<(u64, u64)>,
        /// 最近一次交出的候选分区，与交过几次。
        found: RefCell<Vec<Candidate>>,
        found_calls: Cell<usize>,
    }

    impl Control {
        fn new() -> Self {
            Self {
                cancel_after: None,
                checks: Cell::new(0),
                last: Cell::new((0, 0)),
                found: RefCell::new(Vec::new()),
                found_calls: Cell::new(0),
            }
        }
    }

    impl SearchControl for Control {
        fn found(&self, candidates: &[Candidate], _truncated: bool) {
            self.found_calls.set(self.found_calls.get() + 1);
            *self.found.borrow_mut() = candidates.to_vec();
        }
    }

    impl ScanControl for Control {
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
        run_on(&memory(disk), SearchMode::Deep)
    }

    fn run_on(device: &Arc<dyn BlockDevice>, mode: SearchMode) -> SearchReport {
        let control = Control::new();
        let report = search(device, mode, &control);
        assert_eq!(report.stopped_at, None);
        assert_eq!(control.last.get(), (report.size, report.size));
        // 边搜边交出的最后一份就是结果
        assert_eq!(*control.found.borrow(), report.candidates);
        report
    }

    /// 记下读了多少字节的设备。
    struct Counting {
        inner: MemoryDisk,
        bytes: AtomicU64,
    }

    impl BlockDevice for Counting {
        fn size(&self) -> u64 {
            self.inner.size()
        }

        fn sector_size(&self) -> u32 {
            512
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
            self.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
            self.inner.read_at(offset, buf)
        }
    }

    fn counting(image: Vec<u8>) -> Arc<Counting> {
        Arc::new(Counting {
            inner: MemoryDisk::new(image, 512),
            bytes: AtomicU64::new(0),
        })
    }

    /// 两种搜法对同一个分区的结论相同（与别的候选分区是否重叠除外：快速搜索不看分区里面）。
    fn same_partition(quick: &Candidate, deep: &Candidate) {
        let mut deep = deep.clone();
        deep.overlaps_candidate = quick.overlaps_candidate;
        assert_eq!(*quick, deep);
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
        let report = search(&device, SearchMode::Deep, &Control::new());
        assert_eq!(report.unreadable_bytes, 1 << 20);
        let offsets: Vec<u64> = report.candidates.iter().map(|c| c.offset).collect();
        assert!(!offsets.contains(&disk.exfat.0), "{offsets:?}");
        assert!(offsets.contains(&disk.ntfs.0), "{offsets:?}");
    }

    #[test]
    fn stopping_keeps_what_was_found() {
        let disk = search_disk();
        // 读完前 3 MiB 停下：第 1 MiB 起的 FAT32 已经找到
        let control = Control {
            cancel_after: Some(3),
            ..Control::new()
        };
        let report = search(&memory(&disk), SearchMode::Deep, &control);
        assert_eq!(report.stopped_at, Some(3 << 20));
        assert_eq!(control.last.get().0, 3 << 20);
        let offsets: Vec<u64> = report.candidates.iter().map(|c| c.offset).collect();
        assert_eq!(offsets, [disk.fat32.0]);
        assert_eq!(find(&report, disk.fat32.0).found_by, FoundBy::Both);
        assert_eq!(*control.found.borrow(), report.candidates);

        // 快速搜索看完第 0 扇区就停：停在下一个起点（第 63 扇区），什么也没找到
        let control = Control {
            cancel_after: Some(1),
            ..Control::new()
        };
        let report = search(&memory(&disk), SearchMode::Quick, &control);
        assert_eq!(report.stopped_at, Some(63 * 512));
        assert!(report.candidates.is_empty());
        assert_eq!(control.found_calls.get(), 0);
    }

    #[test]
    fn quick_search_skips_over_found_partitions() {
        let mut disk = search_disk();
        disk.clear_table();
        let deep = run(&disk);
        let device = counting(disk.image.clone());
        let quick = run_on(
            &(Arc::clone(&device) as Arc<dyn BlockDevice>),
            SearchMode::Quick,
        );
        assert_eq!(quick.mode, SearchMode::Quick);
        // NTFS 分区里的 FAT12 镜像文件被跳过了，其余与深度搜索相同
        let offsets: Vec<u64> = quick.candidates.iter().map(|c| c.offset).collect();
        assert_eq!(offsets, [disk.fat32.0, disk.ntfs.0, disk.exfat.0]);
        for candidate in &quick.candidates {
            same_partition(candidate, find(&deep, candidate.offset));
        }
        assert!(!find(&quick, disk.ntfs.0).overlaps_candidate);
        let read = device.bytes.load(Ordering::Relaxed);
        assert!(read < quick.size / 8, "读了 {read} 字节");
    }

    #[test]
    fn quick_search_finds_partitions_by_their_backups() {
        let mut disk = search_disk();
        disk.clear_table();
        for offset in [disk.fat32.0, disk.ntfs.0, disk.exfat.0] {
            disk.wipe_primary_boot(offset);
        }
        let deep = run(&disk);
        let quick = run_on(&memory(&disk), SearchMode::Quick);
        for offset in [disk.fat32.0, disk.ntfs.0, disk.exfat.0] {
            let candidate = find(&quick, offset);
            assert_eq!(candidate.found_by, FoundBy::Backup);
            same_partition(candidate, find(&deep, offset));
        }
    }

    #[test]
    fn a_volume_on_the_whole_device_is_found_at_once() {
        // 不分区直接格式化的 U 盘：第 0 扇区就是 exFAT 的引导扇区
        let volume = ExfatImage::new().finish();
        let size = volume.len() as u64;
        let device = counting(volume);
        let report = run_on(
            &(Arc::clone(&device) as Arc<dyn BlockDevice>),
            SearchMode::Quick,
        );
        assert_eq!(report.candidates.len(), 1);
        let candidate = &report.candidates[0];
        assert_eq!((candidate.offset, candidate.kind), (0, FsKind::Exfat));
        assert_eq!(candidate.length, size);
        let read = device.bytes.load(Ordering::Relaxed);
        assert!(read < 1 << 20, "读了 {read} 字节");
    }

    #[test]
    fn an_unaligned_partition_needs_the_deep_search() {
        // 起点既不在 1 MiB 边界、也不在柱面边界上
        let volume = ExfatImage::new().finish();
        let at = (3 << 20) + (64 << 10);
        let mut image = vec![0u8; at + volume.len() + (1 << 20)];
        image[at..at + volume.len()].copy_from_slice(&volume);
        let device: Arc<dyn BlockDevice> = Arc::new(MemoryDisk::new(image, 512));
        assert!(run_on(&device, SearchMode::Quick).candidates.is_empty());
        let deep = run_on(&device, SearchMode::Deep);
        let offsets: Vec<u64> = deep.candidates.iter().map(|c| c.offset).collect();
        assert_eq!(offsets, [at as u64]);
    }

    #[test]
    fn quick_search_follows_partitions_laid_out_every_64_kib() {
        // 小容量盘的排法：第一个分区在 64 KiB，后面的各在前一个结束之后 64 KiB（逻辑分区前面是 EBR），都不在 1 MiB 边界上
        let volume = ExfatImage::new().finish();
        let length = volume.len() as u64;
        let starts = [
            SMALL_ALIGNMENT,
            2 * SMALL_ALIGNMENT + length,
            3 * SMALL_ALIGNMENT + 2 * length,
        ];
        let mut image = vec![0u8; (starts[2] + length + ALIGNMENT) as usize];
        for &at in &starts {
            image[at as usize..(at + length) as usize].copy_from_slice(&volume);
        }
        let device = counting(image);
        let quick = run_on(
            &(Arc::clone(&device) as Arc<dyn BlockDevice>),
            SearchMode::Quick,
        );
        let offsets: Vec<u64> = quick.candidates.iter().map(|c| c.offset).collect();
        assert_eq!(offsets, starts);
        assert!(quick.candidates.iter().all(|c| c.length == length));
        let read = device.bytes.load(Ordering::Relaxed);
        assert!(read < quick.size / 8, "读了 {read} 字节");
    }

    #[test]
    fn quick_search_looks_right_after_the_partitions_in_the_table() {
        // 表里的分区认不出（引导区全没了），丢失的分区在它结束之后 64 KiB：按表里分区的末尾接着看
        let volume = ExfatImage::new().finish();
        let length = volume.len() as u64;
        let listed = SMALL_ALIGNMENT;
        let lost = listed + length + SMALL_ALIGNMENT;
        let mut image = yisi_part::samples::mbr_disk(
            (lost + length + ALIGNMENT) as usize,
            &[yisi_part::samples::MbrEntry::new(
                0x07,
                (listed / 512) as u32,
                (length / 512) as u32,
            )],
        );
        image[lost as usize..(lost + length) as usize].copy_from_slice(&volume);
        let device: Arc<dyn BlockDevice> = Arc::new(MemoryDisk::new(image, 512));
        let quick = run_on(&device, SearchMode::Quick);
        let offsets: Vec<u64> = quick.candidates.iter().map(|c| c.offset).collect();
        assert_eq!(offsets, [lost]);
        assert_eq!(quick.candidates[0].relation, Relation::Lost);
    }

    #[test]
    fn quick_search_starts_are_aligned_old_style_or_from_the_table() {
        let size = 1 << 40;
        let after = |from: u64| next_start(from, size, &BTreeSet::from([1000 * 512])).unwrap();
        assert_eq!(after(0), 0);
        assert_eq!(after(1), TRACK);
        assert_eq!(after(TRACK + 1), 1000 * 512);
        assert_eq!(after(1000 * 512 + 1), ALIGNMENT);
        assert_eq!(after(7 * ALIGNMENT + 1), CYLINDER);
        assert_eq!(after(CYLINDER + 1), CYLINDER + TRACK);
        assert_eq!(next_start(size - 511, size, &BTreeSet::new()), None);
    }
}
