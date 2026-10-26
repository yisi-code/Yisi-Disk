//! 文件系统识别；目录读取与文件提取；空闲空间位图；引导扇区与超级块的检查和重建；exFAT / FAT 一致性检查。
//!
//! 目前能浏览的是 FAT12/16/32 与 exFAT：列目录（含已删除的项，并判断还能不能完整找回）、按簇读出文件。
//! 只读，不写盘。
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod clusters;
mod entry;
mod exfat;
mod fat;
mod reader;
mod region;
#[cfg(any(test, feature = "test-samples"))]
pub mod samples;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use yisi_io::{BlockDevice, IoError};

pub use entry::{Entry, FileRef, Listing, State};
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
        matches!(self, Self::Fat12 | Self::Fat16 | Self::Fat32 | Self::Exfat)
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
        Some(b"NTFS    ") => return Ok(FsKind::Ntfs),
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

enum Inner {
    Fat(fat::Fat),
    Exfat(exfat::Exfat),
}

/// 一个已打开、可以浏览的卷。
pub struct Volume {
    kind: FsKind,
    inner: Inner,
}

impl Volume {
    /// 打开设备上 `offset` 起 `length` 字节的分区。不是 FAT / exFAT 时返回 [`FsError::Unsupported`]。
    pub fn open(device: Arc<dyn BlockDevice>, offset: u64, length: u64) -> Result<Self, FsError> {
        let kind = detect(device.as_ref(), offset, length)?;
        let region = Region::new(device, offset, length);
        let inner = match kind {
            FsKind::Exfat => Inner::Exfat(exfat::Exfat::open(region)?),
            FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 => Inner::Fat(fat::Fat::open(region)?),
            other => return Err(FsError::Unsupported(other)),
        };
        let kind = match &inner {
            Inner::Fat(fat) => fat.kind(),
            Inner::Exfat(_) => FsKind::Exfat,
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
        }
    }

    pub fn cluster_size(&self) -> u64 {
        match &self.inner {
            Inner::Fat(fat) => fat.cluster_size(),
            Inner::Exfat(exfat) => exfat.cluster_size(),
        }
    }

    pub fn root(&self) -> FileRef {
        match &self.inner {
            Inner::Fat(fat) => fat.root(),
            Inner::Exfat(exfat) => exfat.root(),
        }
    }

    /// 列出目录 `dir` 里的项，按在目录里的顺序。
    pub fn list(&self, dir: &FileRef) -> Result<Listing, FsError> {
        match &self.inner {
            Inner::Fat(fat) => fat.list(dir),
            Inner::Exfat(exfat) => exfat.list(dir),
        }
    }

    /// 打开文件读内容。
    pub fn open_file(&self, file: &FileRef) -> Result<FileReader, FsError> {
        match &self.inner {
            Inner::Fat(fat) => fat.open_file(file),
            Inner::Exfat(exfat) => exfat.open_file(file),
        }
    }

    /// 首簇在分区里的字节偏移，供详情显示；没有数据的文件为 `None`。
    pub fn first_offset(&self, file: &FileRef) -> Option<u64> {
        match &self.inner {
            Inner::Fat(fat) => fat.cluster_offset(file.first_cluster),
            Inner::Exfat(exfat) => exfat.cluster_offset(file.first_cluster),
        }
    }
}
