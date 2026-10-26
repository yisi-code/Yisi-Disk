//! 保存「先只扫描」时选中的文件：把源上每个文件所在的那一段当作一块小设备，只启用它的格式，
//! 再跑一遍深度恢复。保存时因此会重做一遍格式校验；找到的文件起点、长度与扫描时一致才算保存成功，
//! 否则（源上的内容变了）不保留。文件名与边扫边保存时一样。

use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::time::Instant;

use yisi_carve::{CarveControl, CarveError, CarveOptions, DirectorySink, RecoveredFile, carve};
use yisi_engine::protocol::{CopiedFile, CopySummary, ExtractItem};
use yisi_formats::Registry;
use yisi_io::{BlockDevice, Window};

use crate::copy_job::Reporter;
use crate::job::{JobContext, JobOutcome};
use crate::runtime::error_chain;

pub fn run(
    device: &dyn BlockDevice,
    registry: &Registry,
    items: Vec<ExtractItem>,
    output_dir: &Path,
    context: &JobContext,
) -> Result<JobOutcome, String> {
    let started = Instant::now();
    let reporter = Reporter::new(context);
    let total: u64 = items.iter().map(|item| item.length).sum();
    let mut done = 0u64;
    let (mut saved, mut bytes, mut failed) = (0u64, 0u64, 0u64);
    context.progress(0, total);

    for item in items {
        if context.is_cancelled() {
            reporter.flush();
            return Ok(JobOutcome::Cancelled);
        }
        let name = yisi_carve::recovered_name(item.offset, &item.extension);
        match extract_one(device, registry, &item, output_dir, context, done, total) {
            Ok(Some(Ok(()))) => {
                saved += 1;
                bytes += item.length;
                reporter.push(CopiedFile {
                    path: name,
                    size: item.length,
                    error: None,
                    // 深度恢复的文件由格式插件确认过结构，不会整份是 0
                    all_zero: false,
                });
            }
            Ok(Some(Err(reason))) => {
                failed += 1;
                reporter.push(CopiedFile {
                    path: name,
                    size: item.length,
                    error: Some(reason),
                    all_zero: false,
                });
            }
            Ok(None) => {
                reporter.flush();
                return Ok(JobOutcome::Cancelled);
            }
            Err(message) => {
                reporter.flush();
                return Err(message);
            }
        }
        done += item.length;
        context.progress(done, total);
    }
    reporter.flush();
    context.progress(total, total);
    let summary = CopySummary {
        output_dir: output_dir.display().to_string(),
        files: saved,
        bytes,
        failed,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    serde_json::to_value(summary)
        .map(JobOutcome::Completed)
        .map_err(|error| format!("无法编码保存结果：{error}"))
}

/// 保存一个文件。外层 `Err` 是整个任务失败（写输出出错）；`None` 是被取消；
/// `Some(Err)` 是这个文件没能保存的原因。
fn extract_one(
    device: &dyn BlockDevice,
    registry: &Registry,
    item: &ExtractItem,
    output_dir: &Path,
    context: &JobContext,
    done: u64,
    total: u64,
) -> Result<Option<Result<(), String>>, String> {
    let Some(position) = registry.position(&item.extension) else {
        return Ok(Some(Err(format!("不认识的格式 {}", item.extension))));
    };
    let mut enabled = vec![false; registry.formats().len()];
    if let Some(flag) = enabled.get_mut(position) {
        *flag = true;
    }
    let options = CarveOptions {
        block_size: device.sector_size(),
        enabled,
    };
    let window = Window::new(device, item.offset, item.length);
    let mut sink = DirectorySink::with_origin(output_dir, item.offset)
        .map_err(|error| format!("无法创建 {}：{error}", output_dir.display()))?;
    let control = ItemControl {
        context,
        done,
        total,
        found: RefCell::new(Vec::new()),
    };
    match carve(&window, registry, &options, &mut sink, &control) {
        Ok(Some(_)) => {}
        Ok(None) => return Ok(None),
        Err(error @ CarveError::Output(_)) => return Err(error_chain(&error)),
        Err(error) => return Ok(Some(Err(error_chain(&error)))),
    }
    let found = control.found.into_inner();
    let matched = found
        .iter()
        .any(|file| file.offset == item.offset && file.length == item.length);
    // 与扫描时对不上的文件不保留，免得把内容已变的文件当成找回的结果
    for file in found
        .iter()
        .filter(|file| !(file.offset == item.offset && file.length == item.length))
    {
        let _ = fs::remove_file(output_dir.join(&file.name));
    }
    Ok(Some(if matched {
        Ok(())
    } else if let Some(file) = found.first() {
        Err(format!(
            "源上的内容与扫描时不同：现在长 {} 字节，扫描时 {} 字节",
            file.length, item.length
        ))
    } else {
        Err("源上这一段已不是完整的文件，内容可能在扫描后变了".to_owned())
    }))
}

struct ItemControl<'a> {
    context: &'a JobContext,
    /// 之前各文件累计的字节数与全部文件的总字节数，进度按全部文件算。
    done: u64,
    total: u64,
    found: RefCell<Vec<RecoveredFile>>,
}

impl CarveControl for ItemControl<'_> {
    fn is_cancelled(&self) -> bool {
        self.context.is_cancelled()
    }

    fn progress(&self, done: u64, _total: u64) {
        self.context
            .progress(self.done.saturating_add(done).min(self.total), self.total);
    }

    fn found(&self, file: &RecoveredFile) {
        self.found.borrow_mut().push(file.clone());
    }
}
