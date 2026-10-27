//! 单独看一个引导扇区：是哪种文件系统、卷有多大、备份引导扇区在哪、拿什么结构核对。
//! 供丢失分区搜索使用：在磁盘上逐扇区找引导扇区时，还不知道分区从哪开始。参数是否自洽沿用各文件系统打开时的规则。

use yisi_part::bytes::{le16, le64};

use crate::clusters::FatWidth;
use crate::{FsKind, exfat, fat, ntfs};

/// exFAT 的主引导区占 12 个扇区，备份引导区紧跟其后。
const EXFAT_BACKUP_SECTOR: u64 = 12;

/// 一个参数自洽的引导扇区。偏移都以分区开头为 0，单位是字节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootSector {
    /// FAT12/16/32、exFAT 或 NTFS。
    pub kind: FsKind,
    pub bytes_per_sector: u64,
    /// 卷占的字节数；NTFS 含末尾的备份引导扇区。
    pub volume_bytes: u64,
    /// 备份引导扇区的位置：FAT32 是 BPB 里记的扇区（一般是第 6 扇区），exFAT 是第 12 扇区，
    /// NTFS 是卷的最后一个扇区；FAT12/16 没有备份。
    pub backup_at: Option<u64>,
    /// 引导扇区之外可以拿来核对的结构。
    pub layout: BootLayout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootLayout {
    /// 各份 FAT 的位置；FAT 的第一个字节是介质描述符，之后是 0xFF。
    Fat { media: u8, fats: Vec<u64> },
    /// FAT 的位置（第 0 项固定是 `F8 FF FF FF`，第 1 项是 `FF FF FF FF`）；
    /// 引导区第 11 扇区是前 11 个扇区的校验和，按扇区大小计。
    Exfat { fat_offset: u64 },
    /// `$MFT` 与 `$MFTMirr` 的位置，两处都以 `FILE` 记录开头。
    Ntfs { mft_offset: u64, mirror_offset: u64 },
}

/// 解析一个 512 字节的引导扇区；不是 FAT / exFAT / NTFS，或参数不自洽时返回 `None`。
pub fn inspect_boot(sector: &[u8]) -> Option<BootSector> {
    match sector.get(3..11) {
        Some(b"EXFAT   ") => inspect_exfat(sector),
        Some(b"NTFS    ") => inspect_ntfs(sector),
        _ => inspect_fat(sector),
    }
}

fn inspect_fat(sector: &[u8]) -> Option<BootSector> {
    let geometry = fat::parse_boot(sector)?;
    let bps = geometry.bytes_per_sector;
    let kind = match geometry.width {
        FatWidth::Fat12 => FsKind::Fat12,
        FatWidth::Fat16 => FsKind::Fat16,
        _ => FsKind::Fat32,
    };
    // FAT32 的备份引导扇区号记在 BPB 第 50 字节，0 或 0xFFFF 表示没有；必须落在保留区里
    let backup_at = if kind == FsKind::Fat32 {
        le16(sector, 50)
            .map(u64::from)
            .filter(|&backup| backup != 0 && backup < geometry.reserved)
            .and_then(|backup| backup.checked_mul(bps))
    } else {
        None
    };
    let fats = (0..geometry.fats)
        .map(|copy| {
            copy.checked_mul(geometry.fat_sectors)?
                .checked_add(geometry.reserved)?
                .checked_mul(bps)
        })
        .collect::<Option<Vec<u64>>>()?;
    Some(BootSector {
        kind,
        bytes_per_sector: bps,
        volume_bytes: geometry.total_sectors.checked_mul(bps)?,
        backup_at,
        layout: BootLayout::Fat {
            media: *sector.get(21)?,
            fats,
        },
    })
}

fn inspect_exfat(sector: &[u8]) -> Option<BootSector> {
    let geometry = exfat::parse_boot(sector)?;
    let bps = 1u64 << geometry.sector_shift;
    let volume_sectors = le64(sector, 72)?;
    // 规范：卷至少要装得下簇区
    let heap_end = u64::from(geometry.cluster_count)
        .checked_shl(geometry.cluster_shift)?
        .checked_add(geometry.heap_offset)?;
    if volume_sectors < heap_end {
        return None;
    }
    Some(BootSector {
        kind: FsKind::Exfat,
        bytes_per_sector: bps,
        volume_bytes: volume_sectors.checked_mul(bps)?,
        backup_at: Some(EXFAT_BACKUP_SECTOR * bps),
        layout: BootLayout::Exfat {
            fat_offset: geometry.fat_offset.checked_mul(bps)?,
        },
    })
}

fn inspect_ntfs(sector: &[u8]) -> Option<BootSector> {
    let geometry = ntfs::parse_boot(sector)?;
    let bps = u64::from(le16(sector, 0x0B)?);
    // 引导扇区里的扇区数不含末尾的备份引导扇区
    Some(BootSector {
        kind: FsKind::Ntfs,
        bytes_per_sector: bps,
        volume_bytes: geometry.total_bytes.checked_add(bps)?,
        backup_at: Some(geometry.total_bytes),
        layout: BootLayout::Ntfs {
            mft_offset: geometry.mft_lcn.checked_mul(geometry.cluster_size)?,
            mirror_offset: geometry.mirror_lcn.checked_mul(geometry.cluster_size)?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples::{ExfatImage, FatImage, FatType, NtfsImage};

    #[test]
    fn fat_boot_sectors_report_their_layout() {
        let image = FatImage::new(FatType::Fat16).finish();
        let boot = inspect_boot(&image[..512]).unwrap();
        assert_eq!(boot.kind, FsKind::Fat16);
        assert_eq!(boot.volume_bytes, image.len() as u64);
        assert_eq!(boot.backup_at, None);
        let BootLayout::Fat { media, fats } = &boot.layout else {
            panic!("应为 FAT：{boot:?}");
        };
        assert_eq!(*media, 0xF8);
        assert_eq!(fats.len(), 2);
        assert_eq!(image[fats[1] as usize], 0xF8);
    }

    #[test]
    fn fat32_backup_sector_comes_from_the_bpb() {
        let mut image = FatImage::new(FatType::Fat32).finish();
        assert_eq!(
            inspect_boot(&image[..512]).unwrap().backup_at,
            Some(6 * 512)
        );
        // 0 表示没有备份
        image[50..52].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(inspect_boot(&image[..512]).unwrap().backup_at, None);
        // 超出保留区的不算
        image[50..52].copy_from_slice(&40u16.to_le_bytes());
        assert_eq!(inspect_boot(&image[..512]).unwrap().backup_at, None);
    }

    #[test]
    fn exfat_and_ntfs_volumes_cover_the_whole_sample() {
        let exfat = ExfatImage::new().finish();
        let boot = inspect_boot(&exfat[..512]).unwrap();
        assert_eq!(
            (boot.kind, boot.volume_bytes, boot.backup_at),
            (FsKind::Exfat, exfat.len() as u64, Some(12 * 512))
        );
        let ntfs = NtfsImage::new().finish();
        let boot = inspect_boot(&ntfs[..512]).unwrap();
        assert_eq!(
            (boot.kind, boot.volume_bytes, boot.backup_at),
            (
                FsKind::Ntfs,
                ntfs.len() as u64,
                Some(ntfs.len() as u64 - 512)
            )
        );
        let BootLayout::Ntfs { mft_offset, .. } = boot.layout else {
            panic!("应为 NTFS");
        };
        assert_eq!(&ntfs[mft_offset as usize..mft_offset as usize + 4], b"FILE");
    }

    #[test]
    fn other_sectors_are_rejected() {
        assert_eq!(inspect_boot(&[0u8; 512]), None);
        assert_eq!(inspect_boot(&[0u8; 10]), None);
        let mut exfat = ExfatImage::new().finish();
        // 卷长度装不下簇区
        exfat[72..80].copy_from_slice(&16u64.to_le_bytes());
        assert_eq!(inspect_boot(&exfat[..512]), None);
    }
}
