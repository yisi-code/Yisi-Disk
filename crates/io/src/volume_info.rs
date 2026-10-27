//! 已挂载的卷的属性（卷标、文件系统、是否支持稀疏文件、总容量与剩余空间），以及各盘符落在哪些物理磁盘上。
//! 只向系统查询，与资源管理器显示容量的方式相同，不直接读磁盘，不需要管理员权限。

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
};

use crate::volume::{Existing, nearest_existing};
use crate::{volume_disk_numbers, volume_root as root_of};

/// `GetVolumeInformationW` 报告的文件系统标志：支持稀疏文件（`FILE_SUPPORTS_SPARSE_FILES`）。
const SUPPORTS_SPARSE_FILES: u32 = 0x0000_0040;
/// `GetDriveTypeW` 的结果里只看本机的硬盘与可移动盘（`DRIVE_REMOVABLE`、`DRIVE_FIXED`）；
/// 网络盘、光驱、内存盘不在物理磁盘上，也免得查网络盘时卡住。
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
/// 卷根路径、卷标与文件系统名的缓冲区长度（UTF-16 单元）。
const NAME_BUFFER: usize = 1024;

/// 一个卷的属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// 卷标；没有时为空。
    pub label: String,
    /// 文件系统名，如 NTFS、exFAT、FAT32、ReFS。
    pub file_system: String,
    /// 支持稀疏文件。
    pub sparse: bool,
    pub total_bytes: u64,
    /// 当前用户还能用的剩余空间。
    pub free_bytes: u64,
}

/// 挂载在盘符上的卷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountedVolume {
    /// 盘符，如 `C`。
    pub letter: char,
    /// 卷的 GUID 名（[`volume_guid`]）。
    pub guid: String,
    pub info: VolumeInfo,
    /// 卷所在的物理磁盘编号；跨盘的卷有多个。
    pub disks: Vec<u32>,
}

/// `path` 最近一级已存在的目录（以 0 结尾的 UTF-16）：`path` 可以还不存在、也可以是已有的文件。
fn existing_dir(path: &Path) -> io::Result<Vec<u16>> {
    Ok(wide(
        nearest_existing(path, Existing::Directory)?.as_os_str(),
    ))
}

/// 目录所在卷的根路径，如 `C:\`（以 0 结尾）。
fn volume_root(existing: &[u16]) -> io::Result<Vec<u16>> {
    let mut root = vec![0u16; NAME_BUFFER];
    // SAFETY: `existing` 以 0 结尾；`root` 的指针与长度来自独占借用的缓冲区，函数最多写入这么多个单元。
    let found =
        unsafe { GetVolumePathNameW(existing.as_ptr(), root.as_mut_ptr(), NAME_BUFFER as u32) };
    if found == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(root)
}

/// `path` 所在的卷的 GUID 名，如 `\\?\Volume{0a1b…}\`：同一个卷换了盘符、重新插拔也不变，用来认卷、比较两个路径在不在同一个卷上。
pub fn volume_guid(path: impl AsRef<Path>) -> io::Result<String> {
    let root = volume_root(&existing_dir(path.as_ref())?)?;
    let mut name = vec![0u16; NAME_BUFFER];
    // SAFETY: `root` 是以 0 结尾、以反斜杠结尾的卷根路径；`name` 的指针与长度来自独占借用的缓冲区。
    let found = unsafe {
        GetVolumeNameForVolumeMountPointW(root.as_ptr(), name.as_mut_ptr(), NAME_BUFFER as u32)
    };
    if found == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(until_nul(&name))
}

/// 查询 `path` 所在的卷。`path` 可以还不存在、也可以是已有的文件：取最近一级已存在的目录。
pub fn volume_info(path: impl AsRef<Path>) -> io::Result<VolumeInfo> {
    let existing = existing_dir(path.as_ref())?;
    let root = volume_root(&existing)?;

    let mut flags = 0u32;
    let mut label = vec![0u16; NAME_BUFFER];
    let mut name = vec![0u16; NAME_BUFFER];
    // SAFETY: `root` 是上面写入的、以 0 结尾的卷根路径；不要序列号与最长文件名，传空指针；
    // 卷标、标志与文件系统名分别写进独占借用的 `label`、`flags`、`name`，长度如实传入。
    let queried = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            NAME_BUFFER as u32,
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

    let mut free = 0u64;
    let mut total = 0u64;
    // SAFETY: `existing` 以 0 结尾；可用空间与总容量写进独占借用的两个变量，剩余的总空闲量不要，传空指针。
    let measured =
        unsafe { GetDiskFreeSpaceExW(existing.as_ptr(), &mut free, &mut total, ptr::null_mut()) };
    if measured == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(VolumeInfo {
        label: until_nul(&label),
        file_system: until_nul(&name),
        sparse: flags & SUPPORTS_SPARSE_FILES != 0,
        total_bytes: total,
        free_bytes: free,
    })
}

/// 本机硬盘与可移动盘上挂载的卷，按盘符排序。没有介质的读卡器、查不到所在磁盘的卷跳过。
pub fn mounted_volumes() -> Vec<MountedVolume> {
    // SAFETY: 没有参数，只返回各盘符是否存在的位掩码。
    let letters = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|index| letters & (1 << index) != 0)
        .filter_map(|index| {
            let letter = char::from(b'A' + index);
            let root = root_of(letter);
            // SAFETY: 参数是以 0 结尾的根路径。
            let kind = unsafe { GetDriveTypeW(wide(OsStr::new(&root)).as_ptr()) };
            if kind != DRIVE_FIXED && kind != DRIVE_REMOVABLE {
                return None;
            }
            let info = volume_info(&root).ok()?;
            let disks = volume_disk_numbers(&root).ok()?;
            let guid = volume_guid(&root).ok()?;
            Some(MountedVolume {
                letter,
                guid,
                info,
                disks,
            })
        })
        .collect()
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain([0]).collect()
}

fn until_nul(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_directory_volume_is_described() {
        let dir = std::env::temp_dir();
        let volume = volume_info(dir.join("not-yet").join("image.img")).unwrap();
        assert!(!volume.file_system.is_empty());
        assert!(volume.free_bytes > 0);
        assert!(volume.total_bytes >= volume.free_bytes);
        // 已有的文件：按它所在的目录查
        let file = dir.join(format!("yisi-volume-{}.img", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let of_file = volume_info(&file);
        std::fs::remove_file(&file).unwrap();
        assert_eq!(of_file.unwrap().file_system, volume.file_system);
    }

    #[test]
    fn the_system_drive_is_listed_with_its_disk() {
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        let letter = system.chars().next().unwrap().to_ascii_uppercase();
        let volumes = mounted_volumes();
        let found = volumes
            .iter()
            .find(|volume| volume.letter == letter)
            .unwrap_or_else(|| panic!("没列出系统盘 {letter}：{volumes:?}"));
        assert!(!found.disks.is_empty());
        assert!(found.info.total_bytes > 0);
    }
}
