//! 另存丢失簇的任务（`repair.saveLost`）：每段连续的丢失簇存成一个 `lost-<起始簇>.bin`，先写 `.part` 再改名、
//! 不覆盖。只读源；读不出的段整段算失败、删掉没写完的文件，接着存下一段。

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

use yisi_carve::{partial_path, rename_new};
use yisi_engine::protocol::{CopySummary, PartitionRef};
use yisi_io::BlockDevice;
use yisi_repair::fs_fix::LostClusters;

use crate::job::{JobContext, JobOutcome};

const CHUNK: u64 = 1 << 20;

pub fn run(
    device: &dyn BlockDevice,
    partition: PartitionRef,
    lost: &LostClusters,
    output_dir: &Path,
    context: &JobContext,
) -> Result<JobOutcome, String> {
    let started = Instant::now();
    let total: u64 = lost
        .runs
        .iter()
        .map(|run| u64::from(run.count) * lost.cluster_size)
        .sum();
    let (mut done, mut files, mut failed) = (0u64, 0u64, 0u64);
    context.progress(0, total);
    for run in &lost.runs {
        if context.is_cancelled() {
            return Ok(JobOutcome::Cancelled);
        }
        let length = u64::from(run.count) * lost.cluster_size;
        let start = partition.offset
            + lost.heap_offset
            + u64::from(run.first.saturating_sub(2)) * lost.cluster_size;
        let target = output_dir.join(format!("lost-{}.bin", run.first));
        match save(device, start, length, &target, context, &mut done, total) {
            Ok(true) => files += 1,
            Ok(false) => return Ok(JobOutcome::Cancelled),
            Err(error) => {
                tracing::warn!(cluster = run.first, %error, "这一段丢失簇没能另存");
                failed += 1;
            }
        }
    }
    context.progress(total, total);
    let summary = CopySummary {
        output_dir: output_dir.display().to_string(),
        files,
        bytes: done,
        failed,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    serde_json::to_value(summary)
        .map(JobOutcome::Completed)
        .map_err(|error| format!("无法编码另存结果：{error}"))
}

/// 读设备上 `start` 起 `length` 字节存成 `target`。被取消时返回 `Ok(false)`（删掉没写完的文件）。
fn save(
    device: &dyn BlockDevice,
    start: u64,
    length: u64,
    target: &Path,
    context: &JobContext,
    done: &mut u64,
    total: u64,
) -> io::Result<bool> {
    let partial = partial_path(target);
    let result = (|| -> io::Result<bool> {
        let mut output = File::create_new(&partial)?;
        let mut buffer = vec![0u8; CHUNK as usize];
        let mut at = 0u64;
        while at < length {
            if context.is_cancelled() {
                return Ok(false);
            }
            let take = (length - at).min(CHUNK);
            let slice = buffer.get_mut(..take as usize).unwrap_or_default();
            device
                .read_exact_at(start + at, slice)
                .map_err(|error| io::Error::other(error.to_string()))?;
            output.write_all(slice)?;
            at += take;
            *done += take;
            context.progress(*done, total);
        }
        output.sync_all()?;
        Ok(true)
    })();
    match result {
        Ok(true) => {
            rename_new(&partial, target)?;
            Ok(true)
        }
        other => {
            let _ = std::fs::remove_file(&partial);
            other
        }
    }
}
