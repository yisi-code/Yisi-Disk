//! 文件系统修复的写入计划（FAT12/16/32、exFAT，第一批）：按用户勾选的几类问题，从检查得出的结论
//! （[`check_for_repair`]，与检查同一套规则）算出要写的内容。只生成计划，不写盘。
//!
//! - FSInfo 签名不对（FAT32）：换成新的 FSInfo，空闲簇数记为未知。
//! - 文件占用的簇在分配位图里是空闲的（exFAT）：在位图里标成已占用。
//! - 链比文件大小需要的长：在应有长度处写链尾，多出来、只属于这个文件的簇释放（exFAT 另清位图）。
//! - 丢失簇：表项清零（exFAT 另清位图）。另存在前（引擎的另存任务），这里只释放。
//! - 未正常卸下：标回正常卸下（FAT 第 1 项的标志位，exFAT 引导扇区的卷标志，它不参与校验和）。只在把别的勾选项
//!   修好之后，重新检查已经没有「会丢数据 / 结构错误」的问题时才做，免得 Windows 以后不再自己检查。
//!
//! FAT 镜像时各份一起改，不镜像时只改在用的那份；各份 FAT 本来就不一致时不改 FAT。主引导扇区不能用（按备份检查的）
//! 时不修，先修引导扇区。预演：把写入叠加在源上再检查一遍，勾选的几类必须全部消失，别的问题不能变多。

use std::collections::BTreeMap;
use std::sync::Arc;

use yisi_fs::FsKind;
use yisi_fs::boot_write::fat32_fs_info;
use yisi_fs::format::{FatWidth, Run, fat_entry_raw, fat_entry_span, put_fat_entry};
use yisi_io::{BlockDevice, MAX_PLAN_BYTES, MAX_WRITES, Overlay, SectorWrite};

use crate::check::{Control, FatLayout, Findings, IssueKind, Report, Stage, check_for_repair};

/// exFAT 引导扇区里卷标志的位置，第 1 位是「未正常卸下」。
const EXFAT_FLAGS: u64 = 106;
const EXFAT_FLAG_DIRTY: u8 = 0x02;

/// 修哪一类问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FsFix {
    FsInfo,
    Bitmap,
    TooLong,
    Lost,
    Dirty,
}

impl FsFix {
    /// 修好之后检查里应当不再有的问题。
    fn kinds(self) -> &'static [IssueKind] {
        match self {
            Self::FsInfo => &[IssueKind::FsInfoInvalid],
            Self::Bitmap => &[IssueKind::NotInBitmap],
            Self::TooLong => &[IssueKind::ChainTooLong],
            Self::Lost => &[IssueKind::LostClusters, IssueKind::LostClustersMore],
            Self::Dirty => &[IssueKind::VolumeDirty],
        }
    }

    /// 要改 FAT。
    fn touches_fat(self) -> bool {
        matches!(self, Self::TooLong | Self::Lost)
    }
}

/// 一类修复改了多少：文件数（链过长）或段数，以及簇数（FSInfo 与卸下标志为 0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixCount {
    pub fix: FsFix,
    pub items: u64,
    pub clusters: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsFixPlan {
    pub filesystem: FsKind,
    pub writes: Vec<SectorWrite>,
    pub counts: Vec<FixCount>,
}

#[derive(Debug, thiserror::Error)]
pub enum FsFixError {
    #[error("这个分区不是 FAT 或 exFAT（{0:?}），这一版只修 FAT 与 exFAT")]
    NotFat(FsKind),
    #[error("主引导扇区不能用，检查是按备份做的：先修复引导扇区")]
    FromBackup,
    #[error("卷太大，检查没有跟踪每个簇归谁，修不了丢失簇、分配位图与过长的链")]
    NotTracked,
    #[error("两份 FAT 本来就不一致，先不改 FAT")]
    FatCopiesDiffer,
    #[error("没有可以这样修的问题：{0:?}")]
    NothingToFix(FsFix),
    #[error("还有别的「会丢数据」或「结构错误」的问题，不清除未正常卸下的标记")]
    DirtyWithProblems,
    #[error("要改的地方太多：{segments} 段、{bytes} 字节，超过一份计划的上限")]
    TooLarge { segments: usize, bytes: u64 },
    #[error("读不出偏移 {0} 处")]
    Unreadable(u64),
    #[error("预演写入之后，{0}")]
    Rehearsal(String),
}

/// 丢失簇所在的位置：另存任务按它读出每一段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LostClusters {
    pub cluster_size: u64,
    /// 2 号簇在分区里的字节偏移。
    pub heap_offset: u64,
    pub runs: Vec<Run>,
}

/// 检查时不暂停、不报进度。
struct Quiet;

impl Control for Quiet {
    fn checkpoint(&self) -> bool {
        true
    }

    fn progress(&self, _stage: Stage, _done: u64, _total: u64) {}
}

/// 检查一遍分区，交出结论。[`Quiet`] 从不取消，检查总有结果；万一没有，按认不出文件系统处理。
fn inspect(
    device: &dyn BlockDevice,
    offset: u64,
    length: u64,
) -> Result<(Report, Findings, FatLayout), FsFixError> {
    let (report, findings) = check_for_repair(device, offset, length, &Quiet)
        .ok_or(FsFixError::NotFat(FsKind::Unknown))?;
    let layout = findings
        .layout
        .clone()
        .ok_or(FsFixError::NotFat(report.fs))?;
    Ok((report, findings, layout))
}

/// 分区里的丢失簇（另存用）。卷不是 FAT / exFAT、按备份检查的、没有跟踪认领时报错。
pub fn lost_clusters(
    device: &dyn BlockDevice,
    offset: u64,
    length: u64,
) -> Result<LostClusters, FsFixError> {
    let (_, findings, layout) = inspect(device, offset, length)?;
    if findings.from_backup {
        return Err(FsFixError::FromBackup);
    }
    if !findings.tracked {
        return Err(FsFixError::NotTracked);
    }
    Ok(LostClusters {
        cluster_size: layout.cluster_size,
        heap_offset: layout.heap_offset,
        runs: findings.lost,
    })
}

/// 为起点 `offset`、长 `length` 字节的分区生成修复 `fixes` 这几类问题的计划。只读源。
pub fn plan_fs_fix(
    device: &Arc<dyn BlockDevice>,
    offset: u64,
    length: u64,
    fixes: &[FsFix],
) -> Result<FsFixPlan, FsFixError> {
    let mut fixes = fixes.to_vec();
    fixes.sort_unstable();
    fixes.dedup();
    let (report, findings, layout) = inspect(device.as_ref(), offset, length)?;
    if findings.from_backup {
        return Err(FsFixError::FromBackup);
    }
    let needs_tracking = |fix: &FsFix| matches!(fix, FsFix::Bitmap | FsFix::TooLong | FsFix::Lost);
    if !findings.tracked && fixes.iter().any(needs_tracking) {
        return Err(FsFixError::NotTracked);
    }
    let fat_dirty = fixes.contains(&FsFix::Dirty) && layout.kind != FsKind::Exfat;
    if findings.fat_copies_differ && (fat_dirty || fixes.iter().any(|fix| fix.touches_fat())) {
        return Err(FsFixError::FatCopiesDiffer);
    }

    let mut patch = Patch::new(device.as_ref(), offset);
    let mut counts = Vec::new();
    for &fix in &fixes {
        let count = match fix {
            FsFix::FsInfo => {
                let at = layout
                    .fs_info
                    .filter(|_| findings.fs_info_invalid)
                    .ok_or(FsFixError::NothingToFix(fix))?;
                let sector = fat32_fs_info(u16::try_from(layout.bytes_per_sector).unwrap_or(512));
                for (index, &byte) in sector.iter().enumerate() {
                    patch.set(at + index as u64, byte)?;
                }
                FixCount {
                    fix,
                    items: 1,
                    clusters: 0,
                }
            }
            FsFix::Bitmap => {
                if findings.not_in_bitmap.is_empty() || layout.kind != FsKind::Exfat {
                    return Err(FsFixError::NothingToFix(fix));
                }
                let mut clusters = 0;
                for cluster in clusters_of(&findings.not_in_bitmap) {
                    patch.bitmap(&layout, cluster, true)?;
                    clusters += 1;
                }
                FixCount {
                    fix,
                    items: findings.not_in_bitmap.len() as u64,
                    clusters,
                }
            }
            FsFix::TooLong => {
                if findings.too_long.is_empty() {
                    return Err(FsFixError::NothingToFix(fix));
                }
                let mut clusters = 0;
                for chain in &findings.too_long {
                    patch.fat(&layout, chain.last, layout.width.end_of_chain())?;
                    for &cluster in &chain.extra {
                        patch.fat(&layout, cluster, 0)?;
                        if layout.kind == FsKind::Exfat {
                            patch.bitmap(&layout, cluster, false)?;
                        }
                        clusters += 1;
                    }
                }
                FixCount {
                    fix,
                    items: findings.too_long.len() as u64,
                    clusters,
                }
            }
            FsFix::Lost => {
                if findings.lost.is_empty() {
                    return Err(FsFixError::NothingToFix(fix));
                }
                let mut clusters = 0;
                for cluster in clusters_of(&findings.lost) {
                    patch.fat(&layout, cluster, 0)?;
                    if layout.kind == FsKind::Exfat {
                        patch.bitmap(&layout, cluster, false)?;
                    }
                    clusters += 1;
                }
                FixCount {
                    fix,
                    items: findings.lost.len() as u64,
                    clusters,
                }
            }
            FsFix::Dirty => {
                if !findings.dirty {
                    return Err(FsFixError::NothingToFix(fix));
                }
                patch.clean(&layout)?;
                FixCount {
                    fix,
                    items: 1,
                    clusters: 0,
                }
            }
        };
        counts.push(count);
    }

    let writes = patch.writes();
    let bytes: u64 = writes.iter().map(|write| write.after.len() as u64).sum();
    if writes.len() > MAX_WRITES || bytes > MAX_PLAN_BYTES {
        return Err(FsFixError::TooLarge {
            segments: writes.len(),
            bytes,
        });
    }
    rehearse(device, offset, length, &fixes, &report, &writes)?;
    Ok(FsFixPlan {
        filesystem: layout.kind,
        writes,
        counts,
    })
}

/// 预演：叠加写入后再检查一遍。勾选的几类必须都没了、别的问题不能变多；清除卸下标记时不能还有严重的问题。
fn rehearse(
    device: &Arc<dyn BlockDevice>,
    offset: u64,
    length: u64,
    fixes: &[FsFix],
    before: &Report,
    writes: &[SectorWrite],
) -> Result<(), FsFixError> {
    let overlay: Arc<dyn BlockDevice> = Arc::new(Overlay::new(Arc::clone(device), writes));
    let (after, _, _) = inspect(overlay.as_ref(), offset, length)?;
    let fixed: Vec<IssueKind> = fixes
        .iter()
        .flat_map(|fix| fix.kinds().iter().copied())
        .collect();
    let left = |kind: IssueKind| {
        after
            .issues
            .iter()
            .filter(|issue| issue.kind == kind)
            .count()
    };
    if let Some(kind) = fixed.iter().copied().find(|&kind| left(kind) > 0) {
        return Err(FsFixError::Rehearsal(format!("仍有 {kind:?}")));
    }
    let had = |kind: IssueKind| {
        before
            .issues
            .iter()
            .filter(|issue| issue.kind == kind)
            .count()
    };
    if let Some(issue) = after
        .issues
        .iter()
        .find(|issue| !fixed.contains(&issue.kind) && left(issue.kind) > had(issue.kind))
    {
        return Err(FsFixError::Rehearsal(format!("{:?} 变多了", issue.kind)));
    }
    if fixes.contains(&FsFix::Dirty) && (after.totals.data_loss > 0 || after.totals.structure > 0) {
        return Err(FsFixError::DirtyWithProblems);
    }
    Ok(())
}

fn clusters_of(runs: &[Run]) -> impl Iterator<Item = u32> + '_ {
    runs.iter()
        .flat_map(|run| (0..run.count).map(move |index| run.first + index))
}

/// 在内存里改扇区：第一次改到一个扇区时读出原来的内容，最后按设备扇区合并成连续的写入段。
struct Patch<'a> {
    device: &'a dyn BlockDevice,
    /// 分区起点（设备上的字节偏移）。
    base: u64,
    sector: u64,
    /// 设备上的扇区起点 → （原来的内容，改后的内容）。
    sectors: BTreeMap<u64, (Vec<u8>, Vec<u8>)>,
}

impl<'a> Patch<'a> {
    fn new(device: &'a dyn BlockDevice, base: u64) -> Self {
        Self {
            device,
            base,
            sector: u64::from(device.sector_size()).max(512),
            sectors: BTreeMap::new(),
        }
    }

    /// 分区里 `at` 处那个字节所在的扇区（改后的内容）与字节在扇区里的位置。
    fn slot(&mut self, at: u64) -> Result<(&mut Vec<u8>, usize), FsFixError> {
        let absolute = self.base + at;
        let start = absolute - absolute % self.sector;
        if !self.sectors.contains_key(&start) {
            let mut bytes = vec![0u8; self.sector as usize];
            self.device
                .read_exact_at(start, &mut bytes)
                .map_err(|_| FsFixError::Unreadable(start))?;
            self.sectors.insert(start, (bytes.clone(), bytes));
        }
        let (_, after) = self
            .sectors
            .get_mut(&start)
            .ok_or(FsFixError::Unreadable(start))?;
        Ok((after, (absolute - start) as usize))
    }

    fn get(&mut self, at: u64) -> Result<u8, FsFixError> {
        let (sector, index) = self.slot(at)?;
        Ok(sector.get(index).copied().unwrap_or(0))
    }

    fn set(&mut self, at: u64, value: u8) -> Result<(), FsFixError> {
        let (sector, index) = self.slot(at)?;
        if let Some(byte) = sector.get_mut(index) {
            *byte = value;
        }
        Ok(())
    }

    /// 把各份 FAT 里的表项 `cluster` 改成 `value`。
    fn fat(&mut self, layout: &FatLayout, cluster: u32, value: u32) -> Result<(), FsFixError> {
        let (at, len) = fat_entry_span(layout.width, cluster);
        for &fat in &layout.fats {
            let mut bytes = [0u8; 4];
            for (index, slot) in bytes.iter_mut().take(len).enumerate() {
                *slot = self.get(fat + at + index as u64)?;
            }
            put_fat_entry(layout.width, cluster, value, &mut bytes);
            for (index, &byte) in bytes.iter().take(len).enumerate() {
                self.set(fat + at + index as u64, byte)?;
            }
        }
        Ok(())
    }

    /// exFAT 分配位图里簇 `cluster` 那一位设为已占用或空闲。
    fn bitmap(&mut self, layout: &FatLayout, cluster: u32, used: bool) -> Result<(), FsFixError> {
        let bit = u64::from(cluster.saturating_sub(2));
        let mut byte = bit / 8;
        // 位图按簇存放：找出第 `byte` 个字节落在哪一段簇里
        let mut at = None;
        for run in &layout.bitmap {
            let length = u64::from(run.count) * layout.cluster_size;
            if byte < length {
                at = Some(
                    layout.heap_offset + u64::from(run.first - 2) * layout.cluster_size + byte,
                );
                break;
            }
            byte -= length;
        }
        let at = at.ok_or(FsFixError::Unreadable(self.base))?;
        let mask = 1u8 << (bit % 8);
        let current = self.get(at)?;
        self.set(
            at,
            if used {
                current | mask
            } else {
                current & !mask
            },
        )
    }

    /// 标回正常卸下：FAT16 第 1 项第 15 位、FAT32 第 27 位置 1；exFAT 卷标志第 1 位清零。
    fn clean(&mut self, layout: &FatLayout) -> Result<(), FsFixError> {
        match layout.width {
            FatWidth::Exfat => {
                let flags = self.get(EXFAT_FLAGS)?;
                self.set(EXFAT_FLAGS, flags & !EXFAT_FLAG_DIRTY)
            }
            FatWidth::Fat16 | FatWidth::Fat32 => {
                let bit = if layout.width == FatWidth::Fat16 {
                    0x8000
                } else {
                    0x0800_0000
                };
                let (at, len) = fat_entry_span(layout.width, 1);
                let first = *layout
                    .fats
                    .first()
                    .ok_or(FsFixError::NothingToFix(FsFix::Dirty))?;
                let mut bytes = [0u8; 4];
                for (index, slot) in bytes.iter_mut().take(len).enumerate() {
                    *slot = self.get(first + at + index as u64)?;
                }
                let entry1 = fat_entry_raw(layout.width, 1, &bytes).unwrap_or(0);
                self.fat(layout, 1, entry1 | bit)
            }
            // FAT12 没有这个标志
            FatWidth::Fat12 => Err(FsFixError::NothingToFix(FsFix::Dirty)),
        }
    }

    /// 改过的扇区按设备上的位置合并成连续的段；内容没变的扇区不写。
    fn writes(self) -> Vec<SectorWrite> {
        let mut writes: Vec<SectorWrite> = Vec::new();
        for (start, (before, after)) in self.sectors {
            if before == after {
                continue;
            }
            match writes.last_mut() {
                Some(last) if last.offset + last.after.len() as u64 == start => {
                    last.before.extend(before);
                    last.after.extend(after);
                }
                _ => writes.push(SectorWrite {
                    offset: start,
                    before,
                    after,
                }),
            }
        }
        writes
    }
}

#[cfg(test)]
mod tests;
