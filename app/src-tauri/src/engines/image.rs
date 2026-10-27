//! 制作镜像：起任务；做之前看一眼源与镜像；查看以前做过的镜像现在怎样。

use serde::Deserialize;
use tauri::AppHandle;
use yisi_engine::protocol::{
    ByteRange, IMAGE_CREATE, IMAGE_DESCRIBE, IMAGE_INSPECT, IMAGE_RESUME, ImageDescribeParams,
    ImageDescriptions, ImageInspectParams, ImageInspection, ImageParams, ImageStarted,
};

use super::{
    BROWSE_TIMEOUT, EngineProcess, Engines, HostError, Target, from_value, query_with,
    resolve_source, to_params,
};
use crate::broker::BrokerSession;

/// 这一次怎样做（界面传来）：读哪些范围、先清掉哪些范围的进度、是否只做复制这一轮。都不给时读整个源。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagePlan {
    #[serde(default)]
    pub ranges: Option<Vec<ByteRange>>,
    #[serde(default)]
    pub copy_only: bool,
    #[serde(default)]
    pub reset: Vec<ByteRange>,
}

impl Engines {
    /// 制作镜像（`resume` 时按坏区图续做）。源为物理磁盘时，引擎在开始前核对镜像不在这块盘上（`outputOnSource`）；
    /// 新建时镜像或坏区图已存在答复 `imageExists`，续做时坏区图对不上答复 `imageMismatch`。
    #[expect(clippy::too_many_arguments, reason = "与界面命令的参数一一对应")]
    pub fn start_image(
        &self,
        app: AppHandle,
        broker: &BrokerSession,
        job_id: String,
        target: Target,
        image_path: String,
        resume: bool,
        plan: ImagePlan,
    ) -> Result<ImageStarted, HostError> {
        target.validate()?;
        let method = if resume { IMAGE_RESUME } else { IMAGE_CREATE };
        let params_job_id = job_id.clone();
        let started = self.start_job(app, job_id, method, |engine_pid| {
            to_params(&ImageParams {
                job_id: params_job_id,
                source: resolve_source(target, broker, engine_pid)?,
                image_path,
                ranges: plan.ranges,
                copy_only: plan.copy_only,
                reset: plan.reset,
            })
        })?;
        from_value(started)
    }

    /// 做镜像之前看一眼：另起一个引擎读源的分区表、查输出位置，镜像已存在时对比分区表并抽查，答复后退出。
    /// 物理盘第一次需要时提权启动磁盘代理。
    pub fn inspect_image(
        &self,
        broker: &BrokerSession,
        target: Target,
        image_path: String,
    ) -> Result<ImageInspection, HostError> {
        target.validate()?;
        let process = EngineProcess::spawn(None, || {})?;
        let result = resolve_source(target, broker, process.pid)
            .and_then(|source| to_params(&ImageInspectParams { source, image_path }))
            .and_then(|params| process.request(IMAGE_INSPECT, params, BROWSE_TIMEOUT));
        process.abandon();
        from_value(result?)
    }

    /// 以前做过的镜像现在怎样：只读坏区图，不碰源。
    pub fn describe_images(
        &self,
        image_paths: Vec<String>,
    ) -> Result<ImageDescriptions, HostError> {
        query_with(
            IMAGE_DESCRIBE,
            to_params(&ImageDescribeParams { image_paths })?,
        )
    }
}
