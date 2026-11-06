//! 文件系统识别；目录读取与文件提取；空闲空间位图；引导扇区与超级块的检查和重建；exFAT / FAT 一致性检查。
//!
//! 目前能浏览的是 FAT12/16/32、exFAT、NTFS 与 ext2/3/4：列目录（含已删除的项，并判断还能不能完整找回）、读出文件。
//! NTFS 的已删除项要先扫一遍 MFT（[`Volume::scan`]）才列得出；FAT / exFAT 可以按需扫一遍空闲簇，找回原来的目录
//! 已找不到的文件夹。主引导扇区（ext 是主超级块）坏了时可以按备份打开
//! （[`Volume::open_with`]）。只读，不写盘。
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

pub mod boot;
pub mod boot_write;
pub mod bpb;
mod clusters;
mod entry;
mod exfat;
mod ext;
mod fat;
pub mod format;
mod ntfs;
mod orphans;
mod reader;
mod region;
#[cfg(any(test, feature = "test-samples"))]
pub mod samples;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use yisi_io::{BlockDevice, IoError};

pub use boot::{BootCopy, BootLayout, BootSector, inspect_boot};
pub use entry::{
    ClusterRef, Entry, FileAttributes, FileRef, Listing, Location, Moment, Stamp, State,
    Unsupported,
};
/// ext 超级块：丢失分区搜索按它推算分区起点（[`ext_superblock_copy`]），再按打开卷的规则核对（[`ext_copy_confirmed`]）。
pub use ext::{
    ExtCopy, ext_copy_confirmed, ext_superblock_copy, has_magic as ext_superblock_magic,
};
/// FAT 短文件名的长文件名校验和与显示形式。文件系统检查（`yisi-repair`）用的也是这一份。
pub use fat::{short_checksum as fat_short_checksum, short_display as fat_short_display};
pub use ntfs::{
    AuditStage as NtfsAuditStage, Listed, NamedStream, NtfsAudit, SharedClusters, UnmarkedClusters,
    audit_ntfs, mft_bad_clusters_length, mft_record_layout, mft_record_ok, mft_self_record_ok,
    root_index_block_size,
};
pub use reader::FileReader;
pub use region::Region;

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
            Self::Fat12 | Self::Fat16 | Self::Fat32 | Self::Exfat | Self::Ntfs | Self::Ext
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

/// 扫描的结果：NTFS 扫 MFT；FAT / exFAT 读空闲簇找原来的目录已找不到的文件夹（[`Volume::optional_scan`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanSummary {
    /// MFT 里的记录数（FAT / exFAT 为 0）。
    pub records: u64,
    pub in_use: u64,
    /// 未使用、但还留着内容的文件与目录记录（FAT / exFAT 为 0）。
    pub deleted: u64,
    /// 已删除的名字数（有硬链接的文件有几个名字）；FAT / exFAT 是认出的文件夹里的文件与子文件夹项数。
    pub deleted_names: u64,
    /// 列在「找不到原目录的文件」里的项数：NTFS 是名字数，FAT / exFAT 是文件夹数。
    pub orphans: u64,
    /// 读不出的记录数；FAT / exFAT 是读不出的簇数。
    pub unreadable: u64,
    /// FAT / exFAT：读过的空闲空间字节数。
    pub free_bytes: u64,
}

/// 按分区开头的引导扇区（ext 是超级块）识别文件系统。读不出时返回错误，认不出时返回 [`FsKind::Unknown`]。
pub fn detect(device: &dyn BlockDevice, offset: u64, length: u64) -> Result<FsKind, IoError> {
    if length < 512 {
        return Ok(FsKind::Unknown);
    }
    let mut sector = [0u8; 512];
    device.read_exact_at(offset, &mut sector)?;
    if let Some(kind) = boot::usable_kind(&sector) {
        return Ok(kind);
    }
    match boot::remnant_kind(&sector) {
        Some(FsKind::BitLocker) => return Ok(FsKind::BitLocker),
        Some(FsKind::Exfat | FsKind::Ntfs) => return Ok(FsKind::Unknown),
        _ => {}
    }
    // ext 的超级块在分区第 1024 字节处：参数自洽才算（与打开卷同一条规则）
    if length >= 2048 {
        let mut superblock = [0u8; 1024];
        if let Some(at) = offset.checked_add(1024)
            && device.read_exact_at(at, &mut superblock).is_ok()
            && ext::superblock_ok(&superblock)
        {
            return Ok(FsKind::Ext);
        }
    }
    Ok(FsKind::Unknown)
}

/// 同 [`detect`]，`copy` 为 [`BootCopy::Backup`] 时按备份引导扇区（ext 是块组开头的备份超级块）识别；
/// 没有有效的备份时返回 [`FsKind::Unknown`]。
pub fn detect_with(
    device: &dyn BlockDevice,
    offset: u64,
    length: u64,
    copy: BootCopy,
) -> Result<FsKind, IoError> {
    match copy {
        BootCopy::Primary => detect(device, offset, length),
        BootCopy::Backup => Ok(match boot::read_backup(device, offset, length)? {
            Some((kind, _)) => kind,
            None if ext::find_backup(device, offset, length).is_some() => FsKind::Ext,
            None => FsKind::Unknown,
        }),
    }
}

enum Inner {
    Fat(fat::Fat),
    Exfat(exfat::Exfat),
    Ntfs(ntfs::Ntfs),
    Ext(ext::Ext),
}

/// 一个已打开、可以浏览的卷。
pub struct Volume {
    kind: FsKind,
    inner: Inner,
}

impl Volume {
    /// 打开设备上 `offset` 起 `length` 字节的分区。不是 FAT / exFAT / NTFS / ext 时返回 [`FsError::Unsupported`]。
    pub fn open(device: Arc<dyn BlockDevice>, offset: u64, length: u64) -> Result<Self, FsError> {
        Self::open_with(device, offset, length, BootCopy::Primary)
    }

    /// 同 [`open`](Self::open)，按 `copy` 指定的那一份引导扇区（ext 是超级块）打开。用备份时文件系统的类型也按备份识别，
    /// 没有有效的备份引导扇区或备份超级块时返回 [`FsError::Invalid`]。
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
                if kind == FsKind::Ext {
                    let region = Region::new(device, offset, length);
                    let superblock = ext::read_primary(&region)?;
                    return Ok(Self::ext(ext::Ext::open(region, &superblock)?));
                }
                let mut sector = [0u8; 512];
                device.read_exact_at(offset, &mut sector)?;
                (kind, sector)
            }
            BootCopy::Backup => match boot::read_backup(device.as_ref(), offset, length)? {
                Some(found) => found,
                None => {
                    let superblock = ext::find_backup(device.as_ref(), offset, length)
                        .ok_or(FsError::Invalid("没有找到有效的备份引导扇区或备份超级块"))?;
                    let region = Region::new(device, offset, length);
                    return Ok(Self::ext(ext::Ext::open(region, &superblock)?));
                }
            },
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
            Inner::Ext(_) => FsKind::Ext,
        };
        Ok(Self { kind, inner })
    }

    fn ext(ext: ext::Ext) -> Self {
        Self {
            kind: FsKind::Ext,
            inner: Inner::Ext(ext),
        }
    }

    pub fn kind(&self) -> FsKind {
        self.kind
    }

    pub fn label(&self) -> Option<&str> {
        match &self.inner {
            Inner::Fat(fat) => fat.label(),
            Inner::Exfat(exfat) => exfat.label(),
            Inner::Ntfs(ntfs) => ntfs.label(),
            Inner::Ext(ext) => ext.label(),
        }
    }

    pub fn cluster_size(&self) -> u64 {
        match &self.inner {
            Inner::Fat(fat) => fat.cluster_size(),
            Inner::Exfat(exfat) => exfat.cluster_size(),
            Inner::Ntfs(ntfs) => ntfs.cluster_size(),
            Inner::Ext(ext) => ext.block_size(),
        }
    }

    pub fn root(&self) -> FileRef {
        match &self.inner {
            Inner::Fat(fat) => fat.root(),
            Inner::Exfat(exfat) => exfat.root(),
            Inner::Ntfs(ntfs) => ntfs.root(),
            Inner::Ext(ext) => ext.root(),
        }
    }

    /// 卷上的时间是不是 UTC：NTFS 与 ext 是，FAT / exFAT 记的是写入时的本地时间。
    pub fn times_utc(&self) -> bool {
        matches!(self.inner, Inner::Ntfs(_) | Inner::Ext(_))
    }

    /// 列出已删除的项之前是否要先 [`scan`](Self::scan)：NTFS 还没扫过 MFT 时为 `true`。
    pub fn needs_scan(&self) -> bool {
        match &self.inner {
            Inner::Ntfs(ntfs) => ntfs.needs_scan(),
            _ => false,
        }
    }

    /// 可以按需扫描（FAT / exFAT）：不扫也列得出已删除的项，扫过之后多出原来的目录已找不到的文件夹。
    /// 要读一遍空闲空间，大分区很慢，所以不自动扫。
    pub fn optional_scan(&self) -> bool {
        matches!(self.inner, Inner::Fat(_) | Inner::Exfat(_))
    }

    /// 已经扫过（[`scan`](Self::scan) 的结果在）。
    pub fn is_scanned(&self) -> bool {
        match &self.inner {
            Inner::Fat(fat) => fat.is_scanned(),
            Inner::Exfat(exfat) => exfat.is_scanned(),
            Inner::Ntfs(ntfs) => !ntfs.needs_scan(),
            // ext 不用扫：删除的项列目录时就在
            Inner::Ext(_) => true,
        }
    }

    /// 扫一遍，找出已删除的项：NTFS 扫 MFT；FAT / exFAT 读目录树没用到的空闲簇，按内容认出原来的目录已找不到的
    /// 文件夹，列进根目录下的虚拟目录；ext 什么也不做。扫过一次之后直接返回上次的结果。
    /// `progress(已处理字节, 总字节)` 返回 `false` 时停下，结果为 `None`，下次重新扫；总字节还不知道时为 0。
    pub fn scan(
        &self,
        progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<Option<ScanSummary>, FsError> {
        match &self.inner {
            Inner::Fat(fat) => fat.scan(progress),
            Inner::Exfat(exfat) => exfat.scan(progress),
            Inner::Ntfs(ntfs) => ntfs.scan(progress),
            Inner::Ext(_) => Ok(Some(ScanSummary::default())),
        }
    }

    /// 列出目录 `dir` 里的项，按在目录里的顺序。
    pub fn list(&self, dir: &FileRef) -> Result<Listing, FsError> {
        match &self.inner {
            Inner::Fat(fat) => fat.list(dir),
            Inner::Exfat(exfat) => exfat.list(dir),
            Inner::Ntfs(ntfs) => ntfs.list(dir),
            Inner::Ext(ext) => ext.list(dir),
        }
    }

    /// 打开文件读内容。
    pub fn open_file(&self, file: &FileRef) -> Result<FileReader, FsError> {
        match &self.inner {
            Inner::Fat(fat) => fat.open_file(file),
            Inner::Exfat(exfat) => exfat.open_file(file),
            Inner::Ntfs(ntfs) => ntfs.open_file(file),
            Inner::Ext(ext) => ext.open_file(file),
        }
    }

    /// 文件的备用数据流（只有 NTFS 有，其他文件系统为空）。
    pub fn streams(&self, file: &FileRef) -> Result<Vec<NamedStream>, FsError> {
        match &self.inner {
            Inner::Ntfs(ntfs) => ntfs.streams(file),
            _ => Ok(Vec::new()),
        }
    }

    /// 读文件的一个备用数据流。
    pub fn open_stream(&self, file: &FileRef, name: &str) -> Result<FileReader, FsError> {
        match &self.inner {
            Inner::Ntfs(ntfs) => ntfs.open_stream(file, name),
            _ => Err(FsError::BadRef),
        }
    }
}
