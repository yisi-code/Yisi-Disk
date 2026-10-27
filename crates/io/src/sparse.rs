//! 稀疏文件：只读一部分时镜像要建成稀疏文件，没读的区域不占空间，往远处写也不用先把前面填 0。
//! 卷支不支持稀疏文件见 [`crate::volume_info`]。只设置文件标志，不读写磁盘内容。

use std::fs::File;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SPARSE_FILE;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;

use crate::ioctl::ioctl;

/// 把文件标成稀疏文件：之后没写过的区域不占空间、读出来是 0。已经写过的数据不变。
pub fn make_sparse(file: &File) -> io::Result<()> {
    ioctl(file.as_raw_handle(), FSCTL_SET_SPARSE, &[], &mut []).map(|_| ())
}

/// 文件是不是稀疏文件。
pub fn is_sparse(file: &File) -> io::Result<bool> {
    Ok(file.metadata()?.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0)
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;

    use super::*;

    #[test]
    fn sparse_file_keeps_unwritten_areas_as_zero() {
        let dir = std::env::temp_dir();
        if !crate::volume_info(&dir).unwrap().sparse {
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
