//! 宿主与引擎之间的方法、通知与参数。消息本身是 JSON-RPC 2.0（见 `yisi-rpc`），经引擎的 stdin / stdout 传递。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 返回 [`Capabilities`]。
pub const CAPABILITIES: &str = "engine.capabilities";
/// 读取扫描（任务）：参数 [`ReadScanParams`]，立即返回空对象，结果经 [`COMPLETED`] 通知送达，内容是 [`ReadScanReport`]。
pub const READ_SCAN: &str = "disk.readScan";
/// 取消任务：参数 [`JobRef`]。
pub const CANCEL: &str = "job.cancel";
/// 列出物理磁盘：没有参数，结果是 [`DiskList`]。只查询设备属性、不读磁盘内容，不需要管理员权限；目前只在 Windows 上提供。
pub const LIST_DISKS: &str = "disk.list";

/// 引擎每隔 [`HEARTBEAT_INTERVAL`] 发一次，没有参数。
pub const HEARTBEAT: &str = "engine.heartbeat";
/// 参数 [`Progress`]，每秒最多 10 次。
pub const PROGRESS: &str = "job.progress";
/// 参数 [`JobLog`]。
pub const LOG: &str = "job.log";
/// 参数 [`JobCompleted`]。
pub const COMPLETED: &str = "job.completed";
/// 参数 [`JobFailed`]。
pub const FAILED: &str = "job.failed";
/// 参数 [`JobRef`]。
pub const CANCELLED: &str = "job.cancelled";

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

/// 引擎自定义的错误码（JSON-RPC 留给实现的 -32000 到 -32099）。
pub mod error_code {
    /// 每个引擎进程只运行一个任务。
    pub const JOB_ALREADY_RUNNING: i64 = -32001;
    /// 打不开要扫描的设备或镜像。
    pub const SOURCE_UNAVAILABLE: i64 = -32002;
    /// 没有这个任务。
    pub const JOB_NOT_FOUND: i64 = -32003;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub version: String,
    pub methods: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadScanParams {
    /// 由宿主分配，之后的通知都带着它。
    pub job_id: String,
    pub source: Source,
}

/// 要读取的对象。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Source {
    #[serde(rename_all = "camelCase")]
    Image { path: String, sector_size: u32 },
    /// 物理磁盘。Windows 上 `handle` 是磁盘代理复制进引擎进程的只读句柄。
    #[serde(rename_all = "camelCase")]
    Disk { handle: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskList {
    pub disks: Vec<DiskSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskSummary {
    pub number: u32,
    pub path: String,
    pub size: u64,
    /// 逻辑扇区大小。
    pub sector_size: u32,
    pub vendor: String,
    pub model: String,
    pub serial: String,
    /// ata、sata、scsi、sas、nvme、usb、sd、mmc、virtual、other 之一。
    pub bus: String,
    pub removable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRef {
    pub job_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub job_id: String,
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobLog {
    pub job_id: String,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCompleted {
    pub job_id: String,
    pub result: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobFailed {
    pub job_id: String,
    pub message: String,
}

/// 读取扫描的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadScanReport {
    pub size: u64,
    pub bytes_read: u64,
    /// 读不出的区段，相邻的已合并；最多记录 [`MAX_UNREADABLE_RANGES`] 段。
    pub unreadable: Vec<ByteRange>,
    pub unreadable_bytes: u64,
    /// 读不出的区段超过上限、没有全部列出。
    pub unreadable_ranges_truncated: bool,
    pub elapsed_ms: u64,
}

pub const MAX_UNREADABLE_RANGES: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn read_scan_params_wire_format() {
        let params = ReadScanParams {
            job_id: "j1".to_owned(),
            source: Source::Image {
                path: "a.img".to_owned(),
                sector_size: 512,
            },
        };
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            json!({"jobId": "j1", "source": {"type": "image", "path": "a.img", "sectorSize": 512}})
        );
    }
}
