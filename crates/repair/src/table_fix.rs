//! 改写分区表的写入计划：把搜索找到的丢失分区写回分区表（落在扩展分区里的写成逻辑分区），把类型与文件系统
//! 不符的分区改成对的类型，或者用一份完好的 GPT 修好另一份。
//!
//! 只生成计划，不写盘（分区表的编码在 `yisi_part::plan_table_edit`、`plan_gpt_repair`）。加分区前核对那个位置确实
//! 认得出 FAT、exFAT 或 NTFS（主引导扇区或备份），按文件系统定类型；改类型前重新诊断，分区表与引导扇区的类型必须
//! 确实不符。生成后把写入叠加在源上预演：重新读分区表，原有的分区都还在；新分区要在表里、认得出文件系统；改过类型的
//! 分区不再报类型不符；修过 GPT 的不再报主备有一份坏了。

use std::sync::Arc;

use yisi_fs::{BootCopy, FsKind, detect_with};
use yisi_io::{BlockDevice, Overlay, SectorWrite};
use yisi_part::{
    BASIC_DATA, EditError, GptRepair, Guid, Note, PartError, Partition, Scheme, TableEdit,
    plan_gpt_repair, plan_table_edit, read_table,
};

use crate::boot::{TableIssue, diagnose_partition};

/// 怎么改分区表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableFix {
    /// 把起点 `offset`、长 `length` 字节的丢失分区加进分区表。
    AddLost { offset: u64, length: u64 },
    /// 把编号为 `partition` 的分区的类型改成与它的文件系统相符的值。
    SetType { partition: u32 },
    /// 主 GPT 或备份 GPT 坏了：用完好的那一份修好它。
    RepairGpt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableFixPlan {
    pub scheme: Scheme,
    /// 加上的或改了类型的分区的文件系统；修 GPT 时为 `Unknown`。
    pub filesystem: FsKind,
    /// 改完之后分区表里的这个分区（预演时读出的）；修 GPT 时为 `None`。
    pub partition: Option<Partition>,
    /// 修 GPT 的方向。
    pub gpt: Option<GptRepair>,
    pub writes: Vec<SectorWrite>,
}

#[derive(Debug, thiserror::Error)]
pub enum TableFixError {
    #[error("读不出分区表")]
    Table(#[from] PartError),
    #[error("无法改写分区表：{0}")]
    Edit(#[from] EditError),
    #[error("这个位置认不出文件分配表（FAT）、扩展文件分配表（exFAT）或新技术文件系统（NTFS）")]
    NoFilesystem,
    #[error("分区表里没有分区 {0}")]
    NoPartition(u32),
    /// 诊断没有报类型不符（可能在诊断之后改过）。
    #[error("分区 {0} 的类型与文件系统相符，不用改")]
    NotApplicable(u32),
    #[error("预演写入之后{0}")]
    Rehearsal(&'static str),
}

/// 生成改写分区表的计划。只读源。
pub fn plan_table_fix(
    device: &Arc<dyn BlockDevice>,
    fix: TableFix,
) -> Result<TableFixPlan, TableFixError> {
    let table = read_table(device.as_ref())?;
    match fix {
        TableFix::AddLost { offset, length } => {
            let (filesystem, copy) = recognise(device.as_ref(), offset, length)?;
            let (mbr_type, gpt_type) = types_for(filesystem).ok_or(TableFixError::NoFilesystem)?;
            let writes = plan_table_edit(
                device.as_ref(),
                &TableEdit::Add {
                    offset,
                    length,
                    mbr_type,
                    gpt_type,
                },
            )?;
            let rehearsal = overlay(device, &writes);
            let after = read_table(rehearsal.as_ref())?;
            keeps_partitions(&table.partitions, &after.partitions)?;
            let partition = after
                .partitions
                .into_iter()
                .find(|partition| partition.offset == offset && partition.length == length)
                .ok_or(TableFixError::Rehearsal("分区表里没有这个分区"))?;
            let kind = detect_with(rehearsal.as_ref(), offset, length, copy)
                .map_err(|_| TableFixError::Rehearsal("读不出这个分区"))?;
            if kind != filesystem {
                return Err(TableFixError::Rehearsal("认不出这个分区的文件系统"));
            }
            Ok(TableFixPlan {
                scheme: table.scheme,
                filesystem,
                partition: Some(partition),
                gpt: None,
                writes,
            })
        }
        TableFix::SetType { partition: number } => {
            let partition = table
                .partitions
                .iter()
                .find(|partition| partition.number == number)
                .ok_or(TableFixError::NoPartition(number))?;
            let report = diagnose_partition(device, partition);
            if !report.table_issues.contains(&TableIssue::TypeMismatch) {
                return Err(TableFixError::NotApplicable(number));
            }
            let (mbr_type, gpt_type) =
                types_for(report.filesystem).ok_or(TableFixError::NotApplicable(number))?;
            let writes = plan_table_edit(
                device.as_ref(),
                &TableEdit::SetType {
                    number,
                    mbr_type,
                    gpt_type,
                },
            )?;
            let rehearsal = overlay(device, &writes);
            let listed = read_table(rehearsal.as_ref())?.partitions;
            keeps_partitions(&table.partitions, &listed)?;
            let after = listed
                .into_iter()
                .find(|partition| partition.number == number)
                .ok_or(TableFixError::Rehearsal("分区表里没有这个分区"))?;
            if diagnose_partition(&rehearsal, &after)
                .table_issues
                .contains(&TableIssue::TypeMismatch)
            {
                return Err(TableFixError::Rehearsal("类型仍与文件系统不符"));
            }
            Ok(TableFixPlan {
                scheme: table.scheme,
                filesystem: report.filesystem,
                partition: Some(after),
                gpt: None,
                writes,
            })
        }
        TableFix::RepairGpt => {
            let (repair, writes) = plan_gpt_repair(device.as_ref())?;
            let after = read_table(overlay(device, &writes).as_ref())?;
            if after.scheme != Scheme::Gpt
                || after
                    .notes
                    .iter()
                    .any(|note| matches!(note, Note::PrimaryGptInvalid | Note::BackupGptInvalid))
            {
                return Err(TableFixError::Rehearsal("仍有一份 GUID 分区表（GPT）是坏的"));
            }
            keeps_partitions(&table.partitions, &after.partitions)?;
            Ok(TableFixPlan {
                scheme: Scheme::Gpt,
                filesystem: FsKind::Unknown,
                partition: None,
                gpt: Some(repair),
                writes,
            })
        }
    }
}

/// 预演之后原有的分区都还在（起点与长度不变；编号可能因为插进一个逻辑分区而顺延）。
fn keeps_partitions(before: &[Partition], after: &[Partition]) -> Result<(), TableFixError> {
    let kept = before.iter().all(|old| {
        after
            .iter()
            .any(|new| new.offset == old.offset && new.length == old.length)
    });
    if kept {
        Ok(())
    } else {
        Err(TableFixError::Rehearsal("原有的分区不见了"))
    }
}

/// 这个位置是哪种文件系统，按哪一份引导扇区认出来的：先看主引导扇区，再看备份。
fn recognise(
    device: &dyn BlockDevice,
    offset: u64,
    length: u64,
) -> Result<(FsKind, BootCopy), TableFixError> {
    for copy in [BootCopy::Primary, BootCopy::Backup] {
        if let Ok(kind) = detect_with(device, offset, length, copy)
            && types_for(kind).is_some()
        {
            return Ok((kind, copy));
        }
    }
    Err(TableFixError::NoFilesystem)
}

/// 文件系统对应的 MBR 类型字节与 GPT 类型（Windows 建分区时用的值）。只认 FAT、exFAT、NTFS。
fn types_for(kind: FsKind) -> Option<(u8, Guid)> {
    let mbr_type = match kind {
        FsKind::Fat12 => 0x01,
        FsKind::Fat16 => 0x0E,
        FsKind::Fat32 => 0x0C,
        FsKind::Exfat | FsKind::Ntfs => 0x07,
        FsKind::BitLocker | FsKind::Ext | FsKind::Unknown => return None,
    };
    Some((mbr_type, BASIC_DATA))
}

fn overlay(device: &Arc<dyn BlockDevice>, writes: &[SectorWrite]) -> Arc<dyn BlockDevice> {
    Arc::new(Overlay::new(Arc::clone(device), writes))
}

#[cfg(test)]
mod tests;
