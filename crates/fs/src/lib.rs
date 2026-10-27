//! 文件系统识别；目录读取与文件提取；空闲空间位图；引导扇区与超级块的检查和重建；exFAT / FAT 一致性检查。
//!
//! 目前能浏览的是 FAT12/16/32、exFAT 与 NTFS：列目录（含已删除的项，并判断还能不能完整找回）、读出文件。
//! NTFS 的已删除项要先扫一遍 MFT（[`Volume::scan`]）才列得出。主引导扇区坏了时可以按备份引导扇区打开
//! （[`Volume::open_with`]）。只读，不写盘。
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

pub mod boot;
pub mod bpb;
mod clusters;
mod entry;
mod exfat;
mod fat;
mod ntfs;
mod reader;
mod region;
#[cfg(any(test, feature = "test-samples"))]
pub mod samples;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use yisi_io::{BlockDevice, IoError};

pub use boot::{BootCopy, BootLayout, BootSector, inspect_boot};
pub use entry::{ClusterRef, Entry, FileRef, Listing, Location, State, Unsupported};
/// FAT 短文件名的长文件名校验和与显示形式。文件系统检查（`yisi-repair`）用的也是这一份。
pub use fat::{short_checksum as fat_short_checksum, short_display as fat_short_display};
pub use reader::FileReader;

use crate::region::Region;

/// 一个目录最多读这么多字节（FAT 规范的上限是 2 MiB，exFAT 是 256 MiB），挡住构造出来的超长目录。
pub(crate) const MAX_DIR_BYTES: u64 = 64 << 20;
/// 一个目录最多列这么多项。
pub(crate) const MAX_ENTRIES: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    Fat12,
    Fat16,
    Fat32,
    Exfat,
    Ntfs,
    /// BitLocker 加密的卷，不解密就读不出内容。
    BitLocker,
    /// ext2 / ext3 / ext4。
    Ext,
    Unknown,
}

impl FsKind {
    /// 目前能浏览目录的文件系统。
    pub fn is_browsable(self) -> bool {
        matches!(
            self,
            Self::Fat12 | Self::Fat16 | Self::Fat32 | Self::Exfat | Self::Ntfs
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error(transparent)]
    Io(#[from] IoError),
    #[error("位置 {offset} 超出分区范围")]
    OutOfRange { offset: u64 },
    #[error("{0}")]
    Invalid(&'static str),
    #[error("这种文件系统暂不支持浏览")]
    Unsupported(FsKind),
    #[error("文件的簇链在第 {offset} 字节处中断，之后的内容读不出来")]
    ChainBroken { offset: u64 },
    #[error("文件定位信息不合法")]
    BadRef,
    #[error("这个文件的存放方式暂不支持读出：{0}")]
    Unreadable(Unsupported),
}

/// 扫描 MFT 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanSummary {
    /// MFT 里的记录数。
    pub records: u64,
    pub in_use: u64,
    /// 未使用、但还留着内容的文件与目录记录。
    pub deleted: u64,
    /// 已删除的名字数（有硬链接的文件有几个名字）。
    pub deleted_names: u64,
    /// 其中原来的目录已找不到的名字数。
    pub orphans: u64,
    /// 读不出的记录数。
    pub unreadable: u64,
}

/// 按分区开头的引导扇区（ext 是超级块）识别文件系统。读不出时返回错误，认不出时返回 [`FsKind::Unknown`]。
pub fn detect(device: &dyn BlockDevice, offset: u64, length: u64) -> Result<FsKind, IoError> {
    if length < 512 {
        return Ok(FsKind::Unknown);
    }
    let mut sector = [0u8; 512];
    device.read_exact_at(offset, &mut sector)?;
    match sector.get(3..11) {
        Some(b"EXFAT   ") => {
            return Ok(if exfat::parse_boot(&sector).is_some() {
                FsKind::Exfat
            } else {
                FsKind::Unknown
            });
        }
        Some(b"NTFS    ") => {
            return Ok(if ntfs::parse_boot(&sector).is_some() {
                FsKind::Ntfs
            } else {
                FsKind::Unknown
            });
        }
        Some(b"-FVE-FS-") => return Ok(FsKind::BitLocker),
        _ => {}
    }
    if let Some(geometry) = fat::parse_boot(&sector) {
        return Ok(match geometry.width {
            clusters::FatWidth::Fat12 => FsKind::Fat12,
            clusters::FatWidth::Fat16 => FsKind::Fat16,
            _ => FsKind::Fat32,
        });
    }
    // ext 的超级块在分区第 1024 字节处，魔数在其中第 56 字节
    if length >= 2048 {
        let mut magic = [0u8; 2];
        if device.read_exact_at(offset + 1024 + 56, &mut magic).is_ok() && magic == [0x53, 0xEF] {
            return Ok(FsKind::Ext);
        }
    }
    Ok(FsKind::Unknown)
}

/// 同 [`detect`]，`copy` 为 [`BootCopy::Backup`] 时按备份引导扇区识别；没有有效的备份时返回 [`FsKind::Unknown`]。
pub fn detect_with(
    device: &dyn BlockDevice,
    offset: u64,
    length: u64,
    copy: BootCopy,
) -> Result<FsKind, IoError> {
    match copy {
        BootCopy::Primary => detect(device, offset, length),
        BootCopy::Backup => Ok(
            boot::read_backup(device, offset, length)?.map_or(FsKind::Unknown, |(kind, _)| kind)
        ),
    }
}

enum Inner {
    Fat(fat::Fat),
    Exfat(exfat::Exfat),
    Ntfs(ntfs::Ntfs),
}

/// 一个已打开、可以浏览的卷。
pub struct Volume {
    kind: FsKind,
    inner: Inner,
}

impl Volume {
    /// 打开设备上 `offset` 起 `length` 字节的分区。不是 FAT / exFAT / NTFS 时返回 [`FsError::Unsupported`]。
    pub fn open(device: Arc<dyn BlockDevice>, offset: u64, length: u64) -> Result<Self, FsError> {
        Self::open_with(device, offset, length, BootCopy::Primary)
    }

    /// 同 [`open`](Self::open)，按 `copy` 指定的那一份引导扇区打开。用备份时文件系统的类型也按备份识别，
    /// 没有有效的备份引导扇区时返回 [`FsError::Invalid`]。
    pub fn open_with(
        device: Arc<dyn BlockDevice>,
        offset: u64,
        length: u64,
        copy: BootCopy,
    ) -> Result<Self, FsError> {
        let (kind, sector) = match copy {
            BootCopy::Primary => {
                let kind = detect(device.as_ref(), offset, length)?;
                if !kind.is_browsable() {
                    return Err(FsError::Unsupported(kind));
                }
                let mut sector = [0u8; 512];
                device.read_exact_at(offset, &mut sector)?;
                (kind, sector)
            }
            BootCopy::Backup => boot::read_backup(device.as_ref(), offset, length)?
                .ok_or(FsError::Invalid("没有找到有效的备份引导扇区"))?,
        };
        let region = Region::new(device, offset, length);
        let inner = match kind {
            FsKind::Exfat => Inner::Exfat(exfat::Exfat::open(region, &sector)?),
            FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 => {
                Inner::Fat(fat::Fat::open(region, &sector)?)
            }
            FsKind::Ntfs => Inner::Ntfs(ntfs::Ntfs::open(region, &sector)?),
            other => return Err(FsError::Unsupported(other)),
        };
        let kind = match &inner {
            Inner::Fat(fat) => fat.kind(),
            Inner::Exfat(_) => FsKind::Exfat,
            Inner::Ntfs(_) => FsKind::Ntfs,
        };
        Ok(Self { kind, inner })
    }

    pub fn kind(&self) -> FsKind {
        self.kind
    }

    pub fn label(&self) -> Option<&str> {
        match &self.inner {
            Inner::Fat(fat) => fat.label(),
            Inner::Exfat(exfat) => exfat.label(),
            Inner::Ntfs(ntfs) => ntfs.label(),
        }
    }

    pub fn cluster_size(&self) -> u64 {
        match &self.inner {
            Inner::Fat(fat) => fat.cluster_size(),
            Inner::Exfat(exfat) => exfat.cluster_size(),
            Inner::Ntfs(ntfs) => ntfs.cluster_size(),
        }
    }

    pub fn root(&self) -> FileRef {
        match &self.inner {
            Inner::Fat(fat) => fat.root(),
            Inner::Exfat(exfat) => exfat.root(),
            Inner::Ntfs(ntfs) => ntfs.root(),
        }
    }

    /// 卷上的时间是不是 UTC：NTFS 是，FAT / exFAT 记的是写入时的本地时间。
    pub fn times_utc(&self) -> bool {
        matches!(self.inner, Inner::Ntfs(_))
    }

    /// 列出已删除的项之前是否要先 [`scan`](Self::scan)：NTFS 还没扫过 MFT 时为 `true`。
    pub fn needs_scan(&self) -> bool {
        match &self.inner {
            Inner::Ntfs(ntfs) => ntfs.needs_scan(),
            _ => false,
        }
    }

    /// 扫一遍 MFT，找出已删除的项（NTFS；其他文件系统什么也不做）。扫过一次之后直接返回上次的结果。
    /// `progress(已处理字节, 总字节)` 返回 `false` 时停下，结果为 `None`，下次重新扫。
    pub fn scan(
        &self,
        progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<Option<ScanSummary>, FsError> {
        match &self.inner {
            Inner::Ntfs(ntfs) => ntfs.scan(progress),
            _ => Ok(Some(ScanSummary::default())),
        }
    }

    /// 列出目录 `dir` 里的项，按在目录里的顺序。
    pub fn list(&self, dir: &FileRef) -> Result<Listing, FsError> {
        match &self.inner {
            Inner::Fat(fat) => fat.list(dir),
            Inner::Exfat(exfat) => exfat.list(dir),
            Inner::Ntfs(ntfs) => ntfs.list(dir),
        }
    }

    /// 打开文件读内容。
    pub fn open_file(&self, file: &FileRef) -> Result<FileReader, FsError> {
        match &self.inner {
            Inner::Fat(fat) => fat.open_file(file),
            Inner::Exfat(exfat) => exfat.open_file(file),
            Inner::Ntfs(ntfs) => ntfs.open_file(file),
        }
    }
}
