//! 深度恢复的扫描记录：每次扫描（含之后的续扫）一条，记着源、格式、保存位置、扫到哪了、找到多少个文件。
//! 程序关掉以后，只扫描的结果还能接着挑选保存，停下的扫描还能从停下处接着扫。
//!
//! 记录清单存在程序文件夹下 `data` 里的 `carve-history.json`（读写规则见 [`record_file`]）；找到的文件追加写进
//! `carve-history\<记录 ID>.jsonl`，引擎每发来一批写一行（一个 JSON 数组），不整份重写。记录由任务观察者
//! [`CarveRecorder`] 按引擎的通知更新：找到的文件、检查点（从哪里接着扫不漏不重）、终结状态。
//! 部分恢复存下的不完整文件在清单里带着 `incomplete`（原因），完整的没有这一项。

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use yisi_engine::protocol::FoundFile;

use crate::engines::{HostError, JobObserver, Target, lock};
use crate::record_file::{self, now_ms};

/// 最多记这么多次扫描，最近的在前；多出来的连同找到的文件清单一起删掉（不碰恢复出的文件）。
const MAX_RECORDS: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CarveStatus {
    /// 正在扫。程序上次没正常结束时留下的「正在扫」读出来按停下算。
    Running,
    /// 被停下，可以接着扫。
    Stopped,
    /// 扫完了。
    Completed,
    /// 出错停下（例如源读不了），可以接着扫。
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarveRecord {
    /// 第一次扫描的任务 ID；续扫的任务另有 ID，记录不变。
    pub id: String,
    pub target: Target,
    /// 界面给的来源描述（型号、容量、镜像文件名等），源没接上时也认得出是哪一个。
    pub source_label: String,
    pub extensions: Vec<String>,
    /// 部分恢复：不完整的文件也存下（接着扫时照样）。以前的记录没有这一项，按不存算。
    #[serde(default)]
    pub keep_incomplete: bool,
    /// 边扫边保存时实际的目录（`recovered-…`）；只扫描时为空。
    pub output_dir: Option<String>,
    pub status: CarveStatus,
    /// 从这里接着扫（最后一个检查点）。
    pub resume_at: u64,
    /// 源的总字节数（第一次进度里给出）。
    pub size: u64,
    /// 找到的文件数。
    pub files: u64,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
}

pub struct CarveHistory {
    file: PathBuf,
    /// 各次扫描找到的文件清单所在的目录。
    dir: PathBuf,
    /// 读改写之间不让别的命令插进来。
    guard: Mutex<()>,
    /// 正在运行的扫描（记录 ID），用来区分「正在扫」与「上次没正常结束」。
    live: Mutex<Vec<String>>,
}

impl CarveHistory {
    pub fn new(data_dir: &std::path::Path) -> Self {
        Self {
            file: data_dir.join("carve-history.json"),
            dir: data_dir.join("carve-history"),
            guard: Mutex::new(()),
            live: Mutex::new(Vec::new()),
        }
    }

    /// 全部记录，最近开始的在前。不在运行的「正在扫」按停下给出。
    pub fn records(&self) -> Vec<CarveRecord> {
        let live = lock(&self.live).clone();
        let _guard = lock(&self.guard);
        self.load()
            .into_iter()
            .map(|mut record| {
                if record.status == CarveStatus::Running && !live.contains(&record.id) {
                    record.status = CarveStatus::Stopped;
                }
                record
            })
            .collect()
    }

    pub fn find(&self, id: &str) -> Option<CarveRecord> {
        self.records().into_iter().find(|record| record.id == id)
    }

    /// 开始一次新的扫描：记下记录，清单文件从空开始。
    pub fn begin(&self, record: CarveRecord) -> Result<(), HostError> {
        let _guard = lock(&self.guard);
        let mut records = self.load();
        records.retain(|old| old.id != record.id);
        records.insert(0, record.clone());
        let dropped: Vec<String> = records
            .iter()
            .skip(MAX_RECORDS)
            .map(|old| old.id.clone())
            .collect();
        records.truncate(MAX_RECORDS);
        self.save(&records)?;
        for id in dropped {
            self.remove_files(&id);
        }
        fs::create_dir_all(&self.dir)
            .map_err(|error| HostError::new("historyUnavailable", error.to_string()))?;
        fs::write(self.files_path(&record.id), b"")
            .map_err(|error| HostError::new("historyUnavailable", error.to_string()))?;
        lock(&self.live).push(record.id);
        Ok(())
    }

    /// 接着扫：记录标成正在扫。
    pub fn resume(&self, id: &str) -> Result<(), HostError> {
        self.update(id, |record| record.status = CarveStatus::Running)?;
        lock(&self.live).push(id.to_owned());
        Ok(())
    }

    /// 任务没能启动：记录回到之前的样子（新扫描整条删掉，续扫标回停下）。
    pub fn abandon(&self, id: &str, was_new: bool) {
        lock(&self.live).retain(|live| live != id);
        let result = if was_new {
            self.remove(id)
        } else {
            self.update(id, |record| record.status = CarveStatus::Stopped)
        };
        if let Err(error) = result {
            tracing::warn!(?error, "没能撤回扫描记录");
        }
    }

    pub fn update(&self, id: &str, change: impl FnOnce(&mut CarveRecord)) -> Result<(), HostError> {
        let _guard = lock(&self.guard);
        let mut records = self.load();
        let Some(record) = records.iter_mut().find(|record| record.id == id) else {
            return Err(HostError::new("carveRecordNotFound", id));
        };
        change(record);
        record.updated_at_ms = now_ms();
        self.save(&records)
    }

    /// 从列表移除，连同找到的文件清单（不碰恢复出的文件）。
    pub fn remove(&self, id: &str) -> Result<(), HostError> {
        if lock(&self.live).iter().any(|live| live == id) {
            return Err(HostError::new("carveRecordRunning", id));
        }
        let _guard = lock(&self.guard);
        let mut records = self.load();
        records.retain(|old| old.id != id);
        self.save(&records)?;
        self.remove_files(id);
        Ok(())
    }

    /// 找到的文件，按找到的先后。同一个起点只算一次（续扫可能从上次已经报过的文件前面接着扫）。
    pub fn files(&self, id: &str) -> Result<Vec<FoundFile>, HostError> {
        let file = fs::File::open(self.files_path(id))
            .map_err(|error| HostError::new("carveRecordNotFound", error.to_string()))?;
        let mut seen = std::collections::HashSet::new();
        let mut files = Vec::new();
        for line in BufReader::new(file).lines() {
            let line =
                line.map_err(|error| HostError::new("historyUnavailable", error.to_string()))?;
            // 写到一半就断电的最后一行读不懂，跳过
            let Ok(batch) = serde_json::from_str::<Vec<FoundFile>>(&line) else {
                continue;
            };
            files.extend(batch.into_iter().filter(|found| seen.insert(found.offset)));
        }
        Ok(files)
    }

    fn append(&self, id: &str, batch: &Value) -> std::io::Result<()> {
        let mut file = OpenOptions::new().append(true).open(self.files_path(id))?;
        writeln!(file, "{batch}")
    }

    fn finish(&self, id: &str, change: impl FnOnce(&mut CarveRecord)) {
        lock(&self.live).retain(|live| live != id);
        if let Err(error) = self.update(id, change) {
            tracing::warn!(?error, "没能更新扫描记录");
        }
    }

    /// 只认任务 ID 那样的名字（字母、数字、连字符），不会拼出别的路径。
    fn files_path(&self, id: &str) -> PathBuf {
        let safe: String = id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        self.dir.join(format!("{safe}.jsonl"))
    }

    fn remove_files(&self, id: &str) {
        let path = self.files_path(id);
        if let Err(error) = fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, path = %path.display(), "没能删掉扫描记录的文件清单");
        }
    }

    fn load(&self) -> Vec<CarveRecord> {
        record_file::load(&self.file, "扫描记录")
    }

    fn save(&self, records: &[CarveRecord]) -> Result<(), HostError> {
        record_file::save(&self.file, records)
            .map_err(|detail| HostError::new("historyUnavailable", detail))
    }
}

/// 一次扫描（或续扫）任务的观察者：把引擎的通知记进扫描记录 `id`。
pub struct CarveRecorder {
    pub history: std::sync::Arc<CarveHistory>,
    pub id: String,
    /// 已经记下的文件数（续扫时从记录里的数接着数）。
    pub found: AtomicU64,
    /// 源的总字节数（进度里给出），检查点与结束时写进记录。
    pub size: AtomicU64,
}

impl CarveRecorder {
    /// 记录 `record` 的观察者：文件数从记录里的数接着数。
    pub fn new(history: std::sync::Arc<CarveHistory>, record: &CarveRecord) -> Self {
        Self {
            history,
            id: record.id.clone(),
            found: AtomicU64::new(record.files),
            size: AtomicU64::new(record.size),
        }
    }

    fn counts(&self) -> (u64, u64) {
        (
            self.found.load(Ordering::Relaxed),
            self.size.load(Ordering::Relaxed),
        )
    }
}

impl JobObserver for CarveRecorder {
    fn event(&self, kind: &str, data: &Value) {
        match kind {
            "found" => {
                let Some(files) = data.get("files") else {
                    return;
                };
                let count = files.as_array().map_or(0, Vec::len) as u64;
                if let Err(error) = self.history.append(&self.id, files) {
                    tracing::warn!(%error, "没能记下找到的文件");
                    return;
                }
                self.found.fetch_add(count, Ordering::Relaxed);
            }
            // 每秒可能好几次，只记在内存里
            "progress" => {
                if let Some(total) = data.get("total").and_then(Value::as_u64) {
                    self.size.store(total, Ordering::Relaxed);
                }
            }
            "checkpoint" => {
                let Some(resume_at) = data.get("resumeAt").and_then(Value::as_u64) else {
                    return;
                };
                let (files, size) = self.counts();
                if let Err(error) = self.history.update(&self.id, |record| {
                    record.resume_at = resume_at;
                    record.files = files;
                    record.size = size.max(record.size);
                }) {
                    tracing::warn!(?error, "没能更新扫描记录");
                }
            }
            "completed" => {
                let (files, size) = self.counts();
                self.history.finish(&self.id, |record| {
                    record.status = CarveStatus::Completed;
                    record.size = size.max(record.size);
                    record.resume_at = record.size;
                    record.files = files;
                });
            }
            "cancelled" | "failed" => {
                let (files, size) = self.counts();
                let status = if kind == "failed" {
                    CarveStatus::Failed
                } else {
                    CarveStatus::Stopped
                };
                self.history.finish(&self.id, |record| {
                    record.status = status;
                    record.size = size.max(record.size);
                    record.files = files;
                });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use yisi_engine::protocol::IncompleteReason;

    use super::*;

    fn history(name: &str) -> Arc<CarveHistory> {
        let dir =
            std::env::temp_dir().join(format!("yisi-carve-history-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Arc::new(CarveHistory::new(&dir))
    }

    fn record(id: &str) -> CarveRecord {
        CarveRecord {
            id: id.to_owned(),
            target: Target::Image {
                path: "d.img".to_owned(),
            },
            source_label: "d.img".to_owned(),
            extensions: vec!["jpg".to_owned()],
            keep_incomplete: false,
            output_dir: None,
            status: CarveStatus::Running,
            resume_at: 0,
            size: 0,
            files: 0,
            started_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn found(offset: u64) -> Value {
        json!({"name": format!("f{offset}.jpg"), "extension": "jpg", "offset": offset, "length": 10})
    }

    #[test]
    fn notifications_keep_the_record_and_file_list_up_to_date() {
        let history = history("notify");
        history.begin(record("scan-1")).unwrap();
        let recorder = CarveRecorder::new(Arc::clone(&history), &record("scan-1"));
        recorder.event("progress", &json!({"done": 0, "total": 5000}));
        recorder.event("found", &json!({"files": [found(512), found(1024)]}));
        recorder.event("checkpoint", &json!({"resumeAt": 2048}));
        recorder.event("found", &json!({"files": [found(3072)]}));
        recorder.event("cancelled", &json!({}));

        let stopped = history.find("scan-1").unwrap();
        assert_eq!(
            (
                stopped.status,
                stopped.resume_at,
                stopped.size,
                stopped.files
            ),
            (CarveStatus::Stopped, 2048, 5000, 3)
        );

        // 续扫从 2048 接着扫，又报了一次 3072：清单里只算一次
        history.resume("scan-1").unwrap();
        let resumed = CarveRecorder::new(Arc::clone(&history), &stopped);
        resumed.event("found", &json!({"files": [found(3072), found(4096)]}));
        resumed.event("completed", &json!({}));
        let offsets: Vec<u64> = history
            .files("scan-1")
            .unwrap()
            .iter()
            .map(|file| file.offset)
            .collect();
        assert_eq!(offsets, [512, 1024, 3072, 4096]);
        let completed = history.find("scan-1").unwrap();
        assert_eq!(
            (completed.status, completed.resume_at),
            (CarveStatus::Completed, 5000)
        );
    }

    /// 不完整的文件在清单里带着原因，完整的没有；以前的记录（没有 `keepIncomplete`）照样读得出。
    #[test]
    fn incomplete_files_keep_their_reason_and_old_records_still_load() {
        let history = history("incomplete");
        history.begin(record("scan-3")).unwrap();
        let recorder = CarveRecorder::new(Arc::clone(&history), &record("scan-3"));
        recorder.event(
            "found",
            &json!({"files": [
                found(512),
                {"name": "f0000000002.incomplete.jpg", "extension": "jpg", "offset": 1024,
                 "length": 700, "incomplete": "broken"},
            ]}),
        );
        let files = history.files("scan-3").unwrap();
        let reasons: Vec<_> = files.iter().map(|file| file.incomplete).collect();
        assert_eq!(reasons, [None, Some(IncompleteReason::Broken)]);

        let old = json!([{
            "id": "old-1", "target": {"kind": "image", "path": "d.img"}, "sourceLabel": "d.img",
            "extensions": ["jpg"], "outputDir": null, "status": "completed", "resumeAt": 10,
            "size": 10, "files": 0, "startedAtMs": 1, "updatedAtMs": 1
        }]);
        let records: Vec<CarveRecord> = serde_json::from_value(old).unwrap();
        assert!(!records[0].keep_incomplete);
    }

    #[test]
    fn records_left_running_by_a_crash_read_as_stopped_and_can_be_removed() {
        let history = history("crash");
        history.begin(record("scan-2")).unwrap();
        assert_eq!(history.find("scan-2").unwrap().status, CarveStatus::Running);
        assert!(history.remove("scan-2").is_err(), "正在扫的不能删");
        // 程序重新启动：没有在运行的任务
        let reopened = CarveHistory::new(history.file.parent().unwrap());
        assert_eq!(
            reopened.find("scan-2").unwrap().status,
            CarveStatus::Stopped
        );
        reopened.remove("scan-2").unwrap();
        assert!(reopened.find("scan-2").is_none());
        assert!(!reopened.files_path("scan-2").exists());
        // 名字里不像任务 ID 的字符不会拼进路径
        assert_eq!(reopened.files_path("../x").file_name().unwrap(), "x.jsonl");
    }
}
