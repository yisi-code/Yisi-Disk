//! 引导扇区的写法：按参数写出 FAT12/16/32、exFAT、NTFS 的引导扇区（exFAT 是整个引导区）。
//! 重建引导扇区（`yisi-repair` 的 `boot_rebuild`）与测试样本共用这一份；写出来的内容按 [`bpb`](crate::bpb)
//! 的规则核对应当没有致命问题。只写各文件系统打开时要用的字段，引导代码留空。

use crate::FsKind;
use crate::boot::exfat_boot_checksum;

/// exFAT 引导区的扇区数：引导扇区、8 个扩展引导扇区、OEM 参数、保留、校验和扇区。
pub const EXFAT_BOOT_REGION_SECTORS: u64 = 12;

/// FAT 引导扇区的参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FatBoot {
    /// FAT12、FAT16 或 FAT32。
    pub kind: FsKind,
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved: u16,
    pub fats: u8,
    /// FAT32 为 0。
    pub root_entries: u16,
    pub total_sectors: u32,
    pub media: u8,
    pub fat_sectors: u32,
    pub hidden_sectors: u32,
    /// 以下三项只用于 FAT32。
    pub root_cluster: u32,
    pub fs_info: u16,
    pub backup_boot: u16,
    pub serial: u32,
    /// 11 个字节，不足的补空格。
    pub label: [u8; 11],
}

/// 一个扇区大小的缓冲区，至少 512 字节，第 510、511 字节是 `55 AA`。
fn sector(bytes_per_sector: u64) -> Vec<u8> {
    let mut buf = vec![0u8; usize::try_from(bytes_per_sector.max(512)).unwrap_or(512)];
    put(&mut buf, 510, &[0x55, 0xAA]);
    buf
}

/// 把 `bytes` 写到 `buf` 的 `at` 处；缓冲区都按扇区大小建，写的位置都在 512 字节之内。
fn put(buf: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(slot) = buf.get_mut(at..at + bytes.len()) {
        slot.copy_from_slice(bytes);
    }
}

/// 写出一个 FAT 引导扇区（`bytes_per_sector` 个字节）。
pub fn fat_boot(params: &FatBoot) -> Vec<u8> {
    let is_fat32 = params.kind == FsKind::Fat32;
    let mut boot = sector(u64::from(params.bytes_per_sector));
    put(
        &mut boot,
        0,
        &[0xEB, if is_fat32 { 0x58 } else { 0x3C }, 0x90],
    );
    put(&mut boot, 3, b"MSDOS5.0");
    put(&mut boot, 11, &params.bytes_per_sector.to_le_bytes());
    put(&mut boot, 13, &[params.sectors_per_cluster]);
    put(&mut boot, 14, &params.reserved.to_le_bytes());
    put(&mut boot, 16, &[params.fats]);
    put(&mut boot, 17, &params.root_entries.to_le_bytes());
    match u16::try_from(params.total_sectors) {
        Ok(total) if !is_fat32 => put(&mut boot, 19, &total.to_le_bytes()),
        _ => put(&mut boot, 32, &params.total_sectors.to_le_bytes()),
    }
    put(&mut boot, 21, &[params.media]);
    put(&mut boot, 28, &params.hidden_sectors.to_le_bytes());
    let label_at = if is_fat32 {
        put(&mut boot, 36, &params.fat_sectors.to_le_bytes());
        put(&mut boot, 44, &params.root_cluster.to_le_bytes());
        put(&mut boot, 48, &params.fs_info.to_le_bytes());
        put(&mut boot, 50, &params.backup_boot.to_le_bytes());
        66
    } else {
        let fat_sectors = u16::try_from(params.fat_sectors).unwrap_or(u16::MAX);
        put(&mut boot, 22, &fat_sectors.to_le_bytes());
        38
    };
    // 扩展引导标记 0x29 之后是卷序列号、卷标、文件系统类型
    put(&mut boot, label_at, &[0x29]);
    put(&mut boot, label_at + 1, &params.serial.to_le_bytes());
    put(&mut boot, label_at + 5, &params.label);
    put(
        &mut boot,
        label_at + 16,
        match params.kind {
            FsKind::Fat12 => b"FAT12   ",
            FsKind::Fat16 => b"FAT16   ",
            _ => b"FAT32   ",
        },
    );
    boot
}

/// FSInfo 扇区的三处签名（偏移与值）：写出与核对同一份。
const FS_INFO_SIGNATURES: [(usize, u32); 3] =
    [(0, 0x4161_5252), (484, 0x6141_7272), (508, 0xAA55_0000)];

/// FSInfo 扇区的三处签名都对。
pub fn fat32_fs_info_ok(sector: &[u8]) -> bool {
    FS_INFO_SIGNATURES
        .iter()
        .all(|&(at, value)| yisi_part::bytes::le32(sector, at) == Some(value))
}

/// FAT32 的 FSInfo 扇区：三处签名，空闲簇数与下一个空闲簇记为「未知」（Windows 挂载时会重新统计）。
pub fn fat32_fs_info(bytes_per_sector: u16) -> Vec<u8> {
    let mut buf = vec![0u8; usize::from(bytes_per_sector).max(512)];
    for (at, value) in FS_INFO_SIGNATURES {
        put(&mut buf, at, &value.to_le_bytes());
    }
    put(&mut buf, 488, &[0xFF; 8]);
    buf
}

/// exFAT 引导扇区的参数。偏移、长度以扇区计。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExfatBoot {
    pub partition_offset: u64,
    pub volume_length: u64,
    pub fat_offset: u32,
    pub fat_length: u32,
    pub heap_offset: u32,
    pub cluster_count: u32,
    pub root_cluster: u32,
    pub serial: u32,
    /// 每扇区字节数、每簇扇区数的以 2 为底的对数。
    pub sector_shift: u8,
    pub cluster_shift: u8,
}

/// 写出 exFAT 的整个引导区（[`EXFAT_BOOT_REGION_SECTORS`] 个扇区）：引导扇区、只有结尾签名的扩展引导扇区、
/// 空的 OEM 参数与保留扇区、重复记满校验和的校验和扇区。主引导区与备份引导区内容相同。
pub fn exfat_boot_region(params: &ExfatBoot) -> Vec<u8> {
    let bytes_per_sector = 1u64
        .checked_shl(u32::from(params.sector_shift))
        .unwrap_or(512);
    let mut boot = sector(bytes_per_sector);
    put(&mut boot, 0, &[0xEB, 0x76, 0x90]);
    put(&mut boot, 3, b"EXFAT   ");
    put(&mut boot, 64, &params.partition_offset.to_le_bytes());
    put(&mut boot, 72, &params.volume_length.to_le_bytes());
    put(&mut boot, 80, &params.fat_offset.to_le_bytes());
    put(&mut boot, 84, &params.fat_length.to_le_bytes());
    put(&mut boot, 88, &params.heap_offset.to_le_bytes());
    put(&mut boot, 92, &params.cluster_count.to_le_bytes());
    put(&mut boot, 96, &params.root_cluster.to_le_bytes());
    put(&mut boot, 100, &params.serial.to_le_bytes());
    // 文件系统版本 1.00；卷标志 0；每扇区字节数与每簇扇区数的对数；1 份 FAT；驱动器号；使用百分比 0xFF 表示未知
    put(&mut boot, 104, &0x0100u16.to_le_bytes());
    put(
        &mut boot,
        108,
        &[params.sector_shift, params.cluster_shift, 1, 0x80, 0xFF],
    );
    let mut region = boot;
    let length = region.len();
    for _ in 1..9 {
        // 扩展引导扇区只有结尾的 `55 AA`
        let mut extended = vec![0u8; length];
        put(&mut extended, length - 2, &[0x55, 0xAA]);
        region.extend(extended);
    }
    region.resize(length * 11, 0);
    let checksum = exfat_boot_checksum(&region);
    region.extend(checksum.to_le_bytes().repeat(length / 4));
    region
}

/// NTFS 引导扇区的参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtfsBoot {
    pub bytes_per_sector: u16,
    pub cluster_size: u64,
    pub hidden_sectors: u32,
    /// 卷的扇区数，不含最后那个放备份引导扇区的扇区。
    pub total_sectors: u64,
    pub mft_lcn: u64,
    pub mirror_lcn: u64,
    pub record_size: u64,
    pub index_size: u64,
    pub serial: u64,
}

/// 写出 NTFS 引导扇区。每簇扇区数超过 128 时按 `256 - log2` 写；记录与索引块的大小不小于一簇时写簇数，
/// 小于一簇时写 `-log2(字节数)`（与 Windows 的写法相同）。
pub fn ntfs_boot(params: &NtfsBoot) -> Vec<u8> {
    let bytes_per_sector = u64::from(params.bytes_per_sector).max(512);
    let mut boot = sector(bytes_per_sector);
    put(&mut boot, 0, &[0xEB, 0x52, 0x90]);
    put(&mut boot, 3, b"NTFS    ");
    put(&mut boot, 0x0B, &params.bytes_per_sector.to_le_bytes());
    let sectors_per_cluster = params.cluster_size / bytes_per_sector;
    let spc_byte = if sectors_per_cluster <= 0x80 {
        sectors_per_cluster as u8
    } else {
        (256 - sectors_per_cluster.trailing_zeros()) as u8
    };
    put(&mut boot, 0x0D, &[spc_byte]);
    put(&mut boot, 0x15, &[0xF8]);
    put(&mut boot, 0x1C, &params.hidden_sectors.to_le_bytes());
    // 驱动器号与扩展引导标记
    put(&mut boot, 0x24, &[0x80, 0x00, 0x80]);
    put(&mut boot, 0x28, &params.total_sectors.to_le_bytes());
    put(&mut boot, 0x30, &params.mft_lcn.to_le_bytes());
    put(&mut boot, 0x38, &params.mirror_lcn.to_le_bytes());
    put(
        &mut boot,
        0x40,
        &[ntfs_size_byte(params.record_size, params.cluster_size)],
    );
    put(
        &mut boot,
        0x44,
        &[ntfs_size_byte(params.index_size, params.cluster_size)],
    );
    put(&mut boot, 0x48, &params.serial.to_le_bytes());
    boot
}

fn ntfs_size_byte(size: u64, cluster_size: u64) -> u8 {
    if size >= cluster_size {
        (size / cluster_size.max(1)) as u8
    } else {
        (256 - size.trailing_zeros()) as u8
    }
}
