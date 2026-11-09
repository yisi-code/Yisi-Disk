//! 设置页列出的程序文件夹：安装目录、程序数据 `data`、写盘前的备份、为打开与复制存的副本。
//! 路径只由宿主算，界面只能按种类要求在资源管理器里打开其中一个，换不成别的路径。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::engines::{self, HostError};
use crate::write_plans::BACKUP_DIR;

/// 设置页里的一个文件夹（协议里用 camelCase 名字）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FolderKind {
    /// 程序所在的文件夹：宿主、引擎、磁盘代理。
    Program,
    /// 程序写的东西都在这里（[`engines::data_dir`]）。
    Data,
    /// 写盘前的备份（[`BACKUP_DIR`]），回滚靠它。
    WriteBackups,
    /// 为打开、复制而存的副本（[`engines::open_cache_root`]），下一次存副本时清掉以前的。
    OpenCache,
}

/// 按设置页上的先后排列。
const ALL: [FolderKind; 4] = [
    FolderKind::Program,
    FolderKind::Data,
    FolderKind::WriteBackups,
    FolderKind::OpenCache,
];

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppFolder {
    pub kind: FolderKind,
    pub path: String,
    /// 还没存过东西的文件夹（例如从没写过盘）不存在，界面不给打开。
    pub exists: bool,
}

fn path_of(kind: FolderKind) -> Result<PathBuf, HostError> {
    let unavailable =
        |error: std::io::Error| HostError::new("folderUnavailable", error.to_string());
    match kind {
        FolderKind::Program => engines::program_dir().map_err(unavailable),
        FolderKind::Data => engines::data_dir().map_err(unavailable),
        FolderKind::WriteBackups => engines::data_dir()
            .map(|dir| dir.join(BACKUP_DIR))
            .map_err(unavailable),
        FolderKind::OpenCache => engines::open_cache_root(),
    }
}

/// 设置页要显示的全部文件夹。
pub fn list() -> Result<Vec<AppFolder>, HostError> {
    ALL.iter()
        .map(|&kind| {
            let path = path_of(kind)?;
            Ok(AppFolder {
                kind,
                exists: path.is_dir(),
                path: path.display().to_string(),
            })
        })
        .collect()
}

/// 在资源管理器里打开一个文件夹；还不存在时报 `folderMissing`，不替它建。
pub fn open(kind: FolderKind) -> Result<(), HostError> {
    let path = path_of(kind)?;
    if !path.is_dir() {
        return Err(HostError::new("folderMissing", path.display().to_string()));
    }
    crate::shell::open_folder(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_use_the_interface_wire_format() {
        let names: Vec<String> = ALL
            .iter()
            .map(|kind| {
                serde_json::to_value(kind)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(names, ["program", "data", "writeBackups", "openCache"]);
        assert!(serde_json::from_str::<FolderKind>("\"C:\\\\Windows\"").is_err());
    }

    #[test]
    fn folders_live_in_the_program_folder() {
        let folders = list().unwrap();
        let program = PathBuf::from(&folders[0].path);
        assert!(folders[0].exists);
        for folder in &folders[1..] {
            assert!(
                PathBuf::from(&folder.path).starts_with(&program),
                "{folder:?}"
            );
        }
    }
}
