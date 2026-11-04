//! 文件系统修复（`repair.planFsFix`）：调用 `yisi-repair` 的 `fs_fix`，把结果换成协议里的形式；另存丢失簇
//! （`repair.saveLost`）先在这里取出丢失簇的位置。都只读源。

use std::sync::Arc;

use yisi_engine::protocol::{
    self, DeviceIdentity, FsFix, FsFixCount, FsFixPlan, PartitionRef, WritePlan,
};
use yisi_io::BlockDevice;
use yisi_repair::fs_fix::{self, FsFixError, LostClusters};
use yisi_rpc::ErrorObject;

use crate::browse::{fs_key, verify_partition};
use crate::runtime::error_chain;

/// 生成计划。分区先按文件系统检查的规则核对还在原处（`verify_partition`）。`identity` 同修复引导扇区。
pub fn plan(
    device: &Arc<dyn BlockDevice>,
    identity: String,
    partition: PartitionRef,
    fixes: &[FsFix],
) -> Result<FsFixPlan, ErrorObject> {
    verify_partition(device.as_ref(), partition)?;
    let wanted: Vec<fs_fix::FsFix> = fixes.iter().map(|&fix| to_repair(fix)).collect();
    let planned =
        fs_fix::plan_fs_fix(device, partition.offset, partition.length, &wanted).map_err(error)?;
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
        counts: planned
            .counts
            .iter()
            .map(|count| FsFixCount {
                fix: from_repair(count.fix),
                items: count.items,
                clusters: count.clusters,
            })
            .collect(),
    })
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

fn error(error: FsFixError) -> ErrorObject {
    let code = match error {
        FsFixError::NotFat(_)
        | FsFixError::FromBackup
        | FsFixError::NotTracked
        | FsFixError::FatCopiesDiffer
        | FsFixError::NothingToFix(_)
        | FsFixError::DirtyWithProblems => protocol::error_code::PLAN_NOT_APPLICABLE,
        FsFixError::TooLarge { .. } | FsFixError::Unreadable(_) | FsFixError::Rehearsal(_) => {
            protocol::error_code::PLAN_UNAVAILABLE
        }
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
    }
}

fn from_repair(fix: fs_fix::FsFix) -> FsFix {
    match fix {
        fs_fix::FsFix::FsInfo => FsFix::FsInfo,
        fs_fix::FsFix::Bitmap => FsFix::Bitmap,
        fs_fix::FsFix::TooLong => FsFix::TooLong,
        fs_fix::FsFix::Lost => FsFix::Lost,
        fs_fix::FsFix::Dirty => FsFix::Dirty,
    }
}
