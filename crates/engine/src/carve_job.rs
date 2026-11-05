//! 深度恢复任务：把 `yisi-carve` 的进度、找到的文件转成任务通知，找到的文件分批发出。

use std::path::Path;

use yisi_carve::{CarveControl, CarveOptions, RecoveredFile, Sink, carve};
use yisi_engine::protocol::{
    CarveSummary, FOUND, FileType, FileTypeList, FoundFile, JobFound, TypeTotal,
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

pub fn run(
    device: &dyn BlockDevice,
    registry: &Registry,
    enabled: Vec<bool>,
    sink: &mut (dyn Sink + Send),
    output_dir: Option<&Path>,
    context: &JobContext,
) -> Result<JobOutcome, String> {
    let control = Reporter {
        context,
        batch: Batcher::new(),
    };
    let options = CarveOptions {
        block_size: device.sector_size(),
        enabled,
    };
    let outcome = carve(device, registry, &options, sink, &control);
    // 终结通知之前把剩下的文件发完
    control.flush();
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
}
