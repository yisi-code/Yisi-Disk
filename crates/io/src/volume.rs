//! 查询一个路径所在的卷落在哪些物理磁盘上：只查询属性，不读写磁盘内容，不需要管理员权限。

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GetFinalPathNameByHandleW, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
    VOLUME_NAME_GUID,
};
use windows_sys::Win32::System::Ioctl::{DISK_EXTENT, VOLUME_DISK_EXTENTS};

use crate::IoError;
use crate::ioctl::{field, invalid_data, ioctl};

/// 磁盘区段查询的缓冲区：结构体头 8 字节，之后每个区段 24 字节，够一百多块盘组成的卷。
const EXTENTS_BUFFER_SIZE: usize = 4096;
/// 卷的 GUID 路径以此开头，例如 `\\?\Volume{0a1b…}\`。
const VOLUME_PREFIX: &str = r"\\?\Volume{";

/// 输出写不写到源盘上的判断：`output_disks`（输出所在卷的磁盘，[`volume_disk_numbers`] 的结果）里没有
/// `source_disk` 才通过。任何一项查不到都不通过：宁可换个位置，也不冒险写回源盘。
/// 引擎的边扫边存、复制、制作镜像与宿主保存检查报告共用。
pub fn ensure_off_source(
    source_disk: Result<u32, IoError>,
    output_disks: Result<Vec<u32>, IoError>,
) -> Result<(), OnSource> {
    let source_disk = source_disk.map_err(OnSource::SourceUnknown)?;
    let output_disks = output_disks.map_err(OnSource::OutputUnknown)?;
    if output_disks.contains(&source_disk) {
        return Err(OnSource::OnSource(source_disk));
    }
    Ok(())
}

/// 源是一个卷（盘符 `letter`）时的判断：输出所在的卷（[`crate::volume_guid`] 的结果）不能就是它。
/// 同一块盘上的别的卷可以：写在那里不会覆盖源卷的空闲空间。任何一项查不到都不通过。
pub fn ensure_off_volume(
    letter: char,
    source_volume: Result<String, IoError>,
    output_volume: Result<String, IoError>,
) -> Result<(), OnSource> {
    let source_volume = source_volume.map_err(OnSource::SourceUnknown)?;
    let output_volume = output_volume.map_err(OnSource::OutputUnknown)?;
    if source_volume.eq_ignore_ascii_case(&output_volume) {
        return Err(OnSource::OnSourceVolume(letter));
    }
    Ok(())
}

/// [`ensure_off_source`]、[`ensure_off_volume`] 不通过的原因。
#[derive(Debug, thiserror::Error)]
pub enum OnSource {
    #[error("无法确认源在哪里")]
    SourceUnknown(#[source] IoError),
    #[error("无法确认输出位置所在的磁盘或卷")]
    OutputUnknown(#[source] IoError),
    #[error("输出位置在源盘（磁盘 {0}）上")]
    OnSource(u32),
    #[error("输出位置就在源卷（{0}:）上")]
    OnSourceVolume(char),
}

/// `path` 所在的卷落在哪些物理磁盘上，编号与 `\\.\PhysicalDriveN` 的 N 相同；跨盘的卷会有多个。
///
/// `path` 可以还不存在：取最近一级已存在的文件或目录，打开它（随目录联接、符号链接走到真实位置），
/// 再查它所在卷的磁盘区段。还不存在的那几级目录之后才创建，不会改变所在的卷。
/// 网络路径、查不到磁盘区段的卷都返回错误。
pub fn volume_disk_numbers(path: impl AsRef<Path>) -> Result<Vec<u32>, IoError> {
    let path = path.as_ref();
    let locate = |path: &Path, source| IoError::Locate {
        path: path.to_path_buf(),
        source,
    };
    let existing = nearest_existing(path, Existing::Any).map_err(|source| locate(path, source))?;
    let open = |path: &Path, flags: u32| {
        OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(flags)
            .open(path)
            .map_err(|source| IoError::Open {
                path: path.to_path_buf(),
                source,
            })
    };
    let directory = open(existing, FILE_FLAG_BACKUP_SEMANTICS)?;
    let final_path = final_path_by_guid(&directory).map_err(|source| locate(existing, source))?;
    let volume = volume_root(&final_path).ok_or_else(|| {
        let shown = String::from_utf16_lossy(&final_path);
        locate(
            existing,
            invalid_data(format!("不是卷的 GUID 路径：{shown}")),
        )
    })?;
    let volume = open(&PathBuf::from(OsString::from_wide(volume)), 0)?;

    let mut extents = [0u8; EXTENTS_BUFFER_SIZE];
    let returned = ioctl(
        volume.as_raw_handle(),
        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
        &[],
        &mut extents,
    )
    .map_err(|source| locate(existing, source))?;
    parse_disk_extents(extents.get(..returned).unwrap_or_default())
        .map_err(|source| locate(existing, source))
}

/// 往上找最近一级已存在的路径时，停在哪一种上。
#[derive(Clone, Copy)]
pub(crate) enum Existing {
    /// 文件或目录都行：打开它时会随符号链接走到真实位置。
    Any,
    /// 只停在目录上：剩余空间只能按目录查，给文件会报「目录名称无效」。
    Directory,
}

/// `path` 本身或最近一级已存在的上级（`path` 可以还不存在）。查输出位置所在的卷时共用。
pub(crate) fn nearest_existing(path: &Path, kind: Existing) -> io::Result<&Path> {
    path.ancestors()
        .find(|ancestor| {
            !ancestor.as_os_str().is_empty()
                && match kind {
                    Existing::Any => ancestor.exists(),
                    Existing::Directory => ancestor.is_dir(),
                }
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "路径及其上级目录都不存在"))
}

/// 以卷的 GUID 路径表示的完整路径，例如 `\\?\Volume{…}\Users\me`，不含结尾的 0。
fn final_path_by_guid(file: &File) -> io::Result<Vec<u16>> {
    let mut buffer = vec![0u16; 512];
    loop {
        let capacity = u32::try_from(buffer.len()).map_err(|_| io::Error::other("路径过长"))?;
        // SAFETY: 句柄来自仍然存活的 `file`；缓冲区指针和长度来自独占借用的 `buffer`，
        // 函数最多写入 `capacity` 个 u16，返回后不再访问这个指针。
        let written = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                capacity,
                VOLUME_NAME_GUID | FILE_NAME_NORMALIZED,
            )
        };
        let written = usize::try_from(written).map_err(|_| io::Error::other("路径过长"))?;
        if written == 0 {
            return Err(io::Error::last_os_error());
        }
        // 缓冲区不够时返回需要的长度（含结尾的 0），够时返回写入的长度（不含 0）
        if written < buffer.len() {
            buffer.truncate(written);
            return Ok(buffer);
        }
        buffer.resize(written, 0);
    }
}

/// 从 `\\?\Volume{…}\…` 里取出卷本身的路径 `\\?\Volume{…}`（不带结尾的反斜杠，打开的是卷设备而不是根目录）。
fn volume_root(path: &[u16]) -> Option<&[u16]> {
    let prefix: Vec<u16> = VOLUME_PREFIX.encode_utf16().collect();
    let rest = path.strip_prefix(prefix.as_slice())?;
    let close = rest.iter().position(|&unit| unit == u16::from(b'}'))?;
    let end = prefix.len() + close + 1;
    let after = path.get(end).copied();
    if after.is_some_and(|unit| unit != u16::from(b'\\')) {
        return None;
    }
    path.get(..end)
}

/// 解析 `VOLUME_DISK_EXTENTS`，返回去重后的磁盘编号。没有任何区段时视为查询失败，不当作「不在任何盘上」。
fn parse_disk_extents(bytes: &[u8]) -> io::Result<Vec<u32>> {
    let count = u32::from_ne_bytes(field(
        bytes,
        offset_of!(VOLUME_DISK_EXTENTS, NumberOfDiskExtents),
    )?);
    let count = usize::try_from(count).map_err(|_| invalid_data(format!("区段数 {count} 过大")))?;
    if count == 0 {
        return Err(invalid_data("卷没有报告所在的磁盘".to_owned()));
    }
    let first = offset_of!(VOLUME_DISK_EXTENTS, Extents);
    let mut disks = Vec::new();
    for index in 0..count {
        let extent = index
            .checked_mul(size_of::<DISK_EXTENT>())
            .and_then(|offset| offset.checked_add(first))
            .ok_or_else(|| invalid_data(format!("区段数 {count} 过大")))?;
        let disk = u32::from_ne_bytes(field(bytes, extent + offset_of!(DISK_EXTENT, DiskNumber))?);
        if !disks.contains(&disk) {
            disks.push(disk);
        }
    }
    disks.sort_unstable();
    Ok(disks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query_failed() -> IoError {
        IoError::Query {
            what: "测试",
            source: io::Error::other("查不到"),
        }
    }

    #[test]
    fn output_must_be_off_the_source_disk() {
        assert!(ensure_off_source(Ok(1), Ok(vec![0])).is_ok());
        assert!(ensure_off_source(Ok(0), Ok(vec![1, 2])).is_ok());
        assert!(matches!(
            ensure_off_source(Ok(0), Ok(vec![0])),
            Err(OnSource::OnSource(0))
        ));
        // 跨盘的卷只要有一段在源盘上就不行
        assert!(matches!(
            ensure_off_source(Ok(2), Ok(vec![1, 2, 3])),
            Err(OnSource::OnSource(2))
        ));
        // 查不到的一律不通过
        assert!(matches!(
            ensure_off_source(Err(query_failed()), Ok(vec![1])),
            Err(OnSource::SourceUnknown(_))
        ));
        assert!(matches!(
            ensure_off_source(Ok(0), Err(query_failed())),
            Err(OnSource::OutputUnknown(_))
        ));
    }

    #[test]
    fn output_must_be_off_the_source_volume() {
        let c = || Ok(r"\\?\Volume{aaaa}\".to_owned());
        let d = || Ok(r"\\?\Volume{bbbb}\".to_owned());
        // 同一块盘上的别的卷可以
        assert!(ensure_off_volume('C', c(), d()).is_ok());
        assert!(matches!(
            ensure_off_volume('C', c(), Ok(r"\\?\VOLUME{AAAA}\".to_owned())),
            Err(OnSource::OnSourceVolume('C'))
        ));
        assert!(matches!(
            ensure_off_volume('C', Err(query_failed()), d()),
            Err(OnSource::SourceUnknown(_))
        ));
        assert!(matches!(
            ensure_off_volume('C', c(), Err(query_failed())),
            Err(OnSource::OutputUnknown(_))
        ));
    }

    #[test]
    fn the_system_volume_has_a_guid_and_is_not_another_volume() {
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        let root = format!(r"{system}\");
        let guid = crate::volume_guid(&root).unwrap();
        assert!(guid.starts_with(VOLUME_PREFIX), "{guid}");
        let inside = crate::volume_guid(std::env::temp_dir());
        assert!(inside.is_ok());
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    /// 按 `VOLUME_DISK_EXTENTS` 的布局拼出给定磁盘编号的区段列表。
    fn extents(disks: &[u32]) -> Vec<u8> {
        let first = offset_of!(VOLUME_DISK_EXTENTS, Extents);
        let mut bytes = vec![0u8; first + disks.len() * size_of::<DISK_EXTENT>()];
        bytes[..4].copy_from_slice(&(disks.len() as u32).to_ne_bytes());
        for (index, disk) in disks.iter().enumerate() {
            let at = first + index * size_of::<DISK_EXTENT>() + offset_of!(DISK_EXTENT, DiskNumber);
            bytes[at..at + 4].copy_from_slice(&disk.to_ne_bytes());
        }
        bytes
    }

    #[test]
    fn disk_extents_are_deduplicated_and_sorted() {
        assert_eq!(parse_disk_extents(&extents(&[0])).unwrap(), [0]);
        assert_eq!(parse_disk_extents(&extents(&[3, 1, 3])).unwrap(), [1, 3]);
    }

    #[test]
    fn empty_or_truncated_extents_are_errors() {
        assert!(parse_disk_extents(&extents(&[])).is_err());
        assert!(parse_disk_extents(&[]).is_err());
        let mut short = extents(&[2, 5]);
        short.truncate(short.len() - size_of::<DISK_EXTENT>());
        assert!(parse_disk_extents(&short).is_err());
    }

    #[test]
    fn volume_root_keeps_only_the_volume() {
        let volume = r"\\?\Volume{0a1b2c3d-0000-1111-2222-333344445555}";
        let root = wide(volume);
        assert_eq!(
            volume_root(&wide(&format!(r"{volume}\Users\me"))),
            Some(root.as_slice())
        );
        assert_eq!(
            volume_root(&wide(&format!(r"{volume}\"))),
            Some(root.as_slice())
        );
        assert_eq!(volume_root(&root), Some(root.as_slice()));
        assert_eq!(volume_root(&wide(r"\\?\C:\Users")), None);
        assert_eq!(volume_root(&wide(r"\\?\UNC\server\share")), None);
        assert_eq!(volume_root(&wide(&format!("{volume}x"))), None);
        assert_eq!(volume_root(&wide(r"\\?\Volume{unterminated")), None);
    }

    /// 只查询属性：本仓库所在的卷一定在某块盘上；还不存在的子目录与它同卷。
    #[test]
    fn existing_and_missing_paths_resolve_to_the_same_disks() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let disks = volume_disk_numbers(here).unwrap();
        assert!(!disks.is_empty());
        assert_eq!(
            volume_disk_numbers(here.join("no-such-dir").join("deeper")).unwrap(),
            disks
        );
    }

    #[test]
    fn relative_path_without_existing_ancestor_is_an_error() {
        assert!(volume_disk_numbers("no-such-dir-yisi/deeper").is_err());
    }
}
