//! 输出文件的两条小规则：路径后面加后缀；先写临时文件、落盘后再换上，任何时候盘上都是一份完整的。

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// `path` 的文件名后面加上 `suffix`（不换掉原来的扩展名）：`a.img` 加 `.map` 是 `a.img.map`。
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// 写 `path` 时先写的临时文件：后面加 `.tmp`。
pub fn temp_path(path: &Path) -> PathBuf {
    with_suffix(path, ".tmp")
}

/// 先把 `bytes` 写进 [`temp_path`] 并落盘，再改名换掉 `path`（已有的会被换掉）：中途断电也不会留下半份。
pub fn write_then_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = temp_path(path);
    let mut file = File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_keep_the_extension_and_replacing_leaves_no_temp_file() {
        assert_eq!(
            with_suffix(Path::new("d/a.img"), ".map"),
            PathBuf::from("d/a.img.map")
        );
        assert_eq!(temp_path(Path::new("x.json")), PathBuf::from("x.json.tmp"));
        let dir = std::env::temp_dir().join(format!("yisi-io-file-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("records.json");
        write_then_replace(&target, b"one").unwrap();
        write_then_replace(&target, b"two").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"two");
        assert!(!temp_path(&target).exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
