//! 修复引导扇区的写入计划：用核对通过的备份覆盖坏了的主引导扇区，或用主引导扇区重写坏了的备份。
//!
//! 只生成计划，不写盘（写入计划见 `yisi_io::WritePlan`，执行在磁盘代理与宿主）。生成前重新诊断，结论必须仍是
//! 「主坏备好」或「主好备坏」；生成后把写入叠加在源上（[`Overlay`]）再诊断一次，结论变成「正常」才给出计划。
//!
//! 复制的范围：FAT32、NTFS 只复制引导扇区本身（FAT32 的 FSInfo 与第 2 扇区的引导代码不动：有的格式化程序
//! 不在备份区写它们，整段复制可能把好的换成空的）；exFAT 复制整个引导区（12 个扇区，校验和扇区一起），
//! 主备各自的校验和才对得上。备份写到主引导扇区自己记的位置（[`recorded_backup`]），与找备份时的第一选择相同。
//!
//! 主备都坏了（或两处都认不出）时可以重建（[`BootFix::Rebuild`]）：按分区里的结构推算出引导扇区
//! （[`boot_rebuild`](crate::boot_rebuild)），主引导扇区与备份都写。预演除了诊断要「正常」，还要按它真的打开卷、
//! 读出根目录，根目录的前几项随计划给出，让人在写之前认出是不是自己的盘。

use std::sync::Arc;

use yisi_fs::boot::{BackupFamily, recorded_backup};
use yisi_fs::boot_write::EXFAT_BOOT_REGION_SECTORS;
use yisi_fs::{BootCopy, FsKind, State, Volume};
use yisi_io::{BlockDevice, Overlay, SectorWrite};
use yisi_part::{PartError, Partition, read_table};

use crate::boot::{Verdict, diagnose_partition};
use crate::boot_rebuild::{self, RebuildError};

/// 重建的预演里列出根目录的前这么多项。
const ROOT_PREVIEW: usize = 8;

/// 往哪个方向修。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootFix {
    /// 主引导扇区坏了、备份核对通过：把备份复制到分区开头。
    PrimaryFromBackup,
    /// 主引导扇区正常、备份坏了：把主引导扇区复制到它记的备份位置。
    BackupFromPrimary,
    /// 主备都坏了（或两处都认不出）：按分区里的结构推算出引导扇区，主引导扇区与备份都写。`serial` 是新的卷序列号。
    Rebuild { serial: u64 },
}

/// 重建出的引导扇区：推算出的各项（规范字段名与值）、卷标，以及按它打开后根目录的前几项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuiltBoot {
    pub fields: Vec<(&'static str, u64)>,
    pub label: Option<String>,
    /// 根目录里没删除的项的名字（前 [`ROOT_PREVIEW`] 个）与总数。
    pub root: Vec<String>,
    pub root_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootFixPlan {
    pub partition: Partition,
    pub filesystem: FsKind,
    /// 从哪里复制、写到哪里（设备上的字节偏移），复制多少字节。
    pub from: u64,
    pub to: u64,
    pub length: u64,
    /// 复制时只有一段：`to` 处的 `length` 字节。重建时是主引导扇区与备份各一段（FAT32 连同 FSInfo），
    /// `from` 与 `to` 都是分区起点，`length` 是第一段的长度。
    pub writes: Vec<SectorWrite>,
    /// 重建时推算出的内容。
    pub rebuilt: Option<RebuiltBoot>,
}

#[derive(Debug, thiserror::Error)]
pub enum BootFixError {
    #[error("读不出分区表")]
    Table(#[from] PartError),
    #[error("分区表里没有分区 {0}")]
    NoPartition(u32),
    /// 诊断结论不是这种修复要求的（可能在诊断之后源被改过，或者界面传错了方向）。
    #[error("这个分区现在的诊断结论是 {0:?}，不适用这种修复")]
    NotApplicable(Verdict),
    #[error("主引导扇区里记的备份位置不合法")]
    NoBackupPosition,
    #[error("读不出偏移 {0} 处的引导扇区")]
    Unreadable(u64),
    #[error("引导扇区的位置或长度没有按设备的扇区对齐")]
    Unaligned,
    #[error("要写的内容与那里现有的内容相同")]
    Unchanged,
    /// 预演写入之后重新诊断，结论不是「正常」：写进去也修不好，不给计划。
    #[error("预演写入之后，诊断结论是 {0:?}")]
    Rehearsal(Verdict),
    #[error("推算不出引导扇区")]
    Rebuild(#[from] RebuildError),
    /// 按重建的引导扇区打不开卷或读不出根目录。
    #[error("按推算的引导扇区打不开卷：{0}")]
    RebuiltUnusable(String),
}

/// 为分区表里编号为 `number` 的分区生成修复引导扇区的计划。只读源。
pub fn plan_boot_fix(
    device: &Arc<dyn BlockDevice>,
    number: u32,
    fix: BootFix,
) -> Result<BootFixPlan, BootFixError> {
    let table = read_table(device.as_ref())?;
    let partition = table
        .partitions
        .into_iter()
        .find(|partition| partition.number == number)
        .ok_or(BootFixError::NoPartition(number))?;
    let report = diagnose_partition(device, &partition);
    let required = match fix {
        BootFix::PrimaryFromBackup => Verdict::PrimaryDamaged,
        BootFix::BackupFromPrimary => Verdict::BackupDamaged,
        BootFix::Rebuild { serial } => {
            if !matches!(report.verdict, Verdict::BothDamaged | Verdict::Unrecognized) {
                return Err(BootFixError::NotApplicable(report.verdict));
            }
            return plan_rebuild(device, partition, report.filesystem, serial);
        }
    };
    if report.verdict != required {
        return Err(BootFixError::NotApplicable(report.verdict));
    }
    let family =
        BackupFamily::of(report.filesystem).ok_or(BootFixError::NotApplicable(report.verdict))?;

    // 核对通过的那一份：位置、它记的扇区大小与内容
    let valid = match fix {
        BootFix::PrimaryFromBackup => report
            .backup
            .as_ref()
            .ok_or(BootFixError::NotApplicable(report.verdict))?,
        BootFix::BackupFromPrimary | BootFix::Rebuild { .. } => &report.primary,
    };
    let sector_size = valid.sector_size.ok_or(BootFixError::NoBackupPosition)?;
    let valid_bytes = read(device, &partition, valid.offset, sector_size)?;
    let (from, to) = match fix {
        BootFix::PrimaryFromBackup => (valid.offset, 0),
        BootFix::BackupFromPrimary | BootFix::Rebuild { .. } => (
            0,
            recorded_backup(&valid_bytes, family, sector_size)
                .ok_or(BootFixError::NoBackupPosition)?,
        ),
    };
    let sectors = match family {
        BackupFamily::Exfat => EXFAT_BOOT_REGION_SECTORS,
        BackupFamily::Fat32 | BackupFamily::Ntfs => 1,
    };
    let length = sectors
        .checked_mul(sector_size)
        .ok_or(BootFixError::Unaligned)?;
    let after = read(device, &partition, from, length)?;
    let before = read(device, &partition, to, length)?;
    if before == after {
        return Err(BootFixError::Unchanged);
    }

    let device_sector = u64::from(device.sector_size());
    let at = partition
        .offset
        .checked_add(to)
        .ok_or(BootFixError::Unaligned)?;
    if at % device_sector != 0 || length % device_sector != 0 {
        return Err(BootFixError::Unaligned);
    }
    let writes = vec![SectorWrite {
        offset: at,
        before,
        after,
    }];

    let rehearsal: Arc<dyn BlockDevice> = Arc::new(Overlay::new(Arc::clone(device), &writes));
    let rehearsed = diagnose_partition(&rehearsal, &partition);
    if rehearsed.verdict != Verdict::Healthy {
        return Err(BootFixError::Rehearsal(rehearsed.verdict));
    }

    Ok(BootFixPlan {
        filesystem: report.filesystem,
        from: partition.offset.saturating_add(from),
        to: at,
        length,
        writes,
        rebuilt: None,
        partition,
    })
}

/// 重建：推算出引导扇区，预演写入后诊断要「正常」、按它打得开卷并读得出根目录。
fn plan_rebuild(
    device: &Arc<dyn BlockDevice>,
    partition: Partition,
    kind: FsKind,
    serial: u64,
) -> Result<BootFixPlan, BootFixError> {
    let rebuilt = boot_rebuild::rebuild(device, &partition, kind, serial)?;
    let rehearsal: Arc<dyn BlockDevice> =
        Arc::new(Overlay::new(Arc::clone(device), &rebuilt.writes));
    let rehearsed = diagnose_partition(&rehearsal, &partition);
    if rehearsed.verdict != Verdict::Healthy {
        return Err(BootFixError::Rehearsal(rehearsed.verdict));
    }
    let unusable = |error: &dyn std::fmt::Display| BootFixError::RebuiltUnusable(error.to_string());
    let volume = Volume::open_with(
        rehearsal,
        partition.offset,
        partition.length,
        BootCopy::Primary,
    )
    .map_err(|error| unusable(&error))?;
    let listing = volume
        .list(&volume.root())
        .map_err(|error| unusable(&error))?;
    let live: Vec<String> = listing
        .entries
        .iter()
        .filter(|entry| entry.state == State::Live)
        .map(|entry| entry.name.clone())
        .collect();
    let length = rebuilt
        .writes
        .first()
        .map_or(0, |write| write.after.len() as u64);
    Ok(BootFixPlan {
        filesystem: rebuilt.kind,
        from: partition.offset,
        to: partition.offset,
        length,
        rebuilt: Some(RebuiltBoot {
            fields: yisi_fs::bpb::significant_fields(rebuilt.kind, &rebuilt.boot),
            label: rebuilt.label,
            root: live.iter().take(ROOT_PREVIEW).cloned().collect(),
            root_count: live.len(),
        }),
        writes: rebuilt.writes,
        partition,
    })
}

/// 读分区里 `at` 起的 `len` 字节；超出分区或读不出时报 [`BootFixError::Unreadable`]。
fn read(
    device: &Arc<dyn BlockDevice>,
    partition: &Partition,
    at: u64,
    len: u64,
) -> Result<Vec<u8>, BootFixError> {
    let unreadable = || BootFixError::Unreadable(partition.offset.saturating_add(at));
    let end = at.checked_add(len).ok_or_else(unreadable)?;
    if end > partition.length {
        return Err(unreadable());
    }
    let mut buf = vec![0u8; usize::try_from(len).map_err(|_| unreadable())?];
    device
        .read_exact_at(
            partition.offset.checked_add(at).ok_or_else(unreadable)?,
            &mut buf,
        )
        .map_err(|_| unreadable())?;
    Ok(buf)
}

#[cfg(test)]
mod tests;
