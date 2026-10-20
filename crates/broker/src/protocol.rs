//! 宿主与磁盘代理之间的方法与参数。消息本身是 JSON-RPC 2.0（见 `yisi-rpc`）。

use serde::{Deserialize, Serialize};

/// 以只读方式打开物理磁盘，把句柄复制进 `targetPid` 进程。参数 [`OpenDiskParams`]，结果 [`DiskOpened`]。
pub const OPEN_DISK: &str = "broker.openDisk";

/// 结束代理。没有参数，结果是空对象。
pub const SHUTDOWN: &str = "broker.shutdown";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenDiskParams {
    pub path: String,
    pub target_pid: u32,
}

/// 句柄已复制进目标进程，`handle` 是它在目标进程里的值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskOpened {
    pub handle: u64,
    pub size: u64,
    pub sector_size: u32,
}

/// 代理答复错误时 `error.data` 的内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureData {
    /// 系统错误码（有的话）。
    pub os_error: Option<i32>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn field_names_are_camel_case() {
        let params = OpenDiskParams {
            path: "disk".to_owned(),
            target_pid: 7,
        };
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            json!({"path": "disk", "targetPid": 7})
        );
        let opened = DiskOpened {
            handle: 0x1a4,
            size: 1024,
            sector_size: 512,
        };
        assert_eq!(
            serde_json::to_value(&opened).unwrap(),
            json!({"handle": 420, "size": 1024, "sectorSize": 512})
        );
    }
}
