//! 消息循环：从宿主读请求、派发、写答复；后台发心跳；任务结束或宿主断开后退出。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use serde::Serialize;
use serde_json::{Value, json};
use yisi_carve::{DirectorySink, ScanOnlySink, Sink};
use yisi_engine::protocol::{
    self, BROWSE_OPEN, BrowseParams, CANCEL, CANCEL_SCAN, CAPABILITIES, CARVE_EXTRACT, CARVE_START,
    COPY_FILES, Capabilities, CarveParams, CarveStarted, CopyParams, CopyStarted, ExtractParams,
    FS_CHECK, FsCheckParams, HEARTBEAT, HEARTBEAT_INTERVAL, IMAGE_CREATE, IMAGE_DESCRIBE,
    IMAGE_INSPECT, IMAGE_RESUME, ImageDescribeParams, ImageInspectParams, ImageParams, JobRef,
    LIST_DISKS, LIST_FILE_TYPES, LIST_FILES, ListFilesParams, PAUSE, PREVIEW_FILE, PREVIEW_FS_FILE,
    PREVIEW_RANGE, PreviewFsParams, PreviewParams, PreviewRangeParams, READ_SCAN, RESUME,
    ReadScanParams, SCAN_DELETED, ScanParams, Source,
};
use yisi_engine::protocol::{DIAGNOSE_BOOT, DiagnoseBootParams};
#[cfg(windows)]
use yisi_engine::protocol::{DiskList, DiskSummary, DiskVolume};
use yisi_engine::protocol::{OPEN_RANGE, OpenRangeParams, SEARCH_PARTITIONS, SearchParams};
use yisi_formats::Registry;
use yisi_io::{BlockDevice, RawImage};
use yisi_rpc::{
    ErrorObject, FrameError, Message, Notification, Request, Response, error_code, read_message,
    write_message,
};

use crate::browse::{self, Session};
use crate::copy_job::{self, Planned};
use crate::job::{self, JobContext, JobOutcome, ProgressSender, RunningJob};
use crate::{
    boot_check, carve_job, extract_job, fs_check_job, image_inspect, image_job, preview, read_scan,
    reading, search_job,
};

/// 浏览任务每隔这么久看一次有没有被取消。
const BROWSE_POLL: std::time::Duration = std::time::Duration::from_millis(100);
/// 一次复制最多选这么多项、路径最多这么多级。
const MAX_COPY_ITEMS: usize = 100_000;
const MAX_COPY_DEPTH: usize = 64;
/// 一次最多查看这么多个以前做过的镜像。
const MAX_DESCRIBE_IMAGES: usize = 100;

/// 向宿主发消息。答复、通知可能来自不同线程，写入时加锁，保证一帧不被拆开。
pub struct Outbox {
    writer: Mutex<Box<dyn Write + Send>>,
}

impl Outbox {
    pub fn new(writer: impl Write + Send + 'static) -> Self {
        Self {
            writer: Mutex::new(Box::new(writer)),
        }
    }

    pub fn send(&self, message: &Message) {
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(error) = write_message(&mut *writer, message) {
            tracing::warn!(%error, "向宿主发送消息失败");
        }
    }

    pub fn notify(&self, method: &str, params: impl Serialize) {
        match serde_json::to_value(params) {
            Ok(params) => self.send(&Message::Notification(Notification::new(method, params))),
            Err(error) => tracing::warn!(%error, method, "通知参数无法编码"),
        }
    }
}

enum Event {
    Incoming(Message),
    InputClosed,
    JobFinished,
}

/// 处理宿主的请求，直到任务结束；宿主断开时，有任务就先取消、等它结束再退出。
pub fn serve(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) {
    let outbox = Arc::new(Outbox::new(writer));
    let (events, inbox) = mpsc::channel();
    spawn_reader(reader, events.clone());
    let stop_heartbeat = spawn_heartbeat(Arc::clone(&outbox));

    let mut state = State::default();
    for event in inbox {
        match event {
            Event::Incoming(Message::Request(request)) => {
                if let Some(outcome) = dispatch(&request, &mut state, &outbox, &events) {
                    outbox.send(&Message::Response(Response {
                        id: request.id,
                        outcome,
                    }));
                }
            }
            // 宿主只发请求，通知和响应都不处理
            Event::Incoming(_) => {}
            Event::InputClosed => match &state.job {
                Some(running) => running.cancel(),
                None => break,
            },
            Event::JobFinished => break,
        }
    }
    stop_heartbeat.store(true, Ordering::Relaxed);
}

/// 每个引擎进程只运行一个任务；浏览任务另外记着会话，供列目录、预览使用。
#[derive(Default)]
struct State {
    job: Option<RunningJob>,
    session: Option<Arc<Session>>,
}

impl State {
    /// 每个引擎进程只运行一个任务：已有任务时拒绝。
    fn ensure_idle(&self) -> Result<(), ErrorObject> {
        if self.job.is_some() {
            return Err(ErrorObject::new(
                protocol::error_code::JOB_ALREADY_RUNNING,
                "每个引擎进程只运行一个任务",
            ));
        }
        Ok(())
    }
}

/// 启动任务：运行期间每秒报一次读源的统计，结束时通知事件循环收尾。
fn start_job<W>(job_id: String, outbox: &Arc<Outbox>, events: &Sender<Event>, work: W) -> RunningJob
where
    W: FnOnce(&JobContext) -> Result<JobOutcome, String> + Send + 'static,
{
    let finished = events.clone();
    let stop_reading = reading::spawn_reporter(job_id.clone(), Arc::clone(outbox));
    job::start(job_id, Arc::clone(outbox), work, move || {
        stop_reading.store(true, Ordering::Relaxed);
        let _ = finished.send(Event::JobFinished);
    })
}

/// 处理一个请求。返回 `None` 表示答复由后台线程稍后发出：列目录、预览、扫描可能要读很多数据，不能挡住取消请求。
fn dispatch(
    request: &Request,
    state: &mut State,
    outbox: &Arc<Outbox>,
    events: &Sender<Event>,
) -> Option<Result<Value, ErrorObject>> {
    match request.method.as_str() {
        LIST_FILES | PREVIEW_FS_FILE | SCAN_DELETED | OPEN_RANGE => {
            let session = match session_for(request, state) {
                Ok(session) => session,
                Err(error) => return Some(Err(error)),
            };
            let request = request.clone();
            let outbox = Arc::clone(outbox);
            thread::spawn(move || {
                let outcome = match request.method.as_str() {
                    LIST_FILES => request
                        .params::<ListFilesParams>()
                        .and_then(|params| session.list(&params))
                        .and_then(|page| to_result(&page)),
                    SCAN_DELETED => request
                        .params::<ScanParams>()
                        .and_then(|params| session.scan(params.partition))
                        .and_then(|result| to_result(&result)),
                    OPEN_RANGE => request
                        .params::<OpenRangeParams>()
                        .and_then(|params| session.open_range(&params))
                        .and_then(|info| to_result(&info)),
                    _ => request
                        .params::<PreviewFsParams>()
                        .and_then(|params| session.preview(params.partition, &params.file))
                        .and_then(|preview| to_result(&preview)),
                };
                outbox.send(&Message::Response(Response {
                    id: request.id,
                    outcome,
                }));
            });
            None
        }
        CANCEL_SCAN => Some(session_for(request, state).and_then(|session| {
            let ScanParams { partition, .. } = request.params()?;
            session.cancel_scan(partition);
            Ok(json!({}))
        })),
        _ => Some(dispatch_now(request, state, outbox, events)),
    }
}

/// 列目录、预览、扫描请求所属的浏览会话：任务 ID 要与正在运行的浏览任务一致。
fn session_for(request: &Request, state: &State) -> Result<Arc<Session>, ErrorObject> {
    let JobRef { job_id } = request.params()?;
    match (&state.job, &state.session) {
        (Some(running), Some(session)) if running.id() == job_id => Ok(Arc::clone(session)),
        _ => Err(ErrorObject::new(
            protocol::error_code::JOB_NOT_FOUND,
            format!("没有正在浏览的任务 {job_id}"),
        )),
    }
}

fn dispatch_now(
    request: &Request,
    state: &mut State,
    outbox: &Arc<Outbox>,
    events: &Sender<Event>,
) -> Result<Value, ErrorObject> {
    match request.method.as_str() {
        CAPABILITIES => {
            let mut methods = vec![
                READ_SCAN.to_owned(),
                CANCEL.to_owned(),
                PAUSE.to_owned(),
                RESUME.to_owned(),
                LIST_FILE_TYPES.to_owned(),
                CARVE_START.to_owned(),
                PREVIEW_FILE.to_owned(),
                CARVE_EXTRACT.to_owned(),
                PREVIEW_RANGE.to_owned(),
                BROWSE_OPEN.to_owned(),
                LIST_FILES.to_owned(),
                PREVIEW_FS_FILE.to_owned(),
                SCAN_DELETED.to_owned(),
                CANCEL_SCAN.to_owned(),
                COPY_FILES.to_owned(),
                IMAGE_CREATE.to_owned(),
                IMAGE_RESUME.to_owned(),
                IMAGE_INSPECT.to_owned(),
                IMAGE_DESCRIBE.to_owned(),
                SEARCH_PARTITIONS.to_owned(),
                OPEN_RANGE.to_owned(),
                DIAGNOSE_BOOT.to_owned(),
                FS_CHECK.to_owned(),
            ];
            if cfg!(windows) {
                methods.push(LIST_DISKS.to_owned());
            }
            to_result(&Capabilities {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                methods,
            })
        }
        #[cfg(windows)]
        LIST_DISKS => to_result(&disk_list()),
        READ_SCAN => {
            state.ensure_idle()?;
            let params: ReadScanParams = request.params()?;
            let device = open_source(&params.source)?;
            state.job = Some(start_job(params.job_id, outbox, events, move |context| {
                read_scan::run(device.as_ref(), context)
            }));
            Ok(json!({}))
        }
        LIST_FILE_TYPES => to_result(&carve_job::file_types(&Registry::builtin())),
        SEARCH_PARTITIONS => {
            state.ensure_idle()?;
            let params: SearchParams = request.params()?;
            let device: Arc<dyn BlockDevice> = Arc::from(open_source(&params.source)?);
            state.job = Some(
                start_job(params.job_id, outbox, events, move |context| {
                    search_job::run(device, context)
                })
                .pausable(),
            );
            Ok(json!({}))
        }
        CARVE_START => {
            state.ensure_idle()?;
            let params: CarveParams = request.params()?;
            let registry = Registry::builtin();
            let enabled = carve_job::enabled_formats(&registry, params.extensions.as_deref())
                .map_err(|extension| {
                    ErrorObject::new(
                        error_code::INVALID_PARAMS,
                        format!("不认识的格式 {extension}"),
                    )
                })?;
            // 不给输出目录时只扫描：什么都不写，也就不用核对输出盘
            let output_dir = params
                .output_dir
                .as_ref()
                .map(|dir| Path::new(dir).join(format!("recovered-{}", short_id(&params.job_id))));
            let (device, mut sink): (Box<dyn BlockDevice>, Box<dyn Sink + Send>) = match &output_dir
            {
                Some(dir) => {
                    let device = open_source_for_output(&params.source, dir)?;
                    (
                        device,
                        Box::new(
                            DirectorySink::new(dir)
                                .map_err(|error| output_unavailable(dir, &error))?,
                        ),
                    )
                }
                None => (open_source(&params.source)?, Box::new(ScanOnlySink)),
            };
            let started = CarveStarted {
                output_dir: output_dir.as_ref().map(|dir| dir.display().to_string()),
            };
            state.job = Some(
                start_job(params.job_id, outbox, events, move |context| {
                    carve_job::run(
                        device.as_ref(),
                        &registry,
                        enabled,
                        sink.as_mut(),
                        output_dir.as_deref(),
                        context,
                    )
                })
                .pausable(),
            );
            to_result(&started)
        }
        CARVE_EXTRACT => {
            state.ensure_idle()?;
            let params: ExtractParams = request.params()?;
            let registry = Registry::builtin();
            check_extract_items(&registry, &params)?;
            // 另存为：直接存进用户选的目录，不另建子目录
            let output_dir = PathBuf::from(&params.output_dir);
            let device = open_source_for_output(&params.source, &output_dir)?;
            std::fs::create_dir_all(&output_dir)
                .map_err(|error| output_unavailable(&output_dir, &error))?;
            let started = CopyStarted {
                output_dir: output_dir.display().to_string(),
            };
            let items = params.items;
            state.job = Some(start_job(params.job_id, outbox, events, move |context| {
                extract_job::run(device.as_ref(), &registry, items, &output_dir, context)
            }));
            to_result(&started)
        }
        DIAGNOSE_BOOT => {
            let params: DiagnoseBootParams = request.params()?;
            let device: Arc<dyn BlockDevice> = Arc::from(open_source(&params.source)?);
            to_result(&boot_check::diagnose(&device)?)
        }
        PREVIEW_RANGE => {
            let params: PreviewRangeParams = request.params()?;
            let device = open_source(&params.source)?;
            let preview = preview::preview_range(device.as_ref(), params.offset, params.length)
                .map_err(|error| {
                    ErrorObject::new(
                        protocol::error_code::PREVIEW_UNAVAILABLE,
                        format!("无法读取偏移 {} 处的文件：{error}", params.offset),
                    )
                })?;
            to_result(&preview)
        }
        BROWSE_OPEN => {
            state.ensure_idle()?;
            let params: BrowseParams = request.params()?;
            let device: Arc<dyn BlockDevice> = Arc::from(open_source(&params.source)?);
            let progress = ProgressSender::new(params.job_id.clone(), Arc::clone(outbox));
            let (session, opened) =
                Session::open(device, progress, boot_check::fs_copy(params.boot))?;
            let session = Arc::new(session);
            state.session = Some(Arc::clone(&session));
            // 浏览任务本身只等取消或宿主断开；列目录、预览、扫描由会话在别的线程里答复
            state.job = Some(start_job(params.job_id, outbox, events, move |context| {
                while !context.is_cancelled() {
                    thread::sleep(BROWSE_POLL);
                }
                session.close();
                Ok(JobOutcome::Cancelled)
            }));
            to_result(&opened)
        }
        COPY_FILES => {
            state.ensure_idle()?;
            let params: CopyParams = request.params()?;
            let items = copy_items(&params)?;
            let output_dir = Path::new(&params.output_dir)
                .join(format!("restored-{}", short_id(&params.job_id)));
            let device: Arc<dyn BlockDevice> =
                Arc::from(open_source_for_output(&params.source, &output_dir)?);
            let volume = copy_volume(device, params.partition, params.boot)?;
            std::fs::create_dir_all(&output_dir)
                .map_err(|error| output_unavailable(&output_dir, &error))?;
            let started = CopyStarted {
                output_dir: output_dir.display().to_string(),
            };
            state.job = Some(start_job(params.job_id, outbox, events, move |context| {
                copy_job::run(&volume, items, &output_dir, context)
            }));
            to_result(&started)
        }
        IMAGE_CREATE | IMAGE_RESUME => {
            state.ensure_idle()?;
            let params: ImageParams = request.params()?;
            let image_path = image_path_param(&params.image_path)?;
            let (device, identity) = open_image_source(&params.source, image_path)?;
            // 确认镜像不在源盘上之后，才创建还不存在的上级目录
            if let Some(parent) = image_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| output_unavailable(parent, &error))?;
            }
            let image_request = image_job::ImageRequest {
                ranges: params.ranges,
                reset: params.reset,
                copy_only: params.copy_only,
            };
            let prepared = if request.method == IMAGE_CREATE {
                image_job::create(device, image_path, identity, &image_request)?
            } else {
                image_job::resume(device, image_path, identity, &image_request)?
            };
            let started = prepared.started();
            state.job = Some(start_job(params.job_id, outbox, events, move |context| {
                image_job::run(prepared, context)
            }));
            to_result(&started)
        }
        IMAGE_INSPECT => {
            let params: ImageInspectParams = request.params()?;
            let image_path = image_path_param(&params.image_path)?;
            let (device, identity) = open_image_source(&params.source, image_path)?;
            to_result(&image_inspect::inspect(
                Arc::from(device),
                image_path,
                &identity,
            ))
        }
        IMAGE_DESCRIBE => {
            let params: ImageDescribeParams = request.params()?;
            if params.image_paths.len() > MAX_DESCRIBE_IMAGES {
                return Err(ErrorObject::new(
                    error_code::INVALID_PARAMS,
                    format!("一次最多查看 {MAX_DESCRIBE_IMAGES} 个镜像"),
                ));
            }
            to_result(&image_inspect::describe(&params.image_paths))
        }
        FS_CHECK => {
            state.ensure_idle()?;
            let params: FsCheckParams = request.params()?;
            let device = open_source(&params.source)?;
            browse::verify_partition(device.as_ref(), params.partition)?;
            let partition = params.partition;
            state.job = Some(
                start_job(params.job_id, outbox, events, move |context| {
                    fs_check_job::run(device.as_ref(), partition, context)
                })
                .pausable(),
            );
            Ok(json!({}))
        }
        PREVIEW_FILE => {
            let PreviewParams { path } = request.params()?;
            let preview = preview::preview(Path::new(&path)).map_err(|error| {
                ErrorObject::new(
                    protocol::error_code::PREVIEW_UNAVAILABLE,
                    format!("无法读取 {path}：{error}"),
                )
            })?;
            to_result(&preview)
        }
        CANCEL => {
            let JobRef { job_id } = request.params()?;
            match &state.job {
                Some(running) if running.id() == job_id => {
                    running.cancel();
                    Ok(json!({}))
                }
                _ => Err(ErrorObject::new(
                    protocol::error_code::JOB_NOT_FOUND,
                    format!("没有正在运行的任务 {job_id}"),
                )),
            }
        }
        PAUSE | RESUME => {
            let JobRef { job_id } = request.params()?;
            match &state.job {
                Some(running) if running.id() == job_id => {
                    if !running.is_pausable() {
                        return Err(ErrorObject::new(
                            error_code::INVALID_PARAMS,
                            format!("任务 {job_id} 不能暂停"),
                        ));
                    }
                    running.set_paused(request.method == PAUSE);
                    Ok(json!({}))
                }
                _ => Err(ErrorObject::new(
                    protocol::error_code::JOB_NOT_FOUND,
                    format!("没有正在运行的任务 {job_id}"),
                )),
            }
        }
        other => Err(ErrorObject::new(
            error_code::METHOD_NOT_FOUND,
            format!("引擎没有方法 {other}"),
        )),
    }
}

/// 镜像路径参数：要是含文件名的完整路径。
fn image_path_param(path: &str) -> Result<&Path, ErrorObject> {
    let image_path = Path::new(path);
    if !image_path.is_absolute() || image_path.file_name().is_none() {
        return Err(ErrorObject::new(
            error_code::INVALID_PARAMS,
            format!("镜像应为含文件名的完整路径：{path}"),
        ));
    }
    Ok(image_path)
}

/// 打开源。打开的源一律经读取计数器（[`reading::metered`]），任务运行期间据此报告读得怎样。
fn open_source(source: &Source) -> Result<Box<dyn BlockDevice>, ErrorObject> {
    match source {
        Source::Image { path, sector_size } => RawImage::open(path, *sector_size)
            .map(|image| reading::metered(Box::new(image)))
            .map_err(|error| {
                ErrorObject::new(
                    protocol::error_code::SOURCE_UNAVAILABLE,
                    error_chain(&error),
                )
            }),
        #[cfg(windows)]
        Source::Disk { handle } | Source::Volume { handle, .. } => {
            adopt_disk(*handle).map(|disk| reading::metered(Box::new(disk)))
        }
        #[cfg(not(windows))]
        Source::Disk { .. } | Source::Volume { .. } => Err(ErrorObject::new(
            protocol::error_code::SOURCE_UNAVAILABLE,
            "当前平台还不支持扫描物理磁盘",
        )),
    }
}

/// 打开要往外写结果的任务（深度恢复、复制）的源。物理盘要先确认输出目录不在这块盘上，
/// 而且必须在创建任何输出目录之前确认：单是建目录，就已经写到了这块盘上。
#[cfg_attr(not(windows), expect(unused_variables))]
fn open_source_for_output(
    source: &Source,
    output_dir: &Path,
) -> Result<Box<dyn BlockDevice>, ErrorObject> {
    match source {
        #[cfg(windows)]
        Source::Disk { handle } => {
            let disk = adopt_disk(*handle)?;
            ensure_output_elsewhere(
                disk.device_number(),
                yisi_io::volume_disk_numbers(output_dir),
            )?;
            Ok(reading::metered(Box::new(disk)))
        }
        #[cfg(windows)]
        Source::Volume { handle, letter } => {
            let letter = letter_param(letter)?;
            let disk = adopt_disk(*handle)?;
            ensure_output_off_volume(letter, output_dir)?;
            Ok(reading::metered(Box::new(disk)))
        }
        // 其他平台还打不开物理盘，open_source 直接拒绝，走不到写输出那一步
        _ => open_source(source),
    }
}

/// 打开要做镜像的源，给出它的标识（续做时据此确认是同一个源）。物理盘先确认镜像不在这块盘上；
/// 镜像文件不能把自己或自己的坏区图当作输出。
fn open_image_source(
    source: &Source,
    image_path: &Path,
) -> Result<(Box<dyn BlockDevice>, String), ErrorObject> {
    match source {
        Source::Image { path, .. } => {
            let device = open_source(source)?;
            let real_source = std::fs::canonicalize(path).map_err(|error| {
                ErrorObject::new(
                    protocol::error_code::SOURCE_UNAVAILABLE,
                    format!("无法确认镜像文件 {path} 的位置：{error}"),
                )
            })?;
            let outputs = [image_path.to_path_buf(), image_job::map_path(image_path)];
            if outputs
                .iter()
                .any(|output| std::fs::canonicalize(output).is_ok_and(|real| real == real_source))
            {
                return Err(ErrorObject::new(
                    error_code::INVALID_PARAMS,
                    format!("镜像不能写到来源文件 {path} 上"),
                ));
            }
            Ok((device, format!("image {}", real_source.display())))
        }
        #[cfg(windows)]
        Source::Disk { handle } => {
            let disk = adopt_disk(*handle)?;
            ensure_output_elsewhere(
                disk.device_number(),
                yisi_io::volume_disk_numbers(image_path),
            )?;
            let identity = disk_identity(&disk);
            Ok((reading::metered(Box::new(disk)), identity))
        }
        #[cfg(windows)]
        Source::Volume { handle, letter } => {
            let letter = letter_param(letter)?;
            let disk = adopt_disk(*handle)?;
            ensure_output_off_volume(letter, image_path)?;
            let guid = yisi_io::volume_guid(yisi_io::volume_root(letter)).map_err(|error| {
                ErrorObject::new(
                    protocol::error_code::SOURCE_UNAVAILABLE,
                    format!("查不到 {letter}: 是哪个卷：{error}"),
                )
            })?;
            Ok((reading::metered(Box::new(disk)), volume_identity(&guid)))
        }
        #[cfg(not(windows))]
        Source::Disk { .. } | Source::Volume { .. } => {
            open_source(source).map(|device| (device, "disk".to_owned()))
        }
    }
}

/// 物理盘的标识：厂商、型号、序列号。磁盘编号不算在内，同一块盘重新插拔后编号可能变。
#[cfg(windows)]
fn disk_identity(disk: &yisi_io::WindowsDisk) -> String {
    let info = disk.device_number().ok().and_then(|number| {
        yisi_io::list_disks()
            .into_iter()
            .find(|info| info.number == number)
    });
    identity_of(info.as_ref())
}

/// 由磁盘属性得出标识；查不到属性时是 `disk`。列出磁盘时给界面的标识也用它，与坏区图里记的一致。
#[cfg(windows)]
fn identity_of(info: Option<&yisi_io::DiskInfo>) -> String {
    let words = match info {
        Some(info) => format!("disk {} {} {}", info.vendor, info.model, info.serial),
        None => "disk".to_owned(),
    };
    words.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 输出目录建不起来时的答复。
fn output_unavailable(dir: &Path, error: &std::io::Error) -> ErrorObject {
    ErrorObject::new(
        protocol::error_code::OUTPUT_UNAVAILABLE,
        format!("无法创建输出目录 {}：{error}", dir.display()),
    )
}

/// 保存项：数量有上限，格式要认识，长度不能为 0、不能超过该格式的最大尺寸。
fn check_extract_items(registry: &Registry, params: &ExtractParams) -> Result<(), ErrorObject> {
    let invalid = |message: String| ErrorObject::new(error_code::INVALID_PARAMS, message);
    if params.items.is_empty() || params.items.len() > MAX_COPY_ITEMS {
        return Err(invalid(format!("保存项应为 1 到 {MAX_COPY_ITEMS} 个")));
    }
    for item in &params.items {
        let format = registry
            .position(&item.extension)
            .and_then(|position| registry.formats().get(position))
            .ok_or_else(|| invalid(format!("不认识的格式 {}", item.extension)))?;
        if item.length == 0 || item.length > format.info().max_size {
            return Err(invalid(format!(
                "偏移 {} 处的 {} 长度 {} 不合理",
                item.offset, item.extension, item.length
            )));
        }
    }
    Ok(())
}

/// 解码复制项的定位信息，限制数量与路径层级。
fn copy_items(params: &CopyParams) -> Result<Vec<Planned>, ErrorObject> {
    let invalid = |message: String| ErrorObject::new(error_code::INVALID_PARAMS, message);
    if params.items.is_empty() || params.items.len() > MAX_COPY_ITEMS {
        return Err(invalid(format!("复制项应为 1 到 {MAX_COPY_ITEMS} 个")));
    }
    params
        .items
        .iter()
        .map(|item| {
            if item.path.is_empty() || item.path.len() > MAX_COPY_DEPTH {
                return Err(invalid(format!("路径层级应为 1 到 {MAX_COPY_DEPTH} 级")));
            }
            Ok(Planned {
                file: browse::decode(&item.file)?,
                path: item.path.clone(),
            })
        })
        .collect()
}

/// 核对浏览时看到的分区还在原处（[`browse::verify_partition`]），再按浏览时那一份引导扇区打开它上面的卷。
fn copy_volume(
    device: Arc<dyn BlockDevice>,
    partition: protocol::PartitionRef,
    boot: protocol::BootCopy,
) -> Result<yisi_fs::Volume, ErrorObject> {
    browse::verify_partition(device.as_ref(), partition)?;
    yisi_fs::Volume::open_with(
        device,
        partition.offset,
        partition.length,
        boot_check::fs_copy(boot),
    )
    .map_err(|error| ErrorObject::new(protocol::error_code::FS_UNAVAILABLE, error_chain(&error)))
}

#[cfg(windows)]
fn adopt_disk(handle: u64) -> Result<yisi_io::WindowsDisk, ErrorObject> {
    yisi_io::WindowsDisk::adopt_handle(handle).map_err(|error| {
        ErrorObject::new(
            protocol::error_code::SOURCE_UNAVAILABLE,
            error_chain(&error),
        )
    })
}

/// 盘符参数：一个英文字母，统一成大写。
#[cfg(windows)]
fn letter_param(letter: &str) -> Result<char, ErrorObject> {
    let mut chars = letter.chars();
    match (chars.next(), chars.next()) {
        (Some(first), None) if first.is_ascii_alphabetic() => Ok(first.to_ascii_uppercase()),
        _ => Err(ErrorObject::new(
            error_code::INVALID_PARAMS,
            format!("盘符应为一个字母：{letter}"),
        )),
    }
}

/// 卷的标识：卷的 GUID 名。列出磁盘时给界面的标识也用它，与坏区图里记的一致。
#[cfg(windows)]
fn volume_identity(guid: &str) -> String {
    format!("volume {guid}")
}

/// 源是一个卷时，输出不能写到这个卷上（规则见 [`yisi_io::ensure_off_volume`]）。
#[cfg(windows)]
fn ensure_output_off_volume(letter: char, output: &Path) -> Result<(), ErrorObject> {
    let query = |what: &'static str| move |source| yisi_io::IoError::Query { what, source };
    yisi_io::ensure_off_volume(
        letter,
        yisi_io::volume_guid(yisi_io::volume_root(letter)).map_err(query("源卷")),
        yisi_io::volume_guid(output).map_err(query("输出位置所在的卷")),
    )
    .map_err(|error| ErrorObject::new(protocol::error_code::OUTPUT_ON_SOURCE, error_chain(&error)))
}

/// 输出不能写到源盘上（规则见 [`yisi_io::ensure_off_source`]）。
#[cfg(windows)]
fn ensure_output_elsewhere(
    source_disk: Result<u32, yisi_io::IoError>,
    output_disks: Result<Vec<u32>, yisi_io::IoError>,
) -> Result<(), ErrorObject> {
    yisi_io::ensure_off_source(source_disk, output_disks).map_err(|error| {
        ErrorObject::new(protocol::error_code::OUTPUT_ON_SOURCE, error_chain(&error))
    })
}

#[cfg(windows)]
fn disk_list() -> DiskList {
    use yisi_io::BusType;
    let volumes = yisi_io::mounted_volumes();
    let disks = yisi_io::list_disks()
        .into_iter()
        .map(|disk| DiskSummary {
            identity: identity_of(Some(&disk)),
            solid_state: disk.solid_state,
            partition_style: disk.partition_style.map(|style| {
                match style {
                    yisi_io::PartitionStyle::Mbr => "mbr",
                    yisi_io::PartitionStyle::Gpt => "gpt",
                    yisi_io::PartitionStyle::Raw => "raw",
                }
                .to_owned()
            }),
            volumes: volumes
                .iter()
                .filter(|volume| volume.disks.contains(&disk.number))
                .map(|volume| DiskVolume {
                    letter: volume.letter.to_string(),
                    identity: volume_identity(&volume.guid),
                    label: volume.info.label.clone(),
                    file_system: volume.info.file_system.clone(),
                    total_bytes: volume.info.total_bytes,
                    free_bytes: volume.info.free_bytes,
                })
                .collect(),
            number: disk.number,
            path: disk.path,
            size: disk.size,
            sector_size: disk.sector_size,
            vendor: disk.vendor,
            model: disk.model,
            serial: disk.serial,
            bus: match disk.bus {
                BusType::Ata => "ata",
                BusType::Sata => "sata",
                BusType::Scsi => "scsi",
                BusType::Sas => "sas",
                BusType::Nvme => "nvme",
                BusType::Usb => "usb",
                BusType::Sd => "sd",
                BusType::Mmc => "mmc",
                BusType::Virtual => "virtual",
                BusType::Other => "other",
            }
            .to_owned(),
            removable: disk.removable,
        })
        .collect();
    DiskList { disks }
}

/// 输出子目录名里用的任务 ID：只取字母、数字和连字符，最多 8 个；一个也没有时用 `job`。
pub(crate) fn short_id(job_id: &str) -> String {
    let id: String = job_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
        .take(8)
        .collect();
    if id.is_empty() { "job".to_owned() } else { id }
}

fn to_result(value: &impl Serialize) -> Result<Value, ErrorObject> {
    serde_json::to_value(value).map_err(|error| {
        ErrorObject::new(error_code::INTERNAL_ERROR, format!("无法编码答复：{error}"))
    })
}

fn spawn_reader(mut reader: impl Read + Send + 'static, events: Sender<Event>) {
    thread::spawn(move || {
        loop {
            let event = match read_message(&mut reader) {
                Ok(message) => Event::Incoming(message),
                Err(FrameError::Closed) => Event::InputClosed,
                Err(error) => {
                    tracing::warn!(%error, "读取宿主消息失败，按断开处理");
                    Event::InputClosed
                }
            };
            let closed = matches!(event, Event::InputClosed);
            if events.send(event).is_err() || closed {
                return;
            }
        }
    });
}

fn spawn_heartbeat(outbox: Arc<Outbox>) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    thread::spawn(move || {
        while !stopped.load(Ordering::Relaxed) {
            outbox.send(&Message::Notification(Notification::new(
                HEARTBEAT,
                Value::Null,
            )));
            thread::sleep(HEARTBEAT_INTERVAL);
        }
    });
    stop
}

pub(crate) use yisi_rpc::error_chain;
