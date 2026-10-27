//! 块设备抽象：Windows / macOS / Linux 后端，raw、分段 raw、E01 镜像，读缓存，
//! 坏扇区读取策略，能识别坏区的镜像，读取计数，已挂载的卷的属性与稀疏文件。
//!
//! 允许 `unsafe`，仅用于平台系统调用，每处都要写 `// SAFETY:` 说明。

mod device;
#[cfg(windows)]
mod disk_list;
mod disk_path;
mod error;
mod imaging;
#[cfg(windows)]
mod ioctl;
mod metered;
mod raw;
mod read;
mod scan;
#[cfg(windows)]
mod sparse;
#[cfg(windows)]
mod volume;
#[cfg(windows)]
mod volume_info;
mod window;
#[cfg(windows)]
mod windows_disk;

pub use device::BlockDevice;
#[cfg(windows)]
pub use disk_list::{BusType, DiskInfo, PartitionStyle, list_disks};
pub use disk_path::{
    MAX_DISK_NUMBER, physical_drive_number, physical_drive_path, volume_letter, volume_path,
    volume_root,
};
pub use error::IoError;
pub use imaging::{
    BadMap, ImageStore, ImagingControl, ImagingOptions, ImagingOutcome, MapError, MapHeader,
    RecordedPartition, Round, Scope, Segment, SegmentState, run_imaging,
};
pub use metered::{MeteredDevice, ReadMeter, ReadStats, SLOW_READ};
pub use raw::RawImage;
pub use scan::{SCAN_BLOCK, ScanControl, scan_device};
#[cfg(windows)]
pub use sparse::{is_sparse, make_sparse};
#[cfg(windows)]
pub use volume::{OnSource, ensure_off_source, ensure_off_volume, volume_disk_numbers};
#[cfg(windows)]
pub use volume_info::{MountedVolume, VolumeInfo, mounted_volumes, volume_guid, volume_info};
pub use window::Window;
#[cfg(windows)]
pub use windows_disk::WindowsDisk;
