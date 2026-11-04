//! 写入计划在宿主里的流程：请引擎生成计划 → 先存备份 → 界面确认 → 执行 → 记进写盘记录；
//! 回滚是把备份里的计划反过来，同样先存备份、确认、执行（docs/architecture.md 4.2）。
//!
//! 镜像文件由宿主以普通权限、独占方式打开写入（`yisi_broker::execute::ImageFile`），不经磁盘代理；整块物理盘交给
//! 磁盘代理（`broker.executeWrite`），代理读回同一份备份执行。卷不写（卷上的偏移不是盘上的偏移）。计划存在宿主里，
//! 界面只拿到计划 ID 与摘要，执行的是宿主存着的那一份，界面换不掉要写的内容。
//!
//! 备份存在程序文件夹下 `data` 里的 `write-backups`（以计划 ID 命名），写盘记录存在同一处的 `write-history.json`
//! （读写规则见 [`record_file`]）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};
use yisi_broker::backup::SavedBackup;
use yisi_broker::execute::{self, ImageFile};
/// 执行的结果：成功，或停在哪一步、失败前已经写下去几段。与磁盘代理的答复同一个类型。
pub use yisi_broker::protocol::WriteExecuted as WriteOutcome;
use yisi_engine::protocol::{
    BootFix, BootSectorPlan, ClusterRun, FsFix, FsFixCount, FsFixPlan, MftFix, MftPlan,
    PartitionRef, PartitionTableFix, PartitionTablePlan, RebuiltBoot, WritePlan, image_identity,
};

use crate::broker::BrokerSession;
use crate::engines::{Engines, HostError, IMAGE_SECTOR_SIZE, Target, lock};
use crate::record_file::{self, now_ms};

/// 写盘记录最多留这么多条，最新的在前。备份文件不随记录删除。
const MAX_RECORDS: usize = 200;

/// 修复引导扇区的计划最多写这么多字节：exFAT 的引导区 12 个扇区、扇区最大 4 KiB。
const MAX_BOOT_SECTOR_BYTES: u64 = 12 * 4096;

/// 改写分区表只写设备开头与末尾这么大的范围：MBR 在第 0 扇区，GPT 的主表在开头、备份表在末尾
/// （128 项 × 128 字节的表项区，按 512 或 4096 字节扇区都在 64 KiB 之内）。
const TABLE_AREA: u64 = 64 * 1024;

/// `$MFT` 与 `$MFTMirr` 互修最多改 4 条记录，每条最大 4 KiB。
const MAX_MFT_RECORDS: usize = 4;
const MAX_MFT_RECORD_BYTES: u64 = 4096;

/// 计划做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanKind {
    BootSector,
    PartitionTable,
    Mft,
    FsFix,
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
    /// 重建时推算出的内容（写盘记录里重建之前的记录没有这一项）。
    #[serde(default)]
    pub rebuilt: Option<RebuiltBoot>,
}

/// 改写分区表的细节，同引擎答复的 [`PartitionTablePlan`]（不含要写的内容）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionTableDetail {
    pub fix: PartitionTableFix,
    pub scheme: String,
    pub filesystem: String,
    pub partition: u32,
    pub offset: u64,
    pub length: u64,
    pub type_id: String,
    pub role: Option<String>,
}

/// `$MFT` 与 `$MFTMirr` 互修的细节，同引擎答复的 [`MftPlan`]（不含要写的内容）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MftDetail {
    pub fix: MftFix,
    pub partition: u32,
    pub partition_offset: u64,
    pub partition_length: u64,
    pub records: Vec<u64>,
    pub from: u64,
    pub to: u64,
}

/// 文件系统修复的细节，同引擎答复的 [`FsFixPlan`]（不含要写的内容）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsFixDetail {
    pub partition: u32,
    pub partition_offset: u64,
    pub partition_length: u64,
    pub filesystem: String,
    pub counts: Vec<FsFixCount>,
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
    /// 改写分区表的计划（以及回滚它时）的细节。
    #[serde(default)]
    pub partition_table: Option<PartitionTableDetail>,
    /// `$MFT` 与 `$MFTMirr` 互修的计划（以及回滚它时）的细节。
    #[serde(default)]
    pub mft: Option<MftDetail>,
    /// 文件系统修复的计划（以及回滚它时）的细节。
    #[serde(default)]
    pub fs_fix: Option<FsFixDetail>,
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
        let expected = writable(&target, &self.data_dir)?;
        let planned = engines.plan_boot_sector(broker, target.clone(), partition, fix)?;
        check_boot_sector_plan(&planned, &expected)?;
        let detail = BootSectorDetail {
            partition: planned.partition,
            partition_offset: planned.partition_offset,
            partition_length: planned.partition_length,
            filesystem: planned.filesystem,
            fix: planned.fix,
            from: planned.from,
            to: planned.to,
            length: planned.length,
            rebuilt: planned.rebuilt,
        };
        self.propose(
            planned.plan,
            PlanSummary {
                kind: PlanKind::BootSector,
                target,
                writes: Vec::new(),
                boot_sector: Some(detail),
                partition_table: None,
                mft: None,
                fs_fix: None,
                rollback_of: None,
            },
        )
    }

    /// 请引擎生成改写分区表的计划（把丢失的分区写回去、改分区类型），存好备份后交给界面确认。只读源。
    pub fn plan_partition_table(
        &self,
        engines: &Engines,
        broker: &BrokerSession,
        target: Target,
        fix: PartitionTableFix,
    ) -> Result<PlanView, HostError> {
        let expected = writable(&target, &self.data_dir)?;
        let planned = engines.plan_partition_table(broker, target.clone(), fix)?;
        check_table_plan(&planned, &expected)?;
        let detail = PartitionTableDetail {
            fix: planned.fix,
            scheme: planned.scheme,
            filesystem: planned.filesystem,
            partition: planned.partition,
            offset: planned.offset,
            length: planned.length,
            type_id: planned.type_id,
            role: planned.role,
        };
        self.propose(
            planned.plan,
            PlanSummary {
                kind: PlanKind::PartitionTable,
                target,
                writes: Vec::new(),
                boot_sector: None,
                partition_table: Some(detail),
                mft: None,
                fs_fix: None,
                rollback_of: None,
            },
        )
    }

    /// 请引擎生成 `$MFT` 与 `$MFTMirr` 互相修复的计划，存好备份后交给界面确认。只读源。
    pub fn plan_mft(
        &self,
        engines: &Engines,
        broker: &BrokerSession,
        target: Target,
        partition: PartitionRef,
        fix: MftFix,
    ) -> Result<PlanView, HostError> {
        let expected = writable(&target, &self.data_dir)?;
        let planned = engines.plan_mft(broker, target.clone(), partition, fix)?;
        check_mft_plan(&planned, &expected, partition)?;
        let detail = MftDetail {
            fix: planned.fix,
            partition: partition.number,
            partition_offset: partition.offset,
            partition_length: partition.length,
            records: planned.records,
            from: planned.from,
            to: planned.to,
        };
        self.propose(
            planned.plan,
            PlanSummary {
                kind: PlanKind::Mft,
                target,
                writes: Vec::new(),
                boot_sector: None,
                partition_table: None,
                mft: Some(detail),
                fs_fix: None,
                rollback_of: None,
            },
        )
    }

    /// 请引擎生成文件系统修复的计划，存好备份后交给界面确认。只读源。
    pub fn plan_fs_fix(
        &self,
        engines: &Engines,
        broker: &BrokerSession,
        target: Target,
        partition: PartitionRef,
        fixes: Vec<FsFix>,
        bad_clusters: Vec<ClusterRun>,
    ) -> Result<PlanView, HostError> {
        let expected = writable(&target, &self.data_dir)?;
        let planned =
            engines.plan_fs_fix(broker, target.clone(), partition, fixes, bad_clusters)?;
        check_fs_fix_plan(&planned, &expected, partition)?;
        let detail = FsFixDetail {
            partition: partition.number,
            partition_offset: partition.offset,
            partition_length: partition.length,
            filesystem: planned.filesystem,
            counts: planned.counts,
        };
        self.propose(
            planned.plan,
            PlanSummary {
                kind: PlanKind::FsFix,
                target,
                writes: Vec::new(),
                boot_sector: None,
                partition_table: None,
                mft: None,
                fs_fix: Some(detail),
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
        writable(&record.summary.target, &self.data_dir)?;
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
                partition_table: record.summary.partition_table.clone(),
                mft: record.summary.mft.clone(),
                fs_fix: record.summary.fs_fix.clone(),
                rollback_of: Some(record.plan_id.clone()),
            },
        )
    }

    /// 执行已确认的计划，记进写盘记录。写之前就被拒绝（镜像正被别的程序打开、卷正被占用等）时盘没被改动，
    /// 计划留着，处理之后可以再试。
    pub fn execute(&self, broker: &BrokerSession, plan_id: &str) -> Result<WriteRecord, HostError> {
        let _guard = lock(&self.guard);
        let pending = lock(&self.pending)
            .remove(plan_id)
            .ok_or_else(|| HostError::new("planNotFound", plan_id))?;
        let outcome = match execute_on(broker, &pending.summary.target, &pending.backup) {
            Ok(outcome) => outcome,
            Err(error) => {
                lock(&self.pending).insert(plan_id.to_owned(), pending);
                return Err(error);
            }
        };
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

/// 可以写的目标应有的设备标识与扇区大小：计划里的 `device` 要与它一致。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Writable {
    identity: String,
    sector_size: u32,
}

/// 镜像文件与整块物理盘可以写，卷不行（卷上的偏移不是盘上的偏移）。物理盘：Windows 所在的盘不写，
/// 写前备份所在的 `data` 也不能在这块盘上（盘写坏了备份要还在）。
fn writable(target: &Target, data_dir: &Path) -> Result<Writable, HostError> {
    match target {
        Target::Image { path } => {
            let real = std::fs::canonicalize(path).map_err(|error| {
                HostError::new("writeTargetUnavailable", format!("{path}：{error}"))
            })?;
            Ok(Writable {
                identity: image_identity(&real),
                sector_size: IMAGE_SECTOR_SIZE,
            })
        }
        #[cfg(windows)]
        Target::Disk { disk_number } => writable_disk(*disk_number, data_dir),
        #[cfg(not(windows))]
        Target::Disk { .. } => {
            let _ = data_dir;
            Err(HostError::new("writeTargetUnsupported", ""))
        }
        Target::Volume { .. } => Err(HostError::new("writeTargetUnsupported", "")),
    }
}

#[cfg(windows)]
fn writable_disk(number: u32, data_dir: &Path) -> Result<Writable, HostError> {
    let system = yisi_io::system_disks()
        .map_err(|error| HostError::new("writeSystemDisk", yisi_rpc::error_chain(&error)))?;
    if system.contains(&number) {
        return Err(HostError::new(
            "writeSystemDisk",
            format!("磁盘 {number} 上有 Windows 所在的卷"),
        ));
    }
    yisi_io::ensure_off_source(Ok(number), yisi_io::volume_disk_numbers(data_dir))
        .map_err(|error| HostError::new("backupOnTarget", yisi_rpc::error_chain(&error)))?;
    let disk = yisi_io::list_disks()
        .into_iter()
        .find(|disk| disk.number == number)
        .ok_or_else(|| HostError::new("invalidDisk", format!("找不到磁盘 {number}")))?;
    Ok(Writable {
        identity: disk.identity(),
        sector_size: disk.sector_size,
    })
}

/// 执行一份计划：镜像在宿主里写，物理盘交给磁盘代理。写之前就被拒绝时返回错误（盘没被改动）。
fn execute_on(
    broker: &BrokerSession,
    target: &Target,
    backup: &SavedBackup,
) -> Result<WriteOutcome, HostError> {
    match target {
        Target::Image { path } => {
            let mut image =
                ImageFile::open(Path::new(path), IMAGE_SECTOR_SIZE).map_err(|error| {
                    let code = if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
                        "imageInUse"
                    } else {
                        "writeTargetUnavailable"
                    };
                    HostError::new(code, format!("{path}：{error}"))
                })?;
            Ok(execute::execute_reported(&mut image, backup))
        }
        #[cfg(windows)]
        Target::Disk { disk_number } => {
            broker.execute_write(*disk_number, &backup.path().display().to_string())
        }
        #[cfg(not(windows))]
        Target::Disk { .. } => {
            let _ = broker;
            Err(HostError::new("writeTargetUnsupported", ""))
        }
        Target::Volume { .. } => Err(HostError::new("writeTargetUnsupported", "")),
    }
}

/// 引擎给的计划要与请求对得上：写的是这个镜像或这块盘、扇区大小与读它时一致；复制时只有一段、重建时最多两段
/// （主引导扇区与备份），都落在这个分区里，每段不超过一个引导区，第一段从计划说的位置开始。
/// 引擎解析的是不可信的数据，宿主不只看它的摘要。
fn check_boot_sector_plan(planned: &BootSectorPlan, expected: &Writable) -> Result<(), HostError> {
    let plan = &planned.plan;
    let mismatch = |what: &str| HostError::new("planMismatch", what.to_owned());
    check_device(plan, expected)?;
    let segments = if planned.fix == BootFix::Rebuild {
        2
    } else {
        1
    };
    if plan.writes.is_empty() || plan.writes.len() > segments {
        return Err(mismatch("修复引导扇区的计划段数不对"));
    }
    let partition_end = planned
        .partition_offset
        .saturating_add(planned.partition_length);
    let inside = |offset: u64, length: u64| {
        offset >= planned.partition_offset && offset.saturating_add(length) <= partition_end
    };
    let starts_right = plan
        .writes
        .first()
        .is_some_and(|write| write.offset == planned.to);
    if !starts_right
        || plan
            .writes
            .iter()
            .any(|write| !inside(write.offset, write.len()) || write.len() > MAX_BOOT_SECTOR_BYTES)
    {
        return Err(mismatch("计划要写的位置不在这个分区的引导扇区上"));
    }
    Ok(())
}

/// 改写分区表的计划只写设备开头或末尾 [`TABLE_AREA`] 之内（分区表所在的地方），最多两段（GPT 主、备各一段）。
fn check_table_plan(planned: &PartitionTablePlan, expected: &Writable) -> Result<(), HostError> {
    let plan = &planned.plan;
    check_device(plan, expected)?;
    let size = plan.device.size;
    let in_table_area = |offset: u64, length: u64| {
        let end = offset.saturating_add(length);
        end <= TABLE_AREA || offset >= size.saturating_sub(TABLE_AREA)
    };
    if plan.writes.len() > 2
        || plan
            .writes
            .iter()
            .any(|write| !in_table_area(write.offset, write.len()))
    {
        return Err(HostError::new(
            "planMismatch",
            "计划要写的位置不在分区表所在的区域",
        ));
    }
    Ok(())
}

/// `$MFT` 与 `$MFTMirr` 互修的计划最多 4 段、每段不超过一条记录的最大长度，都落在请求的分区里
/// （按宿主自己请求的位置核对，不按引擎回传的）。
fn check_mft_plan(
    planned: &MftPlan,
    expected: &Writable,
    partition: PartitionRef,
) -> Result<(), HostError> {
    let plan = &planned.plan;
    check_device(plan, expected)?;
    let end = partition.offset.saturating_add(partition.length);
    let inside = |offset: u64, length: u64| {
        offset >= partition.offset && offset.saturating_add(length) <= end
    };
    if plan.writes.len() > MAX_MFT_RECORDS
        || plan
            .writes
            .iter()
            .any(|write| write.len() > MAX_MFT_RECORD_BYTES || !inside(write.offset, write.len()))
    {
        return Err(HostError::new(
            "planMismatch",
            "计划要写的位置不在这个分区的主文件表或它的副本上",
        ));
    }
    Ok(())
}

/// 文件系统修复的计划：每段都落在请求的分区里（按宿主自己请求的位置核对，不按引擎回传的）；段数与字节数的上限
/// 由计划本身的核对管（[`WritePlan::validate`]）。
fn check_fs_fix_plan(
    planned: &FsFixPlan,
    expected: &Writable,
    partition: PartitionRef,
) -> Result<(), HostError> {
    let plan = &planned.plan;
    check_device(plan, expected)?;
    let end = partition.offset.saturating_add(partition.length);
    if plan.writes.is_empty()
        || plan.writes.iter().any(|write| {
            write.offset < partition.offset || write.offset.saturating_add(write.len()) > end
        })
    {
        return Err(HostError::new(
            "planMismatch",
            "计划要写的位置不在这个分区里",
        ));
    }
    Ok(())
}

/// 计划写的是请求的那个镜像或那块盘、扇区大小与读它时一致，计划本身合法。
fn check_device(plan: &WritePlan, expected: &Writable) -> Result<(), HostError> {
    let mismatch = |what: &str| HostError::new("planMismatch", what.to_owned());
    if plan.device.identity != expected.identity {
        return Err(mismatch("计划写的不是这个镜像文件或这块盘"));
    }
    if plan.device.sector_size != expected.sector_size {
        return Err(mismatch("计划的扇区大小与读源时不同"));
    }
    plan.validate()
        .map_err(|error| mismatch(&error.to_string()))
}

#[cfg(test)]
mod tests;
