//! 写入计划在宿主里的流程：请引擎生成计划 → 先存备份 → 界面确认 → 执行 → 记进写盘记录；
//! 回滚是把备份里的计划反过来，同样先存备份、确认、执行（docs/architecture.md 4.2）。
//!
//! 目前只执行镜像文件：宿主以普通权限、独占方式打开镜像写入（`yisi_broker::execute::ImageFile`），不经磁盘代理，
//! 也不弹 UAC。计划存在宿主里，界面只拿到计划 ID 与摘要，执行的是宿主存着的那一份，界面换不掉要写的内容。
//!
//! 备份存在程序文件夹下 `data` 里的 `write-backups`（以计划 ID 命名），写盘记录存在同一处的 `write-history.json`
//! （读写规则见 [`record_file`]）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};
use yisi_broker::backup::SavedBackup;
use yisi_broker::execute::{self, ImageFile, Step};
use yisi_engine::protocol::{BootFix, BootSectorPlan, WritePlan, image_identity};

use crate::broker::BrokerSession;
use crate::engines::{Engines, HostError, IMAGE_SECTOR_SIZE, Target, lock};
use crate::record_file::{self, now_ms};

/// 写盘记录最多留这么多条，最新的在前。备份文件不随记录删除。
const MAX_RECORDS: usize = 200;

/// 修复引导扇区的计划最多写这么多字节：exFAT 的引导区 12 个扇区、扇区最大 4 KiB。
const MAX_BOOT_SECTOR_BYTES: u64 = 12 * 4096;

/// 计划做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanKind {
    BootSector,
    Rollback,
}

/// 要写的一段在设备上的位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteRange {
    pub offset: u64,
    pub length: u64,
}

/// 修复引导扇区的细节，同引擎答复的 [`BootSectorPlan`]（不含要写的内容）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootSectorDetail {
    pub partition: u32,
    pub partition_offset: u64,
    pub partition_length: u64,
    pub filesystem: String,
    pub fix: BootFix,
    pub from: u64,
    pub to: u64,
    pub length: u64,
}

/// 计划的摘要：给界面确认用，也记进写盘记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanSummary {
    pub kind: PlanKind,
    pub target: Target,
    pub writes: Vec<WriteRange>,
    /// 修复引导扇区的计划，以及回滚这类计划时，原来那次修复的细节。
    pub boot_sector: Option<BootSectorDetail>,
    /// 回滚的是哪一次写入（计划 ID）。
    pub rollback_of: Option<String>,
}

/// 等界面确认的计划。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanView {
    pub plan_id: String,
    pub backup_path: String,
    #[serde(flatten)]
    pub summary: PlanSummary,
}

/// 执行的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteOutcome {
    pub done: bool,
    /// 失败时停在哪一步。
    pub step: Option<Step>,
    /// 失败前已经写下去的段数；为 0 时设备没被改动。成功时是全部段数。
    pub written: usize,
    /// 失败的技术细节。
    pub detail: Option<String>,
}

/// 写盘记录里的一条：执行过的计划（成功或失败）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteRecord {
    pub plan_id: String,
    #[serde(flatten)]
    pub summary: PlanSummary,
    pub backup_path: String,
    pub executed_at_ms: u64,
    pub outcome: WriteOutcome,
}

struct Pending {
    summary: PlanSummary,
    backup: SavedBackup,
}

pub struct WritePlans {
    /// 程序文件夹下的 `data`（`engines::data_dir`）。
    data_dir: PathBuf,
    pending: Mutex<HashMap<String, Pending>>,
    /// 执行与读改写写盘记录都在这把锁里串行。
    guard: Mutex<()>,
    next_id: AtomicU32,
}

impl WritePlans {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            pending: Mutex::new(HashMap::new()),
            guard: Mutex::new(()),
            next_id: AtomicU32::new(0),
        }
    }

    /// 请引擎生成修复引导扇区的计划，存好备份后交给界面确认。只读源。
    pub fn plan_boot_sector(
        &self,
        engines: &Engines,
        broker: &BrokerSession,
        target: Target,
        partition: u32,
        fix: BootFix,
    ) -> Result<PlanView, HostError> {
        let image = writable_image(&target)?;
        let planned = engines.plan_boot_sector(broker, target.clone(), partition, fix)?;
        check_boot_sector_plan(&planned, &image)?;
        let detail = BootSectorDetail {
            partition: planned.partition,
            partition_offset: planned.partition_offset,
            partition_length: planned.partition_length,
            filesystem: planned.filesystem,
            fix: planned.fix,
            from: planned.from,
            to: planned.to,
            length: planned.length,
        };
        self.propose(
            planned.plan,
            PlanSummary {
                kind: PlanKind::BootSector,
                target,
                writes: Vec::new(),
                boot_sector: Some(detail),
                rollback_of: None,
            },
        )
    }

    /// 回滚一次写入：读回它的备份、反过来生成计划，存好新的备份后交给界面确认。
    /// 失败的那次只回滚已经写下去的段；盘上的内容必须仍是写后的，执行时才会写。
    pub fn rollback(&self, plan_id: &str) -> Result<PlanView, HostError> {
        let records = self.records();
        let record = records
            .iter()
            .find(|record| record.plan_id == plan_id)
            .ok_or_else(|| HostError::new("writeRecordNotFound", plan_id))?;
        if record.outcome.written == 0 {
            return Err(HostError::new("rollbackNothing", plan_id));
        }
        let rolled_back = records.iter().any(|other| {
            other.summary.rollback_of.as_deref() == Some(plan_id) && other.outcome.done
        });
        if rolled_back {
            return Err(HostError::new("rollbackDone", plan_id));
        }
        writable_image(&record.summary.target)?;
        let backup = SavedBackup::load(Path::new(&record.backup_path))
            .map_err(|error| HostError::new("backupUnreadable", yisi_rpc::error_chain(&error)))?;
        let mut inverse = backup.plan().inverse();
        inverse.writes.truncate(record.outcome.written);
        self.propose(
            inverse,
            PlanSummary {
                kind: PlanKind::Rollback,
                target: record.summary.target.clone(),
                writes: Vec::new(),
                boot_sector: record.summary.boot_sector.clone(),
                rollback_of: Some(record.plan_id.clone()),
            },
        )
    }

    /// 执行已确认的计划，记进写盘记录。镜像正被别的程序打开时不执行，计划留着，关掉那个程序后可以再试。
    pub fn execute(&self, plan_id: &str) -> Result<WriteRecord, HostError> {
        let _guard = lock(&self.guard);
        let pending = lock(&self.pending)
            .remove(plan_id)
            .ok_or_else(|| HostError::new("planNotFound", plan_id))?;
        let Target::Image { path } = pending.summary.target.clone() else {
            return Err(HostError::new("writeTargetUnsupported", ""));
        };
        let mut image = match ImageFile::open(Path::new(&path), IMAGE_SECTOR_SIZE) {
            Ok(image) => image,
            Err(error) => {
                let code = if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
                    "imageInUse"
                } else {
                    "writeTargetUnavailable"
                };
                lock(&self.pending).insert(plan_id.to_owned(), pending);
                return Err(HostError::new(code, format!("{path}：{error}")));
            }
        };
        let outcome = match execute::execute(&mut image, &pending.backup) {
            Ok(()) => WriteOutcome {
                done: true,
                step: None,
                written: pending.backup.plan().writes.len(),
                detail: None,
            },
            Err(failure) => {
                tracing::warn!(plan_id, error = %yisi_rpc::error_chain(&failure.error), "写入计划执行失败");
                WriteOutcome {
                    done: false,
                    step: Some(failure.error.step()),
                    written: failure.written,
                    detail: Some(yisi_rpc::error_chain(&failure.error)),
                }
            }
        };
        drop(image);
        let record = WriteRecord {
            plan_id: plan_id.to_owned(),
            summary: pending.summary,
            backup_path: pending.backup.path().display().to_string(),
            executed_at_ms: now_ms(),
            outcome,
        };
        if let Err(detail) = self.append(record.clone()) {
            // 写已经发生了：照样把结果告诉界面，记录没存下来只影响以后从列表里回滚
            tracing::warn!(%detail, plan_id, "写盘记录没存下来");
        }
        Ok(record)
    }

    /// 放弃一份还没执行的计划，删掉它的备份（没写过盘，备份用不上）。
    pub fn discard(&self, plan_id: &str) -> Result<(), HostError> {
        let pending = lock(&self.pending)
            .remove(plan_id)
            .ok_or_else(|| HostError::new("planNotFound", plan_id))?;
        if let Err(error) = std::fs::remove_file(pending.backup.path()) {
            tracing::warn!(%error, path = %pending.backup.path().display(), "删不掉放弃的计划的备份");
        }
        Ok(())
    }

    /// 写盘记录，最新的在前。
    pub fn records(&self) -> Vec<WriteRecord> {
        let _guard = lock(&self.guard);
        self.load()
    }

    /// 存好备份，登记为等确认的计划。
    fn propose(&self, plan: WritePlan, mut summary: PlanSummary) -> Result<PlanView, HostError> {
        let dir = &self.data_dir;
        summary.writes = plan
            .writes
            .iter()
            .map(|write| WriteRange {
                offset: write.offset,
                length: write.len(),
            })
            .collect();
        let plan_id = format!(
            "{}-{}",
            now_ms(),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join("write-backups").join(format!("{plan_id}.json"));
        let backup = SavedBackup::save(&path, plan)
            .map_err(|error| HostError::new("backupFailed", yisi_rpc::error_chain(&error)))?;
        let view = PlanView {
            plan_id: plan_id.clone(),
            backup_path: path.display().to_string(),
            summary: summary.clone(),
        };
        lock(&self.pending).insert(plan_id, Pending { summary, backup });
        Ok(view)
    }

    fn history_file(&self) -> PathBuf {
        self.data_dir.join("write-history.json")
    }

    fn load(&self) -> Vec<WriteRecord> {
        record_file::load(&self.history_file(), "写盘记录")
    }

    /// 调用方已持有 `guard`。
    fn append(&self, record: WriteRecord) -> Result<(), String> {
        let mut records = self.load();
        records.insert(0, record);
        records.truncate(MAX_RECORDS);
        record_file::save(&self.history_file(), &records)
    }
}

/// 别的程序开着文件时 Windows 的错误码。
const ERROR_SHARING_VIOLATION: i32 = 32;

/// 目前只执行镜像文件：物理盘与卷要等磁盘代理能写盘。返回镜像的规范路径。
fn writable_image(target: &Target) -> Result<PathBuf, HostError> {
    match target {
        Target::Image { path } => std::fs::canonicalize(path)
            .map_err(|error| HostError::new("writeTargetUnavailable", format!("{path}：{error}"))),
        Target::Disk { .. } | Target::Volume { .. } => {
            Err(HostError::new("writeTargetUnsupported", ""))
        }
    }
}

/// 引擎给的计划要与请求对得上：写的是这个镜像、按读镜像时的扇区大小；只有一段，落在这个分区里，
/// 长度不超过一个引导区。引擎解析的是不可信的数据，宿主不只看它的摘要。
fn check_boot_sector_plan(planned: &BootSectorPlan, image: &Path) -> Result<(), HostError> {
    let plan = &planned.plan;
    let mismatch = |what: &str| HostError::new("planMismatch", what.to_owned());
    if plan.device.identity != image_identity(image) {
        return Err(mismatch("计划写的不是这个镜像文件"));
    }
    if plan.device.sector_size != IMAGE_SECTOR_SIZE {
        return Err(mismatch("计划的扇区大小与读镜像时不同"));
    }
    plan.validate()
        .map_err(|error| mismatch(&error.to_string()))?;
    let [write] = plan.writes.as_slice() else {
        return Err(mismatch("修复引导扇区的计划应该只有一段"));
    };
    let partition_end = planned
        .partition_offset
        .saturating_add(planned.partition_length);
    let inside = write.offset >= planned.partition_offset
        && write.offset.saturating_add(write.len()) <= partition_end;
    if !inside || write.len() > MAX_BOOT_SECTOR_BYTES || write.offset != planned.to {
        return Err(mismatch("计划要写的位置不在这个分区的引导扇区上"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
