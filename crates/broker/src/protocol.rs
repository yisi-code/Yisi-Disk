//! 宿主与磁盘代理之间的消息。

use serde::{Deserialize, Serialize};

/// 宿主发给代理的请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    /// 以只读方式打开物理磁盘，把句柄复制进 `target_pid` 进程。
    #[serde(rename = "broker.openDisk", rename_all = "camelCase")]
    OpenDisk { path: String, target_pid: u32 },

    /// 结束代理。
    #[serde(rename = "broker.shutdown")]
    Shutdown,
}

/// 代理的答复。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "camelCase")]
pub enum Response {
    /// 句柄已复制进目标进程，`handle` 是它在目标进程里的值。
    #[serde(rename_all = "camelCase")]
    DiskOpened {
        handle: u64,
        size: u64,
        sector_size: u32,
    },

    /// 已收到结束请求。
    ShuttingDown,

    /// 请求失败；`os_error` 是系统错误码（有的话）。
    #[serde(rename_all = "camelCase")]
    Failed {
        message: String,
        os_error: Option<i32>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_matches_rpc_naming() {
        let request = Request::OpenDisk {
            path: "disk".to_owned(),
            target_pid: 7,
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"method":"broker.openDisk","params":{"path":"disk","targetPid":7}}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Shutdown).unwrap(),
            r#"{"method":"broker.shutdown"}"#
        );
        let response = Response::DiskOpened {
            handle: 0x1a4,
            size: 1024,
            sector_size: 512,
        };
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"result":"diskOpened","handle":420,"size":1024,"sectorSize":512}"#
        );
    }
}
