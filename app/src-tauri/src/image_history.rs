//! 做过的镜像记在哪：每次开始制作镜像时记下镜像的位置与来源，下次直接从列表里选。镜像被删除、移走后，
//! 由界面提示，可以重新指定位置（核对是同一个镜像才改）或从列表移除——都只改这份记录，不碰镜像文件。
//! 记录存在程序数据目录的 `image-history.json`，先写临时文件再改名替换。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::engines::{HostError, Target, lock};

/// 最多记这么多个，按最近使用排在前面，多出来的丢掉最久没用的。
const MAX_RECORDS: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRecord {
    pub image_path: String,
    /// 做镜像时选的源。物理盘的编号重新插拔后可能变，界面按 `source_identity` 找现在的编号。
    pub target: Target,
    /// 坏区图里记的来源标识，物理盘与磁盘列表里的 `identity` 相同。
    pub source_identity: String,
    /// 界面给的来源描述（型号、容量等），原来的盘没接上时也认得出是哪一块。
    pub source_label: String,
    pub size: u64,
    /// 最近一次开始做的时间（毫秒，Unix 纪元起）。
    pub used_at_ms: u64,
}

pub struct ImageHistory {
    /// 查不到程序数据目录时为空：不记，列表一直是空的。
    file: Option<PathBuf>,
    /// 读改写之间不让别的命令插进来。
    guard: Mutex<()>,
}

impl ImageHistory {
    pub fn new(file: Option<PathBuf>) -> Self {
        Self {
            file,
            guard: Mutex::new(()),
        }
    }

    /// 全部记录，最近使用的在前。文件不存在或读不懂时为空（读不懂的那份会在下次记录时被替换）。
    pub fn records(&self) -> Vec<ImageRecord> {
        let _guard = lock(&self.guard);
        self.load()
    }

    /// 位置是 `image_path` 的那条记录。
    pub fn find(&self, image_path: &str) -> Option<ImageRecord> {
        self.records()
            .into_iter()
            .find(|record| same_path(&record.image_path, image_path))
    }

    /// 记下（或更新）一个镜像，时间记为现在。
    pub fn record(&self, mut record: ImageRecord) -> Result<(), HostError> {
        record.used_at_ms = now_ms();
        let _guard = lock(&self.guard);
        let mut records = self.load();
        records.retain(|old| !same_path(&old.image_path, &record.image_path));
        records.insert(0, record);
        self.save(records)
    }

    /// 从列表移除（不删除文件）。
    pub fn remove(&self, image_path: &str) -> Result<(), HostError> {
        let _guard = lock(&self.guard);
        let mut records = self.load();
        records.retain(|old| !same_path(&old.image_path, image_path));
        self.save(records)
    }

    /// 镜像移到了 `new_path`：改记录里的位置，其余不变。新位置已有一条记录时合并成一条。
    pub fn relocate(&self, image_path: &str, new_path: &str) -> Result<ImageRecord, HostError> {
        let _guard = lock(&self.guard);
        let mut records = self.load();
        let index = records
            .iter()
            .position(|old| same_path(&old.image_path, image_path))
            .ok_or_else(|| HostError::new("imageRecordNotFound", image_path))?;
        let mut moved = records.remove(index);
        moved.image_path = new_path.to_owned();
        records.retain(|old| !same_path(&old.image_path, new_path));
        records.insert(index.min(records.len()), moved.clone());
        self.save(records)?;
        Ok(moved)
    }

    fn load(&self) -> Vec<ImageRecord> {
        let Some(file) = &self.file else {
            return Vec::new();
        };
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                tracing::warn!(%error, path = %file.display(), "读不到镜像记录");
                return Vec::new();
            }
        };
        serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::warn!(%error, path = %file.display(), "镜像记录读不懂，按空处理");
            Vec::new()
        })
    }

    fn save(&self, mut records: Vec<ImageRecord>) -> Result<(), HostError> {
        let Some(file) = &self.file else {
            return Err(HostError::new("historyUnavailable", ""));
        };
        records.truncate(MAX_RECORDS);
        let unavailable = |error: &dyn std::fmt::Display| {
            HostError::new("historyUnavailable", format!("{}：{error}", file.display()))
        };
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|error| unavailable(&error))?;
        }
        let text = serde_json::to_string_pretty(&records).map_err(|error| unavailable(&error))?;
        let mut temp = file.clone().into_os_string();
        temp.push(".tmp");
        let temp = PathBuf::from(temp);
        std::fs::write(&temp, text).map_err(|error| unavailable(&error))?;
        std::fs::rename(&temp, file).map_err(|error| unavailable(&error))
    }
}

/// 同一个位置：Windows 上路径不分大小写。
fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str) -> ImageRecord {
        ImageRecord {
            image_path: path.to_owned(),
            target: Target::Disk { disk_number: 1 },
            source_identity: "disk WD Elements 123".to_owned(),
            source_label: "WD Elements".to_owned(),
            size: 1000,
            used_at_ms: 0,
        }
    }

    fn history(name: &str) -> (ImageHistory, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("yisi-image-history-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("image-history.json");
        (ImageHistory::new(Some(file.clone())), file)
    }

    fn paths(history: &ImageHistory) -> Vec<String> {
        history
            .records()
            .into_iter()
            .map(|record| record.image_path)
            .collect()
    }

    #[test]
    fn records_are_kept_newest_first_without_duplicates() {
        let (history, file) = history("order");
        assert!(history.records().is_empty());
        history.record(record("D:/a.img")).unwrap();
        history.record(record("D:/b.img")).unwrap();
        history.record(record("D:/a.img")).unwrap();
        assert_eq!(paths(&history), vec!["D:/a.img", "D:/b.img"]);
        assert!(history.records()[0].used_at_ms > 0);
        for index in 0..MAX_RECORDS + 5 {
            history.record(record(&format!("D:/{index}.img"))).unwrap();
        }
        assert_eq!(history.records().len(), MAX_RECORDS);
        assert!(file.exists());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn remove_and_relocate_only_change_the_records() {
        let (history, file) = history("relocate");
        history.record(record("D:/old/disk.img")).unwrap();
        history.record(record("D:/other.img")).unwrap();
        let moved = history
            .relocate("D:/old/disk.img", "E:/new/disk.img")
            .unwrap();
        assert_eq!(moved.image_path, "E:/new/disk.img");
        assert_eq!(paths(&history), vec!["D:/other.img", "E:/new/disk.img"]);
        // 移到一个已经有记录的位置：合并成一条
        history.relocate("E:/new/disk.img", "D:/other.img").unwrap();
        assert_eq!(paths(&history), vec!["D:/other.img"]);
        assert_eq!(
            history
                .relocate("D:/none.img", "D:/x.img")
                .unwrap_err()
                .code,
            "imageRecordNotFound"
        );
        history.remove("D:/other.img").unwrap();
        assert!(history.records().is_empty());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn unreadable_file_counts_as_empty_and_is_replaced() {
        let (history, file) = history("broken");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "not json").unwrap();
        assert!(history.records().is_empty());
        history.record(record("D:/a.img")).unwrap();
        assert_eq!(paths(&history), vec!["D:/a.img"]);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn paths_differing_only_in_case_are_the_same_image() {
        let (history, file) = history("case");
        history.record(record("D:/Images/Disk.img")).unwrap();
        history.record(record("d:/images/disk.IMG")).unwrap();
        assert_eq!(history.records().len(), 1);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }
}
