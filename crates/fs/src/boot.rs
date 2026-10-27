//! 引导扇区：单独解析一个引导扇区（[`inspect_boot`]，丢失分区搜索在不知道分区从哪开始时用），
//! 以及引导扇区的两份副本。主引导扇区在分区开头；备份的位置各文件系统不同：FAT32 一般在第 6 扇区
//! （引导扇区里记着，主引导扇区坏了就按 6 找），exFAT 的备份引导区从第 12 扇区起，NTFS 在卷的最后一个扇区
//! （卷长度比分区少一个扇区，正好是分区最后一个扇区）。FAT12/16 没有备份。参数是否自洽一律按 [`bpb`] 的规则。
//!
//! 主引导扇区坏了时，扇区大小也就不可信了：按主引导扇区里还认得出的值、设备的逻辑扇区大小与其他常见值依次试，
//! 读出的扇区参数自洽、且记的扇区大小与它所在的位置相符才算数。

use yisi_io::{BlockDevice, IoError};
use yisi_part::bytes::{le16, le32, le64};

use crate::format::FatWidth;
use crate::{FsKind, bpb, exfat, fat, ntfs};

/// 打开卷时用哪一份引导扇区。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BootCopy {
    #[default]
    Primary,
    Backup,
}

/// 有备份引导扇区的文件系统。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupFamily {
    Fat32,
    Exfat,
    Ntfs,
}

impl BackupFamily {
    /// 这种文件系统有没有备份引导扇区（FAT12/16 没有）。
    pub fn of(kind: FsKind) -> Option<Self> {
        match kind {
            FsKind::Fat32 => Some(Self::Fat32),
            FsKind::Exfat => Some(Self::Exfat),
            FsKind::Ntfs => Some(Self::Ntfs),
            _ => None,
        }
    }

    pub fn kind(self) -> FsKind {
        match self {
            Self::Fat32 => FsKind::Fat32,
            Self::Exfat => FsKind::Exfat,
            Self::Ntfs => FsKind::Ntfs,
        }
    }
}

/// 备份引导扇区可能在的一个位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub family: BackupFamily,
    /// 分区内的字节偏移。
    pub offset: u64,
    /// 按这个扇区大小算出的位置；那里的引导扇区记的扇区大小也要是它。
    pub sector_size: u64,
}

/// FAT32 的备份引导扇区通常在第 6 扇区（引导扇区第 50 字节记着）。
pub const FAT32_BACKUP_SECTOR: u64 = 6;
/// exFAT 的备份引导区从第 12 扇区起，与主引导区一样占 12 个扇区。
pub const EXFAT_BACKUP_SECTOR: u64 = 12;
const SECTOR_SIZES: [u64; 4] = [512, 4096, 1024, 2048];

/// 备份引导扇区可能在的位置，按优先次序：主引导扇区标识的文件系统在前；
/// 扇区大小先用主引导扇区里还认得出的，再用设备的逻辑扇区大小与其他常见值；
/// FAT32 先用主引导扇区里记的备份位置；NTFS 先按主引导扇区记的卷长度找，再找分区最后一个扇区。
pub fn backup_candidates(primary: &[u8], device_sector: u32, length: u64) -> Vec<Candidate> {
    let hinted = hinted_family(primary);
    let mut families = vec![BackupFamily::Fat32, BackupFamily::Exfat, BackupFamily::Ntfs];
    if let Some(family) = hinted {
        families.retain(|&other| other != family);
        families.insert(0, family);
    }
    let mut candidates = Vec::new();
    let mut push = |family, offset: Option<u64>, sector_size: u64| {
        let Some(offset) = offset else { return };
        let fits = offset > 0 && offset.checked_add(512).is_some_and(|end| end <= length);
        let candidate = Candidate {
            family,
            offset,
            sector_size,
        };
        if fits && !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    };
    for family in families {
        let hinted_size = (hinted == Some(family))
            .then(|| primary_sector_size(primary, family))
            .flatten();
        let mut sizes: Vec<u64> = hinted_size.into_iter().collect();
        for size in std::iter::once(u64::from(device_sector)).chain(SECTOR_SIZES) {
            if SECTOR_SIZES.contains(&size) && !sizes.contains(&size) {
                sizes.push(size);
            }
        }
        for size in sizes {
            match family {
                BackupFamily::Fat32 => {
                    if hinted == Some(family) && hinted_size == Some(size) {
                        let recorded = le16(primary, 50).map(u64::from);
                        let reserved = le16(primary, 14).map_or(0, u64::from);
                        if let Some(sector) =
                            recorded.filter(|&sector| sector > 0 && sector < reserved)
                        {
                            push(family, sector.checked_mul(size), size);
                        }
                    }
                    push(family, FAT32_BACKUP_SECTOR.checked_mul(size), size);
                }
                BackupFamily::Exfat => push(family, EXFAT_BACKUP_SECTOR.checked_mul(size), size),
                BackupFamily::Ntfs => {
                    if hinted == Some(family) && hinted_size == Some(size) {
                        let total = le64(primary, 0x28).filter(|&total| total > 0);
                        push(
                            family,
                            total.and_then(|total| total.checked_mul(size)),
                            size,
                        );
                    }
                    push(
                        family,
                        (length / size).checked_sub(1).map(|last| last * size),
                        size,
                    );
                }
            }
        }
    }
    candidates
}

/// `sector` 是不是 `candidate` 那种文件系统的有效备份引导扇区：参数自洽，且记的扇区大小与它的位置相符。
pub fn backup_kind(candidate: &Candidate, sector: &[u8]) -> Option<FsKind> {
    match candidate.family {
        BackupFamily::Fat32 => fat::parse_boot(sector)
            .filter(|geometry| {
                geometry.kind == FsKind::Fat32 && geometry.bytes_per_sector == candidate.sector_size
            })
            .map(|_| FsKind::Fat32),
        BackupFamily::Exfat => exfat::parse_boot(sector)
            .filter(|geometry| geometry.bytes_per_sector() == candidate.sector_size)
            .map(|_| FsKind::Exfat),
        BackupFamily::Ntfs => ntfs::parse_boot(sector)
            .filter(|geometry| geometry.bytes_per_sector == candidate.sector_size)
            .map(|_| FsKind::Ntfs),
    }
}

/// 读出分区里第一个有效的备份引导扇区。没找到时返回 `None`；每个位置都读不出时返回最后一个读错误。
pub(crate) fn read_backup(
    device: &dyn BlockDevice,
    offset: u64,
    length: u64,
) -> Result<Option<(FsKind, [u8; 512])>, IoError> {
    let mut primary = [0u8; 512];
    // 主引导扇区读不出也照样找备份，只是没有线索
    let primary_read = length >= 512 && device.read_exact_at(offset, &mut primary).is_ok();
    if !primary_read {
        primary = [0u8; 512];
    }
    let mut last_error = None;
    let mut read_any = false;
    for candidate in backup_candidates(&primary, device.sector_size(), length) {
        let Some(at) = offset.checked_add(candidate.offset) else {
            continue;
        };
        let mut sector = [0u8; 512];
        match device.read_exact_at(at, &mut sector) {
            Ok(()) => read_any = true,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        }
        if let Some(kind) = backup_kind(&candidate, &sector) {
            return Ok(Some((kind, sector)));
        }
    }
    match last_error {
        Some(error) if !read_any => Err(error),
        _ => Ok(None),
    }
}

/// 主引导扇区里的文件系统标识，参数坏了也还认得出：exFAT、NTFS 的 OEM 标识，FAT32 的类型字符串。
fn hinted_family(primary: &[u8]) -> Option<BackupFamily> {
    BackupFamily::of(remnant_kind(primary)?)
}

/// 参数能用的引导扇区是哪种文件系统（FAT12/16/32、exFAT、NTFS）：打开卷与文件系统检查都按它认主引导扇区。
pub fn usable_kind(sector: &[u8]) -> Option<FsKind> {
    match sector.get(3..11) {
        Some(b"EXFAT   ") => exfat::parse_boot(sector).map(|_| FsKind::Exfat),
        Some(b"NTFS    ") => ntfs::parse_boot(sector).map(|_| FsKind::Ntfs),
        Some(b"-FVE-FS-") => None,
        _ => fat::parse_boot(sector).map(|bpb| bpb.kind),
    }
}

/// 参数坏了也还认得出的文件系统标识：exFAT、NTFS、BitLocker 的 OEM 标识，FAT 的类型字符串
/// （FAT 的类型字符串只是提示，规范不要求，这里只用来在参数坏了时猜是哪种）。
pub fn remnant_kind(sector: &[u8]) -> Option<FsKind> {
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

/// 主引导扇区里记的扇区大小；不是常见值时为 `None`。
fn primary_sector_size(primary: &[u8], family: BackupFamily) -> Option<u64> {
    let size = match family {
        BackupFamily::Exfat => 1u64.checked_shl(u32::from(*primary.get(108)?))?,
        BackupFamily::Fat32 | BackupFamily::Ntfs => u64::from(le16(primary, 11)?),
    };
    SECTOR_SIZES.contains(&size).then_some(size)
}

/// exFAT 引导区的校验和：前 11 个扇区逐字节累加（每次先循环右移一位），跳过卷标志（106、107）与使用百分比（112）。
pub fn exfat_boot_checksum(region: &[u8]) -> u32 {
    region
        .iter()
        .enumerate()
        .filter(|(index, _)| !matches!(index, 106 | 107 | 112))
        .fold(0u32, |sum, (_, &byte)| {
            sum.rotate_right(1).wrapping_add(u32::from(byte))
        })
}

/// exFAT 校验和扇区里记的值（整个扇区重复同一个 32 位数）；各处不一致时为 `None`。
pub fn exfat_stored_checksum(sector: &[u8]) -> Option<u32> {
    let first = le32(sector, 0)?;
    sector
        .as_chunks::<4>()
        .0
        .iter()
        .all(|word| u32::from_le_bytes(*word) == first)
        .then_some(first)
}

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
    /// 各份 FAT 的位置；FAT 的第 0 项是介质描述符、其余位全为 1（[`fat_media_ok`](crate::format::fat_media_ok)）。
    Fat {
        width: FatWidth,
        media: u8,
        fats: Vec<u64>,
    },
    /// FAT 的位置（第 0 项固定是 `F8 FF FF FF`）；引导区第 11 扇区是前 11 个扇区的校验和，按扇区大小计。
    Exfat { fat_offset: u64 },
    /// `$MFT` 与 `$MFTMirr` 的位置，两处的第一条记录都是 `$MFT` 自己（[`mft_self_record_ok`](crate::mft_self_record_ok)）。
    Ntfs {
        mft_offset: u64,
        mirror_offset: u64,
        record_size: u64,
    },
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
    let bpb = bpb::check_fat(sector, None).geometry?;
    let bps = bpb.bytes_per_sector;
    let backup_at = bpb
        .backup_sector()
        .and_then(|sector| sector.checked_mul(bps));
    let fats = (0..bpb.fats).map(|copy| bpb.fat_offset(copy)).collect();
    Some(BootSector {
        kind: bpb.kind,
        bytes_per_sector: bps,
        volume_bytes: bpb.volume_bytes(),
        backup_at,
        layout: BootLayout::Fat {
            width: bpb.width(),
            media: bpb.media,
            fats,
        },
    })
}

fn inspect_exfat(sector: &[u8]) -> Option<BootSector> {
    let checked = bpb::check_exfat(sector);
    // 卷装不下簇区时不致命（照样能打开），但长度不可信，推算不出分区的范围
    if checked
        .problems
        .iter()
        .any(|problem| problem.rule == bpb::Rule::Layout)
    {
        return None;
    }
    let geometry = checked.geometry?;
    let bps = geometry.bytes_per_sector();
    Some(BootSector {
        kind: FsKind::Exfat,
        bytes_per_sector: bps,
        volume_bytes: geometry.volume_length.checked_mul(bps)?,
        backup_at: EXFAT_BACKUP_SECTOR.checked_mul(bps),
        layout: BootLayout::Exfat {
            fat_offset: geometry.fat_offset.checked_mul(bps)?,
        },
    })
}

fn inspect_ntfs(sector: &[u8]) -> Option<BootSector> {
    let geometry = bpb::check_ntfs(sector).geometry?;
    let bps = geometry.bytes_per_sector;
    let total = geometry.total_bytes();
    // 引导扇区里的扇区数不含末尾的备份引导扇区
    Some(BootSector {
        kind: FsKind::Ntfs,
        bytes_per_sector: bps,
        volume_bytes: total.checked_add(bps)?,
        backup_at: Some(total),
        layout: BootLayout::Ntfs {
            mft_offset: geometry.mft_lcn.checked_mul(geometry.cluster_size)?,
            mirror_offset: geometry.mirror_lcn.checked_mul(geometry.cluster_size)?,
            record_size: geometry.record_size,
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
        let BootLayout::Fat { media, fats, .. } = &boot.layout else {
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
