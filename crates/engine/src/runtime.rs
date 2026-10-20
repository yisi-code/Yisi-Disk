//! 消息循环：从宿主读请求、派发、写答复；后台发心跳；任务结束或宿主断开后退出。

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use serde::Serialize;
use serde_json::{Value, json};
use yisi_engine::protocol::{
    self, CANCEL, CAPABILITIES, Capabilities, HEARTBEAT, HEARTBEAT_INTERVAL, JobRef, READ_SCAN,
    ReadScanParams, Source,
};
use yisi_io::{BlockDevice, RawImage};
use yisi_rpc::{
    ErrorObject, FrameError, Message, Notification, Request, Response, error_code, read_message,
    write_message,
};

use crate::job::{self, RunningJob};
use crate::read_scan;

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
        CAPABILITIES => to_result(&Capabilities {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            methods: vec![READ_SCAN.to_owned(), CANCEL.to_owned()],
        }),
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
    }
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
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push('：');
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}
