//! 镜像输出位置的属性与稀疏文件：只读一部分时镜像要建成稀疏文件，没读的区域不占空间，
//! 往远处写也不用先把前面填 0。只查询属性、设置文件标志，不读写磁盘内容。

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_SPARSE_FILE, GetDiskFreeSpaceExW, GetVolumeInformationW, GetVolumePathNameW,
};
use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;

use crate::ioctl::ioctl;

/// `GetVolumeInformationW` 报告的文件系统标志：支持稀疏文件（`FILE_SUPPORTS_SPARSE_FILES`）。
const SUPPORTS_SPARSE_FILES: u32 = 0x0000_0040;
/// 卷根路径与文件系统名的缓冲区长度（UTF-16 单元）。
const NAME_BUFFER: usize = 1024;

/// 输出位置所在的卷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputVolume {
    /// 文件系统名，如 NTFS、exFAT、FAT32、ReFS。
    pub file_system: String,
    /// 支持稀疏文件。
    pub sparse: bool,
    /// 当前用户还能用的剩余空间（字节）。
    pub free_bytes: u64,
}

/// 查询 `path` 所在的卷。`path` 可以还不存在、也可以是已有的文件：取最近一级已存在的目录
/// （剩余空间只能按目录查，给文件会报「目录名称无效」）。
pub fn output_volume(path: impl AsRef<Path>) -> io::Result<OutputVolume> {
    let path = path.as_ref();
    let existing = path
        .ancestors()
        .find(|ancestor| !ancestor.as_os_str().is_empty() && ancestor.is_dir())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "路径及其上级目录都不存在"))?;
    let existing = wide(existing.as_os_str());

    let mut root = vec![0u16; NAME_BUFFER];
    // SAFETY: `existing` 以 0 结尾；`root` 的指针与长度来自独占借用的缓冲区，函数最多写入这么多个单元。
    let found =
        unsafe { GetVolumePathNameW(existing.as_ptr(), root.as_mut_ptr(), NAME_BUFFER as u32) };
    if found == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut flags = 0u32;
    let mut name = vec![0u16; NAME_BUFFER];
    // SAFETY: `root` 是上面写入的、以 0 结尾的卷根路径；不要卷名、序列号、最长文件名，传空指针与 0 长度；
    // 标志与文件系统名分别写进独占借用的 `flags` 与 `name`，长度如实传入。
    let queried = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut flags,
            name.as_mut_ptr(),
            NAME_BUFFER as u32,
        )
    };
    if queried == 0 {
        return Err(io::Error::last_os_error());
    }
    let name_len = name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(name.len());

    let mut free = 0u64;
    // SAFETY: `existing` 以 0 结尾；只要调用方可用的剩余空间，另外两项传空指针。
    let measured = unsafe {
        GetDiskFreeSpaceExW(
            existing.as_ptr(),
            &mut free,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if measured == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(OutputVolume {
        file_system: String::from_utf16_lossy(&name[..name_len]),
        sparse: flags & SUPPORTS_SPARSE_FILES != 0,
        free_bytes: free,
    })
}

/// 把文件标成稀疏文件：之后没写过的区域不占空间、读出来是 0。已经写过的数据不变。
pub fn make_sparse(file: &File) -> io::Result<()> {
    ioctl(file.as_raw_handle(), FSCTL_SET_SPARSE, &[], &mut []).map(|_| ())
}

/// 文件是不是稀疏文件。
pub fn is_sparse(file: &File) -> io::Result<bool> {
    Ok(file.metadata()?.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0)
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;

    use super::*;

    #[test]
    fn temp_directory_volume_is_described() {
        let dir = std::env::temp_dir();
        let volume = output_volume(dir.join("not-yet").join("image.img")).unwrap();
        assert!(!volume.file_system.is_empty());
        assert!(volume.free_bytes > 0);
        // 已有的文件：按它所在的目录查
        let file = dir.join(format!("yisi-volume-{}.img", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let of_file = output_volume(&file);
        std::fs::remove_file(&file).unwrap();
        assert_eq!(of_file.unwrap().file_system, volume.file_system);
    }

    #[test]
    fn sparse_file_keeps_unwritten_areas_as_zero() {
        let dir = std::env::temp_dir();
        if !output_volume(&dir).unwrap().sparse {
            return;
        }
        let path = dir.join(format!("yisi-sparse-{}.img", std::process::id()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        assert!(!is_sparse(&file).unwrap());
        make_sparse(&file).unwrap();
        assert!(is_sparse(&file).unwrap());
        file.set_len(1 << 30).unwrap();
        std::os::windows::fs::FileExt::seek_write(&file, b"tail", (1 << 30) - 4).unwrap();
        let mut writer = &file;
        writer.flush().unwrap();
        let mut head = [1u8; 16];
        std::os::windows::fs::FileExt::seek_read(&file, &mut head, 4096).unwrap();
        assert_eq!(head, [0u8; 16]);
        drop(file);
        std::fs::remove_file(&path).unwrap();
    }
}
