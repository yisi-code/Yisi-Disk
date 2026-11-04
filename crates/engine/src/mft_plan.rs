//! `$MFT` 与 `$MFTMirr` 互相修复的写入计划（`repair.planMftRepair`）：调用 `yisi-repair`，把结果换成协议里的形式。只读源。

use std::sync::Arc;

use yisi_engine::protocol::{self, DeviceIdentity, MftFix, MftPlan, PartitionRef, WritePlan};
use yisi_io::BlockDevice;
use yisi_repair::mft_fix::{self, MftFixError};
use yisi_rpc::ErrorObject;

use crate::browse::verify_partition;
use crate::runtime::error_chain;

/// 生成计划。分区先按文件系统检查的规则核对还在原处（`verify_partition`）。`identity` 同修复引导扇区。
pub fn plan(
    device: &Arc<dyn BlockDevice>,
    identity: String,
    partition: PartitionRef,
    fix: MftFix,
) -> Result<MftPlan, ErrorObject> {
    verify_partition(device.as_ref(), partition)?;
    let direction = match fix {
        MftFix::MirrorFromMft => mft_fix::MftFix::MirrorFromMft,
        MftFix::MftFromMirror => mft_fix::MftFix::MftFromMirror,
    };
    let planned = mft_fix::plan_mft_fix(device, partition.offset, partition.length, direction)
        .map_err(|error| {
            let code = match error {
                MftFixError::NotApplicable | MftFixError::BootInvalid => {
                    protocol::error_code::PLAN_NOT_APPLICABLE
                }
                MftFixError::OutOfRange
                | MftFixError::Unreadable(_)
                | MftFixError::Unaligned
                | MftFixError::Rehearsal => protocol::error_code::PLAN_UNAVAILABLE,
            };
            ErrorObject::new(code, error_chain(&error))
        })?;
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
    Ok(MftPlan {
        plan,
        fix,
        partition: partition.number,
        partition_offset: partition.offset,
        partition_length: partition.length,
        records: planned.records,
        from: planned.from,
        to: planned.to,
    })
}
