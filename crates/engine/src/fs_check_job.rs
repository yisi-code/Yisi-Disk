//! 文件系统一致性检查（任务，只读）：核对分区还在原处，逐阶段检查、报进度，可以暂停、取消，结果是问题清单。

use std::time::Instant;

use yisi_engine::protocol::{FsCheckReport, FsIssue, FsIssueTotals, PartitionRef};
use yisi_io::BlockDevice;
use yisi_repair::check::{self, Control, Issue, Report, Stage};

use crate::job::{JobContext, JobOutcome};

/// 问题清单编码后最多这么多字节，留出余量，保证整条消息不超过 1 MiB。
const ISSUES_BYTES: usize = 768 * 1024;

/// 检查点上过暂停闸门，进度带上阶段名。
struct JobControl<'a>(&'a JobContext);

impl Control for JobControl<'_> {
    fn checkpoint(&self) -> bool {
        self.0.wait_while_paused();
        !self.0.is_cancelled()
    }

    fn progress(&self, stage: Stage, done: u64, total: u64) {
        self.0.stage_progress(stage.key(), done, total);
    }
}

pub fn run(
    device: &dyn BlockDevice,
    partition: PartitionRef,
    context: &JobContext,
) -> Result<JobOutcome, String> {
    let started = Instant::now();
    let Some(report) = check::check(
        device,
        partition.offset,
        partition.length,
        &JobControl(context),
    ) else {
        return Ok(JobOutcome::Cancelled);
    };
    let elapsed = started.elapsed().saturating_sub(context.paused_total());
    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    serde_json::to_value(to_protocol(report, partition.offset, elapsed_ms))
        .map(JobOutcome::Completed)
        .map_err(|error| format!("无法编码检查结果：{error}"))
}

/// 换成协议里的结果：偏移换成源上的位置；清单超出消息大小的部分只计数。
fn to_protocol(report: Report, partition_offset: u64, elapsed_ms: u64) -> FsCheckReport {
    let listed = report.issues.len();
    let mut issues = Vec::with_capacity(listed);
    let mut bytes = 0usize;
    // 放不下就从这一项起都不列，保持按发现的顺序
    for issue in report.issues {
        let issue = to_issue(issue, partition_offset);
        let size = serde_json::to_vec(&issue).map_or(usize::MAX, |encoded| encoded.len());
        bytes = bytes.saturating_add(size);
        if bytes > ISSUES_BYTES {
            break;
        }
        issues.push(issue);
    }
    let omitted = report.omitted + (listed - issues.len()) as u64;
    FsCheckReport {
        filesystem: report.fs.key().to_owned(),
        cluster_size: report.cluster_size,
        cluster_count: report.cluster_count,
        used_clusters: report.used_clusters,
        files: report.files,
        directories: report.directories,
        issues,
        omitted_issues: omitted,
        totals: FsIssueTotals {
            data_loss: report.totals.data_loss,
            structure: report.totals.structure,
            minor: report.totals.minor,
        },
        unreadable_bytes: report.unreadable_bytes,
        checked: report
            .checked
            .iter()
            .map(|item| item.key().to_owned())
            .collect(),
        skipped: report
            .skipped
            .iter()
            .map(|item| item.key().to_owned())
            .collect(),
        elapsed_ms,
    }
}

fn to_issue(issue: Issue, partition_offset: u64) -> FsIssue {
    FsIssue {
        kind: issue.kind.key().to_owned(),
        severity: issue.severity.key().to_owned(),
        area: issue.area.map(|area| area.key().to_owned()),
        path: issue.path,
        other_path: issue.other_path,
        cluster: issue.cluster,
        count: issue.count,
        offset: issue
            .offset
            .map(|offset| offset.saturating_add(partition_offset)),
        expected: issue.expected,
        actual: issue.actual,
        field: issue.field.map(str::to_owned),
    }
}
