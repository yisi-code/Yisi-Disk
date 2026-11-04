//! 引导扇区诊断（`repair.diagnoseBoot`）与修复引导扇区的写入计划（`repair.planBootSector`）：调用 `yisi-repair`，
//! 把结果换成协议里的形式。都只读源。

use std::sync::Arc;

use std::time::{SystemTime, UNIX_EPOCH};

use yisi_engine::protocol::{
    self, BootCopy, BootCopyReport, BootDiagnosis, BootDifference, BootField, BootFix, BootIssue,
    BootSectorPlan, DeviceIdentity, PartitionBoot, PartitionRef, RebuiltBoot, WritePlan,
};
use yisi_io::BlockDevice;
use yisi_repair::boot::{self, CopyReport, Issue, PartitionReport, TableIssue, Verdict};
use yisi_repair::boot_fix::{self, BootFixError};
use yisi_repair::boot_rebuild::{self, RebuildError};
use yisi_rpc::ErrorObject;

use crate::browse::{fs_key, note_key, role_key, scheme_name, table_kind};
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
        scheme: scheme_name(report.table.scheme).to_owned(),
        notes: report.table.notes.iter().map(note_key).collect(),
        partitions: report.partitions.iter().map(partition).collect(),
    })
}

/// 为一个分区生成修复引导扇区的写入计划。`identity` 是源的标识（镜像文件的规范路径、物理盘的厂商型号序列号），
/// 执行前与实际打开的设备核对。
pub fn plan(
    device: &Arc<dyn BlockDevice>,
    identity: String,
    partition: u32,
    fix: BootFix,
) -> Result<BootSectorPlan, ErrorObject> {
    let direction = match fix {
        BootFix::PrimaryFromBackup => boot_fix::BootFix::PrimaryFromBackup,
        BootFix::BackupFromPrimary => boot_fix::BootFix::BackupFromPrimary,
        BootFix::Rebuild => boot_fix::BootFix::Rebuild {
            serial: new_serial(),
        },
    };
    let planned = boot_fix::plan_boot_fix(device, partition, direction).map_err(|error| {
        let code = match error {
            BootFixError::Table(_) => protocol::error_code::SOURCE_UNAVAILABLE,
            BootFixError::NoPartition(_)
            | BootFixError::NotApplicable(_)
            | BootFixError::Rebuild(RebuildError::Unsupported(_)) => {
                protocol::error_code::PLAN_NOT_APPLICABLE
            }
            BootFixError::NoBackupPosition
            | BootFixError::Unreadable(_)
            | BootFixError::Unaligned
            | BootFixError::Unchanged
            | BootFixError::Rehearsal(_)
            | BootFixError::Rebuild(_)
            | BootFixError::RebuiltUnusable(_) => protocol::error_code::PLAN_UNAVAILABLE,
        };
        ErrorObject::new(code, error_chain(&error))
    })?;
    let plan = WritePlan {
        device: DeviceIdentity {
            identity,
            size: device.size(),
            sector_size: device.sector_size(),
        },
        writes: planned.writes,
    };
    plan.validate().map_err(|error| {
        ErrorObject::new(protocol::error_code::PLAN_UNAVAILABLE, error_chain(&error))
    })?;
    Ok(BootSectorPlan {
        plan,
        partition: planned.partition.number,
        partition_offset: planned.partition.offset,
        partition_length: planned.partition.length,
        filesystem: fs_key(planned.filesystem).to_owned(),
        fix,
        from: planned.from,
        to: planned.to,
        length: planned.length,
        rebuilt: planned.rebuilt.map(|rebuilt| RebuiltBoot {
            fields: rebuilt
                .fields
                .into_iter()
                .map(|(field, value)| BootField {
                    field: field.to_owned(),
                    value,
                })
                .collect(),
            label: rebuilt.label,
            root: rebuilt.root,
            root_count: rebuilt.root_count as u64,
        }),
    })
}

/// 协议里的引导扇区选择换成 `yisi-fs` 的。按重建的引导扇区（`Rebuilt`）时设备已换成叠加视图（[`boot_view`]），
/// 在视图上按主引导扇区打开。
pub fn fs_copy(copy: BootCopy) -> yisi_fs::BootCopy {
    match copy {
        BootCopy::Primary | BootCopy::Rebuilt => yisi_fs::BootCopy::Primary,
        BootCopy::Backup => yisi_fs::BootCopy::Backup,
    }
}

/// 打开卷要用的设备：按重建的引导扇区（`Rebuilt`）时，把分区表里主备都坏了的分区（`only` 给了时只换这一个）
/// 换成推算出的引导扇区的叠加视图，不写盘；其余情况原样返回。
pub fn boot_view(
    device: Arc<dyn BlockDevice>,
    copy: BootCopy,
    only: Option<PartitionRef>,
) -> Result<Arc<dyn BlockDevice>, ErrorObject> {
    if copy != BootCopy::Rebuilt {
        return Ok(device);
    }
    let table = yisi_part::read_table(device.as_ref()).map_err(|error| {
        ErrorObject::new(
            protocol::error_code::SOURCE_UNAVAILABLE,
            error_chain(&error),
        )
    })?;
    let partitions: Vec<_> = table
        .partitions
        .into_iter()
        .filter(|partition| {
            only.is_none_or(|wanted| {
                partition.offset == wanted.offset && partition.length == wanted.length
            })
        })
        .collect();
    Ok(boot_rebuild::overlay_rebuilt(
        &device,
        &partitions,
        new_serial(),
    ))
}

/// 新的卷序列号：重建的引导扇区推不出原来的，按当前时间取一个。
fn new_serial() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    // 打散一下，高低两半都用上
    let mixed = (nanos as u64) ^ ((nanos >> 64) as u64);
    mixed.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17)
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
        Issue::BadBackupSector(value) => ("badBackupSector", Some(value), None),
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
