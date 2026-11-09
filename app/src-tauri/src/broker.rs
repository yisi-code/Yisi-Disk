//! 磁盘代理会话：第一次需要打开物理磁盘时弹出 UAC 启动代理，之后整个会话复用；宿主退出时代理随之退出。
//! 用户在 UAC 里取消后，下次用到时（或在设置页、底栏点「授权」）会重新请求；授权的状态经 `disk-access` 事件告诉界面。

#[cfg(windows)]
use std::sync::Mutex;
#[cfg(windows)]
use std::sync::atomic::{AtomicIsize, Ordering};
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use yisi_broker::launcher::{BrokerConnection, CallError, LaunchError};
#[cfg(windows)]
use yisi_broker::protocol::{FailureData, WriteExecuted, refusal};
#[cfg(windows)]
use yisi_rpc::ErrorObject;

use std::sync::OnceLock;

use serde::Serialize;

use crate::engines::HostError;
#[cfg(windows)]
use crate::engines::{lock, sibling_program};

/// 等用户在 UAC 里作答、代理连上管道的最长时间。
#[cfg(windows)]
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);

/// 磁盘访问（管理员授权）的状态：设置页与底栏显示，取消后给出重新授权的入口。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DiskAccess {
    /// 这次运行还没请求过（或代理已经退出，下次用到时再请求）。
    #[default]
    NotRequested,
    /// 用户已授权，磁盘代理在运行。
    Granted,
    /// 用户在 UAC 里取消了。
    Cancelled,
    /// 代理没能启动（找不到程序、超时等）。
    Failed,
    /// 这个平台没有磁盘代理。
    Unsupported,
}

type AccessNotifier = Box<dyn Fn(DiskAccess) + Send + Sync>;

#[derive(Default)]
pub struct BrokerSession {
    access: std::sync::Mutex<DiskAccess>,
    /// 状态变了时通知界面（宿主启动时设好）。
    notifier: OnceLock<AccessNotifier>,
    #[cfg(windows)]
    connection: Mutex<Option<BrokerConnection>>,
    /// 主窗口的 `HWND`（0 为还没有）：UAC 以它为请求方，直接弹到最前面。
    #[cfg(windows)]
    owner: AtomicIsize,
}

impl BrokerSession {
    /// 状态变了时调用 `notify`（只设一次）。
    pub fn set_notifier(&self, notify: impl Fn(DiskAccess) + Send + Sync + 'static) {
        let _ = self.notifier.set(Box::new(notify));
    }

    /// 现在的磁盘访问状态。
    pub fn access(&self) -> DiskAccess {
        if cfg!(windows) {
            *crate::engines::lock(&self.access)
        } else {
            DiskAccess::Unsupported
        }
    }

    fn set_access(&self, value: DiskAccess) {
        let changed = {
            let mut access = crate::engines::lock(&self.access);
            let changed = *access != value;
            *access = value;
            changed
        };
        if changed && let Some(notify) = self.notifier.get() {
            notify(value);
        }
    }

    /// 现在就请求授权（设置页、底栏的「授权」）：代理已在运行时直接返回，否则弹 UAC 启动它。
    #[cfg(windows)]
    pub fn authorize(&self) -> Result<(), HostError> {
        let mut connection = lock(&self.connection);
        if connection.is_none() {
            *connection = Some(self.launch()?);
        }
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn authorize(&self) -> Result<(), HostError> {
        Err(HostError::new("unsupportedPlatform", ""))
    }

    /// 弹 UAC 启动代理，按结果记下状态。
    #[cfg(windows)]
    fn launch(&self) -> Result<BrokerConnection, HostError> {
        let launched = launch(self.owner.load(Ordering::Relaxed));
        self.set_access(match &launched {
            Ok(_) => DiskAccess::Granted,
            Err(error) if error.code == "elevationCancelled" => DiskAccess::Cancelled,
            Err(_) => DiskAccess::Failed,
        });
        launched
    }

    /// 记下主窗口，之后弹 UAC 时用它作请求方。
    #[cfg(windows)]
    pub fn set_owner(&self, hwnd: isize) {
        self.owner.store(hwnd, Ordering::Relaxed);
    }

    /// 请代理以只读方式打开 `path`，把句柄复制进 `target_pid` 进程，返回句柄在那个进程里的值。
    #[cfg(windows)]
    pub fn open_disk(&self, path: &str, target_pid: u32) -> Result<u64, HostError> {
        self.call(
            |broker| broker.open_disk(path, target_pid),
            |error| HostError::new("brokerRejected", error.message),
        )
        .map(|opened| opened.handle)
    }

    /// 请代理执行备份文件 `backup_path` 里的计划，写到磁盘 `disk_number`。写之前就被拒绝时按原因给出错误码
    /// （系统盘、编号已不是那块盘、卷正被占用等），这时盘没被改动。
    #[cfg(windows)]
    pub fn execute_write(
        &self,
        disk_number: u32,
        backup_path: &str,
    ) -> Result<WriteExecuted, HostError> {
        self.call(
            |broker| broker.execute_write(disk_number, backup_path),
            |error| HostError::new(refusal_code(&error), error.message),
        )
    }

    /// 与代理之间的一次调用：第一次需要时提权启动代理；代理答复错误时按 `rejected` 换成错误码，连接留着复用；
    /// 通信失败（例如代理被结束）时丢掉连接，下次重新启动。
    #[cfg(windows)]
    fn call<T>(
        &self,
        request: impl FnOnce(&mut BrokerConnection) -> Result<T, CallError>,
        rejected: impl FnOnce(ErrorObject) -> HostError,
    ) -> Result<T, HostError> {
        // 持锁期间可能正在等 UAC：其他请求排在后面，不会同时弹出多个 UAC
        let mut connection = lock(&self.connection);
        let mut broker = match connection.take() {
            Some(broker) => broker,
            None => self.launch()?,
        };
        match request(&mut broker) {
            Ok(value) => {
                *connection = Some(broker);
                Ok(value)
            }
            Err(CallError::Remote(error)) => {
                *connection = Some(broker);
                Err(rejected(error))
            }
            Err(error) => {
                // 连接断了（代理被结束等）：下次用到时重新请求授权
                self.set_access(DiskAccess::NotRequested);
                Err(HostError::new("brokerIo", yisi_rpc::error_chain(&error)))
            }
        }
    }

    #[cfg(not(windows))]
    pub fn open_disk(&self, _path: &str, _target_pid: u32) -> Result<u64, HostError> {
        Err(HostError::new("unsupportedPlatform", ""))
    }
}

/// 代理拒绝写盘的原因换成界面的错误码；没有原因的统一为 `brokerRejected`。
#[cfg(windows)]
fn refusal_code(error: &ErrorObject) -> &'static str {
    let reason = error
        .data
        .clone()
        .and_then(|data| serde_json::from_value::<FailureData>(data).ok())
        .and_then(|data| data.reason);
    match reason.as_deref() {
        Some(refusal::INVALID_DISK) => "invalidDisk",
        Some(refusal::BACKUP_UNREADABLE) => "backupUnreadable",
        Some(refusal::SYSTEM_DISK) => "writeSystemDisk",
        Some(refusal::WRONG_DISK) => "writeWrongDisk",
        Some(refusal::VOLUME_IN_USE) => "volumeInUse",
        Some(refusal::DISK_UNAVAILABLE) => "writeTargetUnavailable",
        _ => "brokerRejected",
    }
}

#[cfg(windows)]
fn launch(owner: isize) -> Result<BrokerConnection, HostError> {
    let program = sibling_program("yisi-broker", "brokerNotFound")?;
    BrokerConnection::launch(&program, LAUNCH_TIMEOUT, owner).map_err(|error| match error {
        LaunchError::Cancelled => HostError::new("elevationCancelled", ""),
        other => HostError::new("brokerStart", yisi_rpc::error_chain(&other)),
    })
}
