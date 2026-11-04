//! 修复引导扇区的写入计划：用核对通过的备份覆盖坏了的主引导扇区，或用主引导扇区重写坏了的备份。
//!
//! 只生成计划，不写盘（写入计划见 `yisi_io::WritePlan`，执行在磁盘代理与宿主）。生成前重新诊断，结论必须仍是
//! 「主坏备好」或「主好备坏」；生成后把写入叠加在源上（[`Overlay`]）再诊断一次，结论变成「正常」才给出计划。
//!
//! 复制的范围：FAT32、NTFS 只复制引导扇区本身（FAT32 的 FSInfo 与第 2 扇区的引导代码不动：有的格式化程序
//! 不在备份区写它们，整段复制可能把好的换成空的）；exFAT 复制整个引导区（12 个扇区，校验和扇区一起），
//! 主备各自的校验和才对得上。备份写到主引导扇区自己记的位置（[`recorded_backup`]），与找备份时的第一选择相同。

use std::sync::Arc;

use yisi_fs::FsKind;
use yisi_fs::boot::{BackupFamily, recorded_backup};
use yisi_io::{BlockDevice, Overlay, SectorWrite};
use yisi_part::{PartError, Partition, read_table};

use crate::boot::{Verdict, diagnose_partition};

/// exFAT 的引导区：引导扇区、8 个扩展引导扇区、OEM 参数、保留、校验和扇区。
const EXFAT_BOOT_REGION_SECTORS: u64 = 12;

/// 往哪个方向修。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootFix {
    /// 主引导扇区坏了、备份核对通过：把备份复制到分区开头。
    PrimaryFromBackup,
    /// 主引导扇区正常、备份坏了：把主引导扇区复制到它记的备份位置。
    BackupFromPrimary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootFixPlan {
    pub partition: Partition,
    pub filesystem: FsKind,
    /// 从哪里复制、写到哪里（设备上的字节偏移），复制多少字节。
    pub from: u64,
    pub to: u64,
    pub length: u64,
    /// 只有一段：`to` 处的 `length` 字节。
    pub writes: Vec<SectorWrite>,
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
        BootFix::BackupFromPrimary => &report.primary,
    };
    let sector_size = valid.sector_size.ok_or(BootFixError::NoBackupPosition)?;
    let valid_bytes = read(device, &partition, valid.offset, sector_size)?;
    let (from, to) = match fix {
        BootFix::PrimaryFromBackup => (valid.offset, 0),
        BootFix::BackupFromPrimary => (
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
