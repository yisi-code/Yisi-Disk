use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::mem::{offset_of, size_of};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::{io, ptr};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DISK_GEOMETRY, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY,
    IOCTL_DISK_GET_LENGTH_INFO,
};

use crate::device::validate_sector_size;
use crate::read::{read_aligned, read_file_at};
use crate::{BlockDevice, IoError};

/// 本进程认领过的句柄值。同一个值在进程生命周期内只能认领一次，引擎每个任务一个进程，够用。
static ADOPTED: Mutex<BTreeSet<usize>> = Mutex::new(BTreeSet::new());

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
        let size = query_size(file.as_raw_handle()).map_err(|source| IoError::Query {
            what: "容量",
            source,
        })?;
        let sector_size =
            query_sector_size(file.as_raw_handle()).map_err(|source| IoError::Query {
                what: "扇区大小",
                source,
            })?;
        Ok(Self {
            file,
            size,
            sector_size: validate_sector_size(sector_size)?,
        })
    }

    /// 认领磁盘代理复制进本进程的只读磁盘句柄，`handle` 是代理答复里的句柄值。
    ///
    /// 只接受能回答磁盘 IOCTL 的句柄，同一个值只能认领一次。认领失败时既不接管、也不关闭这个句柄。
    pub fn adopt_handle(handle: u64) -> Result<Self, IoError> {
        let reject = |source: io::Error| IoError::Adopt { handle, source };
        let raw: HANDLE = usize::try_from(handle)
            .map(ptr::without_provenance_mut)
            .map_err(|_| reject(io::Error::other("句柄值超出地址范围")))?;

        // 检查和登记在同一把锁内完成，两个线程不会同时认领同一个值
        let mut adopted = ADOPTED.lock().unwrap_or_else(PoisonError::into_inner);
        if adopted.contains(&raw.addr()) {
            return Err(reject(io::Error::other("这个句柄已经被认领过")));
        }
        let size = query_size(raw).map_err(reject)?;
        let sector_size = validate_sector_size(query_sector_size(raw).map_err(reject)?)?;
        adopted.insert(raw.addr());

        // SAFETY: 句柄能回答磁盘 IOCTL，说明它是本进程里有效的磁盘设备句柄。引擎没有权限自己打开磁盘，
        // 这类句柄只能是代理用 DuplicateHandle 送进来的；ADOPTED 保证同一个值只被认领一次，
        // 所以这里构造的 OwnedHandle 独占它，关闭时不会与其他所有者重复关闭。
        let file = File::from(unsafe { OwnedHandle::from_raw_handle(raw) });
        Ok(Self {
            file,
            size,
            sector_size,
        })
    }
}

impl AsHandle for WindowsDisk {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.file.as_handle()
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

fn query_size(handle: HANDLE) -> io::Result<u64> {
    let mut out = [0u8; size_of::<GET_LENGTH_INFORMATION>()];
    ioctl_read(handle, IOCTL_DISK_GET_LENGTH_INFO, &mut out)?;
    let length = i64::from_ne_bytes(field(&out, offset_of!(GET_LENGTH_INFORMATION, Length))?);
    u64::try_from(length).map_err(|_| invalid_data(format!("容量为负数：{length}")))
}

fn query_sector_size(handle: HANDLE) -> io::Result<u32> {
    let mut out = [0u8; size_of::<DISK_GEOMETRY>()];
    ioctl_read(handle, IOCTL_DISK_GET_DRIVE_GEOMETRY, &mut out)?;
    Ok(u32::from_ne_bytes(field(
        &out,
        offset_of!(DISK_GEOMETRY, BytesPerSector),
    )?))
}

/// 只有输出缓冲区的同步 `DeviceIoControl`，要求驱动把 `out` 写满。
///
/// 输出按字节接收、再按字段偏移取值，不把驱动写入的内容直接当成结构体。
fn ioctl_read(handle: HANDLE, code: u32, out: &mut [u8]) -> io::Result<()> {
    let out_len = u32::try_from(out.len()).map_err(|_| io::Error::other("IOCTL 输出缓冲区过大"))?;
    let mut returned = 0u32;
    // SAFETY: 句柄无效时内核直接返回错误，不会访问本进程的其他内存；输出指针和长度都来自独占借用的 `out`，
    // 字节缓冲区可以接受驱动写入的任何内容；没有输入缓冲区；
    // 不传 OVERLAPPED，调用同步完成，返回后不再访问这些指针。
    let succeeded = unsafe {
        DeviceIoControl(
            handle,
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
