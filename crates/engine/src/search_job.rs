//! 丢失分区搜索任务：把 `yisi-repair` 的搜索结果转成 [`SearchSummary`]，边搜边经 [`PARTITIONS`] 交出找到的分区。
//! 只读，可以暂停；被取消时照样答复停下之前找到的分区。

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::{Duration, Instant};

use yisi_engine::protocol::{
    BootCopy, FoundPartition, JobPartitions, PARTITIONS, SearchMode, SearchSummary,
};
use yisi_io::BlockDevice;
use yisi_io::ScanControl;
use yisi_repair::search::{self, Candidate, FoundBy, Relation, SearchControl};

use crate::browse::{fs_key, scheme_name};
use crate::job::{JobContext, JobOutcome};

/// 找到的分区最多每隔这么久发一次。
const PARTITIONS_INTERVAL: Duration = Duration::from_millis(500);

pub fn run(
    device: Arc<dyn BlockDevice>,
    mode: SearchMode,
    context: &JobContext,
) -> Result<JobOutcome, String> {
    let reporter = Reporter {
        context,
        pending: RefCell::new(None),
        last_sent: Cell::new(None),
    };
    let report = search::search(
        &device,
        match mode {
            SearchMode::Quick => search::SearchMode::Quick,
            SearchMode::Deep => search::SearchMode::Deep,
        },
        &reporter,
    );
    let summary = SearchSummary {
        size: report.size,
        sector_size: device.sector_size(),
        mode,
        stopped_at: report.stopped_at,
        scheme: report
            .table
            .as_ref()
            .map_or("unreadable", |table| scheme_name(table.scheme))
            .to_owned(),
        candidates: report.candidates.iter().map(found_partition).collect(),
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

fn found_partition(candidate: &Candidate) -> FoundPartition {
    let (relation, table_partition) = match candidate.relation {
        Relation::InTable { number } => ("inTable", Some(number)),
        Relation::Overlaps { number } => ("overlaps", Some(number)),
        Relation::Lost => ("lost", None),
    };
    FoundPartition {
        offset: candidate.offset,
        length: candidate.length,
        filesystem: fs_key(candidate.kind).to_owned(),
        label: candidate.label.clone(),
        found_by: match candidate.found_by {
            FoundBy::Primary => "primary",
            FoundBy::Backup => "backup",
            FoundBy::Both => "both",
            FoundBy::Structure => "structure",
        }
        .to_owned(),
        boot: match candidate.boot {
            _ if candidate.rebuilt => BootCopy::Rebuilt,
            yisi_fs::BootCopy::Primary => BootCopy::Primary,
            yisi_fs::BootCopy::Backup => BootCopy::Backup,
        },
        truncated: candidate.truncated,
        relation: relation.to_owned(),
        table_partition,
        overlaps_candidate: candidate.overlaps_candidate,
    }
}

struct Reporter<'a> {
    context: &'a JobContext,
    /// 还没发出的最新一份找到的分区（距上次发出不到 [`PARTITIONS_INTERVAL`]），下次报进度时补发。
    pending: RefCell<Option<JobPartitions>>,
    last_sent: Cell<Option<Instant>>,
}

impl Reporter<'_> {
    fn flush(&self, force: bool) {
        let due = force
            || self
                .last_sent
                .get()
                .is_none_or(|sent| sent.elapsed() >= PARTITIONS_INTERVAL);
        if !due {
            return;
        }
        if let Some(partitions) = self.pending.borrow_mut().take() {
            self.last_sent.set(Some(Instant::now()));
            self.context.notify(PARTITIONS, partitions);
        }
    }
}

impl ScanControl for Reporter<'_> {
    /// 暂停时在这里等着，不再读盘。暂停前先把攒着的分区发出去，界面上停着的列表才是全的。
    fn is_cancelled(&self) -> bool {
        if self.context.is_paused() {
            self.flush(true);
        }
        self.context.wait_while_paused();
        self.context.is_cancelled()
    }

    fn progress(&self, done: u64, total: u64) {
        self.context.progress(done, total);
        self.flush(false);
    }
}

impl SearchControl for Reporter<'_> {
    fn found(&self, candidates: &[Candidate], truncated: bool) {
        *self.pending.borrow_mut() = Some(JobPartitions {
            job_id: self.context.id().to_owned(),
            candidates: candidates.iter().map(found_partition).collect(),
            candidates_truncated: truncated,
        });
        self.flush(false);
    }
}
