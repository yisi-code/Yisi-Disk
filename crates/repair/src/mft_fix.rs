//! NTFS `$MFT` 与 `$MFTMirr` 互相修复的写入计划：两边都看前 4 条记录（`$MFTMirr` 镜像的就是这 4 条：
//! `$MFT`、`$MFTMirr`、`$LogFile`、`$Volume`），与文件系统检查比的范围相同。
//!
//! 逐条判断哪一份可信（签名、更新序列、记录头都对，`yisi_fs::mft_record_ok`），好的不会被坏的盖掉：
//! - 用 `$MFT` 重写 `$MFTMirr`：`$MFT` 自己那条记录要可用；只写 `$MFT` 里可信、又与镜像不同的记录；
//! - 用 `$MFTMirr` 修复 `$MFT`：只把 `$MFT` 里坏了、镜像里可信的记录换掉。
//!
//! 只生成计划，不写盘。位置按主引导扇区（参数要能用，规则同打开卷）。生成后在叠加了写入的视图上重新算一遍，
//! 不应再有要改的记录。

use std::sync::Arc;

use yisi_fs::bpb;
use yisi_fs::{mft_record_ok, mft_self_record_ok};
use yisi_io::{BlockDevice, Overlay, SectorWrite};

/// 比较、修复前这么多条记录。
const MIRRORED_RECORDS: u64 = 4;

/// 往哪个方向修。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MftFix {
    /// 用 `$MFT` 重写 `$MFTMirr`。
    MirrorFromMft,
    /// 用 `$MFTMirr` 修复 `$MFT`。
    MftFromMirror,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MftFixPlan {
    /// 要改写的记录号。
    pub records: Vec<u64>,
    /// 从哪一份复制到哪一份：两份前 4 条记录的起点（设备上的字节偏移）。
    pub from: u64,
    pub to: u64,
    pub writes: Vec<SectorWrite>,
}

#[derive(Debug, thiserror::Error)]
pub enum MftFixError {
    #[error("主引导扇区的参数不能用，先修复引导扇区")]
    BootInvalid,
    #[error("主文件表（$MFT）或它的副本（$MFTMirr）的位置超出了卷")]
    OutOfRange,
    #[error("读不出偏移 {0} 处的主文件表记录")]
    Unreadable(u64),
    #[error("主文件表记录的大小没有按设备的扇区对齐")]
    Unaligned,
    /// 没有要改的记录（可能已经修过，或者这个方向不适用）。
    #[error("没有可以这样修的记录：可能已经修过，或者要拿来复制的那一份也坏了")]
    NotApplicable,
    #[error("预演写入之后仍有记录要改")]
    Rehearsal,
}

/// 为起点 `offset`、长 `length` 字节的 NTFS 分区生成计划。只读源。
pub fn plan_mft_fix(
    device: &Arc<dyn BlockDevice>,
    offset: u64,
    length: u64,
    fix: MftFix,
) -> Result<MftFixPlan, MftFixError> {
    let mut boot = [0u8; 512];
    device
        .read_exact_at(offset, &mut boot)
        .map_err(|_| MftFixError::Unreadable(offset))?;
    let geometry = bpb::check_ntfs(&boot)
        .geometry
        .ok_or(MftFixError::BootInvalid)?;
    let record_size = geometry.record_size;
    if record_size % u64::from(device.sector_size()) != 0 {
        return Err(MftFixError::Unaligned);
    }
    let area = MIRRORED_RECORDS
        .checked_mul(record_size)
        .ok_or(MftFixError::OutOfRange)?;
    let limit = geometry
        .total_sectors
        .saturating_mul(geometry.bytes_per_sector)
        .min(length);
    let place = |lcn: u64| {
        lcn.checked_mul(geometry.cluster_size)
            .filter(|&start| start.checked_add(area).is_some_and(|end| end <= limit))
            .and_then(|start| offset.checked_add(start))
            .ok_or(MftFixError::OutOfRange)
    };
    let mft_at = place(geometry.mft_lcn)?;
    let mirror_at = place(geometry.mirror_lcn)?;
    let (from, to) = match fix {
        MftFix::MirrorFromMft => (mft_at, mirror_at),
        MftFix::MftFromMirror => (mirror_at, mft_at),
    };

    let writes = changes(device.as_ref(), mft_at, mirror_at, record_size, area, fix)?;
    if writes.is_empty() {
        return Err(MftFixError::NotApplicable);
    }
    let records = writes
        .iter()
        .map(|write| (write.offset - to) / record_size)
        .collect();

    let rehearsal: Arc<dyn BlockDevice> = Arc::new(Overlay::new(Arc::clone(device), &writes));
    if !changes(
        rehearsal.as_ref(),
        mft_at,
        mirror_at,
        record_size,
        area,
        fix,
    )?
    .is_empty()
    {
        return Err(MftFixError::Rehearsal);
    }
    Ok(MftFixPlan {
        records,
        from,
        to,
        writes,
    })
}

/// 按方向逐条比较两份前 4 条记录，给出要写的各段（每条记录一段）。
fn changes(
    device: &dyn BlockDevice,
    mft_at: u64,
    mirror_at: u64,
    record_size: u64,
    area: u64,
    fix: MftFix,
) -> Result<Vec<SectorWrite>, MftFixError> {
    let mft = read(device, mft_at, area)?;
    let mirror = read(device, mirror_at, area)?;
    let size = usize::try_from(record_size).map_err(|_| MftFixError::OutOfRange)?;
    let mut writes = Vec::new();
    for (number, (in_mft, in_mirror)) in mft.chunks(size).zip(mirror.chunks(size)).enumerate() {
        let number = number as u64;
        let mft_ok = mft_record_ok(in_mft, number);
        let mirror_ok = mft_record_ok(in_mirror, number);
        let (at, before, after) = match fix {
            MftFix::MirrorFromMft if mft_ok && in_mft != in_mirror => {
                (mirror_at, in_mirror, in_mft)
            }
            MftFix::MftFromMirror if !mft_ok && mirror_ok => (mft_at, in_mft, in_mirror),
            _ => continue,
        };
        writes.push(SectorWrite {
            offset: at + number * record_size,
            before: before.to_vec(),
            after: after.to_vec(),
        });
    }
    // 用 $MFT 重写镜像，前提是 $MFT 自己那条记录可用：否则 $MFT 本身就不可信
    if fix == MftFix::MirrorFromMft && !mft_self_record_ok(mft.get(..size).unwrap_or_default()) {
        return Ok(Vec::new());
    }
    Ok(writes)
}

fn read(device: &dyn BlockDevice, at: u64, len: u64) -> Result<Vec<u8>, MftFixError> {
    let mut buf = vec![0u8; usize::try_from(len).map_err(|_| MftFixError::OutOfRange)?];
    device
        .read_exact_at(at, &mut buf)
        .map_err(|_| MftFixError::Unreadable(at))?;
    Ok(buf)
}

#[cfg(test)]
mod tests;
