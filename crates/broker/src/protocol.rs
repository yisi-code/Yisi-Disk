//! 宿主与磁盘代理之间的方法与参数。消息本身是 JSON-RPC 2.0（见 `yisi-rpc`）。

use serde::{Deserialize, Serialize};

use crate::execute::Step;

/// 物理磁盘与卷的路径规则（代理只接受 `\\.\PhysicalDriveN` 与 `\\.\C:`），宿主据此拼路径。
pub use yisi_io::{MAX_DISK_NUMBER, physical_drive_path, volume_path};

/// 以只读方式打开物理磁盘，把句柄复制进 `targetPid` 进程。参数 [`OpenDiskParams`]，结果 [`DiskOpened`]。
pub const OPEN_DISK: &str = "broker.openDisk";

/// 执行一份物理盘的写入计划：参数 [`ExecuteWriteParams`]，结果 [`WriteExecuted`]。代理读出宿主存好的备份文件，
/// 执行的就是备份里的计划。写之前就拒绝时答复错误，`error.data` 是 [`FailureData`]，`reason` 见 [`refusal`]。
pub const EXECUTE_WRITE: &str = "broker.executeWrite";

/// 结束代理。没有参数，结果是空对象。
pub const SHUTDOWN: &str = "broker.shutdown";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteWriteParams {
    /// `\\.\PhysicalDriveN` 的 N；执行前核对它现在就是计划里的那块盘。
    pub disk_number: u32,
    /// 宿主存好的备份文件（`backup::SavedBackup`）。
    pub backup_path: String,
}

/// 开始执行之后的结果：成功，或停在哪一步、失败前已经写下去几段（为 0 时盘没被改动）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteExecuted {
    pub done: bool,
    pub step: Option<Step>,
    pub written: usize,
    pub detail: Option<String>,
}

/// 写之前就拒绝的原因（[`FailureData::reason`]）。这时盘一个字节都没改。
pub mod refusal {
    pub const INVALID_DISK: &str = "invalidDisk";
    /// 备份文件读不出、摘要对不上，或计划本身不合法。
    pub const BACKUP_UNREADABLE: &str = "backupUnreadable";
    /// 盘上有 Windows 所在的卷，或者查不到系统盘在哪。
    pub const SYSTEM_DISK: &str = "systemDisk";
    /// 这个编号现在不是计划里那块盘（重新插拔后编号变了，或容量、扇区大小不同）。
    pub const WRONG_DISK: &str = "wrongDisk";
    /// 与写入范围重叠的卷锁不上：有程序正开着里面的文件。
    pub const VOLUME_IN_USE: &str = "volumeInUse";
    /// 盘打不开或查不了卷。
    pub const DISK_UNAVAILABLE: &str = "diskUnavailable";
}

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
    /// 拒绝执行写入计划的原因（[`refusal`]）；其他请求没有。
    #[serde(default)]
    pub reason: Option<String>,
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
