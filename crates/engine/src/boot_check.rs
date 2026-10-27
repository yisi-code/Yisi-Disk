//! 引导扇区诊断（`repair.diagnoseBoot`）：调用 `yisi-repair`，把结果换成协议里的形式。只读。

use std::sync::Arc;

use yisi_engine::protocol::{
    self, BootCopy, BootCopyReport, BootDiagnosis, BootDifference, BootIssue, PartitionBoot,
};
use yisi_io::BlockDevice;
use yisi_part::Scheme;
use yisi_repair::boot::{self, CopyReport, Issue, PartitionReport, TableIssue, Verdict};
use yisi_rpc::ErrorObject;

use crate::browse::{fs_key, note_key, role_key, table_kind};
use crate::runtime::error_chain;

pub fn diagnose(device: &Arc<dyn BlockDevice>) -> Result<BootDiagnosis, ErrorObject> {
    let report = boot::diagnose(device).map_err(|error| {
        ErrorObject::new(
            protocol::error_code::SOURCE_UNAVAILABLE,
            error_chain(&error),
        )
    })?;
    Ok(BootDiagnosis {
        size: device.size(),
        sector_size: device.sector_size(),
        scheme: match report.table.scheme {
            Scheme::Mbr => "mbr",
            Scheme::Gpt => "gpt",
            Scheme::None => "none",
        }
        .to_owned(),
        notes: report.table.notes.iter().map(note_key).collect(),
        partitions: report.partitions.iter().map(partition).collect(),
    })
}

/// 协议里的引导扇区选择换成 `yisi-fs` 的。
pub fn fs_copy(copy: BootCopy) -> yisi_fs::BootCopy {
    match copy {
        BootCopy::Primary => yisi_fs::BootCopy::Primary,
        BootCopy::Backup => yisi_fs::BootCopy::Backup,
    }
}

fn partition(report: &PartitionReport) -> PartitionBoot {
    let partition = &report.partition;
    let (type_id, name, _, logical) = table_kind(partition);
    let base = partition.offset;
    PartitionBoot {
        number: partition.number,
        offset: partition.offset,
        length: partition.length,
        type_id,
        role: partition.role().map(|role| role_key(role).to_owned()),
        name,
        logical,
        filesystem: fs_key(report.filesystem).to_owned(),
        verdict: verdict_key(report.verdict).to_owned(),
        label: report.primary.label.clone().or_else(|| {
            report
                .backup
                .as_ref()
                .and_then(|backup| backup.label.clone())
        }),
        primary: copy(&report.primary, base),
        backup: report.backup.as_ref().map(|backup| copy(backup, base)),
        differences: report
            .differences
            .iter()
            .map(|difference| BootDifference {
                field: difference.field.to_owned(),
                primary: difference.primary,
                backup: difference.backup,
            })
            .collect(),
        table_issues: report.table_issues.iter().map(table_issue).collect(),
    }
}

fn copy(report: &CopyReport, base: u64) -> BootCopyReport {
    BootCopyReport {
        offset: base.saturating_add(report.offset),
        issues: report.issues.iter().map(issue).collect(),
        notes: report.notes.iter().map(issue).collect(),
        volume_bytes: report.volume_bytes,
        cluster_size: report.cluster_size,
        opened: report.opened(),
        open_error: match &report.open {
            Some(Err(reason)) => Some(reason.clone()),
            _ => None,
        },
    }
}

fn verdict_key(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Healthy => "healthy",
        Verdict::BackupDamaged => "backupDamaged",
        Verdict::PrimaryDamaged => "primaryDamaged",
        Verdict::BothDamaged => "bothDamaged",
        Verdict::Unsupported => "unsupported",
        Verdict::Unrecognized => "unrecognized",
    }
}

fn issue(issue: &Issue) -> BootIssue {
    let (code, value, expected) = match *issue {
        Issue::Unreadable => ("unreadable", None, None),
        Issue::Blank => ("blank", None, None),
        Issue::MissingSignature => ("missingSignature", None, None),
        Issue::BadJump => ("badJump", None, None),
        Issue::BadOemId => ("badOemId", None, None),
        Issue::BadLegacyFields => ("badLegacyFields", None, None),
        Issue::BadBytesPerSector(value) => ("badBytesPerSector", Some(value), None),
        Issue::SectorSizeMismatch { recorded, expected } => {
            ("sectorSizeMismatch", Some(recorded), Some(expected))
        }
        Issue::BadSectorsPerCluster(value) => ("badSectorsPerCluster", Some(value), None),
        Issue::BadReservedSectors(value) => ("badReservedSectors", Some(value), None),
        Issue::BadFatCount(value) => ("badFatCount", Some(value), None),
        Issue::BadFatSize(value) => ("badFatSize", Some(value), None),
        Issue::BadMedia(value) => ("badMedia", Some(value), None),
        Issue::BadRootEntries(value) => ("badRootEntries", Some(value), None),
        Issue::BadTotalSectors(value) => ("badTotalSectors", Some(value), None),
        Issue::BadLayout => ("badLayout", None, None),
        Issue::BadRootCluster(value) => ("badRootCluster", Some(value), None),
        Issue::BadActiveFat(value) => ("badActiveFat", Some(value), None),
        Issue::BadRecordSize => ("badRecordSize", None, None),
        Issue::BadIndexSize => ("badIndexSize", None, None),
        Issue::MftOutside(value) => ("mftOutside", Some(value), None),
        Issue::MirrorOutside(value) => ("mirrorOutside", Some(value), None),
        Issue::ChecksumMismatch { stored, computed } => (
            "checksumMismatch",
            stored.map(u64::from),
            Some(u64::from(computed)),
        ),
    };
    BootIssue {
        code: code.to_owned(),
        value,
        expected,
    }
}

fn table_issue(issue: &TableIssue) -> BootIssue {
    let (code, value, expected) = match *issue {
        TableIssue::TypeMismatch => ("typeMismatch", None, None),
        TableIssue::VolumeLarger { volume, partition } => {
            ("volumeLarger", Some(volume), Some(partition))
        }
        TableIssue::VolumeSmaller { volume, partition } => {
            ("volumeSmaller", Some(volume), Some(partition))
        }
    };
    BootIssue {
        code: code.to_owned(),
        value,
        expected,
    }
}
