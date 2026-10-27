//! 引导扇区参数（BPB）的逐项核对：FAT12/16/32、exFAT 依据微软公开的规范，NTFS 依据公开的结构资料。
//!
//! 打开卷（各文件系统的 `parse_boot`）、丢失分区搜索（[`inspect_boot`](crate::inspect_boot)）、
//! 引导扇区诊断与文件系统检查（`yisi-repair`）共用这一份规则，不各写一套。每一项问题标明是否致命：
//! 有致命问题时参数不可用、卷打不开，诊断算核对不通过，文件系统检查到此为止；不致命的（跳转指令不标准、
//! FAT 装不下全部簇、NTFS 旧字段不为 0 等）只报告，卷照常打开。参数之间有依赖时，前面的不合法就不再核对
//! 依赖它的项，免得一个错报成一串。

use yisi_part::bytes::{le16, le32, le64};

use crate::FsKind;
use crate::format::FatWidth;

/// 一项规则；诊断按它给出问题类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// 结尾没有 `55 AA`。
    Signature,
    /// 开头不是规范写的跳转指令。
    Jump,
    /// exFAT / NTFS 的文件系统标识不对。
    OemId,
    /// 规定为 0 的旧 BPB 字段不是 0。
    LegacyFields,
    BytesPerSector,
    SectorsPerCluster,
    /// FAT 的保留扇区数、exFAT 的 FAT 起点。
    ReservedSectors,
    FatCount,
    Media,
    /// FAT12/16 的根目录项数为 0，或 FAT32 的不为 0。
    RootEntries,
    TotalSectors,
    /// FAT 的大小为 0，或装不下全部簇。
    FatSize,
    /// 各区域的位置对不上：数据区不在卷内、簇区在 FAT 之前或超出卷。
    Layout,
    /// 簇数为 0、超出这种 FAT 的上限。
    ClusterCount,
    /// FAT32 不镜像时指定的那一份 FAT 不存在。
    ActiveFat,
    /// FAT32 的备份引导扇区不在保留区里：按没有备份处理。
    BackupBoot,
    RootCluster,
    RecordSize,
    IndexSize,
    /// `$MFT`、`$MFTMirr` 的起始簇不在卷内。
    MftCluster,
    MirrorCluster,
}

/// 一项问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Problem {
    pub rule: Rule,
    /// 规范里的字段名（`bytesPerSectorShift`、`clusterHeapOffset` 这样），文件系统检查的界面按它显示。
    pub field: &'static str,
    /// 字段里记的值；扇区与簇的大小一律换算成字节或扇区数。
    pub value: u64,
    /// 致命：参数不可用，卷打不开。
    pub fatal: bool,
}

/// 一份引导扇区的核对结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked<G> {
    /// 没有致命问题时的参数。
    pub geometry: Option<G>,
    pub problems: Vec<Problem>,
    /// 下面三项各自所需的字段合法就给出，参数整体不可用时也尽量给出，供诊断说明这一份记的是什么。
    pub bytes_per_sector: Option<u64>,
    pub cluster_size: Option<u64>,
    /// 卷的字节数；NTFS 不含卷后的备份引导扇区。
    pub volume_bytes: Option<u64>,
}

impl<G> Checked<G> {
    fn new() -> Self {
        Self {
            geometry: None,
            problems: Vec::new(),
            bytes_per_sector: None,
            cluster_size: None,
            volume_bytes: None,
        }
    }

    fn fatal(&mut self, rule: Rule, field: &'static str, value: u64) {
        self.problems.push(Problem {
            rule,
            field,
            value,
            fatal: true,
        });
    }

    fn minor(&mut self, rule: Rule, field: &'static str, value: u64) {
        self.problems.push(Problem {
            rule,
            field,
            value,
            fatal: false,
        });
    }

    fn usable(&self) -> bool {
        self.problems.iter().all(|problem| !problem.fatal)
    }
}

/// FAT12/16/32 引导扇区里的参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FatBpb {
    /// FAT12、FAT16 或 FAT32：按簇数判定（规范第 3.5 节），16 位的 FAT 扇区数为 0 时按 FAT32。
    pub kind: FsKind,
    pub bytes_per_sector: u64,
    pub sectors_per_cluster: u64,
    pub reserved: u64,
    pub fats: u64,
    pub root_entries: u64,
    pub total_sectors: u64,
    pub fat_sectors: u64,
    pub media: u8,
    /// 按数据区算出的簇数（没有按 FAT 的大小截短）。
    pub cluster_count: u64,
    /// FAT 装得下的簇数（表项数减去前两项）。
    pub fat_capacity: u64,
    /// FAT32：根目录的起始簇、FSInfo 与备份引导扇区所在的扇区（0 表示没有）。FAT12/16 为 0。
    pub root_cluster: u32,
    pub fs_info: u64,
    pub backup_boot: u64,
    /// FAT32 不镜像时只用这一份 FAT；镜像时为 `None`。
    pub active_fat: Option<u64>,
    /// 引导扇区里的卷标（Windows 改卷标只改根目录里的那一项）。
    pub label: Option<String>,
}

impl FatBpb {
    pub fn root_dir_sectors(&self) -> u64 {
        self.root_entries
            .saturating_mul(32)
            .div_ceil(self.bytes_per_sector)
    }

    pub fn first_data_sector(&self) -> u64 {
        self.fats
            .saturating_mul(self.fat_sectors)
            .saturating_add(self.reserved)
            .saturating_add(self.root_dir_sectors())
    }

    pub fn cluster_size(&self) -> u64 {
        self.sectors_per_cluster * self.bytes_per_sector
    }

    pub fn volume_bytes(&self) -> u64 {
        self.total_sectors.saturating_mul(self.bytes_per_sector)
    }

    pub fn width(&self) -> FatWidth {
        match self.kind {
            FsKind::Fat12 => FatWidth::Fat12,
            FsKind::Fat16 => FatWidth::Fat16,
            _ => FatWidth::Fat32,
        }
    }

    /// 第 `copy` 份 FAT 在分区里的字节偏移。
    pub fn fat_offset(&self, copy: u64) -> u64 {
        copy.saturating_mul(self.fat_sectors)
            .saturating_add(self.reserved)
            .saturating_mul(self.bytes_per_sector)
    }

    /// 要用的那份 FAT：镜像时是第一份，不镜像时是指定的那份。
    pub fn active_fat_offset(&self) -> u64 {
        self.fat_offset(self.active_fat.unwrap_or(0))
    }

    pub fn fat_bytes(&self) -> u64 {
        self.fat_sectors.saturating_mul(self.bytes_per_sector)
    }

    /// FAT12/16 的根目录区在分区里的字节偏移（FAT32 的根目录在簇里）。
    pub fn root_dir_offset(&self) -> u64 {
        self.fat_offset(self.fats)
    }

    /// FAT32 备份引导扇区的扇区号：记了、且落在保留区里才算有（0 与 0xFFFF 表示没有）。
    pub fn backup_sector(&self) -> Option<u64> {
        (self.kind == FsKind::Fat32 && self.backup_boot != 0 && self.backup_boot < self.reserved)
            .then_some(self.backup_boot)
    }

    /// 2 号簇在分区里的字节偏移。
    pub fn heap_offset(&self) -> u64 {
        self.first_data_sector()
            .saturating_mul(self.bytes_per_sector)
    }

    /// 实际能用的簇数：按数据区算出的、FAT 装得下的、FAT32 的上限、分区（`length` 字节）里放得下的，取最小。
    pub fn usable_clusters(&self, length: u64) -> u64 {
        let fits = length.saturating_sub(self.heap_offset()) / self.cluster_size().max(1);
        self.cluster_count
            .min(self.fat_capacity)
            .min(0x0FFF_FFF5)
            .min(fits)
    }
}

/// 核对 FAT 引导扇区。`hint` 为 FAT32 时按 FAT32 核对（诊断按残留的类型字符串认出 FAT32、参数却坏了的情况），
/// 不给时按 16 位的 FAT 扇区数是否为 0 判断。
pub fn check_fat(sector: &[u8], hint: Option<FsKind>) -> Checked<FatBpb> {
    let mut checked = Checked::new();
    check_jump(sector, &mut checked);
    check_signature(sector, &mut checked);
    let u8_at = |at| u64::from(sector.get(at).copied().unwrap_or(0));
    let u16_at = |at| u64::from(le16(sector, at).unwrap_or(0));
    let u32_at = |at| u64::from(le32(sector, at).unwrap_or(0));

    let bps = u16_at(11);
    let bps_valid = matches!(bps, 512 | 1024 | 2048 | 4096);
    if !bps_valid {
        checked.fatal(Rule::BytesPerSector, "bytesPerSector", bps);
    }
    let spc = u8_at(13);
    let spc_valid = spc.is_power_of_two() && spc <= 128;
    if !spc_valid {
        checked.fatal(Rule::SectorsPerCluster, "sectorsPerCluster", spc);
    }
    let reserved = u16_at(14);
    if reserved == 0 {
        checked.fatal(Rule::ReservedSectors, "reservedSectors", reserved);
    }
    let fats = u8_at(16);
    let fats_valid = matches!(fats, 1 | 2);
    if !fats_valid {
        checked.fatal(Rule::FatCount, "fatCount", fats);
    }
    let media = sector.get(21).copied().unwrap_or(0);
    if !(media == 0xF0 || media >= 0xF8) {
        checked.fatal(Rule::Media, "mediaDescriptor", u64::from(media));
    }
    let total = match u16_at(19) {
        0 => u32_at(32),
        total => total,
    };
    if total == 0 {
        checked.fatal(Rule::TotalSectors, "totalSectors", 0);
    }
    let fat16_sectors = u16_at(22);
    let is_fat32 = hint.map_or(fat16_sectors == 0, |kind| kind == FsKind::Fat32);
    let fat_sectors = if is_fat32 { u32_at(36) } else { fat16_sectors };
    if fat_sectors == 0 {
        checked.fatal(Rule::FatSize, "fatSize", 0);
    } else if is_fat32 && fat16_sectors != 0 {
        // FAT32 的 16 位 FAT 扇区数必须为 0
        checked.fatal(Rule::FatSize, "fatSize", fat16_sectors);
    }
    let root_entries = u16_at(17);
    if is_fat32 != (root_entries == 0) {
        checked.fatal(Rule::RootEntries, "rootEntries", root_entries);
    }
    if bps_valid {
        checked.bytes_per_sector = Some(bps);
        checked.cluster_size = spc_valid.then(|| spc * bps);
        checked.volume_bytes = (total > 0).then(|| total.saturating_mul(bps));
    }

    let mut bpb = FatBpb {
        kind: FsKind::Fat32,
        bytes_per_sector: bps,
        sectors_per_cluster: spc,
        reserved,
        fats,
        root_entries,
        total_sectors: total,
        fat_sectors,
        media,
        cluster_count: 0,
        fat_capacity: 0,
        root_cluster: 0,
        fs_info: 0,
        backup_boot: 0,
        active_fat: None,
        label: None,
    };
    let sizes_valid = bps_valid && spc_valid && reserved > 0 && fats_valid && total > 0;
    if !sizes_valid || fat_sectors == 0 {
        return checked;
    }
    let data_sectors = total
        .checked_sub(bpb.first_data_sector())
        .filter(|&data| data >= spc);
    let Some(data_sectors) = data_sectors else {
        checked.fatal(Rule::Layout, "totalSectors", total);
        return checked;
    };
    let clusters = data_sectors / spc;
    bpb.cluster_count = clusters;
    bpb.kind = if is_fat32 {
        FsKind::Fat32
    } else if clusters < 4085 {
        FsKind::Fat12
    } else if clusters < 65525 {
        FsKind::Fat16
    } else {
        checked.fatal(Rule::ClusterCount, "clusterCount", clusters);
        return checked;
    };
    if is_fat32 && clusters > 0x0FFF_FFF5 {
        checked.fatal(Rule::ClusterCount, "clusterCount", clusters);
    }
    let bits: u64 = match bpb.kind {
        FsKind::Fat12 => 12,
        FsKind::Fat16 => 16,
        _ => 32,
    };
    bpb.fat_capacity = (fat_sectors.saturating_mul(bps).saturating_mul(8) / bits).saturating_sub(2);
    if bpb.fat_capacity < clusters {
        checked.minor(Rule::FatSize, "fatSize", fat_sectors);
    }

    let label_at = if is_fat32 {
        let flags = u16_at(40);
        // 第 7 位置 1 表示不镜像，只用低 4 位指定的那一份
        if flags & 0x80 != 0 {
            let active = flags & 0x0F;
            if active >= fats {
                checked.fatal(Rule::ActiveFat, "extFlags", flags);
            }
            bpb.active_fat = Some(active);
        }
        let root = u32_at(44);
        if root < 2 || root >= clusters.saturating_add(2) {
            checked.fatal(Rule::RootCluster, "rootCluster", root);
        }
        bpb.root_cluster = u32::try_from(root).unwrap_or(0);
        bpb.fs_info = u16_at(48);
        bpb.backup_boot = u16_at(50);
        if !matches!(bpb.backup_boot, 0 | 0xFFFF) && bpb.backup_boot >= reserved {
            checked.minor(Rule::BackupBoot, "backupBootSector", bpb.backup_boot);
        }
        66
    } else {
        38
    };
    if sector.get(label_at) == Some(&0x29) {
        bpb.label = sector
            .get(label_at + 5..label_at + 16)
            .and_then(short_label);
    }
    if checked.usable() {
        checked.geometry = Some(bpb);
    }
    checked
}

/// exFAT 引导扇区里的参数。扇区与簇的大小记的是 2 的幂次。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExfatBpb {
    pub sector_shift: u32,
    pub cluster_shift: u32,
    /// 以下以扇区计。
    pub volume_length: u64,
    pub fat_offset: u64,
    pub fat_length: u64,
    pub heap_offset: u64,
    pub cluster_count: u64,
    pub root_cluster: u32,
    /// 卷标志：第 0 位为用第二份 FAT，第 1 位为未正常卸下，第 2 位为介质故障。
    pub flags: u16,
    pub fats: u64,
}

impl ExfatBpb {
    pub fn bytes_per_sector(&self) -> u64 {
        1 << self.sector_shift
    }

    pub fn cluster_size(&self) -> u64 {
        1 << (self.sector_shift + self.cluster_shift)
    }

    /// 两份 FAT、并且卷标志指定用第二份。
    pub fn use_second_fat(&self) -> bool {
        self.fats == 2 && self.flags & 0x01 != 0
    }

    /// 要用的那份 FAT 在分区里的字节偏移。
    pub fn active_fat_offset(&self) -> u64 {
        let copy = u64::from(self.use_second_fat());
        copy.saturating_mul(self.fat_length)
            .saturating_add(self.fat_offset)
            .saturating_mul(self.bytes_per_sector())
    }

    pub fn fat_bytes(&self) -> u64 {
        self.fat_length.saturating_mul(self.bytes_per_sector())
    }

    /// 2 号簇在分区里的字节偏移。
    pub fn heap_offset_bytes(&self) -> u64 {
        self.heap_offset.saturating_mul(self.bytes_per_sector())
    }

    pub fn volume_bytes(&self) -> u64 {
        self.volume_length.saturating_mul(self.bytes_per_sector())
    }

    /// 实际能用的簇数：引导扇区记的、FAT 装得下的（每项 4 字节，前两项不是簇）、分区（`length` 字节）里放得下的，取最小。
    pub fn usable_clusters(&self, length: u64) -> u64 {
        let capacity = (self.fat_bytes() / 4).saturating_sub(2);
        let fits = length.saturating_sub(self.heap_offset_bytes()) / self.cluster_size();
        self.cluster_count.min(capacity).min(fits)
    }
}

/// 核对 exFAT 引导扇区（引导区的校验和要读 12 个扇区，另用 [`exfat_boot_checksum`](crate::boot::exfat_boot_checksum) 核对）。
pub fn check_exfat(sector: &[u8]) -> Checked<ExfatBpb> {
    let mut checked = Checked::new();
    if sector.get(..3) != Some([0xEB, 0x76, 0x90].as_slice()) {
        checked.minor(Rule::Jump, "jumpBoot", 0);
    }
    if sector.get(3..11) != Some(b"EXFAT   ".as_slice()) {
        checked.fatal(Rule::OemId, "fileSystemName", 0);
    }
    // 规范要求 BPB 的旧字段区（11 到 63 字节）全为 0，FAT 的驱动程序才不会误认
    if sector
        .get(11..64)
        .is_none_or(|legacy| legacy.iter().any(|&byte| byte != 0))
    {
        checked.fatal(Rule::LegacyFields, "mustBeZero", 0);
    }
    check_signature(sector, &mut checked);
    let u8_at = |at| u32::from(sector.get(at).copied().unwrap_or(0));
    let u32_at = |at| u64::from(le32(sector, at).unwrap_or(0));

    let shift = u8_at(108);
    let shift_valid = (9..=12).contains(&shift);
    if !shift_valid {
        checked.fatal(
            Rule::BytesPerSector,
            "bytesPerSectorShift",
            1u64.checked_shl(shift).unwrap_or(0),
        );
    }
    let cluster_shift = u8_at(109);
    let cluster_valid = shift_valid && shift + cluster_shift <= 25;
    if shift_valid && !cluster_valid {
        checked.fatal(
            Rule::SectorsPerCluster,
            "sectorsPerClusterShift",
            1u64.checked_shl(cluster_shift).unwrap_or(0),
        );
    }
    let fats = u64::from(u8_at(110));
    let fats_valid = matches!(fats, 1 | 2);
    if !fats_valid {
        checked.fatal(Rule::FatCount, "numberOfFats", fats);
    }
    let volume_length = le64(sector, 72).unwrap_or(0);
    let fat_offset = u32_at(80);
    if fat_offset < 24 {
        checked.fatal(Rule::ReservedSectors, "fatOffset", fat_offset);
    }
    let fat_length = u32_at(84);
    if fat_length == 0 {
        checked.fatal(Rule::FatSize, "fatLength", 0);
    }
    let heap_offset = u32_at(88);
    if fats_valid
        && fat_length > 0
        && heap_offset < fat_length.saturating_mul(fats).saturating_add(fat_offset)
    {
        checked.fatal(Rule::Layout, "clusterHeapOffset", heap_offset);
    }
    let cluster_count = u32_at(92);
    let count_valid = cluster_count > 0 && cluster_count <= 0xFFFF_FFF5;
    if !count_valid {
        checked.fatal(Rule::ClusterCount, "clusterCount", cluster_count);
    }
    let root = u32_at(96);
    if root < 2 || (count_valid && root >= cluster_count + 2) {
        checked.fatal(Rule::RootCluster, "firstClusterOfRootDirectory", root);
    }
    if shift_valid {
        let bps = 1u64 << shift;
        checked.bytes_per_sector = Some(bps);
        checked.cluster_size = cluster_valid.then(|| bps << cluster_shift);
        checked.volume_bytes = (volume_length > 0).then(|| volume_length.saturating_mul(bps));
        if cluster_valid && count_valid {
            // 卷要装得下簇区；FAT 要装得下全部簇（每项 4 字节）
            let heap_end = heap_offset.saturating_add(cluster_count << cluster_shift);
            if volume_length < heap_end {
                checked.minor(Rule::Layout, "volumeLength", volume_length);
            }
            if fat_length > 0 && fat_length.saturating_mul(bps) < (cluster_count + 2) * 4 {
                checked.minor(Rule::FatSize, "fatLength", fat_length);
            }
        }
    }
    if checked.usable() {
        checked.geometry = Some(ExfatBpb {
            sector_shift: shift,
            cluster_shift,
            volume_length,
            fat_offset,
            fat_length,
            heap_offset,
            cluster_count,
            root_cluster: u32::try_from(root).unwrap_or(0),
            flags: le16(sector, 106).unwrap_or(0),
            fats,
        });
    }
    checked
}

/// NTFS 引导扇区里的参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtfsBpb {
    pub bytes_per_sector: u64,
    pub cluster_size: u64,
    /// 卷的扇区数，不含卷后的备份引导扇区。
    pub total_sectors: u64,
    pub mft_lcn: u64,
    pub mirror_lcn: u64,
    pub record_size: u64,
    /// 索引块的大小；不合法时为 `None`（只影响列目录，不影响打开卷）。
    pub index_size: Option<u64>,
}

impl NtfsBpb {
    pub fn total_bytes(&self) -> u64 {
        self.total_sectors.saturating_mul(self.bytes_per_sector)
    }
}

/// 簇大小的上限：64 KiB 以上的簇有专门的写法，Windows 最大到 2 MiB。
const MAX_NTFS_CLUSTER: u64 = 2 << 20;

/// 核对 NTFS 引导扇区。
pub fn check_ntfs(sector: &[u8]) -> Checked<NtfsBpb> {
    let mut checked = Checked::new();
    check_jump(sector, &mut checked);
    if sector.get(3..11) != Some(b"NTFS    ".as_slice()) {
        checked.fatal(Rule::OemId, "fileSystemName", 0);
    }
    check_signature(sector, &mut checked);
    let bps = u64::from(le16(sector, 0x0B).unwrap_or(0));
    let bps_valid = bps.is_power_of_two() && (512..=4096).contains(&bps);
    if !bps_valid {
        checked.fatal(Rule::BytesPerSector, "bytesPerSector", bps);
    }
    // 每簇扇区数：大于 0x80 时表示 2^(256-n)（64 KiB 以上的簇）
    let raw = sector.get(0x0D).copied().unwrap_or(0);
    let sectors_per_cluster = if raw <= 0x80 {
        Some(u64::from(raw)).filter(|&count| count.is_power_of_two())
    } else {
        1u64.checked_shl(256 - u32::from(raw))
    };
    let cluster_size = sectors_per_cluster
        .filter(|_| bps_valid)
        .and_then(|count| count.checked_mul(bps))
        .filter(|&size| size <= MAX_NTFS_CLUSTER);
    if bps_valid && cluster_size.is_none() {
        checked.fatal(Rule::SectorsPerCluster, "sectorsPerCluster", u64::from(raw));
    }
    // NTFS 不用的 BPB 字段规定为 0：保留扇区数、FAT 个数、根目录项数、16 位总扇区数、FAT 扇区数、32 位总扇区数
    let legacy_zero = le16(sector, 14) == Some(0)
        && sector.get(16) == Some(&0)
        && le16(sector, 17) == Some(0)
        && le16(sector, 19) == Some(0)
        && le16(sector, 22) == Some(0)
        && le32(sector, 32) == Some(0);
    if !legacy_zero {
        checked.minor(Rule::LegacyFields, "mustBeZero", 0);
    }
    let total = le64(sector, 0x28).unwrap_or(0);
    if total == 0 {
        checked.fatal(Rule::TotalSectors, "totalSectors", 0);
    }
    let record_raw = sector.get(0x40).copied().unwrap_or(0);
    let record = cluster_size.and_then(|size| ntfs_size(record_raw, size));
    if cluster_size.is_some() && record.is_none() {
        checked.fatal(
            Rule::RecordSize,
            "clustersPerMftRecord",
            u64::from(record_raw),
        );
    }
    let index_raw = sector.get(0x44).copied().unwrap_or(0);
    let index = cluster_size.and_then(|size| ntfs_size(index_raw, size));
    if cluster_size.is_some() && index.is_none() {
        checked.minor(
            Rule::IndexSize,
            "clustersPerIndexBlock",
            u64::from(index_raw),
        );
    }
    if bps_valid {
        checked.bytes_per_sector = Some(bps);
        checked.cluster_size = cluster_size;
        checked.volume_bytes = (total > 0).then(|| total.saturating_mul(bps));
    }
    let mft = le64(sector, 0x30).unwrap_or(0);
    let mirror = le64(sector, 0x38).unwrap_or(0);
    if let (Some(cluster), Some(record), Some(volume)) =
        (cluster_size, record, checked.volume_bytes)
    {
        // 起始簇不为 0，并且至少一条记录落在卷内
        let inside = |lcn: u64| {
            lcn > 0
                && lcn
                    .checked_mul(cluster)
                    .and_then(|start| start.checked_add(record))
                    .is_some_and(|end| end <= volume)
        };
        if !inside(mft) {
            checked.fatal(Rule::MftCluster, "mftCluster", mft);
        }
        if !inside(mirror) {
            checked.minor(Rule::MirrorCluster, "mftMirrorCluster", mirror);
        }
    }
    if checked.usable()
        && let (Some(cluster_size), Some(record_size)) = (cluster_size, record)
    {
        checked.geometry = Some(NtfsBpb {
            bytes_per_sector: bps,
            cluster_size,
            total_sectors: total,
            mft_lcn: mft,
            mirror_lcn: mirror,
            record_size,
            index_size: index,
        });
    }
    checked
}

/// NTFS「每条记录几簇」这类字段：正数是簇数，负数 -n 表示 2^n 字节；结果要是 512 字节到 64 KiB 之间的 2 的幂。
fn ntfs_size(raw: u8, cluster_size: u64) -> Option<u64> {
    let value = raw as i8;
    let size = if value > 0 {
        u64::try_from(value).ok()?.checked_mul(cluster_size)?
    } else {
        1u64.checked_shl(u32::from(value.unsigned_abs()))?
    };
    (size.is_power_of_two() && (512..=65536).contains(&size)).then_some(size)
}

/// 规范写的跳转指令：`EB xx 90` 或 `E9 xx xx`。
fn check_jump<G>(sector: &[u8], checked: &mut Checked<G>) {
    let jump = matches!(
        (sector.first(), sector.get(2)),
        (Some(0xEB), Some(0x90)) | (Some(0xE9), _)
    );
    if !jump {
        checked.minor(Rule::Jump, "jumpBoot", 0);
    }
}

fn check_signature<G>(sector: &[u8], checked: &mut Checked<G>) {
    if le16(sector, 510) != Some(0xAA55) {
        checked.fatal(Rule::Signature, "signature", 0);
    }
}

/// 主备引导扇区之间不一样的一个字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDifference {
    pub field: &'static str,
    pub primary: u64,
    pub backup: u64,
}

/// 主备引导扇区应当一致的字段：规范里的名字、记的值（扇区与簇的大小换算成字节）。值都不超过 2^53，界面上不丢精度：
/// NTFS 的 64 位卷序列号拆成低、高 32 位两项（Windows 显示的是低 32 位）。
/// 只在主引导扇区里更新的不算：exFAT 的卷标志与使用百分比（FAT 的脏标记在 FAT 里，不在引导扇区）。
/// 引导扇区诊断与文件系统检查都按这一份比对主备。
pub fn significant_fields(kind: FsKind, sector: &[u8]) -> Vec<(&'static str, u64)> {
    let u8_at = |at| u64::from(sector.get(at).copied().unwrap_or(0));
    let u16_at = |at| u64::from(le16(sector, at).unwrap_or(0));
    let u32_at = |at| u64::from(le32(sector, at).unwrap_or(0));
    let u64_at = |at| le64(sector, at).unwrap_or(0);
    let shifted = |shift: u64| 1u64.checked_shl(shift as u32).unwrap_or(0);
    match kind {
        FsKind::Exfat => vec![
            ("partitionOffset", u64_at(64)),
            ("volumeLength", u64_at(72)),
            ("fatOffset", u32_at(80)),
            ("fatLength", u32_at(84)),
            ("clusterHeapOffset", u32_at(88)),
            ("clusterCount", u32_at(92)),
            ("firstClusterOfRootDirectory", u32_at(96)),
            ("volumeSerial", u32_at(100)),
            ("bytesPerSectorShift", shifted(u8_at(108))),
            (
                "sectorsPerClusterShift",
                shifted(u8_at(108).saturating_add(u8_at(109))),
            ),
            ("numberOfFats", u8_at(110)),
        ],
        FsKind::Ntfs => {
            let cluster_size = check_ntfs(sector).cluster_size.unwrap_or(0);
            let size = |at| {
                let raw = sector.get(at).copied().unwrap_or(0);
                ntfs_size(raw, cluster_size).unwrap_or(u64::from(raw))
            };
            vec![
                ("bytesPerSector", u16_at(0x0B)),
                ("sectorsPerCluster", u8_at(0x0D)),
                ("hiddenSectors", u32_at(0x1C)),
                ("totalSectors", u64_at(0x28)),
                ("mftCluster", u64_at(0x30)),
                ("mftMirrorCluster", u64_at(0x38)),
                ("clustersPerMftRecord", size(0x40)),
                ("clustersPerIndexBlock", size(0x44)),
                ("volumeSerial", u32_at(0x48)),
                ("volumeSerialHigh", u32_at(0x4C)),
            ]
        }
        _ => {
            let is_fat32 = kind == FsKind::Fat32;
            let total = match u16_at(19) {
                0 => u32_at(32),
                total => total,
            };
            let mut fields = vec![
                ("bytesPerSector", u16_at(11)),
                ("sectorsPerCluster", u8_at(13)),
                ("reservedSectors", u16_at(14)),
                ("fatCount", u8_at(16)),
                ("rootEntries", u16_at(17)),
                ("totalSectors", total),
                ("mediaDescriptor", u8_at(21)),
                ("fatSize", if is_fat32 { u32_at(36) } else { u16_at(22) }),
                ("hiddenSectors", u32_at(28)),
            ];
            if is_fat32 {
                fields.extend([
                    ("extFlags", u16_at(40)),
                    ("rootCluster", u32_at(44)),
                    ("fsInfo", u16_at(48)),
                    ("backupBootSector", u16_at(50)),
                ]);
            }
            fields.push(("volumeSerial", u32_at(if is_fat32 { 67 } else { 39 })));
            fields
        }
    }
}

/// 按 [`significant_fields`] 比对主备，列出不一样的字段。
pub fn differing_fields(kind: FsKind, primary: &[u8], backup: &[u8]) -> Vec<FieldDifference> {
    significant_fields(kind, primary)
        .into_iter()
        .zip(significant_fields(kind, backup))
        .filter(|((_, a), (_, b))| a != b)
        .map(|((field, primary), (_, backup))| FieldDifference {
            field,
            primary,
            backup,
        })
        .collect()
}

/// 引导扇区与根目录里 11 字节的卷标：非 ASCII 字节显示为 `_`，去掉结尾空格，`NO NAME` 视为没有。
pub(crate) fn short_label(bytes: &[u8]) -> Option<String> {
    let text: String = bytes
        .iter()
        .map(|&byte| {
            if byte.is_ascii_graphic() || byte == b' ' {
                char::from(byte)
            } else {
                '_'
            }
        })
        .collect();
    let text = text.trim_end();
    (!text.is_empty() && text != "NO NAME").then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples::{ExfatImage, FatImage, FatType, NtfsImage};

    fn rules<G>(checked: &Checked<G>) -> Vec<(Rule, &'static str, bool)> {
        checked
            .problems
            .iter()
            .map(|problem| (problem.rule, problem.field, problem.fatal))
            .collect()
    }

    #[test]
    fn clean_samples_have_no_problems() {
        for kind in [FatType::Fat12, FatType::Fat16, FatType::Fat32] {
            let image = FatImage::new(kind).finish();
            let checked = check_fat(&image[..512], None);
            assert!(
                checked.problems.is_empty(),
                "{kind:?}: {:?}",
                checked.problems
            );
            let bpb = checked.geometry.unwrap();
            assert_eq!(bpb.volume_bytes(), image.len() as u64);
            assert!(bpb.fat_capacity >= bpb.cluster_count);
        }
        let exfat = ExfatImage::new().finish();
        let checked = check_exfat(&exfat[..512]);
        assert!(checked.problems.is_empty(), "{:?}", checked.problems);
        assert_eq!(checked.volume_bytes, Some(exfat.len() as u64));
        let ntfs = NtfsImage::new().finish();
        let checked = check_ntfs(&ntfs[..512]);
        assert!(checked.problems.is_empty(), "{:?}", checked.problems);
        assert_eq!(checked.volume_bytes, Some(ntfs.len() as u64 - 512));
    }

    #[test]
    fn fat_problems_are_itemised_without_cascades() {
        let mut sector = FatImage::new(FatType::Fat16).finish()[..512].to_vec();
        sector[13] = 3;
        let checked = check_fat(&sector, None);
        // 每簇扇区数坏了：依赖它的数据区、簇数不再核对
        assert_eq!(
            rules(&checked),
            [(Rule::SectorsPerCluster, "sectorsPerCluster", true)]
        );
        assert_eq!(checked.problems[0].value, 3);
        assert_eq!(checked.geometry, None);
        assert_eq!(checked.bytes_per_sector, Some(512));
        assert_eq!(checked.cluster_size, None);

        // 跳转指令不标准只是提醒；FAT16 的根目录项数为 0 不行
        let mut sector = FatImage::new(FatType::Fat16).finish()[..512].to_vec();
        sector[0] = 0x90;
        sector[17..19].fill(0);
        let checked = check_fat(&sector, None);
        assert_eq!(
            rules(&checked),
            [
                (Rule::Jump, "jumpBoot", false),
                (Rule::RootEntries, "rootEntries", true)
            ]
        );
    }

    #[test]
    fn fat32_specific_fields_are_checked() {
        let image = FatImage::new(FatType::Fat32).finish();
        let mut sector = image[..512].to_vec();
        // 不镜像、指定第 3 份 FAT（只有两份）；根目录簇号超出范围
        sector[40..42].copy_from_slice(&0x0082u16.to_le_bytes());
        sector[44..48].copy_from_slice(&u32::MAX.to_le_bytes());
        let checked = check_fat(&sector, None);
        assert_eq!(
            rules(&checked),
            [
                (Rule::ActiveFat, "extFlags", true),
                (Rule::RootCluster, "rootCluster", true)
            ]
        );
        // 认出是 FAT32、16 位的 FAT 扇区数却不为 0
        let mut sector = image[..512].to_vec();
        sector[22..24].copy_from_slice(&7u16.to_le_bytes());
        let hinted = check_fat(&sector, Some(FsKind::Fat32));
        assert_eq!(hinted.problems[0].rule, Rule::FatSize);
        assert_eq!(hinted.problems[0].value, 7);
        // FAT 太小不致命
        let mut sector = image[..512].to_vec();
        let fat = u32::from_le_bytes(sector[36..40].try_into().unwrap());
        sector[36..40].copy_from_slice(&(fat / 2).to_le_bytes());
        let checked = check_fat(&sector, None);
        assert_eq!(rules(&checked), [(Rule::FatSize, "fatSize", false)]);
        assert!(checked.geometry.is_some());
    }

    #[test]
    fn exfat_and_ntfs_problems_carry_specification_names() {
        let mut sector = ExfatImage::new().finish()[..512].to_vec();
        sector[109] = 20;
        sector[96..100].copy_from_slice(&1u32.to_le_bytes());
        let checked = check_exfat(&sector);
        assert_eq!(
            rules(&checked),
            [
                (Rule::SectorsPerCluster, "sectorsPerClusterShift", true),
                (Rule::RootCluster, "firstClusterOfRootDirectory", true)
            ]
        );
        assert_eq!(checked.problems[0].value, 1 << 20);

        let mut sector = NtfsImage::new().finish()[..512].to_vec();
        sector[0x30..0x38].copy_from_slice(&u64::MAX.to_le_bytes());
        sector[0x44] = 0x7F;
        sector[16] = 2;
        let checked = check_ntfs(&sector);
        assert_eq!(
            rules(&checked),
            [
                (Rule::LegacyFields, "mustBeZero", false),
                (Rule::IndexSize, "clustersPerIndexBlock", false),
                (Rule::MftCluster, "mftCluster", true)
            ]
        );
        assert_eq!(checked.geometry, None);
        assert!(checked.volume_bytes.is_some());
    }

    #[test]
    fn fat32_backup_outside_the_reserved_area_is_a_note() {
        let mut sector = FatImage::new(FatType::Fat32).finish()[..512].to_vec();
        let bpb = check_fat(&sector, None).geometry.unwrap();
        assert_eq!(bpb.backup_sector(), Some(6));
        sector[50..52].copy_from_slice(&40u16.to_le_bytes());
        let checked = check_fat(&sector, None);
        assert_eq!(
            rules(&checked),
            [(Rule::BackupBoot, "backupBootSector", false)]
        );
        assert_eq!(checked.geometry.unwrap().backup_sector(), None);
        // 0 与 0xFFFF 表示没有备份，不算问题
        for none in [0u16, 0xFFFF] {
            sector[50..52].copy_from_slice(&none.to_le_bytes());
            let checked = check_fat(&sector, None);
            assert!(checked.problems.is_empty(), "{none}");
            assert_eq!(checked.geometry.unwrap().backup_sector(), None);
        }
    }

    #[test]
    fn backup_comparison_covers_every_significant_field() {
        let primary = FatImage::new(FatType::Fat32).finish()[..512].to_vec();
        let mut backup = primary.clone();
        assert!(differing_fields(FsKind::Fat32, &primary, &backup).is_empty());
        // 隐藏扇区数、FAT 镜像标志、FSInfo 位置、卷序列号
        backup[28] ^= 1;
        backup[40] ^= 0x80;
        backup[48] ^= 1;
        backup[67] ^= 1;
        let fields: Vec<&str> = differing_fields(FsKind::Fat32, &primary, &backup)
            .iter()
            .map(|difference| difference.field)
            .collect();
        assert_eq!(
            fields,
            ["hiddenSectors", "extFlags", "fsInfo", "volumeSerial"]
        );
        // 引导代码不同不算
        let mut backup = primary.clone();
        backup[100] ^= 1;
        assert!(differing_fields(FsKind::Fat32, &primary, &backup).is_empty());

        // exFAT 的卷标志与使用百分比只在主引导扇区里更新，不算
        let primary = ExfatImage::new().finish()[..512].to_vec();
        let mut backup = primary.clone();
        backup[106] ^= 2;
        backup[112] = 50;
        assert!(differing_fields(FsKind::Exfat, &primary, &backup).is_empty());
        backup[108] += 1;
        let difference = &differing_fields(FsKind::Exfat, &primary, &backup)[0];
        assert_eq!(
            (difference.field, difference.primary * 2),
            ("bytesPerSectorShift", difference.backup)
        );

        // NTFS 的 64 位卷序列号拆成两项，每项都在 2^53 以内
        let primary = NtfsImage::new().finish()[..512].to_vec();
        let mut backup = primary.clone();
        backup[0x4F] ^= 0x80;
        let fields = differing_fields(FsKind::Ntfs, &primary, &backup);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].field, "volumeSerialHigh");
        assert!(fields[0].primary.max(fields[0].backup) <= u64::from(u32::MAX));
    }
}
