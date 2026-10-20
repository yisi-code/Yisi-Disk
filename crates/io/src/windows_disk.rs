use std::fs::{File, OpenOptions};
use std::mem::{offset_of, size_of};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::{io, ptr};

use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DISK_GEOMETRY, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY,
    IOCTL_DISK_GET_LENGTH_INFO,
};

use crate::device::validate_sector_size;
use crate::read::{read_aligned, read_file_at};
use crate::{BlockDevice, IoError};

/// Windows 物理磁盘（`\\.\PhysicalDriveN`），只读。
#[derive(Debug)]
pub struct WindowsDisk {
    file: File,
    size: u64,
    sector_size: u32,
}

impl WindowsDisk {
    /// 以只读方式打开物理磁盘，例如 `\\.\PhysicalDrive0`，需要管理员权限。
    ///
    /// 句柄只有读权限。共享方式允许其他程序同时读写这块盘（系统盘一定正被占用），本程序不加锁、不独占。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, IoError> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(path)
            .map_err(|source| IoError::Open {
                path: path.to_path_buf(),
                source,
            })?;
        let size = query_size(&file).map_err(|source| IoError::Query {
            what: "容量",
            source,
        })?;
        let sector_size = query_sector_size(&file).map_err(|source| IoError::Query {
            what: "扇区大小",
            source,
        })?;
        Ok(Self {
            file,
            size,
            sector_size: validate_sector_size(sector_size)?,
        })
    }
}

impl BlockDevice for WindowsDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        read_aligned(
            |position, chunk| read_file_at(&self.file, position, chunk),
            self.sector_size,
            self.size,
            offset,
            buf,
        )
    }
}

fn query_size(file: &File) -> io::Result<u64> {
    let mut out = [0u8; size_of::<GET_LENGTH_INFORMATION>()];
    ioctl_read(file, IOCTL_DISK_GET_LENGTH_INFO, &mut out)?;
    let length = i64::from_ne_bytes(field(&out, offset_of!(GET_LENGTH_INFORMATION, Length))?);
    u64::try_from(length).map_err(|_| invalid_data(format!("容量为负数：{length}")))
}

fn query_sector_size(file: &File) -> io::Result<u32> {
    let mut out = [0u8; size_of::<DISK_GEOMETRY>()];
    ioctl_read(file, IOCTL_DISK_GET_DRIVE_GEOMETRY, &mut out)?;
    Ok(u32::from_ne_bytes(field(
        &out,
        offset_of!(DISK_GEOMETRY, BytesPerSector),
    )?))
}

/// 只有输出缓冲区的同步 `DeviceIoControl`，要求驱动把 `out` 写满。
///
/// 输出按字节接收、再按字段偏移取值，不把驱动写入的内容直接当成结构体。
fn ioctl_read(file: &File, code: u32, out: &mut [u8]) -> io::Result<()> {
    let out_len = u32::try_from(out.len()).map_err(|_| io::Error::other("IOCTL 输出缓冲区过大"))?;
    let mut returned = 0u32;
    // SAFETY: 句柄取自仍然存活的 `file`，调用期间有效；输出指针和长度都来自独占借用的 `out`，
    // 字节缓冲区可以接受驱动写入的任何内容；没有输入缓冲区；
    // 不传 OVERLAPPED，调用同步完成，返回后不再访问这些指针。
    let succeeded = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            ptr::null(),
            0,
            out.as_mut_ptr().cast(),
            out_len,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    if returned != out_len {
        return Err(invalid_data(format!(
            "IOCTL 只返回了 {returned} 字节，应为 {out_len} 字节"
        )));
    }
    Ok(())
}

/// 取出从 `offset` 起 `N` 个字节，供 `from_ne_bytes` 还原字段值。
fn field<const N: usize>(bytes: &[u8], offset: usize) -> io::Result<[u8; N]> {
    bytes
        .get(offset..offset + N)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| invalid_data("IOCTL 输出比结构体短".to_owned()))
}

fn invalid_data(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
