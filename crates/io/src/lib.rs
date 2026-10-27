//! 块设备抽象：Windows / macOS / Linux 后端，raw、分段 raw、E01 镜像，读缓存，
//! 坏扇区读取策略，能识别坏区的镜像。
//!
//! 允许 `unsafe`，仅用于平台系统调用，每处都要写 `// SAFETY:` 说明。

mod device;
#[cfg(windows)]
mod disk_list;
mod error;
mod imaging;
#[cfg(windows)]
mod ioctl;
mod raw;
mod read;
#[cfg(windows)]
mod volume;
mod window;
#[cfg(windows)]
mod windows_disk;

pub use device::BlockDevice;
#[cfg(windows)]
pub use disk_list::{BusType, DiskInfo, list_disks};
pub use error::IoError;
pub use imaging::{
    BadMap, ImageStore, ImagingControl, ImagingOptions, ImagingOutcome, MapError, MapHeader, Round,
    Segment, SegmentState, run_imaging,
};
pub use raw::RawImage;
#[cfg(windows)]
pub use volume::volume_disk_numbers;
pub use window::Window;
#[cfg(windows)]
pub use windows_disk::WindowsDisk;
