//! 消息循环：从宿主读请求、派发、写答复；后台发心跳；任务结束或宿主断开后退出。

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use serde::Serialize;
use serde_json::{Value, json};
use yisi_carve::DirectorySink;
use yisi_engine::protocol::{
    self, CANCEL, CAPABILITIES, CARVE_START, Capabilities, CarveParams, HEARTBEAT,
    HEARTBEAT_INTERVAL, JobRef, LIST_DISKS, LIST_FILE_TYPES, READ_SCAN, ReadScanParams, Source,
};
#[cfg(windows)]
use yisi_engine::protocol::{DiskList, DiskSummary};
use yisi_formats::Registry;
use yisi_io::{BlockDevice, RawImage};
use yisi_rpc::{
    ErrorObject, FrameError, Message, Notification, Request, Response, error_code, read_message,
    write_message,
};

use crate::job::{self, RunningJob};
use crate::{carve_job, read_scan};

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

    let mut job: Option<RunningJob> = None;
    for event in inbox {
        match event {
            Event::Incoming(Message::Request(request)) => {
                let outcome = dispatch(&request, &mut job, &outbox, &events);
                outbox.send(&Message::Response(Response {
                    id: request.id,
                    outcome,
                }));
            }
            // 宿主只发请求，通知和响应都不处理
            Event::Incoming(_) => {}
            Event::InputClosed => match &job {
                Some(running) => running.cancel(),
                None => break,
            },
            Event::JobFinished => break,
        }
    }
    stop_heartbeat.store(true, Ordering::Relaxed);
}

fn dispatch(
    request: &Request,
    job: &mut Option<RunningJob>,
    outbox: &Arc<Outbox>,
    events: &Sender<Event>,
) -> Result<Value, ErrorObject> {
    match request.method.as_str() {
        CAPABILITIES => {
            let mut methods = vec![
                READ_SCAN.to_owned(),
                CANCEL.to_owned(),
                LIST_FILE_TYPES.to_owned(),
                CARVE_START.to_owned(),
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
            if job.is_some() {
                return Err(ErrorObject::new(
                    protocol::error_code::JOB_ALREADY_RUNNING,
                    "每个引擎进程只运行一个任务",
                ));
            }
            let params: ReadScanParams = request.params()?;
            let device = open_source(&params.source)?;
            let finished = events.clone();
            *job = Some(job::start(
                params.job_id,
                Arc::clone(outbox),
                move |context| read_scan::run(device.as_ref(), context),
                move || {
                    let _ = finished.send(Event::JobFinished);
                },
            ));
            Ok(json!({}))
        }
        LIST_FILE_TYPES => to_result(&carve_job::file_types(&Registry::builtin())),
        CARVE_START => {
            if job.is_some() {
                return Err(ErrorObject::new(
                    protocol::error_code::JOB_ALREADY_RUNNING,
                    "每个引擎进程只运行一个任务",
                ));
            }
            let params: CarveParams = request.params()?;
            let registry = Registry::builtin();
            let enabled = carve_job::enabled_formats(&registry, params.extensions.as_deref())
                .map_err(|extension| {
                    ErrorObject::new(
                        error_code::INVALID_PARAMS,
                        format!("不认识的格式 {extension}"),
                    )
                })?;
            let output_dir = Path::new(&params.output_dir)
                .join(format!("recovered-{}", short_id(&params.job_id)));
            let device = open_carve_source(&params.source, &output_dir)?;
            let sink = DirectorySink::new(&output_dir).map_err(|error| {
                ErrorObject::new(
                    protocol::error_code::OUTPUT_UNAVAILABLE,
                    format!("无法创建输出目录 {}：{error}", output_dir.display()),
                )
            })?;
            let finished = events.clone();
            *job = Some(job::start(
                params.job_id,
                Arc::clone(outbox),
                move |context| {
                    carve_job::run(
                        device.as_ref(),
                        &registry,
                        enabled,
                        sink,
                        &output_dir,
                        context,
                    )
                },
                move || {
                    let _ = finished.send(Event::JobFinished);
                },
            ));
            Ok(json!({}))
        }
        CANCEL => {
            let JobRef { job_id } = request.params()?;
            match job {
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
        other => Err(ErrorObject::new(
            error_code::METHOD_NOT_FOUND,
            format!("引擎没有方法 {other}"),
        )),
    }
}

fn open_source(source: &Source) -> Result<Box<dyn BlockDevice>, ErrorObject> {
    match source {
        Source::Image { path, sector_size } => RawImage::open(path, *sector_size)
            .map(|image| Box::new(image) as Box<dyn BlockDevice>)
            .map_err(|error| {
                ErrorObject::new(
                    protocol::error_code::SOURCE_UNAVAILABLE,
                    error_chain(&error),
                )
            }),
        #[cfg(windows)]
        Source::Disk { handle } => {
            adopt_disk(*handle).map(|disk| Box::new(disk) as Box<dyn BlockDevice>)
        }
        #[cfg(not(windows))]
        Source::Disk { .. } => Err(ErrorObject::new(
            protocol::error_code::SOURCE_UNAVAILABLE,
            "当前平台还不支持扫描物理磁盘",
        )),
    }
}

/// 打开深度恢复的源。物理盘要先确认输出目录不在这块盘上，而且必须在创建任何输出目录之前确认：
/// 单是建目录，就已经写到了这块盘上。
#[cfg_attr(not(windows), expect(unused_variables))]
fn open_carve_source(
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
            Ok(Box::new(disk))
        }
        // 其他平台还打不开物理盘，open_source 直接拒绝，走不到写输出那一步
        _ => open_source(source),
    }
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

/// 核对输出目录所在的磁盘里没有源盘。任何一项查不到都拒绝：宁可不开始，也不冒险写回源盘。
#[cfg(windows)]
fn ensure_output_elsewhere(
    source_disk: Result<u32, yisi_io::IoError>,
    output_disks: Result<Vec<u32>, yisi_io::IoError>,
) -> Result<(), ErrorObject> {
    let reject =
        |message: String| ErrorObject::new(protocol::error_code::OUTPUT_ON_SOURCE, message);
    let source_disk = source_disk
        .map_err(|error| reject(format!("无法确认源盘的编号：{}", error_chain(&error))))?;
    let output_disks = output_disks.map_err(|error| {
        reject(format!(
            "无法确认输出目录所在的磁盘：{}",
            error_chain(&error)
        ))
    })?;
    if output_disks.contains(&source_disk) {
        return Err(reject(format!("输出目录在被扫描的磁盘 {source_disk} 上")));
    }
    Ok(())
}

#[cfg(windows)]
fn disk_list() -> DiskList {
    use yisi_io::BusType;
    let disks = yisi_io::list_disks()
        .into_iter()
        .map(|disk| DiskSummary {
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

/// 输出子目录名里用的任务 ID：只取字母、数字和连字符，最多 8 个。
fn short_id(job_id: &str) -> String {
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

/// 把错误及其来源串成一句话。
pub(crate) fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push('：');
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

#[cfg(all(test, windows))]
mod tests {
    use std::io;

    use yisi_io::IoError;

    use super::*;

    fn query_failed() -> IoError {
        IoError::Query {
            what: "测试",
            source: io::Error::other("查不到"),
        }
    }

    #[test]
    fn output_on_another_disk_is_accepted() {
        assert!(ensure_output_elsewhere(Ok(1), Ok(vec![0])).is_ok());
        assert!(ensure_output_elsewhere(Ok(0), Ok(vec![1, 2])).is_ok());
    }

    #[test]
    fn output_on_the_source_disk_is_rejected() {
        let error = ensure_output_elsewhere(Ok(0), Ok(vec![0])).unwrap_err();
        assert_eq!(error.code, protocol::error_code::OUTPUT_ON_SOURCE);
        assert!(error.message.contains("磁盘 0"), "{}", error.message);
        // 跨盘的卷只要有一段在源盘上就不行
        let spanned = ensure_output_elsewhere(Ok(2), Ok(vec![1, 2, 3])).unwrap_err();
        assert_eq!(spanned.code, protocol::error_code::OUTPUT_ON_SOURCE);
    }

    #[test]
    fn unknown_locations_are_rejected() {
        let source = ensure_output_elsewhere(Err(query_failed()), Ok(vec![1])).unwrap_err();
        assert_eq!(source.code, protocol::error_code::OUTPUT_ON_SOURCE);
        let output = ensure_output_elsewhere(Ok(0), Err(query_failed())).unwrap_err();
        assert_eq!(output.code, protocol::error_code::OUTPUT_ON_SOURCE);
        assert!(output.message.contains("查不到"), "{}", output.message);
    }
}
