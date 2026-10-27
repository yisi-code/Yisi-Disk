//! 丢失分区搜索：起搜索任务；在浏览会话里按位置打开搜索找到的分区。

use tauri::AppHandle;
use yisi_engine::protocol::{
    OPEN_RANGE, OpenRangeParams, PartitionInfo, SEARCH_PARTITIONS, SearchParams,
};

use super::{
    BROWSE_TIMEOUT, Engines, HostError, Target, disk_path, from_value, resolve_source, to_params,
};
use crate::broker::BrokerSession;

impl Engines {
    /// 搜索丢失的分区（只读，可以暂停）。结果经任务事件的 completed 送达。
    pub fn start_partition_search(
        &self,
        app: AppHandle,
        broker: &BrokerSession,
        job_id: String,
        target: Target,
    ) -> Result<(), HostError> {
        if let Target::Disk { disk_number } = &target {
            disk_path(*disk_number)?;
        }
        let params_job_id = job_id.clone();
        self.start_job(app, job_id, SEARCH_PARTITIONS, |engine_pid| {
            to_params(&SearchParams {
                job_id: params_job_id,
                source: resolve_source(target, broker, engine_pid)?,
            })
        })
        .map(|_| ())
    }

    /// 在浏览会话里按位置打开一个分区；答复的编号之后像表里的分区一样列目录、预览、扫描。
    pub fn open_range(
        &self,
        job_id: String,
        offset: u64,
        length: u64,
        backup_boot: Option<u64>,
    ) -> Result<PartitionInfo, HostError> {
        let params = to_params(&OpenRangeParams {
            job_id: job_id.clone(),
            offset,
            length,
            backup_boot,
        })?;
        from_value(
            self.running(&job_id)?
                .request(OPEN_RANGE, params, BROWSE_TIMEOUT)?,
        )
    }
}
