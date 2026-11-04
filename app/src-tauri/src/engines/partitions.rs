//! 丢失分区搜索：起搜索任务；在浏览会话里按位置打开搜索找到的分区。

use tauri::AppHandle;
use yisi_engine::protocol::{
    BootCopy, OPEN_RANGE, OpenRangeParams, PartitionInfo, SEARCH_PARTITIONS, SearchMode,
    SearchParams,
};

use super::{BROWSE_TIMEOUT, Engines, HostError, Target, from_value, resolve_source, to_params};
use crate::broker::BrokerSession;

impl Engines {
    /// 搜索丢失的分区（只读，可以暂停）。找到的分区经任务事件的 partitions 陆续送达，结果经 completed 送达
    /// （被取消时也是 completed，带停下之前找到的分区）。
    pub fn start_partition_search(
        &self,
        app: AppHandle,
        broker: &BrokerSession,
        job_id: String,
        target: Target,
        mode: SearchMode,
    ) -> Result<(), HostError> {
        target.validate()?;
        let params_job_id = job_id.clone();
        self.start_job(app, job_id, SEARCH_PARTITIONS, |engine_pid| {
            to_params(&SearchParams {
                job_id: params_job_id,
                source: resolve_source(target, broker, engine_pid)?,
                mode,
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
        boot: BootCopy,
    ) -> Result<PartitionInfo, HostError> {
        let params = to_params(&OpenRangeParams {
            job_id: job_id.clone(),
            offset,
            length,
            boot,
        })?;
        from_value(
            self.running(&job_id)?
                .request(OPEN_RANGE, params, BROWSE_TIMEOUT)?,
        )
    }
}
