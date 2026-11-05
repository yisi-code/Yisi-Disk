//! 磁盘代理会话：第一次需要打开物理磁盘时弹出 UAC 启动代理，之后整个会话复用；宿主退出时代理随之退出。
//! 界面选中物理磁盘或卷时就先启动代理（[`BrokerSession::prepare`]），开始任务时不用再等 UAC。

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

use crate::engines::HostError;
#[cfg(windows)]
use crate::engines::{lock, sibling_program};

/// 等用户在 UAC 里作答、代理连上管道的最长时间。
#[cfg(windows)]
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Default)]
pub struct BrokerSession {
    #[cfg(windows)]
    connection: Mutex<Option<BrokerConnection>>,
    /// 主窗口的 `HWND`（0 为还没有）：UAC 以它为请求方，直接弹到最前面。
    #[cfg(windows)]
    owner: AtomicIsize,
}

impl BrokerSession {
    /// 记下主窗口，之后弹 UAC 时用它作请求方。
    #[cfg(windows)]
    pub fn set_owner(&self, hwnd: isize) {
        self.owner.store(hwnd, Ordering::Relaxed);
    }

    /// 还没有代理时先提权启动它，之后的任务直接复用；已经有了就什么也不做。
    #[cfg(windows)]
    pub fn prepare(&self) -> Result<(), HostError> {
        let mut connection = lock(&self.connection);
        if connection.is_none() {
            *connection = Some(launch(self.owner.load(Ordering::Relaxed))?);
        }
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn prepare(&self) -> Result<(), HostError> {
        Ok(())
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
            None => launch(self.owner.load(Ordering::Relaxed))?,
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
            Err(error) => Err(HostError::new("brokerIo", yisi_rpc::error_chain(&error))),
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
