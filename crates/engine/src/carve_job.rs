//! 深度恢复任务：把 `yisi-carve` 的进度、找到的文件转成任务通知，找到的文件分批发出；
//! 检查点每秒最多发一次，发之前先把攒着的文件发完，宿主收到检查点时它之前的文件都已收到。

use std::cell::Cell;
use std::path::Path;
use std::time::{Duration, Instant};

use yisi_carve::{CarveControl, CarveOptions, RecoveredFile, Sink, carve};
use yisi_engine::protocol::{
    CHECKPOINT, CarveSummary, FOUND, FileType, FileTypeList, FoundFile, JobCheckpoint, JobFound,
    TypeTotal,
};
use yisi_formats::Registry;
use yisi_io::BlockDevice;

use crate::job::{Batcher, JobContext, JobOutcome};
use crate::runtime::error_chain;

pub fn file_types(registry: &Registry) -> FileTypeList {
    let types = registry
        .formats()
        .iter()
        .map(|format| {
            let info = format.info();
            FileType {
                extension: info.extension.to_owned(),
                variants: info
                    .variants
                    .iter()
                    .map(|&variant| variant.to_owned())
                    .collect(),
                description: info.description.to_owned(),
                max_size: info.max_size,
                enabled_by_default: info.enabled_by_default,
            }
        })
        .collect();
    FileTypeList { types }
}

/// 按扩展名列表给出每个格式是否启用；不给列表时用各格式的默认设置。不认识的扩展名返回它。
pub fn enabled_formats(
    registry: &Registry,
    extensions: Option<&[String]>,
) -> Result<Vec<bool>, String> {
    let Some(extensions) = extensions else {
        return Ok(registry
            .formats()
            .iter()
            .map(|format| format.info().enabled_by_default)
            .collect());
    };
    let mut enabled = vec![false; registry.formats().len()];
    for extension in extensions {
        let position = registry
            .position(extension)
            .ok_or_else(|| extension.clone())?;
        if let Some(flag) = enabled.get_mut(position) {
            *flag = true;
        }
    }
    Ok(enabled)
}

/// 检查点最多这么久发一次。
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(1);

pub fn run(
    device: &dyn BlockDevice,
    registry: &Registry,
    enabled: Vec<bool>,
    start_at: u64,
    sink: &mut (dyn Sink + Send),
    output_dir: Option<&Path>,
    context: &JobContext,
) -> Result<JobOutcome, String> {
    let control = Reporter {
        context,
        batch: Batcher::new(),
        checkpoint: Cell::new(None),
        checkpoint_sent: Cell::new(None),
    };
    let options = CarveOptions {
        block_size: device.sector_size(),
        enabled,
        start: start_at,
    };
    let outcome = carve(device, registry, &options, sink, &control);
    // 终结通知之前把剩下的文件发完；被取消时再发最后的检查点
    control.flush();
    if matches!(outcome, Ok(None)) {
        control.send_checkpoint();
    }
    match outcome {
        Ok(Some(report)) => {
            let summary = CarveSummary {
                output_dir: output_dir.map(|dir| dir.display().to_string()),
                files: report.files,
                bytes: report.bytes,
                by_type: report
                    .by_type
                    .into_iter()
                    .map(|(extension, totals)| {
                        (
                            extension,
                            TypeTotal {
                                count: totals.count,
                                bytes: totals.bytes,
                            },
                        )
                    })
                    .collect(),
                unreadable_bytes: report.unreadable_bytes,
                // 暂停的时长不算
                elapsed_ms: report.elapsed_ms.saturating_sub(
                    u64::try_from(context.paused_total().as_millis()).unwrap_or(u64::MAX),
                ),
            };
            serde_json::to_value(summary)
                .map(JobOutcome::Completed)
                .map_err(|error| format!("无法编码恢复结果：{error}"))
        }
        Ok(None) => Ok(JobOutcome::Cancelled),
        Err(error) => Err(error_chain(&error)),
    }
}

struct Reporter<'a> {
    context: &'a JobContext,
    batch: Batcher<FoundFile>,
    /// 最近一个检查点，以及上次发出的时间。
    checkpoint: Cell<Option<u64>>,
    checkpoint_sent: Cell<Option<Instant>>,
}

impl Reporter<'_> {
    fn send(&self, files: Vec<FoundFile>) {
        self.context.notify(
            FOUND,
            JobFound {
                job_id: self.context.id().to_owned(),
                files,
            },
        );
    }

    fn flush(&self) {
        self.batch.flush(|files| self.send(files));
    }

    /// 先把攒着的文件发完，再发检查点。
    fn send_checkpoint(&self) {
        let Some(resume_at) = self.checkpoint.get() else {
            return;
        };
        self.flush();
        self.context.notify(
            CHECKPOINT,
            JobCheckpoint {
                job_id: self.context.id().to_owned(),
                resume_at,
            },
        );
        self.checkpoint_sent.set(Some(Instant::now()));
    }
}

impl CarveControl for Reporter<'_> {
    /// 暂停时在这里等着：识别停下，读线程预读满队列后也停下，不再读盘。
    fn is_cancelled(&self) -> bool {
        self.context.wait_while_paused();
        self.context.is_cancelled()
    }

    fn progress(&self, done: u64, total: u64) {
        self.context.progress(done, total);
        self.batch.tick(|files| self.send(files));
    }

    fn found(&self, file: &RecoveredFile) {
        let found = FoundFile {
            name: file.name.clone(),
            extension: file.extension.clone(),
            offset: file.offset,
            length: file.length,
        };
        self.batch.push(found, |files| self.send(files));
    }

    fn checkpoint(&self, resume_at: u64) {
        self.checkpoint.set(Some(resume_at));
        let due = self
            .checkpoint_sent
            .get()
            .is_none_or(|sent| sent.elapsed() >= CHECKPOINT_INTERVAL);
        if due {
            self.send_checkpoint();
        }
    }
}
