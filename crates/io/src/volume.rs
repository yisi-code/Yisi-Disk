//! 查询一个路径所在的卷落在哪些物理磁盘上、一块磁盘上有哪些卷：只查询属性，不读写磁盘内容，不需要管理员权限。

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
    GetFinalPathNameByHandleW, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, VOLUME_NAME_GUID,
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

/// 源是一个卷（盘符 `letter`）时的判断：输出所在的卷（[`volume_guid`] 的结果）不能就是它。
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

/// 卷在一块磁盘上占的一段（字节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskExtent {
    pub disk: u32,
    pub offset: u64,
    pub length: u64,
}

/// 落在某块磁盘上的一个卷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeOnDisk {
    /// `\\?\Volume{…}`，不带结尾的反斜杠：打开的是卷设备，锁住、卸下卷都用它。
    pub path: String,
    /// 这个卷在这块盘上的区段（跨盘的卷只列这块盘上的）。
    pub extents: Vec<DiskExtent>,
}

impl VolumeOnDisk {
    /// 这个卷在这块盘上的区段与 `[offset, offset + length)` 是否相交。
    pub fn overlaps(&self, offset: u64, length: u64) -> bool {
        let end = offset.saturating_add(length);
        self.extents.iter().any(|extent| {
            offset < extent.offset.saturating_add(extent.length) && extent.offset < end
        })
    }
}

/// `path` 所在的卷落在哪些物理磁盘上，编号与 `\\.\PhysicalDriveN` 的 N 相同；跨盘的卷会有多个。
/// 卷怎么找见 [`locate_volume`]。网络路径、查不到磁盘区段的卷都返回错误。
pub fn volume_disk_numbers(path: impl AsRef<Path>) -> Result<Vec<u32>, IoError> {
    let (existing, volume) = locate_volume(path.as_ref())?;
    let extents = volume_extents(&PathBuf::from(OsString::from_wide(&volume))).map_err(
        |error| match error {
            IoError::Locate { source, .. } => locate(existing, source),
            other => other,
        },
    )?;
    let mut disks: Vec<u32> = extents.iter().map(|extent| extent.disk).collect();
    disks.sort_unstable();
    disks.dedup();
    Ok(disks)
}

/// Windows 所在的卷（`SystemRoot`）落在哪些磁盘上。整块系统盘不写：宿主生成计划前就拒绝，磁盘代理执行前再查一次。
/// 查不到时返回错误，调用方按「可能是系统盘」处理。
pub fn system_disks() -> Result<Vec<u32>, IoError> {
    let root = std::env::var_os("SystemRoot").ok_or_else(|| IoError::Query {
        what: "系统目录",
        source: io::Error::new(io::ErrorKind::NotFound, "没有 SystemRoot 环境变量"),
    })?;
    volume_disk_numbers(PathBuf::from(root))
}

/// 系统里落在磁盘 `disk` 上的全部卷，含没有盘符的（按卷的 GUID 名逐个枚举）。查不到区段的卷（光驱、
/// 还没准备好的读卡器等）不在任何盘上，跳过。写盘前据此找出要先锁住、卸下的卷。
pub fn volumes_on_disk(disk: u32) -> Result<Vec<VolumeOnDisk>, IoError> {
    let query = |source| IoError::Query {
        what: "系统里的卷",
        source,
    };
    let mut name = [0u16; 512];
    let capacity = u32::try_from(name.len()).unwrap_or(u32::MAX);
    // SAFETY: 缓冲区指针与长度来自独占借用的 `name`；返回的查找句柄在下面检查后由 FindVolumeClose 关闭。
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), capacity) };
    if find == INVALID_HANDLE_VALUE {
        return Err(query(io::Error::last_os_error()));
    }
    let mut volumes = Vec::new();
    loop {
        let text =
            String::from_utf16_lossy(name.split(|&unit| unit == 0).next().unwrap_or_default());
        let device = text.trim_end_matches('\\').to_owned();
        if let Ok(extents) = volume_extents(Path::new(&device)) {
            let mine: Vec<DiskExtent> = extents
                .into_iter()
                .filter(|extent| extent.disk == disk)
                .collect();
            if !mine.is_empty() {
                volumes.push(VolumeOnDisk {
                    path: device,
                    extents: mine,
                });
            }
        }
        name.fill(0);
        // SAFETY: `find` 是上面 FindFirstVolumeW 返回的有效查找句柄；缓冲区同上。
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), capacity) } == 0 {
            break;
        }
    }
    // SAFETY: `find` 是有效的查找句柄，只在这里关闭一次。
    unsafe { FindVolumeClose(find) };
    Ok(volumes)
}

/// 卷设备 `\\?\Volume{…}`（不带结尾的反斜杠）的磁盘区段。
fn volume_extents(volume: &Path) -> Result<Vec<DiskExtent>, IoError> {
    let opened = open_for_query(volume, 0)?;
    let mut extents = [0u8; EXTENTS_BUFFER_SIZE];
    let returned = ioctl(
        opened.as_raw_handle(),
        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
        &[],
        &mut extents,
    )
    .map_err(|source| locate(volume, source))?;
    parse_disk_extents(extents.get(..returned).unwrap_or_default())
        .map_err(|source| locate(volume, source))
}

/// `path` 所在的卷的 GUID 名，如 `\\?\Volume{0a1b…}\`：同一个卷换了盘符、重新插拔也不变，用来认卷、
/// 比较两个路径在不在同一个卷上。卷怎么找见 [`locate_volume`]，与 [`volume_disk_numbers`] 相同。
pub fn volume_guid(path: impl AsRef<Path>) -> Result<String, IoError> {
    Ok(String::from_utf16_lossy(&volume_guid_root(path.as_ref())?))
}

/// [`volume_guid`] 的 UTF-16 形式（以反斜杠结尾，不带结尾的 0）：按卷根查询卷属性、剩余空间时用。
pub(crate) fn volume_guid_root(path: &Path) -> Result<Vec<u16>, IoError> {
    let (_, mut volume) = locate_volume(path)?;
    volume.push(u16::from(b'\\'));
    Ok(volume)
}

/// `path` 所在的卷 `\\?\Volume{…}`（不带结尾的反斜杠），以及实际打开查询的那一级路径。
///
/// `path` 可以还不存在：取最近一级已存在的文件或目录，打开它（随目录联接、符号链接走到真实位置），
/// 从它的最终路径里取卷。还不存在的那几级目录之后才创建，不会改变所在的卷。
/// 所在磁盘、卷的 GUID 名、卷属性与剩余空间都经这里，同一个路径不会落到两个不同的卷。
fn locate_volume(path: &Path) -> Result<(&Path, Vec<u16>), IoError> {
    let existing = path
        .ancestors()
        .find(|ancestor| !ancestor.as_os_str().is_empty() && ancestor.exists())
        .ok_or_else(|| {
            locate(
                path,
                io::Error::new(io::ErrorKind::NotFound, "路径及其上级目录都不存在"),
            )
        })?;
    let opened = open_for_query(existing, FILE_FLAG_BACKUP_SEMANTICS)?;
    let final_path = final_path_by_guid(&opened).map_err(|source| locate(existing, source))?;
    let volume = volume_root(&final_path).ok_or_else(|| {
        let shown = String::from_utf16_lossy(&final_path);
        locate(
            existing,
            invalid_data(format!("不是卷的 GUID 路径：{shown}")),
        )
    })?;
    Ok((existing, volume.to_vec()))
}

/// 以零访问权限打开，只查询属性、不读写内容。
fn open_for_query(path: &Path, flags: u32) -> Result<File, IoError> {
    OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(flags)
        .open(path)
        .map_err(|source| IoError::Open {
            path: path.to_path_buf(),
            source,
        })
}

fn locate(path: &Path, source: io::Error) -> IoError {
    IoError::Locate {
        path: path.to_path_buf(),
        source,
    }
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

/// 解析 `VOLUME_DISK_EXTENTS`，按原样返回每个区段。没有任何区段时视为查询失败，不当作「不在任何盘上」。
fn parse_disk_extents(bytes: &[u8]) -> io::Result<Vec<DiskExtent>> {
    let count = u32::from_ne_bytes(field(
        bytes,
        offset_of!(VOLUME_DISK_EXTENTS, NumberOfDiskExtents),
    )?);
    let count = usize::try_from(count).map_err(|_| invalid_data(format!("区段数 {count} 过大")))?;
    if count == 0 {
        return Err(invalid_data("卷没有报告所在的磁盘".to_owned()));
    }
    let first = offset_of!(VOLUME_DISK_EXTENTS, Extents);
    let mut extents = Vec::with_capacity(count.min(EXTENTS_BUFFER_SIZE));
    for index in 0..count {
        let extent = index
            .checked_mul(size_of::<DISK_EXTENT>())
            .and_then(|offset| offset.checked_add(first))
            .ok_or_else(|| invalid_data(format!("区段数 {count} 过大")))?;
        let disk = u32::from_ne_bytes(field(bytes, extent + offset_of!(DISK_EXTENT, DiskNumber))?);
        let offset = i64::from_ne_bytes(field(
            bytes,
            extent + offset_of!(DISK_EXTENT, StartingOffset),
        )?);
        let length = i64::from_ne_bytes(field(
            bytes,
            extent + offset_of!(DISK_EXTENT, ExtentLength),
        )?);
        let (Ok(offset), Ok(length)) = (u64::try_from(offset), u64::try_from(length)) else {
            return Err(invalid_data(format!(
                "区段的偏移或长度为负：{offset}，{length}"
            )));
        };
        extents.push(DiskExtent {
            disk,
            offset,
            length,
        });
    }
    Ok(extents)
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
        let guid = volume_guid(&root).unwrap();
        assert!(guid.starts_with(VOLUME_PREFIX), "{guid}");
        let inside = volume_guid(std::env::temp_dir());
        assert!(inside.is_ok());
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    /// 按 `VOLUME_DISK_EXTENTS` 的布局拼出给定磁盘编号的区段列表：第 i 段从 i MiB 起、长 1 MiB。
    fn extents(disks: &[u32]) -> Vec<u8> {
        let first = offset_of!(VOLUME_DISK_EXTENTS, Extents);
        let mut bytes = vec![0u8; first + disks.len() * size_of::<DISK_EXTENT>()];
        bytes[..4].copy_from_slice(&(disks.len() as u32).to_ne_bytes());
        for (index, disk) in disks.iter().enumerate() {
            let base = first + index * size_of::<DISK_EXTENT>();
            let at = base + offset_of!(DISK_EXTENT, DiskNumber);
            bytes[at..at + 4].copy_from_slice(&disk.to_ne_bytes());
            let at = base + offset_of!(DISK_EXTENT, StartingOffset);
            bytes[at..at + 8].copy_from_slice(&((index as i64) << 20).to_ne_bytes());
            let at = base + offset_of!(DISK_EXTENT, ExtentLength);
            bytes[at..at + 8].copy_from_slice(&(1i64 << 20).to_ne_bytes());
        }
        bytes
    }

    #[test]
    fn disk_extents_keep_disk_offset_and_length() {
        let parsed = parse_disk_extents(&extents(&[3, 1])).unwrap();
        assert_eq!(
            parsed,
            [
                DiskExtent {
                    disk: 3,
                    offset: 0,
                    length: 1 << 20
                },
                DiskExtent {
                    disk: 1,
                    offset: 1 << 20,
                    length: 1 << 20
                },
            ]
        );
    }

    #[test]
    fn a_volume_overlaps_only_ranges_that_touch_its_extents() {
        let volume = VolumeOnDisk {
            path: r"\\?\Volume{x}".to_owned(),
            extents: vec![DiskExtent {
                disk: 1,
                offset: 1024,
                length: 1024,
            }],
        };
        assert!(volume.overlaps(1024, 512));
        assert!(volume.overlaps(512, 1024));
        assert!(volume.overlaps(2047, 1));
        assert!(!volume.overlaps(0, 1024));
        assert!(!volume.overlaps(2048, 512));
    }

    /// 只查询属性：本仓库所在的卷出现在它所在那块盘的卷列表里，而且与这个路径的区段一致。
    #[test]
    fn the_volume_of_this_repository_is_listed_on_its_disk() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let guid = volume_guid(here).unwrap();
        let device = guid.trim_end_matches('\\');
        let disk = volume_disk_numbers(here).unwrap()[0];
        let volumes = volumes_on_disk(disk).unwrap();
        assert!(
            volumes
                .iter()
                .any(|volume| volume.path.eq_ignore_ascii_case(device)),
            "{device} 不在 {volumes:?} 里"
        );
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
    fn the_system_disk_is_known() {
        assert!(!system_disks().unwrap().is_empty());
    }

    #[test]
    fn relative_path_without_existing_ancestor_is_an_error() {
        assert!(volume_disk_numbers("no-such-dir-yisi/deeper").is_err());
    }
}
