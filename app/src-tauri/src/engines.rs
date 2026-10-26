//! 管理引擎进程：每个任务一个引擎，经 stdin / stdout 走 JSON-RPC；引擎的通知转成界面事件。

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter};
use yisi_engine::protocol::{
    BROWSE_OPEN, BrowseOpened, BrowseParams, CANCEL, CANCELLED, CAPABILITIES, CARVE_EXTRACT,
    CARVE_START, COMPLETED, COPIED, COPY_FILES, Capabilities, CarveParams, CarveStarted, CopyItem,
    CopyParams, CopyStarted, DiskList, ExtractItem, ExtractParams, FAILED, FOUND, FileListPage,
    FilePreview, FileTypeList, HEARTBEAT, JobRef, LIST_DISKS, LIST_FILE_TYPES, LIST_FILES, LOG,
    ListFilesParams, PREVIEW_FILE, PREVIEW_FS_FILE, PREVIEW_RANGE, PROGRESS, PartitionRef,
    PreviewFsParams, PreviewParams, PreviewRangeParams, READ_SCAN, ReadScanParams, Source,
    error_code,
};

use crate::broker::BrokerSession;
use yisi_rpc::{ErrorObject, FrameError, Id, Message, Request, read_message, write_message};

/// 发给界面的事件名，载荷是 [`JobEvent`]。
pub const JOB_EVENT: &str = "job-event";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 打开分区表、列目录、预览可能要读很多数据（十万级的目录、慢速的 USB 盘），等得久一些。
const BROWSE_TIMEOUT: Duration = Duration::from_secs(120);
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

/// 界面选定的源：镜像文件，或按编号指定的物理磁盘。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Target {
    Image {
        path: String,
    },
    #[serde(rename_all = "camelCase")]
    Disk {
        disk_number: u32,
    },
}

/// 发给界面的任务事件。`kind` 为 progress、found、copied、log、completed、failed、cancelled、unresponsive、responsive。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobEvent {
    pub job_id: String,
    pub kind: &'static str,
    pub data: Value,
}

#[derive(Default)]
pub struct Engines {
    /// 正在运行的任务，键是界面分配的任务 ID。
    jobs: Arc<Mutex<HashMap<String, Arc<EngineProcess>>>>,
    /// 本次运行里启动过的深度恢复任务各自的输出目录，任务结束后仍保留，供预览和打开恢复出的文件。
    recovered_dirs: Mutex<HashMap<String, PathBuf>>,
    /// 本次运行里启动过的复制任务各自的输出目录，供打开、在文件夹中显示复制出的文件。
    copied_dirs: Mutex<HashMap<String, PathBuf>>,
    /// 本次运行里启动过的深度恢复任务各自的源：只扫描时，预览和保存都要回到这个源上读。
    carve_targets: Mutex<HashMap<String, Target>>,
}

impl Engines {
    pub fn capabilities(&self) -> Result<Capabilities, HostError> {
        query(CAPABILITIES)
    }

    /// 列出物理磁盘：引擎只查询设备属性、不读磁盘内容，不需要管理员权限。
    pub fn list_disks(&self) -> Result<DiskList, HostError> {
        query(LIST_DISKS)
    }

    pub fn file_types(&self) -> Result<FileTypeList, HostError> {
        query(LIST_FILE_TYPES)
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
        let path = disk_path(disk_number)?;
        self.start_read_scan(app, job_id, |engine_pid| {
            let handle = broker.open_disk(&path, engine_pid)?;
            Ok(Source::Disk { handle })
        })
    }

    /// `source` 拿到引擎的进程 ID，返回要扫描的对象。
    fn start_read_scan(
        &self,
        app: AppHandle,
        job_id: String,
        source: impl FnOnce(u32) -> Result<Source, HostError>,
    ) -> Result<(), HostError> {
        let params_job_id = job_id.clone();
        self.start_job(app, job_id, READ_SCAN, |engine_pid| {
            to_params(&ReadScanParams {
                job_id: params_job_id,
                source: source(engine_pid)?,
            })
        })
        .map(|_| ())
    }

    /// 深度恢复。源为物理磁盘时，引擎在开始前核对输出目录不在这块盘上，否则拒绝（`outputOnSource`）。
    pub fn start_carve(
        &self,
        app: AppHandle,
        broker: &BrokerSession,
        job_id: String,
        target: Target,
        output_dir: Option<String>,
        extensions: Vec<String>,
    ) -> Result<(), HostError> {
        // 磁盘编号不合法时不启动引擎
        if let Target::Disk { disk_number } = &target {
            disk_path(*disk_number)?;
        }
        let params_job_id = job_id.clone();
        let source_target = target.clone();
        let started = self.start_job(app, job_id.clone(), CARVE_START, |engine_pid| {
            to_params(&CarveParams {
                job_id: params_job_id,
                source: resolve_source(target, broker, engine_pid)?,
                output_dir,
                extensions: Some(extensions),
            })
        })?;
        let CarveStarted { output_dir } = from_value(started)?;
        if let Some(output_dir) = output_dir {
            lock(&self.recovered_dirs).insert(job_id.clone(), PathBuf::from(output_dir));
        }
        lock(&self.carve_targets).insert(job_id, source_target);
        Ok(())
    }

    /// 预览只扫描、还没保存的文件：另起一个引擎回到扫描任务 `scan_job_id` 的源上读这一段。
    pub fn preview_range(
        &self,
        broker: &BrokerSession,
        scan_job_id: &str,
        offset: u64,
        length: u64,
    ) -> Result<FilePreview, HostError> {
        let target = self.carve_target(scan_job_id)?;
        let process = EngineProcess::spawn(None, || {})?;
        let result = resolve_source(target, broker, process.pid)
            .and_then(|source| {
                to_params(&PreviewRangeParams {
                    source,
                    offset,
                    length,
                })
            })
            .and_then(|params| process.request(PREVIEW_RANGE, params, BROWSE_TIMEOUT));
        process.abandon();
        from_value(result?)
    }

    /// 保存只扫描时选中的文件：回到扫描任务 `scan_job_id` 的源上重新读出并校验，写进输出目录。
    /// 之后按这个保存任务的 ID 加文件名打开、在文件夹中显示（与边扫边保存的结果一样）。
    pub fn start_extract(
        &self,
        app: AppHandle,
        broker: &BrokerSession,
        job_id: String,
        scan_job_id: &str,
        items: Vec<ExtractItem>,
        output_dir: String,
    ) -> Result<(), HostError> {
        let target = self.carve_target(scan_job_id)?;
        let params_job_id = job_id.clone();
        let started = self.start_job(app, job_id.clone(), CARVE_EXTRACT, |engine_pid| {
            to_params(&ExtractParams {
                job_id: params_job_id,
                source: resolve_source(target, broker, engine_pid)?,
                items,
                output_dir,
            })
        })?;
        let CopyStarted { output_dir } = from_value(started)?;
        lock(&self.recovered_dirs).insert(job_id, PathBuf::from(output_dir));
        Ok(())
    }

    fn carve_target(&self, scan_job_id: &str) -> Result<Target, HostError> {
        lock(&self.carve_targets)
            .get(scan_job_id)
            .cloned()
            .ok_or_else(|| HostError::new("jobNotFound", scan_job_id))
    }

    /// 深度恢复任务输出目录里的一个文件。只接受恢复时起的文件名，不接受其他路径。
    pub fn recovered_file(&self, job_id: &str, name: &str) -> Result<PathBuf, HostError> {
        if !is_recovered_name(name) {
            return Err(HostError::new("invalidRecoveredName", name));
        }
        let path = lock(&self.recovered_dirs)
            .get(job_id)
            .map(|dir| dir.join(name))
            .ok_or_else(|| HostError::new("recoveredNotFound", job_id))?;
        if path.is_file() {
            Ok(path)
        } else {
            Err(HostError::new(
                "recoveredNotFound",
                path.display().to_string(),
            ))
        }
    }

    /// 由引擎读取并生成预览：图片在引擎里解码，界面只拿到重新编码的缩略图。
    pub fn preview(&self, job_id: &str, name: &str) -> Result<FilePreview, HostError> {
        let path = self.recovered_file(job_id, name)?;
        query_with(
            PREVIEW_FILE,
            to_params(&PreviewParams {
                path: path.display().to_string(),
            })?,
        )
    }

    /// 浏览分区：引擎读出分区表后一直运行，直到取消（关闭浏览）。物理盘与扫描一样，第一次需要时提权启动磁盘代理。
    pub fn open_browse(
        &self,
        app: AppHandle,
        broker: &BrokerSession,
        job_id: String,
        target: Target,
    ) -> Result<BrowseOpened, HostError> {
        if let Target::Disk { disk_number } = &target {
            disk_path(*disk_number)?;
        }
        let params_job_id = job_id.clone();
        let opened =
            self.start_job_with(app, job_id, BROWSE_OPEN, BROWSE_TIMEOUT, |engine_pid| {
                to_params(&BrowseParams {
                    job_id: params_job_id,
                    source: resolve_source(target, broker, engine_pid)?,
                })
            })?;
        from_value(opened)
    }

    /// 列出浏览中的一个目录的一页。
    pub fn list_files(
        &self,
        job_id: String,
        partition: u32,
        dir: Option<String>,
        offset: u32,
        limit: u32,
    ) -> Result<FileListPage, HostError> {
        let params = to_params(&ListFilesParams {
            job_id: job_id.clone(),
            partition,
            dir,
            offset,
            limit,
        })?;
        from_value(
            self.running(&job_id)?
                .request(LIST_FILES, params, BROWSE_TIMEOUT)?,
        )
    }

    /// 预览浏览中的一个文件：与深度恢复的预览一样，图片在引擎里解码，界面只拿到缩略图。
    pub fn preview_fs_file(
        &self,
        job_id: String,
        partition: u32,
        file: String,
    ) -> Result<FilePreview, HostError> {
        let params = to_params(&PreviewFsParams {
            job_id: job_id.clone(),
            partition,
            file,
        })?;
        from_value(
            self.running(&job_id)?
                .request(PREVIEW_FS_FILE, params, BROWSE_TIMEOUT)?,
        )
    }

    /// 把分区里选中的项复制到另一块盘。源为物理磁盘时，引擎在开始前核对输出目录不在这块盘上。
    #[expect(clippy::too_many_arguments, reason = "与界面命令的参数一一对应")]
    pub fn start_copy(
        &self,
        app: AppHandle,
        broker: &BrokerSession,
        job_id: String,
        target: Target,
        partition: PartitionRef,
        items: Vec<CopyItem>,
        output_dir: String,
    ) -> Result<(), HostError> {
        if let Target::Disk { disk_number } = &target {
            disk_path(*disk_number)?;
        }
        let params_job_id = job_id.clone();
        let started = self.start_job_with(
            app,
            job_id.clone(),
            COPY_FILES,
            BROWSE_TIMEOUT,
            |engine_pid| {
                to_params(&CopyParams {
                    job_id: params_job_id,
                    source: resolve_source(target, broker, engine_pid)?,
                    partition,
                    items,
                    output_dir,
                })
            },
        )?;
        let CopyStarted { output_dir } = from_value(started)?;
        lock(&self.copied_dirs).insert(job_id, PathBuf::from(output_dir));
        Ok(())
    }

    /// 复制任务输出目录里的一个文件。`path` 是引擎报告的相对路径（以 `/` 分隔），
    /// 只接受普通的路径分量，解析后必须仍在输出目录之内。
    pub fn copied_file(&self, job_id: &str, path: &str) -> Result<PathBuf, HostError> {
        let invalid = || HostError::new("invalidCopiedPath", path);
        let relative = Path::new(path);
        if path.is_empty()
            || !relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(invalid());
        }
        let dir = lock(&self.copied_dirs)
            .get(job_id)
            .cloned()
            .ok_or_else(|| HostError::new("copiedNotFound", job_id))?;
        // 按分量重新拼，统一成本系统的分隔符
        let full = dir.join(relative.components().collect::<PathBuf>());
        let (Ok(real_dir), Ok(real_file)) = (dir.canonicalize(), full.canonicalize()) else {
            return Err(HostError::new("copiedNotFound", full.display().to_string()));
        };
        if !real_file.starts_with(&real_dir) {
            return Err(invalid());
        }
        if real_file.is_file() {
            Ok(full)
        } else {
            Err(HostError::new("copiedNotFound", full.display().to_string()))
        }
    }

    fn running(&self, job_id: &str) -> Result<Arc<EngineProcess>, HostError> {
        lock(&self.jobs)
            .get(job_id)
            .cloned()
            .ok_or_else(|| HostError::new("jobNotFound", job_id))
    }

    /// 启动引擎并交给它一个任务，返回引擎接下任务时的答复。`params` 拿到引擎的进程 ID，返回任务方法 `method` 的参数。
    fn start_job(
        &self,
        app: AppHandle,
        job_id: String,
        method: &str,
        params: impl FnOnce(u32) -> Result<Value, HostError>,
    ) -> Result<Value, HostError> {
        self.start_job_with(app, job_id, method, REQUEST_TIMEOUT, params)
    }

    /// 同 [`start_job`](Self::start_job)，等引擎接下任务最多 `timeout`。
    fn start_job_with(
        &self,
        app: AppHandle,
        job_id: String,
        method: &str,
        timeout: Duration,
        params: impl FnOnce(u32) -> Result<Value, HostError>,
    ) -> Result<Value, HostError> {
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

        let started =
            params(process.pid).and_then(|params| process.request(method, params, timeout));
        if started.is_err() {
            // 引擎没接下任务：关闭输入让它退出，并且不再为这个任务发事件
            process.abandon();
            lock(&self.jobs).remove(&job_id);
        }
        started
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
        process.request(CANCEL, params, REQUEST_TIMEOUT)?;
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

    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, HostError> {
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
        match receiver.recv_timeout(timeout) {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(HostError::new(engine_error_code(error.code), error.message)),
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
            COPIED => "copied",
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

/// 镜像直接给出路径；物理盘请磁盘代理把只读句柄复制进引擎（第一次需要时提权启动代理）。
fn resolve_source(
    target: Target,
    broker: &BrokerSession,
    engine_pid: u32,
) -> Result<Source, HostError> {
    match target {
        Target::Image { path } => Ok(Source::Image {
            path,
            sector_size: IMAGE_SECTOR_SIZE,
        }),
        Target::Disk { disk_number } => Ok(Source::Disk {
            handle: broker.open_disk(&disk_path(disk_number)?, engine_pid)?,
        }),
    }
}

/// 磁盘代理只接受 `\\.\PhysicalDriveN` 形式的路径。
fn disk_path(disk_number: u32) -> Result<String, HostError> {
    if disk_number > MAX_DISK_NUMBER {
        return Err(HostError::new("invalidDisk", disk_number.to_string()));
    }
    Ok(format!(r"\\.\PhysicalDrive{disk_number}"))
}

fn to_params(params: &impl Serialize) -> Result<Value, HostError> {
    serde_json::to_value(params)
        .map_err(|error| HostError::new("engineProtocol", error.to_string()))
}

/// 引擎的错误码对应的界面错误码；没有专门文案的统一为 `engineRejected`。
fn engine_error_code(code: i64) -> &'static str {
    match code {
        error_code::SOURCE_UNAVAILABLE => "sourceUnavailable",
        error_code::OUTPUT_UNAVAILABLE => "outputUnavailable",
        error_code::OUTPUT_ON_SOURCE => "outputOnSource",
        error_code::PREVIEW_UNAVAILABLE => "previewUnavailable",
        error_code::FS_UNAVAILABLE => "fsUnavailable",
        _ => "engineRejected",
    }
}

/// 临时启动一个引擎发一次查询，查完关闭它的输入，引擎随即退出。
fn query<T: DeserializeOwned>(method: &str) -> Result<T, HostError> {
    query_with(method, Value::Null)
}

fn query_with<T: DeserializeOwned>(method: &str, params: Value) -> Result<T, HostError> {
    let process = EngineProcess::spawn(None, || {})?;
    let result = process.request(method, params, REQUEST_TIMEOUT);
    process.abandon();
    from_value(result?)
}

fn from_value<T: DeserializeOwned>(value: Value) -> Result<T, HostError> {
    serde_json::from_value(value)
        .map_err(|error| HostError::new("engineProtocol", error.to_string()))
}

/// 双击就会被系统直接运行、或会改系统设置的文件类型。找回的文件来路不明，这些类型不交给系统打开，
/// 只能「在文件夹中显示」。
pub fn is_executable_type(path: &Path) -> bool {
    const EXECUTABLE: &[&str] = &[
        "exe",
        "com",
        "scr",
        "pif",
        "cpl",
        "msi",
        "msp",
        "msc",
        "bat",
        "cmd",
        "ps1",
        "psm1",
        "psd1",
        "vbs",
        "vbe",
        "js",
        "jse",
        "wsf",
        "wsh",
        "ws",
        "hta",
        "lnk",
        "url",
        "scf",
        "reg",
        "inf",
        "jar",
        "chm",
        "application",
        "appref-ms",
        "gadget",
        "settingcontent-ms",
        "library-ms",
    ];
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| EXECUTABLE.contains(&extension.to_ascii_lowercase().as_str()))
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

/// 恢复时起的文件名：`f`、十位起始扇区号、`.`、1 到 8 位小写字母或数字的扩展名，例如 `f0000012345.jpg`。
fn is_recovered_name(name: &str) -> bool {
    let Some((stem, extension)) = name.split_once('.') else {
        return false;
    };
    let Some(sector) = stem.strip_prefix('f') else {
        return false;
    };
    sector.len() == 10
        && sector.bytes().all(|byte| byte.is_ascii_digit())
        && (1..=8).contains(&extension.len())
        && extension
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_use_the_interface_wire_format() {
        let image: Target =
            serde_json::from_value(json!({"kind": "image", "path": "a.img"})).unwrap();
        assert!(matches!(image, Target::Image { path } if path == "a.img"));
        let disk: Target =
            serde_json::from_value(json!({"kind": "disk", "diskNumber": 3})).unwrap();
        assert!(matches!(disk, Target::Disk { disk_number: 3 }));
    }

    #[test]
    fn engine_errors_map_to_interface_codes() {
        assert_eq!(
            engine_error_code(error_code::OUTPUT_ON_SOURCE),
            "outputOnSource"
        );
        assert_eq!(
            engine_error_code(error_code::OUTPUT_UNAVAILABLE),
            "outputUnavailable"
        );
        assert_eq!(
            engine_error_code(error_code::SOURCE_UNAVAILABLE),
            "sourceUnavailable"
        );
        assert_eq!(engine_error_code(-32602), "engineRejected");
    }

    #[test]
    fn disk_numbers_are_bounded() {
        assert_eq!(disk_path(7).unwrap(), r"\\.\PhysicalDrive7");
        assert!(disk_path(MAX_DISK_NUMBER + 1).is_err());
    }

    #[test]
    fn only_names_given_by_the_recovery_are_accepted() {
        for good in ["f0000012345.jpg", "f0000000000.mp4", "f9999999999.mp4v2"] {
            assert!(is_recovered_name(good), "{good}");
        }
        for bad in [
            "",
            "f0000012345",
            "f000001234.jpg",
            "g0000012345.jpg",
            "f0000012345.JPG",
            "f0000012345.jpg.part",
            "f0000012345.",
            "../f0000012345.jpg",
            "f0000012345.verylongext",
        ] {
            assert!(!is_recovered_name(bad), "{bad}");
        }
    }

    #[test]
    fn unknown_jobs_and_bad_names_do_not_reach_the_disk() {
        let engines = Engines::default();
        let error = engines.recovered_file("job", "../secret.txt").unwrap_err();
        assert_eq!(error.code, "invalidRecoveredName");
        let error = engines
            .recovered_file("job", "f0000012345.jpg")
            .unwrap_err();
        assert_eq!(error.code, "recoveredNotFound");
    }

    #[test]
    fn copied_paths_stay_inside_the_output_directory() {
        let dir = std::env::temp_dir().join("yisi-host-copied-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Photos")).unwrap();
        std::fs::write(dir.join("Photos").join("a.jpg"), b"x").unwrap();
        let engines = Engines::default();
        lock(&engines.copied_dirs).insert("job".to_owned(), dir.clone());
        assert_eq!(
            engines.copied_file("job", "Photos/a.jpg").unwrap(),
            dir.join("Photos").join("a.jpg")
        );
        for bad in [
            "",
            "../x",
            "Photos/../../x",
            "/etc/passwd",
            "C:/Windows/notepad.exe",
        ] {
            assert_eq!(
                engines.copied_file("job", bad).unwrap_err().code,
                "invalidCopiedPath",
                "{bad}"
            );
        }
        assert_eq!(
            engines.copied_file("job", "Photos").unwrap_err().code,
            "copiedNotFound"
        );
        assert_eq!(
            engines
                .copied_file("other", "Photos/a.jpg")
                .unwrap_err()
                .code,
            "copiedNotFound"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn executable_types_are_not_opened() {
        for name in ["setup.EXE", "run.bat", "a.lnk", "x.ps1", "y.hta", "z.Js"] {
            assert!(is_executable_type(Path::new(name)), "{name}");
        }
        for name in ["photo.jpg", "doc.pdf", "noext", "archive.zip", "page.html"] {
            assert!(!is_executable_type(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn job_ids_are_restricted() {
        assert!(validate_job_id("2f1c9a3e-6b4d-4c55-9f0e-1a2b3c4d5e6f").is_ok());
        for bad in ["", "a b", "../x", "任务", &"x".repeat(65)] {
            assert!(validate_job_id(bad).is_err(), "{bad}");
        }
    }
}
