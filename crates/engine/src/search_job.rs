//! 丢失分区搜索任务：把 `yisi-repair` 的搜索结果转成 [`SearchSummary`]。只读，可以暂停。

use std::sync::Arc;

use yisi_engine::protocol::{BootCopy, FoundPartition, SearchSummary};
use yisi_io::BlockDevice;
use yisi_io::ScanControl;
use yisi_repair::search::{self, FoundBy, Relation};

use crate::browse::{fs_key, scheme_name};
use crate::job::{JobContext, JobOutcome};

pub fn run(device: Arc<dyn BlockDevice>, context: &JobContext) -> Result<JobOutcome, String> {
    let Some(report) = search::search(&device, &Reporter(context)) else {
        return Ok(JobOutcome::Cancelled);
    };
    let summary = SearchSummary {
        size: report.size,
        sector_size: device.sector_size(),
        scheme: report
            .table
            .as_ref()
            .map_or("unreadable", |table| scheme_name(table.scheme))
            .to_owned(),
        candidates: report
            .candidates
            .into_iter()
            .map(|candidate| {
                let (relation, table_partition) = match candidate.relation {
                    Relation::InTable { number } => ("inTable", Some(number)),
                    Relation::Overlaps { number } => ("overlaps", Some(number)),
                    Relation::Lost => ("lost", None),
                };
                FoundPartition {
                    offset: candidate.offset,
                    length: candidate.length,
                    filesystem: fs_key(candidate.kind).to_owned(),
                    label: candidate.label,
                    found_by: match candidate.found_by {
                        FoundBy::Primary => "primary",
                        FoundBy::Backup => "backup",
                        FoundBy::Both => "both",
                    }
                    .to_owned(),
                    boot: match candidate.boot {
                        yisi_fs::BootCopy::Primary => BootCopy::Primary,
                        yisi_fs::BootCopy::Backup => BootCopy::Backup,
                    },
                    truncated: candidate.truncated,
                    relation: relation.to_owned(),
                    table_partition,
                    overlaps_candidate: candidate.overlaps_candidate,
                }
            })
            .collect(),
        candidates_truncated: report.candidates_truncated,
        unreadable_bytes: report.unreadable_bytes,
        // 暂停的时长不算
        elapsed_ms: report
            .elapsed_ms
            .saturating_sub(u64::try_from(context.paused_total().as_millis()).unwrap_or(u64::MAX)),
    };
    serde_json::to_value(summary)
        .map(JobOutcome::Completed)
        .map_err(|error| format!("无法编码搜索结果：{error}"))
}

struct Reporter<'a>(&'a JobContext);

impl ScanControl for Reporter<'_> {
    /// 暂停时在这里等着，不再读盘。
    fn is_cancelled(&self) -> bool {
        self.0.wait_while_paused();
        self.0.is_cancelled()
    }

    fn progress(&self, done: u64, total: u64) {
        self.0.progress(done, total);
    }
}
