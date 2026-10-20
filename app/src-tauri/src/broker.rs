//! 磁盘代理会话：第一次需要打开物理磁盘时弹出 UAC 启动代理，之后整个会话复用；宿主退出时代理随之退出。

#[cfg(windows)]
use std::sync::Mutex;
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use yisi_broker::launcher::{BrokerConnection, CallError, LaunchError};

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
}

impl BrokerSession {
    /// 请代理以只读方式打开 `path`，把句柄复制进 `target_pid` 进程，返回句柄在那个进程里的值。
    #[cfg(windows)]
    pub fn open_disk(&self, path: &str, target_pid: u32) -> Result<u64, HostError> {
        // 持锁期间可能正在等 UAC：其他请求排在后面，不会同时弹出多个 UAC
        let mut connection = lock(&self.connection);
        let mut broker = match connection.take() {
            Some(broker) => broker,
            None => launch()?,
        };
        match broker.open_disk(path, target_pid) {
            Ok(opened) => {
                *connection = Some(broker);
                Ok(opened.handle)
            }
            Err(CallError::Remote(error)) => {
                *connection = Some(broker);
                Err(HostError::new("brokerRejected", error.message))
            }
            // 代理已经不可用（例如被结束）：丢掉这个连接，下次重新启动
            Err(error) => Err(HostError::new("brokerIo", describe(&error))),
        }
    }

    #[cfg(not(windows))]
    pub fn open_disk(&self, _path: &str, _target_pid: u32) -> Result<u64, HostError> {
        Err(HostError::new("unsupportedPlatform", ""))
    }
}

#[cfg(windows)]
fn launch() -> Result<BrokerConnection, HostError> {
    let program = sibling_program("yisi-broker", "brokerNotFound")?;
    BrokerConnection::launch(&program, LAUNCH_TIMEOUT).map_err(|error| match error {
        LaunchError::Cancelled => HostError::new("elevationCancelled", ""),
        other => HostError::new("brokerStart", describe(&other)),
    })
}

/// 把错误及其来源串成一句话。
#[cfg(windows)]
fn describe(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push('：');
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}
