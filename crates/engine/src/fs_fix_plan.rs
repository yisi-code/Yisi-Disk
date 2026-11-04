//! 文件系统修复（`repair.planFsFix`）：调用 `yisi-repair` 的 `fs_fix`，把结果换成协议里的形式；另存丢失簇
//! （`repair.saveLost`）、读空闲簇（`repair.scanFree`）先在这里取出簇的位置；统一两份 FAT 之前的预演
//! （`repair.rateFatCopies`）。都只读源。

use std::sync::Arc;

use yisi_engine::protocol::{
    self, ClusterRun, DeviceIdentity, FatRating, FsFix, FsFixCount, FsFixPlan, FsIssueTotals,
    PartitionRef, WritePlan,
};
use yisi_fs::format::Run;
use yisi_io::BlockDevice;
use yisi_repair::fs_fix::{self, FixCount, FsFixError, LostClusters};
use yisi_rpc::ErrorObject;

use crate::browse::{fs_key, verify_partition};
use crate::runtime::error_chain;

/// 生成计划。分区先按文件系统检查的规则核对还在原处（`verify_partition`）。`identity` 同修复引导扇区。
pub fn plan(
    device: &Arc<dyn BlockDevice>,
    identity: String,
    partition: PartitionRef,
    fixes: &[FsFix],
    bad_clusters: &[ClusterRun],
) -> Result<FsFixPlan, ErrorObject> {
    verify_partition(device.as_ref(), partition)?;
    let wanted: Vec<fs_fix::FsFix> = fixes.iter().map(|&fix| to_repair(fix)).collect();
    let bad: Vec<Run> = bad_clusters
        .iter()
        .map(|run| Run {
            first: run.first,
            count: run.count,
        })
        .collect();
    let planned = fs_fix::plan_fs_fix(device, partition.offset, partition.length, &wanted, &bad)
        .map_err(error)?;
    let plan = WritePlan {
        device: DeviceIdentity {
            identity,
            size: device.size(),
            sector_size: device.sector_size(),
        },
        writes: planned.writes,
    };
    plan.validate().map_err(|error| {
        ErrorObject::new(protocol::error_code::PLAN_UNAVAILABLE, error_chain(&error))
    })?;
    Ok(FsFixPlan {
        plan,
        partition: partition.number,
        partition_offset: partition.offset,
        partition_length: partition.length,
        filesystem: fs_key(planned.filesystem).to_owned(),
        counts: planned.counts.iter().map(to_count).collect(),
    })
}

/// 统一两份 FAT 之前，两种方向各自修完还剩多少问题。
pub fn rate(
    device: &Arc<dyn BlockDevice>,
    partition: PartitionRef,
) -> Result<Vec<FatRating>, ErrorObject> {
    verify_partition(device.as_ref(), partition)?;
    let ratings =
        fs_fix::rate_fat_copies(device, partition.offset, partition.length).map_err(error)?;
    Ok(ratings
        .into_iter()
        .map(|rating| FatRating {
            fix: from_repair(rating.fix),
            totals: FsIssueTotals {
                data_loss: rating.totals.data_loss,
                structure: rating.totals.structure,
                minor: rating.totals.minor,
            },
            sectors: rating.sectors,
        })
        .collect())
}

/// 分区里的丢失簇；一个也没有时拒绝（没有可以另存的）。
pub fn lost(
    device: &dyn BlockDevice,
    partition: PartitionRef,
) -> Result<LostClusters, ErrorObject> {
    let lost = fs_fix::lost_clusters(device, partition.offset, partition.length).map_err(error)?;
    if lost.runs.is_empty() {
        return Err(ErrorObject::new(
            protocol::error_code::PLAN_NOT_APPLICABLE,
            "这个分区没有丢失簇".to_owned(),
        ));
    }
    Ok(lost)
}

/// 分区里的空闲簇（读一遍找坏簇）。
pub fn free(
    device: &dyn BlockDevice,
    partition: PartitionRef,
) -> Result<LostClusters, ErrorObject> {
    fs_fix::free_clusters(device, partition.offset, partition.length).map_err(error)
}

/// 协议里的「这类修复改了多少」。检查结果里的可修项（`repairs`）也用它。
pub fn to_count(count: &FixCount) -> FsFixCount {
    FsFixCount {
        fix: from_repair(count.fix),
        items: count.items,
        clusters: count.clusters,
    }
}

fn error(error: FsFixError) -> ErrorObject {
    let code = match error {
        FsFixError::NotFat(_)
        | FsFixError::FromBackup
        | FsFixError::NotTracked
        | FsFixError::FatCopiesDiffer
        | FsFixError::FatCopiesAlone
        | FsFixError::NothingToFix(_)
        | FsFixError::DirtyWithProblems => protocol::error_code::PLAN_NOT_APPLICABLE,
        FsFixError::TooLarge { .. }
        | FsFixError::NoFreeClusters
        | FsFixError::Unreadable(_)
        | FsFixError::Rehearsal(_) => protocol::error_code::PLAN_UNAVAILABLE,
    };
    ErrorObject::new(code, error_chain(&error))
}

fn to_repair(fix: FsFix) -> fs_fix::FsFix {
    match fix {
        FsFix::FsInfo => fs_fix::FsFix::FsInfo,
        FsFix::Bitmap => fs_fix::FsFix::Bitmap,
        FsFix::TooLong => fs_fix::FsFix::TooLong,
        FsFix::Lost => fs_fix::FsFix::Lost,
        FsFix::Dirty => fs_fix::FsFix::Dirty,
        FsFix::FatFromFirst => fs_fix::FsFix::FatFromFirst,
        FsFix::FatFromSecond => fs_fix::FsFix::FatFromSecond,
        FsFix::Chains => fs_fix::FsFix::Chains,
        FsFix::CrossLinks => fs_fix::FsFix::CrossLinks,
        FsFix::Entries => fs_fix::FsFix::Entries,
        FsFix::BadClusters => fs_fix::FsFix::BadClusters,
    }
}

fn from_repair(fix: fs_fix::FsFix) -> FsFix {
    match fix {
        fs_fix::FsFix::FsInfo => FsFix::FsInfo,
        fs_fix::FsFix::Bitmap => FsFix::Bitmap,
        fs_fix::FsFix::TooLong => FsFix::TooLong,
        fs_fix::FsFix::Lost => FsFix::Lost,
        fs_fix::FsFix::Dirty => FsFix::Dirty,
        fs_fix::FsFix::FatFromFirst => FsFix::FatFromFirst,
        fs_fix::FsFix::FatFromSecond => FsFix::FatFromSecond,
        fs_fix::FsFix::Chains => FsFix::Chains,
        fs_fix::FsFix::CrossLinks => FsFix::CrossLinks,
        fs_fix::FsFix::Entries => FsFix::Entries,
        fs_fix::FsFix::BadClusters => FsFix::BadClusters,
    }
}
