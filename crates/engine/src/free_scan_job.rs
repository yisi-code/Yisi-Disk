//! 读空闲簇的任务（`repair.scanFree`）：把分区里每段空闲簇按 1 MiB 读一遍，整段读不出时逐簇重读，记下读不出的簇，
//! 之后可以标成坏簇。只读源。

use std::time::Instant;

use yisi_engine::protocol::{ClusterRun, FreeScanSummary, MAX_UNREADABLE_RUNS, PartitionRef};
use yisi_fs::format::{Run, push_run};
use yisi_io::BlockDevice;
use yisi_repair::fs_fix::LostClusters;

use crate::job::{JobContext, JobOutcome};

const CHUNK: u64 = 1 << 20;

pub fn run(
    device: &dyn BlockDevice,
    partition: PartitionRef,
    free: &LostClusters,
    context: &JobContext,
) -> Result<JobOutcome, String> {
    let started = Instant::now();
    let size = free.cluster_size.max(1);
    let total: u64 = free
        .runs
        .iter()
        .map(|run| u64::from(run.count) * size)
        .sum();
    let mut done = 0u64;
    let mut unreadable: Vec<Run> = Vec::new();
    let mut buffer = vec![0u8; CHUNK.max(size) as usize];
    context.progress(0, total);
    for run in &free.runs {
        // 每次读整数个簇、不超过 1 MiB（簇比 1 MiB 大时一次读一个簇）
        let per_read = (CHUNK / size).max(1) as u32;
        let mut first = run.first;
        let end = run.first + run.count;
        while first < end {
            if context.is_cancelled() {
                return Ok(JobOutcome::Cancelled);
            }
            let count = per_read.min(end - first);
            let at = partition.offset + free.heap_offset + u64::from(first - 2) * size;
            let length = u64::from(count) * size;
            let slice = buffer.get_mut(..length as usize).unwrap_or_default();
            if device.read_exact_at(at, slice).is_err() {
                // 整段读不出：逐簇重读，只记真正读不出的簇
                for cluster in first..first + count {
                    let at = partition.offset + free.heap_offset + u64::from(cluster - 2) * size;
                    let one = buffer.get_mut(..size as usize).unwrap_or_default();
                    if device.read_exact_at(at, one).is_err() {
                        push_run(&mut unreadable, cluster);
                    }
                }
            }
            first += count;
            done += length;
            context.progress(done, total);
        }
    }
    let unreadable_clusters = unreadable.iter().map(|run| u64::from(run.count)).sum();
    let summary = FreeScanSummary {
        clusters: total / size,
        bytes: total,
        unreadable: unreadable
            .iter()
            .take(MAX_UNREADABLE_RUNS)
            .map(|run| ClusterRun {
                first: run.first,
                count: run.count,
            })
            .collect(),
        unreadable_clusters,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    serde_json::to_value(summary)
        .map(JobOutcome::Completed)
        .map_err(|error| format!("无法编码读空闲簇的结果：{error}"))
}
