//! 引导扇区的两份副本。主引导扇区在分区开头；备份的位置各文件系统不同：FAT32 一般在第 6 扇区
//! （引导扇区里记着，主引导扇区坏了就按 6 找），exFAT 的备份引导区从第 12 扇区起，NTFS 在卷的最后一个扇区
//! （卷长度比分区少一个扇区，正好是分区最后一个扇区）。FAT12/16 没有备份。
//!
//! 主引导扇区坏了时，扇区大小也就不可信了：按主引导扇区里还认得出的值、设备的逻辑扇区大小与其他常见值依次试，
//! 读出的扇区参数自洽、且记的扇区大小与它所在的位置相符才算数。

use yisi_io::{BlockDevice, IoError};
use yisi_part::bytes::{le16, le32, le64};

use crate::clusters::FatWidth;
use crate::{FsKind, exfat, fat, ntfs};

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
                geometry.width == FatWidth::Fat32
                    && geometry.bytes_per_sector == candidate.sector_size
            })
            .map(|_| FsKind::Fat32),
        BackupFamily::Exfat => exfat::parse_boot(sector)
            .filter(|geometry| 1u64 << geometry.sector_shift == candidate.sector_size)
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
    match primary.get(3..11) {
        Some(b"EXFAT   ") => Some(BackupFamily::Exfat),
        Some(b"NTFS    ") => Some(BackupFamily::Ntfs),
        _ if primary.get(82..90) == Some(b"FAT32   ".as_slice()) => Some(BackupFamily::Fat32),
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
