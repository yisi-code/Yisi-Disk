//! 引导扇区诊断：对分区表里的每个分区，读出主引导扇区与备份引导扇区，逐项核对后列出问题，
//! 比较主备之间哪些字段不一样，核对分区表里的类型、长度与引导扇区是否相符，给出结论。只读。
//!
//! 依据微软公开的 FAT 与 exFAT 规范；NTFS 依据公开的结构资料。备份的位置与 `yisi-fs` 打开卷时找的一致
//! （[`yisi_fs::boot::backup_candidates`]），诊断说「备份可用」时就能用备份浏览。

use std::sync::Arc;

use yisi_fs::boot::{
    BackupFamily, Candidate, backup_candidates, backup_kind, exfat_boot_checksum,
    exfat_stored_checksum,
};
use yisi_fs::{BootCopy, FsKind, Volume};
use yisi_io::BlockDevice;
use yisi_part::bytes::{le16, le32, le64};
use yisi_part::{PartError, Partition, PartitionKind, PartitionTable, Role};

/// 分区比引导扇区记的卷大这么多以上，才算长度不符（对齐、分区工具留的零头不算）。
const SLACK: u64 = 1 << 20;
/// exFAT 的引导区：引导扇区、8 个扩展引导扇区、OEM 参数、保留、校验和扇区。
const EXFAT_BOOT_REGION_SECTORS: u64 = 12;
/// NTFS 的簇最大 2 MiB。
const MAX_NTFS_CLUSTER: u64 = 2 << 20;

/// 一个分区的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 主引导扇区正常；有备份的，备份也正常。
    Healthy,
    /// 主引导扇区正常，备份有问题。
    BackupDamaged,
    /// 主引导扇区坏了，备份可用。
    PrimaryDamaged,
    /// 主引导扇区坏了，备份也坏了，或者这种文件系统没有备份（FAT12/16）。
    BothDamaged,
    /// 认得出，但不是 FAT、exFAT、NTFS（BitLocker、ext），这里不诊断。
    Unsupported,
    /// 两处都认不出是哪种文件系统：可能是空的分区、别的文件系统，或者引导扇区和备份都已面目全非。
    Unrecognized,
}

/// 一份引导扇区上发现的问题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// 读不出来。
    Unreadable,
    /// 整个扇区是同一个字节（多半是被清零了）。
    Blank,
    /// 结尾没有 `55 AA`。
    MissingSignature,
    /// 开头不是跳转指令。
    BadJump,
    /// exFAT / NTFS 的文件系统标识不对。
    BadOemId,
    /// 规定为 0 的旧 BPB 字段不是 0（exFAT 的第 11–63 字节、NTFS 的保留扇区数与 FAT 个数等）。
    BadLegacyFields,
    BadBytesPerSector(u64),
    /// 记的扇区大小与这份副本所在的位置不符（备份是按另一种扇区大小找到的）。
    SectorSizeMismatch {
        recorded: u64,
        expected: u64,
    },
    BadSectorsPerCluster(u64),
    /// FAT 的保留扇区数、exFAT 的 FAT 起点。
    BadReservedSectors(u64),
    BadFatCount(u64),
    /// FAT 的大小为 0，或装不下全部簇。
    BadFatSize(u64),
    BadMedia(u64),
    /// FAT12/16 的根目录项数为 0，或 FAT32 的不为 0。
    BadRootEntries(u64),
    BadTotalSectors(u64),
    /// 数据区不在卷内，或簇数与卷的长度对不上。
    BadLayout,
    BadRootCluster(u64),
    /// NTFS 的 MFT 记录大小、索引块大小不合法。
    BadRecordSize,
    BadIndexSize,
    /// NTFS 的 `$MFT`、`$MFTMirr` 起始簇不在卷内。
    MftOutside(u64),
    MirrorOutside(u64),
    /// exFAT 引导区的校验和不对；校验和扇区各处不一致时 `stored` 为空。
    ChecksumMismatch {
        stored: Option<u32>,
        computed: u32,
    },
}

/// 分区表与引导扇区不符的地方。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableIssue {
    /// 分区表里的类型不是这种文件系统用的（例如 FAT32 的分区标成了 NTFS / exFAT，或 Linux）。
    TypeMismatch,
    /// 引导扇区记的卷比分区长：分区表的长度可能被改小了，超出的部分读不到。
    VolumeLarger { volume: u64, partition: u64 },
    /// 分区比引导扇区记的卷长出不少：分区表的长度可能被改大了，或卷没有铺满分区。
    VolumeSmaller { volume: u64, partition: u64 },
}

/// 主备之间不一样的一个字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    pub field: &'static str,
    pub primary: u64,
    pub backup: u64,
}

/// 一份引导扇区的核对结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyReport {
    /// 在分区里的字节偏移。
    pub offset: u64,
    pub issues: Vec<Issue>,
    /// 引导扇区记的卷长度与簇大小（参数读得出时）。
    pub volume_bytes: Option<u64>,
    pub cluster_size: Option<u64>,
    pub sector_size: Option<u64>,
    /// 按这一份打开卷的结果；引导扇区有问题时不试，为 `None`。
    pub open: Option<Result<(), String>>,
    /// 按这一份打开后读到的卷标。
    pub label: Option<String>,
}

impl CopyReport {
    fn new(offset: u64) -> Self {
        Self {
            offset,
            issues: Vec::new(),
            volume_bytes: None,
            cluster_size: None,
            sector_size: None,
            open: None,
            label: None,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.issues.is_empty()
    }

    pub fn opened(&self) -> bool {
        matches!(self.open, Some(Ok(())))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionReport {
    pub partition: Partition,
    /// 认出的文件系统：先看主引导扇区，再看备份，最后按分区表里的类型推测；都不行时为 `Unknown`。
    pub filesystem: FsKind,
    pub primary: CopyReport,
    /// 找到的备份；FAT12/16 没有备份，认不出文件系统时也不找。
    pub backup: Option<CopyReport>,
    pub differences: Vec<Difference>,
    pub table_issues: Vec<TableIssue>,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootReport {
    pub table: PartitionTable,
    pub partitions: Vec<PartitionReport>,
}

/// 读分区表，逐个分区诊断引导扇区。只有分区表本身读不出时返回错误；分区里读不出的扇区记为 [`Issue::Unreadable`]。
pub fn diagnose(device: &Arc<dyn BlockDevice>) -> Result<BootReport, PartError> {
    let table = yisi_part::read_table(device.as_ref())?;
    let partitions = table
        .partitions
        .iter()
        .map(|partition| diagnose_partition(device, partition))
        .collect();
    Ok(BootReport { table, partitions })
}

/// 诊断一个分区。
pub fn diagnose_partition(device: &Arc<dyn BlockDevice>, partition: &Partition) -> PartitionReport {
    let reader = Reader { device, partition };
    let primary = reader.read(0, 512);
    let primary_bytes = primary.as_deref().unwrap_or(&[0u8; 512]);
    let candidates = backup_candidates(primary_bytes, device.sector_size(), partition.length);

    // 主引导扇区认得出的类型：参数自洽时按 `yisi-fs` 的识别，坏了按残留的标识
    let primary_kind = yisi_fs::detect(device.as_ref(), partition.offset, partition.length)
        .ok()
        .filter(|&kind| kind != FsKind::Unknown)
        .or_else(|| primary.as_deref().and_then(identify));
    let filesystem = primary_kind
        .or_else(|| {
            candidates.iter().find_map(|candidate| {
                let sector = reader.read(candidate.offset, 512)?;
                matches_family(candidate, &sector).then_some(family_kind(candidate.family))
            })
        })
        .or_else(|| hinted_by_table(partition))
        .unwrap_or(FsKind::Unknown);

    let mut report = PartitionReport {
        partition: partition.clone(),
        filesystem,
        primary: CopyReport::new(0),
        backup: None,
        differences: Vec::new(),
        table_issues: Vec::new(),
        verdict: Verdict::Unrecognized,
    };
    match filesystem {
        FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 | FsKind::Exfat | FsKind::Ntfs => {}
        FsKind::BitLocker | FsKind::Ext => {
            report.verdict = Verdict::Unsupported;
            return report;
        }
        FsKind::Unknown => return report,
    }

    report.primary = reader.check(filesystem, 0, primary.as_deref(), None);
    let family = backup_family(filesystem);
    if let Some(family) = family {
        let mine: Vec<&Candidate> = candidates
            .iter()
            .filter(|candidate| candidate.family == family)
            .collect();
        // 取第一份核对通过的；都不通过时取第一份认得出的，再不行取最可能的位置，好说明那里是什么情况
        let mut chosen = None;
        let mut identified = None;
        let mut fallback = None;
        for candidate in mine {
            let sector = reader.read(candidate.offset, 512);
            let checked = reader.check(
                filesystem,
                candidate.offset,
                sector.as_deref(),
                Some(candidate.sector_size),
            );
            let recognised = sector
                .as_deref()
                .is_some_and(|bytes| matches_family(candidate, bytes));
            let found = (checked, sector, recognised);
            if found.0.is_valid() {
                chosen = Some(found);
                break;
            }
            if recognised && identified.is_none() {
                identified = Some(found.clone());
            }
            if fallback.is_none() {
                fallback = Some(found);
            }
        }
        if let Some((backup, sector, recognised)) = chosen.or(identified).or(fallback) {
            if let (Some(primary_bytes), Some(backup_bytes)) =
                (primary.as_deref(), sector.as_deref())
                && primary_kind == Some(filesystem)
                && recognised
            {
                report.differences = differences(filesystem, primary_bytes, backup_bytes);
            }
            report.backup = Some(backup);
        }
    }

    if report.primary.is_valid() {
        report.primary = reader.try_open(report.primary, BootCopy::Primary);
    }
    if let Some(backup) = report.backup.take() {
        report.backup = Some(if backup.is_valid() {
            reader.try_open(backup, BootCopy::Backup)
        } else {
            backup
        });
    }

    let backup_valid = report.backup.as_ref().is_some_and(CopyReport::is_valid);
    let effective = if report.primary.is_valid() {
        Some(&report.primary)
    } else {
        report.backup.as_ref().filter(|backup| backup.is_valid())
    };
    if let Some(effective) = effective {
        report.table_issues = table_issues(partition, filesystem, effective, family);
    }
    report.verdict = match (report.primary.is_valid(), backup_valid) {
        (true, true) => Verdict::Healthy,
        (true, false) if report.backup.is_none() => Verdict::Healthy,
        (true, false) => Verdict::BackupDamaged,
        (false, true) => Verdict::PrimaryDamaged,
        (false, false) => Verdict::BothDamaged,
    };
    report
}

struct Reader<'a> {
    device: &'a Arc<dyn BlockDevice>,
    partition: &'a Partition,
}

impl Reader<'_> {
    /// 读分区里 `at` 起的 `len` 字节；超出分区或读不出时为 `None`。
    fn read(&self, at: u64, len: usize) -> Option<Vec<u8>> {
        let end = at.checked_add(len as u64)?;
        if end > self.partition.length {
            return None;
        }
        let mut buf = vec![0u8; len];
        self.device
            .read_exact_at(self.partition.offset.checked_add(at)?, &mut buf)
            .ok()?;
        Some(buf)
    }

    /// 核对一份引导扇区。`expected_sector` 是找到备份时假定的扇区大小（主引导扇区为 `None`）。
    fn check(
        &self,
        kind: FsKind,
        offset: u64,
        sector: Option<&[u8]>,
        expected_sector: Option<u64>,
    ) -> CopyReport {
        let mut report = CopyReport::new(offset);
        let Some(sector) = sector else {
            report.issues.push(Issue::Unreadable);
            return report;
        };
        if let Some(first) = sector.first()
            && sector.iter().all(|byte| byte == first)
        {
            report.issues.push(Issue::Blank);
            return report;
        }
        let checked = match kind {
            FsKind::Exfat => {
                let mut checked = check_exfat(sector, expected_sector);
                if let Some(sector_size) = checked.sector_size {
                    checked
                        .issues
                        .extend(self.exfat_checksum(offset, sector_size));
                }
                checked
            }
            FsKind::Ntfs => check_ntfs(sector, expected_sector),
            _ => check_fat(kind, sector, expected_sector),
        };
        report.issues = checked.issues;
        report.volume_bytes = checked.volume_bytes;
        report.cluster_size = checked.cluster_size;
        report.sector_size = checked.sector_size;
        report
    }

    /// exFAT 引导区（从 `offset` 起的 12 个扇区）的校验和。
    fn exfat_checksum(&self, offset: u64, sector_size: u64) -> Option<Issue> {
        let len = usize::try_from(sector_size.checked_mul(EXFAT_BOOT_REGION_SECTORS)?).ok()?;
        let Some(region) = self.read(offset, len) else {
            return Some(Issue::Unreadable);
        };
        let split =
            usize::try_from(sector_size.checked_mul(EXFAT_BOOT_REGION_SECTORS - 1)?).ok()?;
        let (body, checksum_sector) = region.split_at_checked(split)?;
        let computed = exfat_boot_checksum(body);
        let stored = exfat_stored_checksum(checksum_sector);
        (stored != Some(computed)).then_some(Issue::ChecksumMismatch { stored, computed })
    }

    /// 按这一份引导扇区打开卷，记下结果与卷标。
    fn try_open(&self, mut report: CopyReport, copy: BootCopy) -> CopyReport {
        match Volume::open_with(
            Arc::clone(self.device),
            self.partition.offset,
            self.partition.length,
            copy,
        ) {
            Ok(volume) => {
                report.label = volume.label().map(str::to_owned);
                report.open = Some(Ok(()));
            }
            Err(error) => report.open = Some(Err(error.to_string())),
        }
        report
    }
}

struct Checked {
    issues: Vec<Issue>,
    volume_bytes: Option<u64>,
    cluster_size: Option<u64>,
    /// 引导扇区记的扇区大小（合法时）。
    sector_size: Option<u64>,
}

fn check_jump(sector: &[u8], issues: &mut Vec<Issue>) {
    let jump = matches!(
        (sector.first(), sector.get(2)),
        (Some(0xEB), Some(0x90)) | (Some(0xE9), _)
    );
    if !jump {
        issues.push(Issue::BadJump);
    }
}

fn check_signature(sector: &[u8], issues: &mut Vec<Issue>) {
    if le16(sector, 510) != Some(0xAA55) {
        issues.push(Issue::MissingSignature);
    }
}

/// 扇区大小：合法时返回，不合法或与位置不符时记下问题。
fn check_sector_size(
    recorded: u64,
    valid: bool,
    expected: Option<u64>,
    issues: &mut Vec<Issue>,
) -> Option<u64> {
    if !valid {
        issues.push(Issue::BadBytesPerSector(recorded));
        return None;
    }
    if let Some(expected) = expected.filter(|&expected| expected != recorded) {
        issues.push(Issue::SectorSizeMismatch { recorded, expected });
    }
    Some(recorded)
}

fn check_fat(kind: FsKind, sector: &[u8], expected: Option<u64>) -> Checked {
    let mut issues = Vec::new();
    check_jump(sector, &mut issues);
    check_signature(sector, &mut issues);
    let field16 = |at| u64::from(le16(sector, at).unwrap_or(0));
    let field8 = |at| u64::from(sector.get(at).copied().unwrap_or(0));
    let bps = field16(11);
    let bps = check_sector_size(
        bps,
        matches!(bps, 512 | 1024 | 2048 | 4096),
        expected,
        &mut issues,
    );
    let spc = field8(13);
    let spc_valid = spc.is_power_of_two() && spc <= 128;
    if !spc_valid {
        issues.push(Issue::BadSectorsPerCluster(spc));
    }
    let reserved = field16(14);
    if reserved == 0 {
        issues.push(Issue::BadReservedSectors(reserved));
    }
    let fats = field8(16);
    if !matches!(fats, 1 | 2) {
        issues.push(Issue::BadFatCount(fats));
    }
    let media = field8(21);
    if !(media == 0xF0 || media >= 0xF8) {
        issues.push(Issue::BadMedia(media));
    }
    let root_entries = field16(17);
    let is_fat32 = kind == FsKind::Fat32;
    if is_fat32 != (root_entries == 0) {
        issues.push(Issue::BadRootEntries(root_entries));
    }
    let total = match field16(19) {
        0 => u64::from(le32(sector, 32).unwrap_or(0)),
        total => total,
    };
    if total == 0 {
        issues.push(Issue::BadTotalSectors(total));
    }
    let fat16_size = field16(22);
    let fat_size = if is_fat32 {
        u64::from(le32(sector, 36).unwrap_or(0))
    } else {
        fat16_size
    };
    if fat_size == 0 {
        issues.push(Issue::BadFatSize(0));
    } else if is_fat32 && fat16_size != 0 {
        // FAT32 的 16 位 FAT 扇区数必须为 0
        issues.push(Issue::BadFatSize(fat16_size));
    }

    let mut checked = Checked {
        issues: Vec::new(),
        volume_bytes: None,
        cluster_size: None,
        sector_size: bps,
    };
    if let Some(bps) = bps
        && spc_valid
        && reserved > 0
        && matches!(fats, 1 | 2)
        && total > 0
        && fat_size > 0
    {
        checked.volume_bytes = total.checked_mul(bps);
        checked.cluster_size = spc.checked_mul(bps);
        let root_sectors = root_entries.saturating_mul(32).div_ceil(bps);
        let first_data = fats
            .checked_mul(fat_size)
            .and_then(|fat_area| fat_area.checked_add(reserved))
            .and_then(|start| start.checked_add(root_sectors));
        match first_data.and_then(|first| total.checked_sub(first)) {
            None | Some(0) => issues.push(Issue::BadLayout),
            Some(data_sectors) => {
                let clusters = data_sectors / spc;
                let bits: u64 = match kind {
                    FsKind::Fat12 => 12,
                    FsKind::Fat16 => 16,
                    _ => 32,
                };
                let capacity = fat_size.saturating_mul(bps).saturating_mul(8) / bits;
                if capacity < clusters.saturating_add(2) {
                    issues.push(Issue::BadFatSize(fat_size));
                }
                if is_fat32 {
                    let root = u64::from(le32(sector, 44).unwrap_or(0));
                    if root < 2 || root >= clusters.saturating_add(2) {
                        issues.push(Issue::BadRootCluster(root));
                    }
                }
            }
        }
    }
    checked.issues = issues;
    checked
}

fn check_exfat(sector: &[u8], expected: Option<u64>) -> Checked {
    let mut issues = Vec::new();
    if sector.get(..3) != Some([0xEB, 0x76, 0x90].as_slice()) {
        issues.push(Issue::BadJump);
    }
    if sector.get(3..11) != Some(b"EXFAT   ".as_slice()) {
        issues.push(Issue::BadOemId);
    }
    if sector
        .get(11..64)
        .is_none_or(|legacy| legacy.iter().any(|&byte| byte != 0))
    {
        issues.push(Issue::BadLegacyFields);
    }
    check_signature(sector, &mut issues);
    let field8 = |at| u32::from(sector.get(at).copied().unwrap_or(0));
    let field32 = |at| u64::from(le32(sector, at).unwrap_or(0));
    let shift = field8(108);
    let bps = check_sector_size(
        1u64.checked_shl(shift).unwrap_or(0),
        (9..=12).contains(&shift),
        expected,
        &mut issues,
    );
    let cluster_shift = field8(109);
    let cluster_valid = bps.is_some() && shift + cluster_shift <= 25;
    if bps.is_some() && !cluster_valid {
        issues.push(Issue::BadSectorsPerCluster(
            1u64.checked_shl(cluster_shift).unwrap_or(0),
        ));
    }
    let fats = u64::from(field8(110));
    if !matches!(fats, 1 | 2) {
        issues.push(Issue::BadFatCount(fats));
    }
    let volume_length = le64(sector, 72).unwrap_or(0);
    if volume_length == 0 {
        issues.push(Issue::BadTotalSectors(0));
    }
    let fat_offset = field32(80);
    if fat_offset < 24 {
        issues.push(Issue::BadReservedSectors(fat_offset));
    }
    let fat_length = field32(84);
    let heap = field32(88);
    let count = field32(92);
    let root = field32(96);

    let mut checked = Checked {
        issues: Vec::new(),
        volume_bytes: None,
        cluster_size: None,
        sector_size: bps,
    };
    if let Some(bps) = bps
        && cluster_valid
        && matches!(fats, 1 | 2)
        && volume_length > 0
    {
        checked.volume_bytes = volume_length.checked_mul(bps);
        checked.cluster_size = bps.checked_shl(cluster_shift);
        let fat_end = fat_length
            .checked_mul(fats)
            .and_then(|fat_area| fat_area.checked_add(fat_offset));
        let heap_end = count
            .checked_shl(cluster_shift)
            .and_then(|clusters| clusters.checked_add(heap));
        let layout_ok = fat_end.is_some_and(|end| end <= heap)
            && heap_end.is_some_and(|end| end <= volume_length)
            && count > 0;
        if !layout_ok {
            issues.push(Issue::BadLayout);
        }
        if fat_length == 0
            || fat_length.saturating_mul(bps) < count.saturating_add(2).saturating_mul(4)
        {
            issues.push(Issue::BadFatSize(fat_length));
        }
        if count > 0 && (root < 2 || root >= count.saturating_add(2)) {
            issues.push(Issue::BadRootCluster(root));
        }
    }
    checked.issues = issues;
    checked
}

/// NTFS「每条记录几簇」这类字段：正数是簇数，负数 -n 表示 2^n 字节。
fn ntfs_size(raw: u8, cluster_size: u64) -> Option<u64> {
    let value = raw as i8;
    let size = if value > 0 {
        u64::try_from(value).ok()?.checked_mul(cluster_size)?
    } else {
        1u64.checked_shl(u32::from(value.unsigned_abs()))?
    };
    (size.is_power_of_two() && (512..=65536).contains(&size)).then_some(size)
}

fn check_ntfs(sector: &[u8], expected: Option<u64>) -> Checked {
    let mut issues = Vec::new();
    check_jump(sector, &mut issues);
    if sector.get(3..11) != Some(b"NTFS    ".as_slice()) {
        issues.push(Issue::BadOemId);
    }
    check_signature(sector, &mut issues);
    let bps = u64::from(le16(sector, 0x0B).unwrap_or(0));
    let bps = check_sector_size(
        bps,
        bps.is_power_of_two() && (512..=4096).contains(&bps),
        expected,
        &mut issues,
    );
    let raw = sector.get(0x0D).copied().unwrap_or(0);
    let sectors_per_cluster = if raw <= 0x80 {
        Some(u64::from(raw)).filter(|&count| count.is_power_of_two())
    } else {
        1u64.checked_shl(256 - u32::from(raw))
    };
    let cluster_size = bps
        .zip(sectors_per_cluster)
        .and_then(|(bps, count)| bps.checked_mul(count))
        .filter(|&size| size <= MAX_NTFS_CLUSTER);
    if bps.is_some() && cluster_size.is_none() {
        issues.push(Issue::BadSectorsPerCluster(u64::from(raw)));
    }
    // NTFS 不用的 BPB 字段规定为 0：保留扇区数、FAT 个数、根目录项数、16 位总扇区数、FAT 扇区数、32 位总扇区数
    let legacy_zero = le16(sector, 14) == Some(0)
        && sector.get(16) == Some(&0)
        && le16(sector, 17) == Some(0)
        && le16(sector, 19) == Some(0)
        && le16(sector, 22) == Some(0)
        && le32(sector, 32) == Some(0);
    if !legacy_zero {
        issues.push(Issue::BadLegacyFields);
    }
    let total = le64(sector, 0x28).unwrap_or(0);
    if total == 0 {
        issues.push(Issue::BadTotalSectors(0));
    }
    let record = cluster_size.and_then(|size| ntfs_size(sector.get(0x40).copied()?, size));
    if cluster_size.is_some() && record.is_none() {
        issues.push(Issue::BadRecordSize);
    }
    if cluster_size
        .is_some_and(|size| ntfs_size(sector.get(0x44).copied().unwrap_or(0), size).is_none())
    {
        issues.push(Issue::BadIndexSize);
    }
    let mut checked = Checked {
        issues: Vec::new(),
        volume_bytes: None,
        cluster_size,
        sector_size: bps,
    };
    if let (Some(bps), Some(cluster), Some(record)) = (bps, cluster_size, record)
        && total > 0
    {
        let volume = total.checked_mul(bps);
        checked.volume_bytes = volume;
        let inside = |lcn: u64| {
            lcn > 0
                && lcn
                    .checked_mul(cluster)
                    .and_then(|start| start.checked_add(record))
                    .zip(volume)
                    .is_some_and(|(end, volume)| end <= volume)
        };
        let mft = le64(sector, 0x30).unwrap_or(0);
        if !inside(mft) {
            issues.push(Issue::MftOutside(mft));
        }
        let mirror = le64(sector, 0x38).unwrap_or(0);
        if !inside(mirror) {
            issues.push(Issue::MirrorOutside(mirror));
        }
    }
    checked.issues = issues;
    checked
}

/// 引导扇区参数坏了也还认得出的文件系统标识：exFAT、NTFS、BitLocker 的 OEM 标识，FAT 的类型字符串。
fn identify(sector: &[u8]) -> Option<FsKind> {
    match sector.get(3..11) {
        Some(b"EXFAT   ") => return Some(FsKind::Exfat),
        Some(b"NTFS    ") => return Some(FsKind::Ntfs),
        Some(b"-FVE-FS-") => return Some(FsKind::BitLocker),
        _ => {}
    }
    if sector.get(82..90) == Some(b"FAT32   ".as_slice()) {
        return Some(FsKind::Fat32);
    }
    match sector.get(54..62) {
        Some(b"FAT12   ") => Some(FsKind::Fat12),
        Some(b"FAT16   " | b"FAT     ") => Some(FsKind::Fat16),
        _ => None,
    }
}

/// 候选位置上的扇区看起来是不是这种文件系统的引导扇区（参数可以是坏的）。
fn matches_family(candidate: &Candidate, sector: &[u8]) -> bool {
    identify(sector) == Some(family_kind(candidate.family))
        || backup_kind(candidate, sector).is_some()
}

fn family_kind(family: BackupFamily) -> FsKind {
    match family {
        BackupFamily::Fat32 => FsKind::Fat32,
        BackupFamily::Exfat => FsKind::Exfat,
        BackupFamily::Ntfs => FsKind::Ntfs,
    }
}

fn backup_family(kind: FsKind) -> Option<BackupFamily> {
    match kind {
        FsKind::Fat32 => Some(BackupFamily::Fat32),
        FsKind::Exfat => Some(BackupFamily::Exfat),
        FsKind::Ntfs => Some(BackupFamily::Ntfs),
        _ => None,
    }
}

/// 两处都认不出时，按分区表里的类型推测（只认 FAT 的几种类型；NTFS 与 exFAT 共用 0x07，推不出是哪种）。
fn hinted_by_table(partition: &Partition) -> Option<FsKind> {
    match partition.role()? {
        Role::Fat12 => Some(FsKind::Fat12),
        Role::Fat16 => Some(FsKind::Fat16),
        Role::Fat32 | Role::EfiSystem => Some(FsKind::Fat32),
        _ => None,
    }
}

/// 分区表里的类型、长度与引导扇区是否相符。`copy` 是用来比对的那一份（主的有效就用主的，否则用备份）。
fn table_issues(
    partition: &Partition,
    kind: FsKind,
    copy: &CopyReport,
    family: Option<BackupFamily>,
) -> Vec<TableIssue> {
    let mut issues = Vec::new();
    if !type_matches(partition, kind) {
        issues.push(TableIssue::TypeMismatch);
    }
    if let Some(volume) = copy.volume_bytes {
        // NTFS 的备份引导扇区在卷之后，也要在分区里
        let needed = match (family, copy.sector_size) {
            (Some(BackupFamily::Ntfs), Some(sector)) => volume.saturating_add(sector),
            _ => volume,
        };
        if needed > partition.length {
            issues.push(TableIssue::VolumeLarger {
                volume,
                partition: partition.length,
            });
        } else if partition.length - needed > SLACK {
            issues.push(TableIssue::VolumeSmaller {
                volume,
                partition: partition.length,
            });
        }
    }
    issues
}

/// MBR 类型字节是否是这种文件系统用的（含隐藏的写法与 EFI 系统分区）；GPT 只在类型明显不是 Windows 文件系统时算不符。
fn type_matches(partition: &Partition, kind: FsKind) -> bool {
    match &partition.kind {
        PartitionKind::Whole => true,
        PartitionKind::Mbr { type_id, .. } => {
            let base = type_id & !0x10;
            match kind {
                FsKind::Fat12 => matches!(base, 0x01 | 0x04 | 0x06 | 0x0E) || *type_id == 0xEF,
                FsKind::Fat16 => matches!(base, 0x04 | 0x06 | 0x0E) || *type_id == 0xEF,
                FsKind::Fat32 => matches!(base, 0x0B | 0x0C) || *type_id == 0xEF,
                FsKind::Exfat | FsKind::Ntfs => base == 0x07 || matches!(type_id, 0x27 | 0x42),
                _ => true,
            }
        }
        PartitionKind::Gpt { .. } => !matches!(
            partition.role(),
            Some(
                Role::MicrosoftReserved
                    | Role::Linux
                    | Role::LinuxSwap
                    | Role::LinuxLvm
                    | Role::Apfs
                    | Role::HfsPlus
                    | Role::LdmMetadata
            )
        ),
    }
}

/// 主备之间不一样的字段。exFAT 的卷标志与使用百分比允许不同（只在主引导扇区里更新）。
fn differences(kind: FsKind, primary: &[u8], backup: &[u8]) -> Vec<Difference> {
    let left = fields(kind, primary);
    let right = fields(kind, backup);
    left.into_iter()
        .zip(right)
        .filter(|((_, a), (_, b))| a != b)
        .map(|((field, primary), (_, backup))| Difference {
            field,
            primary,
            backup,
        })
        .collect()
}

fn fields(kind: FsKind, sector: &[u8]) -> Vec<(&'static str, u64)> {
    let u8_at = |at| u64::from(sector.get(at).copied().unwrap_or(0));
    let u16_at = |at| u64::from(le16(sector, at).unwrap_or(0));
    let u32_at = |at| u64::from(le32(sector, at).unwrap_or(0));
    let u64_at = |at| le64(sector, at).unwrap_or(0);
    match kind {
        FsKind::Exfat => vec![
            ("partitionOffset", u64_at(64)),
            ("volumeLength", u64_at(72)),
            ("fatOffset", u32_at(80)),
            ("fatLength", u32_at(84)),
            ("clusterHeapOffset", u32_at(88)),
            ("clusterCount", u32_at(92)),
            ("rootCluster", u32_at(96)),
            ("volumeSerial", u32_at(100)),
            (
                "bytesPerSector",
                1u64.checked_shl(u8_at(108) as u32).unwrap_or(0),
            ),
            (
                "sectorsPerCluster",
                1u64.checked_shl(u8_at(109) as u32).unwrap_or(0),
            ),
            ("fatCount", u8_at(110)),
        ],
        FsKind::Ntfs => vec![
            ("bytesPerSector", u16_at(0x0B)),
            ("sectorsPerCluster", u8_at(0x0D)),
            ("totalSectors", u64_at(0x28)),
            ("mftCluster", u64_at(0x30)),
            ("mirrorCluster", u64_at(0x38)),
            ("recordSize", u8_at(0x40)),
            ("indexSize", u8_at(0x44)),
            ("volumeSerial", u64_at(0x48)),
        ],
        _ => {
            let is_fat32 = kind == FsKind::Fat32;
            let total = match u16_at(19) {
                0 => u32_at(32),
                total => total,
            };
            let mut list = vec![
                ("bytesPerSector", u16_at(11)),
                ("sectorsPerCluster", u8_at(13)),
                ("reservedSectors", u16_at(14)),
                ("fatCount", u8_at(16)),
                ("rootEntries", u16_at(17)),
                ("totalSectors", total),
                ("media", u8_at(21)),
                ("fatSize", if is_fat32 { u32_at(36) } else { u16_at(22) }),
                ("volumeSerial", u32_at(if is_fat32 { 67 } else { 39 })),
            ];
            if is_fat32 {
                list.push(("rootCluster", u32_at(44)));
                list.push(("backupSector", u16_at(50)));
            }
            list
        }
    }
}

#[cfg(test)]
mod tests;
