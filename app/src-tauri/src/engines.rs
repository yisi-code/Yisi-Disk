//! 管理引擎进程：每个任务一个引擎，经 stdin / stdout 走 JSON-RPC；引擎的通知转成界面事件。

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter};
use yisi_engine::protocol::{
    CANCEL, CANCELLED, CAPABILITIES, COMPLETED, Capabilities, DiskList, FAILED, FOUND, HEARTBEAT,
    JobRef, LIST_DISKS, LOG, PROGRESS, READ_SCAN, ReadScanParams, Source,
};

use crate::broker::BrokerSession;
use yisi_rpc::{ErrorObject, FrameError, Id, Message, Request, read_message, write_message};

/// 发给界面的事件名，载荷是 [`JobEvent`]。
pub const JOB_EVENT: &str = "job-event";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 引擎每 2 秒一次心跳，超过这么久没有心跳就标为无响应。
const UNRESPONSIVE_AFTER: Duration = Duration::from_secs(10);
/// 发出取消后等这么久，引擎还没退出就强制结束它。
const CANCEL_GRACE: Duration = Duration::from_secs(5);
/// 镜像文件不记录扇区大小，按最常见的 512 字节处理。
const IMAGE_SECTOR_SIZE: u32 = 512;
/// 磁盘代理只接受 1 到 3 位数字的磁盘编号。
const MAX_DISK_NUMBER: u32 = 999;
/// Windows 的 `CREATE_NO_WINDOW`：引擎是控制台程序，不弹出控制台窗口。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 返回给界面的错误：`code` 由界面翻译成当前语言，`detail` 是附带的技术细节。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostError {
    pub code: &'static str,
    pub detail: String,
}

impl HostError {
    pub(crate) fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// 发给界面的任务事件。`kind` 为 progress、found、log、completed、failed、cancelled、unresponsive、responsive。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobEvent {
    pub job_id: String,
    pub kind: &'static str,
    pub data: Value,
}

/// 正在运行的任务，键是界面分配的任务 ID。
#[derive(Default)]
pub struct Engines {
    jobs: Arc<Mutex<HashMap<String, Arc<EngineProcess>>>>,
}

impl Engines {
    pub fn capabilities(&self) -> Result<Capabilities, HostError> {
        query(CAPABILITIES)
    }

    /// 列出物理磁盘：引擎只查询设备属性、不读磁盘内容，不需要管理员权限。
    pub fn list_disks(&self) -> Result<DiskList, HostError> {
        query(LIST_DISKS)
    }

    pub fn start_image_scan(
        &self,
        app: AppHandle,
        job_id: String,
        path: String,
    ) -> Result<(), HostError> {
        self.start_read_scan(app, job_id, |_| {
            Ok(Source::Image {
                path,
                sector_size: IMAGE_SECTOR_SIZE,
            })
        })
    }

    /// 扫描物理磁盘：先启动引擎，再请磁盘代理（必要时弹出 UAC）把只读句柄复制进这个引擎。
    pub fn start_disk_scan(
        &self,
        app: AppHandle,
        broker: &BrokerSession,
        job_id: String,
        disk_number: u32,
    ) -> Result<(), HostError> {
        if disk_number > MAX_DISK_NUMBER {
            return Err(HostError::new("invalidDisk", disk_number.to_string()));
        }
        let path = format!(r"\\.\PhysicalDrive{disk_number}");
        self.start_read_scan(app, job_id, |engine_pid| {
            let handle = broker.open_disk(&path, engine_pid)?;
            Ok(Source::Disk { handle })
        })
    }

    /// 启动引擎并交给它一个读取扫描任务。`source` 拿到引擎的进程 ID，返回要扫描的对象。
    fn start_read_scan(
        &self,
        app: AppHandle,
        job_id: String,
        source: impl FnOnce(u32) -> Result<Source, HostError>,
    ) -> Result<(), HostError> {
        validate_job_id(&job_id)?;
        if lock(&self.jobs).contains_key(&job_id) {
            return Err(HostError::new("jobExists", job_id));
        }
        let jobs = Arc::clone(&self.jobs);
        let finished_id = job_id.clone();
        let process = EngineProcess::spawn(Some((app, job_id.clone())), move || {
            lock(&jobs).remove(&finished_id);
        })?;
        lock(&self.jobs).insert(job_id.clone(), Arc::clone(&process));

        let started = source(process.pid).and_then(|source| {
            let params = ReadScanParams {
                job_id: job_id.clone(),
                source,
            };
            let params = serde_json::to_value(params)
                .map_err(|error| HostError::new("engineProtocol", error.to_string()))?;
            process.request(READ_SCAN, params)
        });
        if let Err(error) = started {
            // 引擎没接下任务：关闭输入让它退出，并且不再为这个任务发事件
            process.abandon();
            lock(&self.jobs).remove(&job_id);
            return Err(error);
        }
        Ok(())
    }

    pub fn cancel(&self, job_id: &str) -> Result<(), HostError> {
        let process = lock(&self.jobs)
            .get(job_id)
            .cloned()
            .ok_or_else(|| HostError::new("jobNotFound", job_id))?;
        process.cancel_requested.store(true, Ordering::Relaxed);
        let params = serde_json::to_value(JobRef {
            job_id: job_id.to_owned(),
        })
        .map_err(|error| HostError::new("engineProtocol", error.to_string()))?;
        process.request(CANCEL, params)?;
        thread::spawn(move || {
            let deadline = Instant::now() + CANCEL_GRACE;
            while !process.has_exited() {
                if Instant::now() >= deadline {
                    tracing::warn!("引擎在取消后没有按时退出，强制结束");
                    process.kill();
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        });
        Ok(())
    }
}

struct EngineProcess {
    pid: u32,
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Mutex<HashMap<Id, Sender<Result<Value, ErrorObject>>>>,
    next_id: AtomicU64,
    last_heartbeat: Mutex<Instant>,
    /// 已发出终结事件（或不再需要发），之后不再为这个任务发事件。
    finished: AtomicBool,
    cancel_requested: AtomicBool,
}

impl EngineProcess {
    /// 启动引擎。`job` 为界面与任务 ID 时，引擎的通知转成该任务的事件；`on_exit` 在引擎退出后调用。
    fn spawn(
        job: Option<(AppHandle, String)>,
        on_exit: impl FnOnce() + Send + 'static,
    ) -> Result<Arc<Self>, HostError> {
        let path = sibling_program("yisi-engine", "engineNotFound")?;
        let mut command = Command::new(&path);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().map_err(|error| {
            HostError::new("engineStart", format!("{}：{error}", path.display()))
        })?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HostError::new("engineStart", "拿不到引擎的标准输出"))?;

        let process = Arc::new(Self {
            pid,
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            last_heartbeat: Mutex::new(Instant::now()),
            finished: AtomicBool::new(job.is_none()),
            cancel_requested: AtomicBool::new(false),
        });
        if let Some((app, job_id)) = &job {
            spawn_watchdog(Arc::clone(&process), app.clone(), job_id.clone());
        }
        let reader = Arc::clone(&process);
        thread::spawn(move || {
            reader.read_loop(stdout, job.as_ref());
            on_exit();
        });
        Ok(process)
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, HostError> {
        let id = Id::Number(self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        let (sender, receiver) = mpsc::channel();
        lock(&self.pending).insert(id.clone(), sender);
        let message = Message::Request(Request::new(id.clone(), method, params));
        let written = match lock(&self.stdin).as_mut() {
            Some(stdin) => write_message(stdin, &message).map_err(|error| error.to_string()),
            None => Err("引擎的输入已关闭".to_owned()),
        };
        if let Err(detail) = written {
            lock(&self.pending).remove(&id);
            return Err(HostError::new("engineIo", detail));
        }
        match receiver.recv_timeout(REQUEST_TIMEOUT) {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(HostError::new("engineRejected", error.message)),
            Err(RecvTimeoutError::Timeout) => {
                lock(&self.pending).remove(&id);
                Err(HostError::new("engineTimeout", method))
            }
            Err(RecvTimeoutError::Disconnected) => Err(HostError::new("engineExited", method)),
        }
    }

    /// 不再为这个引擎发任务事件，并关闭它的输入；没有任务的引擎读到输入结束后自行退出。
    fn abandon(&self) {
        self.finished.store(true, Ordering::Relaxed);
        lock(&self.stdin).take();
    }

    fn has_exited(&self) -> bool {
        matches!(lock(&self.child).try_wait(), Ok(Some(_)))
    }

    fn kill(&self) {
        if let Err(error) = lock(&self.child).kill() {
            tracing::warn!(%error, "无法结束引擎进程");
        }
    }

    fn read_loop(&self, mut stdout: ChildStdout, job: Option<&(AppHandle, String)>) {
        loop {
            match read_message(&mut stdout) {
                Ok(Message::Response(response)) => {
                    if let Some(waiter) = lock(&self.pending).remove(&response.id) {
                        let _ = waiter.send(response.outcome);
                    }
                }
                Ok(Message::Notification(notification)) if notification.method == HEARTBEAT => {
                    *lock(&self.last_heartbeat) = Instant::now();
                }
                Ok(Message::Notification(notification)) => {
                    if let Some((app, job_id)) = job {
                        self.forward(app, job_id, &notification.method, notification.params);
                    }
                }
                // 引擎不向宿主发请求
                Ok(Message::Request(_)) => {}
                Err(FrameError::Closed) => break,
                Err(error) => {
                    tracing::warn!(%error, "读取引擎消息失败");
                    break;
                }
            }
        }

        // 引擎关闭了输出：让还在等答复的请求立即失败，再等进程退出、取退出码
        lock(&self.pending).clear();
        let exit_code = self.wait_for_exit();
        if let Some((app, job_id)) = job
            && !self.finished.swap(true, Ordering::Relaxed)
        {
            // 引擎没有发出终结事件就退出了：取消后被强制结束算取消，否则算失败
            let (kind, data) = if self.cancel_requested.load(Ordering::Relaxed) {
                ("cancelled", json!({ "forced": true }))
            } else {
                (
                    "failed",
                    json!({ "reason": "engineExited", "exitCode": exit_code }),
                )
            };
            emit(app, job_id, kind, data);
        }
    }

    fn forward(&self, app: &AppHandle, job_id: &str, method: &str, params: Value) {
        if self.finished.load(Ordering::Relaxed) {
            return;
        }
        let kind = match method {
            PROGRESS => "progress",
            FOUND => "found",
            LOG => "log",
            COMPLETED => "completed",
            FAILED => "failed",
            CANCELLED => "cancelled",
            other => {
                tracing::debug!(method = other, "忽略未知的引擎通知");
                return;
            }
        };
        if params.get("jobId").and_then(Value::as_str) != Some(job_id) {
            tracing::warn!(method, "通知里的任务 ID 不符，已忽略");
            return;
        }
        if matches!(kind, "completed" | "failed" | "cancelled") {
            self.finished.store(true, Ordering::Relaxed);
        }
        let mut data = params;
        if let Some(object) = data.as_object_mut() {
            object.remove("jobId");
        }
        emit(app, job_id, kind, data);
    }

    /// 输出已关闭，进程随后就会退出；最多等 5 秒，仍未退出就结束它。
    fn wait_for_exit(&self) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // 先取结果、释放锁，下面的 kill 还要再拿同一把锁
            let state = lock(&self.child).try_wait();
            match state {
                Ok(Some(status)) => return status.code(),
                Ok(None) if Instant::now() < deadline => {}
                Ok(None) | Err(_) => {
                    self.kill();
                    return None;
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// 每秒检查一次心跳：超时发 unresponsive，恢复后发 responsive。
fn spawn_watchdog(process: Arc<EngineProcess>, app: AppHandle, job_id: String) {
    thread::spawn(move || {
        let mut reported = false;
        while !process.finished.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_secs(1));
            let silent = lock(&process.last_heartbeat).elapsed();
            if silent >= UNRESPONSIVE_AFTER && !reported {
                reported = true;
                emit(
                    &app,
                    &job_id,
                    "unresponsive",
                    json!({ "silentSeconds": silent.as_secs() }),
                );
            } else if silent < UNRESPONSIVE_AFTER && reported {
                reported = false;
                emit(&app, &job_id, "responsive", json!({}));
            }
        }
    });
}

fn emit(app: &AppHandle, job_id: &str, kind: &'static str, data: Value) {
    let event = JobEvent {
        job_id: job_id.to_owned(),
        kind,
        data,
    };
    if let Err(error) = app.emit(JOB_EVENT, event) {
        tracing::warn!(%error, "无法向界面发送任务事件");
    }
}

/// 临时启动一个引擎发一次查询，查完关闭它的输入，引擎随即退出。
fn query<T: DeserializeOwned>(method: &str) -> Result<T, HostError> {
    let process = EngineProcess::spawn(None, || {})?;
    let result = process.request(method, Value::Null);
    process.abandon();
    serde_json::from_value(result?)
        .map_err(|error| HostError::new("engineProtocol", error.to_string()))
}

/// 引擎、代理与宿主程序放在同一目录；找不到时返回错误码 `missing_code`。
pub(crate) fn sibling_program(
    stem: &str,
    missing_code: &'static str,
) -> Result<PathBuf, HostError> {
    let host =
        std::env::current_exe().map_err(|error| HostError::new(missing_code, error.to_string()))?;
    let path = host.with_file_name(format!("{stem}{}", std::env::consts::EXE_SUFFIX));
    if path.is_file() {
        Ok(path)
    } else {
        Err(HostError::new(missing_code, path.display().to_string()))
    }
}

/// 任务 ID 由界面生成（UUID），宿主只接受字母、数字和连字符。
fn validate_job_id(job_id: &str) -> Result<(), HostError> {
    let valid = (1..=64).contains(&job_id.len())
        && job_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(HostError::new("invalidJobId", job_id))
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_ids_are_restricted() {
        assert!(validate_job_id("2f1c9a3e-6b4d-4c55-9f0e-1a2b3c4d5e6f").is_ok());
        for bad in ["", "a b", "../x", "任务", &"x".repeat(65)] {
            assert!(validate_job_id(bad).is_err(), "{bad}");
        }
    }
}
