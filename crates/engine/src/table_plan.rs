//! 改写分区表的写入计划（`repair.planPartitionWrite`）：调用 `yisi-repair`，把结果换成协议里的形式。只读源。

use std::sync::Arc;

use yisi_engine::protocol::{
    self, DeviceIdentity, PartitionTableFix, PartitionTablePlan, WritePlan,
};
use yisi_io::BlockDevice;
use yisi_repair::table_fix::{self, TableFix, TableFixError};
use yisi_rpc::ErrorObject;

use crate::browse::{fs_key, role_key, scheme_name, table_kind};
use crate::runtime::error_chain;

/// 生成改写分区表的计划。`identity` 同修复引导扇区（`boot_check::plan`）。
pub fn plan(
    device: &Arc<dyn BlockDevice>,
    identity: String,
    fix: PartitionTableFix,
) -> Result<PartitionTablePlan, ErrorObject> {
    let request = match fix {
        PartitionTableFix::AddLost { offset, length } => TableFix::AddLost { offset, length },
        PartitionTableFix::SetType { partition } => TableFix::SetType { partition },
    };
    let planned = table_fix::plan_table_fix(device, request).map_err(|error| {
        let code = match error {
            TableFixError::Table(_) => protocol::error_code::SOURCE_UNAVAILABLE,
            TableFixError::NoPartition(_) | TableFixError::NotApplicable(_) => {
                protocol::error_code::PLAN_NOT_APPLICABLE
            }
            TableFixError::Edit(_) | TableFixError::NoFilesystem | TableFixError::Rehearsal(_) => {
                protocol::error_code::PLAN_UNAVAILABLE
            }
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
    let (type_id, ..) = table_kind(&planned.partition);
    Ok(PartitionTablePlan {
        plan,
        fix,
        scheme: scheme_name(planned.scheme).to_owned(),
        filesystem: fs_key(planned.filesystem).to_owned(),
        partition: planned.partition.number,
        offset: planned.partition.offset,
        length: planned.partition.length,
        type_id,
        role: planned
            .partition
            .role()
            .map(|role| role_key(role).to_owned()),
    })
}
