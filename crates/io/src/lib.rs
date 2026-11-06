//! 块设备抽象：Windows / macOS / Linux 后端，raw、分段 raw、E01 镜像，读缓存，
//! 坏扇区读取策略，能识别坏区的镜像，读取计数，已挂载的卷的属性与稀疏文件，写入计划（只有数据，不能写）。
//!
//! 允许 `unsafe`，仅用于平台系统调用，每处都要写 `// SAFETY:` 说明。

mod adler32;
pub mod bytes;
mod device;
#[cfg(windows)]
mod disk_list;
mod disk_path;
mod error;
mod ewf;
mod file;
mod http;
mod image;
mod imaging;
#[cfg(windows)]
mod ioctl;
mod local_time;
mod metered;
mod plan;
mod process;
mod range;
mod raw;
mod read;
#[cfg(any(test, feature = "test-samples"))]
pub mod samples;
mod scan;
#[cfg(windows)]
mod signature;
mod sparse;
#[cfg(windows)]
mod volume;
#[cfg(windows)]
mod volume_info;
mod window;
#[cfg(windows)]
mod windows_disk;

pub use adler32::adler32;
pub use device::BlockDevice;
#[cfg(windows)]
pub use disk_list::{BusType, DiskInfo, PartitionStyle, disk_identity, list_disks};
pub use disk_path::{
    MAX_DISK_NUMBER, physical_drive_number, physical_drive_path, volume_letter, volume_path,
    volume_root,
};
pub use error::IoError;
pub use ewf::EwfImage;
pub use file::{temp_path, with_suffix, write_then_replace};
pub use http::{HttpError, http_get};
pub use image::{ImageFormat, OpenedImage, image_format, open_image};
pub use imaging::{
    BadMap, ImageStore, ImagingControl, ImagingOptions, ImagingOutcome, MapError, MapHeader,
    RecordedPartition, Round, Scope, Segment, SegmentState, map_path, recorded_sector_size,
    run_imaging,
};
#[cfg(windows)]
pub use ioctl::device_io_control;
pub use local_time::{NT_UNIX_EPOCH_SECONDS, local_to_utc};
pub use metered::{MeteredDevice, ReadMeter, ReadStats, SLOW_READ};
pub use plan::{
    DeviceIdentity, MAX_PLAN_BYTES, MAX_WRITES, Overlay, PlanError, SectorWrite, WritePlan,
    image_identity, merge_writes,
};
pub use process::wait_for_process_exit;
pub use range::ranges_overlap;
pub use raw::RawImage;
pub use scan::{SCAN_BLOCK, ScanControl, scan_device};
#[cfg(windows)]
pub use signature::verify_p256;
pub use sparse::{is_sparse, make_sparse};
#[cfg(windows)]
pub use volume::{
    DiskExtent, OnSource, VolumeOnDisk, ensure_off_source, ensure_off_volume, system_disks,
    volume_disk_numbers, volume_guid, volumes_on_disk,
};
#[cfg(windows)]
pub use volume_info::{MountedVolume, VolumeInfo, mounted_volumes, volume_info};
pub use window::Window;
#[cfg(windows)]
pub use windows_disk::WindowsDisk;
