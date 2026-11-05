//! 文件系统修复的写入计划（FAT12/16/32、exFAT）：按用户勾选的几类问题，从检查得出的结论
//! （[`check_for_repair`]，与检查同一套规则）算出要写的内容。只生成计划，不写盘。
//!
//! - FSInfo 签名不对（FAT32）：换成新的 FSInfo，空闲簇数记为未知。
//! - 文件占用的簇在分配位图里是空闲的（exFAT）：在位图里标成已占用。
//! - 链比文件大小需要的长：在应有长度处写链尾，多出来、只属于这个文件的簇释放（exFAT 另清位图）。
//! - 丢失簇：表项清零（exFAT 另清位图）。另存在前（引擎的另存任务），这里只释放。
//! - 未正常卸下：标回正常卸下（FAT 第 1 项的标志位，exFAT 引导扇区的卷标志，它不参与校验和）。只在把别的勾选项
//!   修好之后，重新检查已经没有「会丢数据 / 结构错误」的问题时才做，免得 Windows 以后不再自己检查。
//! - 两份 FAT 不一致：以用户选的那份为准，把另一份里不同的扇区改成一样（[`rate_fat_copies`] 先把两种方向都预演一遍，
//!   给出各自修完还剩多少问题）。要单独做，修完重新检查再修别的。
//! - 断了的链：截到最后一个好簇、文件大小跟着改；起始簇就不合法的文件改为空文件（与 Windows 磁盘检查相同）。
//! - 交叉链接：后认领的那个文件，把共用的簇复制到空闲簇，链改指新簇；两个文件的现有内容都留着。
//! - 目录项：exFAT 的次项个数、文件名长度按现有的项重算，校验和重算；孤立的次项、对不上的长文件名项标为已删除；
//!   子目录开头的 `.` 与 `..` 重写；有效长度不超过数据长度。
//! - 坏簇：读不出的空闲簇在 FAT 里标成坏簇（exFAT 另在位图里标成占用），已属于文件的不动。读不出的位置由读空闲簇的
//!   任务（引擎）找出，这里再读一次核对。
//! - exFAT 大写表坏了或找不到：写回标准大写表（[`EXFAT_UPCASE_TABLE`]）；原来的簇够用就写回原处，不够或找不到时
//!   另分配空闲簇（原来的簇释放），找不到表项时补在根目录的空项里。内容本来就是标准表、只是记的校验和不对时只改校验和。
//! - exFAT 分配位图本身坏了：按检查认领的结果重建，写到空闲簇上，位图项改指新簇（找不到时补一项），旧位图的簇释放。
//!   要单独做。
//! - FAT 里起始簇不合法的子目录：分配一个空闲簇写上 `.` 与 `..`，改成空目录，名字留着（算在断链一类）。
//!
//! FAT 镜像时各份一起改，不镜像时只改在用的那份；各份 FAT 本来就不一致时，先统一两份 FAT 才改别的。主引导扇区
//! 不能用（按备份检查的）时不修，先修引导扇区。预演：把写入叠加在源上再检查一遍，勾选的几类里计划修的那些必须消失，
//! 别的问题不能变多。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use yisi_fs::FsKind;
use yisi_fs::boot_write::fat32_fs_info;
use yisi_fs::format::{
    ATTR_DIRECTORY, DIR_ENTRY_SIZE, EXFAT_FLAG_DIRTY, EXFAT_TYPE_BITMAP, EXFAT_TYPE_UPCASE,
    EXFAT_UPCASE_CHECKSUM, EXFAT_UPCASE_TABLE, EXFAT_VOLUME_FLAGS_AT, FAT_DELETED, FatWidth, Run,
    exfat_set_checksum, fat_entry_raw, fat_entry_span, put_fat_entry,
};
use yisi_io::{BlockDevice, MAX_PLAN_BYTES, MAX_WRITES, Overlay, SectorWrite, merge_writes};

use crate::check::{
    Control, EntryAt, EntryChange, FatLayout, Findings, IssueKind, Report, Stage, Totals,
    check_for_repair,
};

/// 改的是扇区里的字节：位置换成 u64、标志换成那一个字节。
const EXFAT_FLAGS: u64 = EXFAT_VOLUME_FLAGS_AT as u64;
const ENTRY: u64 = DIR_ENTRY_SIZE as u64;

/// 修哪一类问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FsFix {
    FsInfo,
    Bitmap,
    TooLong,
    Lost,
    Dirty,
    /// 两份 FAT 不一致：以第一份为准、以第二份为准。
    FatFromFirst,
    FatFromSecond,
    Chains,
    CrossLinks,
    Entries,
    BadClusters,
    /// exFAT：写回标准大写表。
    Upcase,
    /// exFAT：按目录树重建分配位图（要单独做）。
    RebuildBitmap,
}

/// 生成计划时按这个次序做：目录项在断链之前（有效长度先按原来的数据长度改，断链再一起截短）。
const ORDER: [FsFix; 13] = [
    FsFix::RebuildBitmap,
    FsFix::FsInfo,
    FsFix::Bitmap,
    FsFix::TooLong,
    FsFix::Lost,
    FsFix::Dirty,
    FsFix::FatFromFirst,
    FsFix::FatFromSecond,
    FsFix::Entries,
    FsFix::Chains,
    FsFix::CrossLinks,
    FsFix::BadClusters,
    FsFix::Upcase,
];

impl FsFix {
    /// 修好之后检查里应当减少的问题。
    fn kinds(self) -> &'static [IssueKind] {
        match self {
            Self::FsInfo => &[IssueKind::FsInfoInvalid],
            Self::Bitmap => &[IssueKind::NotInBitmap],
            Self::TooLong => &[IssueKind::ChainTooLong],
            Self::Lost => &[IssueKind::LostClusters, IssueKind::LostClustersMore],
            Self::Dirty => &[IssueKind::VolumeDirty],
            Self::FatFromFirst | Self::FatFromSecond => &[IssueKind::FatCopiesDiffer],
            Self::Chains => &[
                IssueKind::FirstClusterInvalid,
                IssueKind::ChainOutOfRange,
                IssueKind::ChainLoop,
                IssueKind::ChainFree,
                IssueKind::ChainBadCluster,
                IssueKind::ChainTooShort,
            ],
            Self::CrossLinks => &[IssueKind::CrossLink],
            Self::Entries => &[
                IssueKind::EntrySetChecksum,
                IssueKind::EntrySetCount,
                IssueKind::EntryNameLength,
                IssueKind::OrphanEntries,
                IssueKind::LfnChecksum,
                IssueKind::LfnOrder,
                IssueKind::DotEntries,
                IssueKind::SizeInvalid,
            ],
            Self::BadClusters => &[],
            Self::Upcase => &[IssueKind::UpcaseChecksum, IssueKind::UpcaseMissing],
            Self::RebuildBitmap => &[IssueKind::BitmapInvalid],
        }
    }

    /// 要改 FAT。
    fn touches_fat(self) -> bool {
        matches!(
            self,
            Self::TooLong
                | Self::Lost
                | Self::Chains
                | Self::CrossLinks
                | Self::BadClusters
                | Self::Upcase
                | Self::RebuildBitmap
        )
    }

    /// 一修就是全部：检查里这几类修完必须一个不剩。其余几类可能只修得了一部分（有的位置读不出、有的不归文件）。
    fn complete(self) -> bool {
        matches!(
            self,
            Self::FsInfo
                | Self::Bitmap
                | Self::TooLong
                | Self::Lost
                | Self::Dirty
                | Self::FatFromFirst
                | Self::FatFromSecond
                | Self::Upcase
                | Self::RebuildBitmap
        )
    }

    fn fat_source(self) -> Option<usize> {
        match self {
            Self::FatFromFirst => Some(0),
            Self::FatFromSecond => Some(1),
            _ => None,
        }
    }
}

/// 一类修复改了多少：文件数、段数或处数（`items`），以及簇数（FSInfo 与卸下标志为 0）。
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

/// 统一两份 FAT 时，以某一份为准修完之后的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FatRating {
    pub fix: FsFix,
    /// 修完还剩的问题（各严重程度的个数）。
    pub totals: Totals,
    /// 要改的扇区数。
    pub sectors: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum FsFixError {
    #[error(
        "这个分区是 {0:?}，不是文件分配表（FAT）或扩展文件分配表（exFAT），这一版只修 FAT 与 exFAT"
    )]
    NotFat(FsKind),
    #[error("主引导扇区不能用，检查是按备份做的：先修复引导扇区")]
    FromBackup,
    #[error("卷太大，检查没有跟踪每个簇归谁，修不了丢失簇、分配位图、过长的链与交叉链接")]
    NotTracked,
    #[error("两份文件分配表（FAT）本来就不一致：先统一两份 FAT，重新检查后再改 FAT")]
    FatCopiesDiffer,
    #[error("统一两份文件分配表（FAT）要单独做：修完重新检查，再修别的")]
    FatCopiesAlone,
    #[error("没有可以这样修的问题：{0:?}")]
    NothingToFix(FsFix),
    #[error("还有别的「会丢数据」或「结构错误」的问题，不清除未正常卸下的标记")]
    DirtyWithProblems,
    #[error("空闲的簇不够")]
    NoFreeClusters,
    #[error("重建分配位图要单独做：修完重新检查，再修别的")]
    BitmapAlone,
    #[error("根目录里没有空着的项，补不上位图项或大写表项")]
    NoRootSlot,
    #[error("要改的地方太多：{segments} 段、{bytes} 字节，超过一份计划的上限")]
    TooLarge { segments: usize, bytes: u64 },
    #[error("读不出偏移 {0} 处")]
    Unreadable(u64),
    #[error("预演写入之后，{0}")]
    Rehearsal(String),
}

/// 丢失簇或空闲簇所在的位置：另存、读空闲簇的任务按它读出每一段。
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
    device: &Arc<dyn BlockDevice>,
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

/// 检查结论里这个分区能修的几类，各有多少（界面按它列出可以勾选的项）。坏簇要先读一遍空闲簇才知道，不在里面。
pub fn available(findings: &Findings) -> Vec<FixCount> {
    let Some(layout) = findings.layout.as_ref() else {
        return Vec::new();
    };
    if findings.from_backup {
        return Vec::new();
    }
    let mut counts = Vec::new();
    let mut add = |fix: FsFix, items: u64, clusters: u64| {
        if items > 0 {
            counts.push(FixCount {
                fix,
                items,
                clusters,
            });
        }
    };
    // 分配位图本身坏了：先重建，别的修法都要用到位图
    if findings.rebuild.is_some() {
        let bytes = u64::from(layout.cluster_count).div_ceil(8);
        add(
            FsFix::RebuildBitmap,
            1,
            bytes.div_ceil(layout.cluster_size.max(1)),
        );
        return counts;
    }
    if findings.fs_info_invalid && layout.fs_info.is_some() {
        add(FsFix::FsInfo, 1, 0);
    }
    // 两份 FAT 不一致时，改 FAT 的都要等统一之后
    if findings.fat_copies_differ {
        if let Some(copies) = &findings.fat_copies {
            let sectors = copies
                .differing
                .iter()
                .map(|&(_, length)| length / layout.bytes_per_sector.max(1))
                .sum();
            add(FsFix::FatFromFirst, sectors, 0);
            add(FsFix::FatFromSecond, sectors, 0);
        }
        return counts;
    }
    let clusters = |runs: &[Run]| runs.iter().map(|run| u64::from(run.count)).sum::<u64>();
    if findings.tracked {
        add(
            FsFix::Bitmap,
            findings.not_in_bitmap.len() as u64,
            clusters(&findings.not_in_bitmap),
        );
        add(
            FsFix::TooLong,
            findings.too_long.len() as u64,
            findings
                .too_long
                .iter()
                .map(|chain| chain.extra.len() as u64)
                .sum(),
        );
        add(
            FsFix::Lost,
            findings.lost.len() as u64,
            clusters(&findings.lost),
        );
        add(
            FsFix::CrossLinks,
            findings.cross.len() as u64,
            findings
                .cross
                .iter()
                .map(|cross| cross.shared.len() as u64)
                .sum(),
        );
    }
    if findings.dirty && layout.width != FatWidth::Fat12 {
        add(FsFix::Dirty, 1, 0);
    }
    add(
        FsFix::Chains,
        (findings.broken.len() + findings.empty_dirs.len()) as u64,
        0,
    );
    add(FsFix::Entries, findings.entries.len() as u64, 0);
    // 内容就是标准表时只改校验和；要写新簇时得知道哪些簇空闲
    if let Some(upcase) = &findings.upcase
        && (upcase.standard || findings.tracked)
    {
        add(FsFix::Upcase, 1, 0);
    }
    counts
}

/// 分区里的丢失簇（另存用）。卷不是 FAT / exFAT、按备份检查的、没有跟踪认领时报错。
pub fn lost_clusters(
    device: &Arc<dyn BlockDevice>,
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

/// 分区里的空闲簇（找坏簇时读它们）。条件同 [`lost_clusters`]。
pub fn free_clusters(
    device: &Arc<dyn BlockDevice>,
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
        runs: findings.free,
    })
}

/// 两份 FAT 不一致时，把「以第一份为准」「以第二份为准」各预演一遍，给出修完还剩多少问题。只读源。
pub fn rate_fat_copies(
    device: &Arc<dyn BlockDevice>,
    offset: u64,
    length: u64,
) -> Result<Vec<FatRating>, FsFixError> {
    let (_, findings, layout) = inspect(device, offset, length)?;
    if findings.from_backup {
        return Err(FsFixError::FromBackup);
    }
    let mut ratings = Vec::new();
    for fix in [FsFix::FatFromFirst, FsFix::FatFromSecond] {
        let mut patch = Patch::new(device.as_ref(), offset);
        let count = unify_fats(&mut patch, &findings, &layout, fix)?;
        let writes = patch.writes();
        let overlay: Arc<dyn BlockDevice> = Arc::new(Overlay::new(Arc::clone(device), &writes));
        let (after, _, _) = inspect(&overlay, offset, length)?;
        ratings.push(FatRating {
            fix,
            totals: after.totals,
            sectors: count.items,
        });
    }
    Ok(ratings)
}

/// 为起点 `offset`、长 `length` 字节的分区生成修复 `fixes` 这几类问题的计划。`bad` 是读空闲簇时读不出的簇
/// （只在修坏簇时用，这里再读一次核对）。只读源。
pub fn plan_fs_fix(
    device: &Arc<dyn BlockDevice>,
    offset: u64,
    length: u64,
    fixes: &[FsFix],
    bad: &[Run],
) -> Result<FsFixPlan, FsFixError> {
    let fixes: Vec<FsFix> = ORDER
        .iter()
        .copied()
        .filter(|fix| fixes.contains(fix))
        .collect();
    let (report, findings, layout) = inspect(device, offset, length)?;
    if findings.from_backup {
        return Err(FsFixError::FromBackup);
    }
    let needs_tracking = |fix: &FsFix| {
        matches!(
            fix,
            FsFix::Bitmap | FsFix::TooLong | FsFix::Lost | FsFix::CrossLinks | FsFix::BadClusters
        )
    };
    if findings.rebuild.is_some() && fixes.iter().any(|&fix| fix != FsFix::RebuildBitmap) {
        return Err(FsFixError::BitmapAlone);
    }
    if !findings.tracked && fixes.iter().any(needs_tracking) {
        return Err(FsFixError::NotTracked);
    }
    let fat_dirty = fixes.contains(&FsFix::Dirty) && layout.kind != FsKind::Exfat;
    let unify = fixes.iter().any(|fix| fix.fat_source().is_some());
    if unify
        && fixes
            .iter()
            .any(|&fix| fix.fat_source().is_none() && fix != FsFix::FsInfo)
    {
        return Err(FsFixError::FatCopiesAlone);
    }
    if !unify
        && findings.fat_copies_differ
        && (fat_dirty || fixes.iter().any(|fix| fix.touches_fat()))
    {
        return Err(FsFixError::FatCopiesDiffer);
    }

    let mut patch = Patch::new(device.as_ref(), offset);
    let mut counts = Vec::new();
    // 计划修掉的问题：类型与个数（预演时核对它们确实少了这么多）
    let mut planned: Vec<IssueKind> = Vec::new();
    // 改过的 exFAT 目录项集合，最后统一重算校验和
    let mut sets: BTreeSet<Vec<u64>> = BTreeSet::new();
    // 要新簇的几类（交叉链接、空目录、大写表、位图）从同一处按簇号顺序取，不会取到同一个簇
    let mut free = FreeClusters::new(&findings.free);
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
            FsFix::FatFromFirst | FsFix::FatFromSecond => {
                unify_fats(&mut patch, &findings, &layout, fix)?
            }
            FsFix::Entries => {
                if findings.entries.is_empty() {
                    return Err(FsFixError::NothingToFix(fix));
                }
                for entry in &findings.entries {
                    edit_entry(&mut patch, &layout, &entry.change, &mut sets)?;
                    planned.extend(entry.kinds.iter().copied());
                }
                FixCount {
                    fix,
                    items: findings.entries.len() as u64,
                    clusters: 0,
                }
            }
            FsFix::Chains => {
                if findings.broken.is_empty() && findings.empty_dirs.is_empty() {
                    return Err(FsFixError::NothingToFix(fix));
                }
                let mut clusters = 0;
                for dir in &findings.empty_dirs {
                    let fresh = free.take(&mut patch, &layout)?;
                    let at = cluster_at(&layout, fresh);
                    for byte in 0..layout.cluster_size {
                        patch.set(at + byte, 0)?;
                    }
                    write_dot(&mut patch, &layout, at, b".          ", fresh)?;
                    write_dot(&mut patch, &layout, at + ENTRY, b"..         ", dir.parent)?;
                    patch.fat(&layout, fresh, layout.width.end_of_chain())?;
                    set_first(&mut patch, &layout, &EntryAt::Fat(dir.at), fresh, &mut sets)?;
                    planned.push(IssueKind::FirstClusterInvalid);
                    clusters += 1;
                }
                for chain in &findings.broken {
                    if let Some(end) = chain.end_at {
                        patch.fat(&layout, end, layout.width.end_of_chain())?;
                    }
                    if chain.empty {
                        set_first(&mut patch, &layout, &chain.at, 0, &mut sets)?;
                        set_size(&mut patch, &chain.at, 0, &mut sets)?;
                    } else if let Some(size) = chain.size {
                        set_size(&mut patch, &chain.at, size, &mut sets)?;
                    }
                    planned.push(chain.kind);
                }
                FixCount {
                    fix,
                    items: (findings.broken.len() + findings.empty_dirs.len()) as u64,
                    clusters,
                }
            }
            FsFix::CrossLinks => {
                let skip: Vec<&str> = if fixes.contains(&FsFix::TooLong) {
                    findings
                        .too_long
                        .iter()
                        .map(|chain| chain.path.as_str())
                        .collect()
                } else {
                    Vec::new()
                };
                let (mut items, mut clusters) = (0u64, 0u64);
                for cross in findings
                    .cross
                    .iter()
                    .filter(|cross| !skip.contains(&cross.path.as_str()))
                {
                    clusters += copy_shared(&mut patch, &layout, cross, &mut free, &mut sets)?;
                    items += 1;
                    let issues = report
                        .issues
                        .iter()
                        .filter(|issue| {
                            issue.kind == IssueKind::CrossLink
                                && issue.path.as_deref() == Some(cross.path.as_str())
                        })
                        .count();
                    planned.extend(std::iter::repeat_n(IssueKind::CrossLink, issues));
                }
                if items == 0 {
                    return Err(FsFixError::NothingToFix(fix));
                }
                FixCount {
                    fix,
                    items,
                    clusters,
                }
            }
            FsFix::Upcase => write_upcase(&mut patch, &layout, &findings, &mut free)?,
            FsFix::RebuildBitmap => rebuild_bitmap(&mut patch, &layout, &findings, &mut free)?,
            FsFix::BadClusters => {
                let marked =
                    mark_bad(&mut patch, &layout, &findings, bad, device.as_ref(), offset)?;
                if marked.clusters == 0 {
                    return Err(FsFixError::NothingToFix(fix));
                }
                marked
            }
        };
        counts.push(count);
    }
    for entries in &sets {
        reseal_set(&mut patch, entries)?;
    }

    let writes = patch.writes();
    let bytes: u64 = writes.iter().map(|write| write.after.len() as u64).sum();
    if writes.len() > MAX_WRITES || bytes > MAX_PLAN_BYTES {
        return Err(FsFixError::TooLarge {
            segments: writes.len(),
            bytes,
        });
    }
    rehearse(device, offset, length, &fixes, &report, &writes, &planned)?;
    Ok(FsFixPlan {
        filesystem: layout.kind,
        writes,
        counts,
    })
}

/// 以 `fix` 指定的那份 FAT 为准，把另一份里不同的扇区改成一样。
fn unify_fats(
    patch: &mut Patch,
    findings: &Findings,
    layout: &FatLayout,
    fix: FsFix,
) -> Result<FixCount, FsFixError> {
    let copies = findings
        .fat_copies
        .as_ref()
        .ok_or(FsFixError::NothingToFix(fix))?;
    let source = fix.fat_source().ok_or(FsFixError::NothingToFix(fix))?;
    let copy = |index: usize| {
        copies
            .copies
            .get(index)
            .copied()
            .ok_or(FsFixError::NothingToFix(fix))
    };
    let (from, to) = (copy(source)?, copy(1 - source)?);
    let mut sectors = 0;
    for &(start, length) in &copies.differing {
        for index in 0..length {
            let byte = patch.get(from + start + index)?;
            patch.set(to + start + index, byte)?;
        }
        sectors += length / layout.bytes_per_sector.max(1);
    }
    Ok(FixCount {
        fix,
        items: sectors,
        clusters: 0,
    })
}

/// 改一处目录项。exFAT 的这组项记进 `sets`，最后重算校验和。
fn edit_entry(
    patch: &mut Patch,
    layout: &FatLayout,
    change: &EntryChange,
    sets: &mut BTreeSet<Vec<u64>>,
) -> Result<(), FsFixError> {
    match change {
        EntryChange::ExfatSet {
            entries,
            count,
            name_length,
            valid_length,
        } => {
            let (&primary, &stream) = (
                entries.first().ok_or(FsFixError::Unreadable(0))?,
                entries.get(1).ok_or(FsFixError::Unreadable(0))?,
            );
            if let Some(count) = count {
                patch.set(primary + 1, *count)?;
            }
            if let Some(length) = name_length {
                patch.set(stream + 3, *length)?;
            }
            if let Some(length) = valid_length {
                patch.set_le(stream + 8, 8, *length)?;
            }
            sets.insert(entries.clone());
        }
        EntryChange::Delete { at, exfat } => {
            for &at in at {
                let first = patch.get(at)?;
                patch.set(at, if *exfat { first & 0x7F } else { FAT_DELETED })?;
            }
        }
        EntryChange::Dots {
            at,
            cluster,
            parent,
        } => {
            write_dot(patch, layout, *at, b".          ", *cluster)?;
            write_dot(patch, layout, at + ENTRY, b"..         ", *parent)?;
        }
    }
    Ok(())
}

/// 写一个 `.` 或 `..` 目录项：名字、目录属性、起始簇，其余为 0。
fn write_dot(
    patch: &mut Patch,
    layout: &FatLayout,
    at: u64,
    name: &[u8; 11],
    cluster: u32,
) -> Result<(), FsFixError> {
    let mut raw = [0u8; ENTRY as usize];
    raw[..11].copy_from_slice(name);
    raw[11] = ATTR_DIRECTORY;
    if layout.width == FatWidth::Fat32 {
        raw[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    }
    raw[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    for (index, &byte) in raw.iter().enumerate() {
        patch.set(at + index as u64, byte)?;
    }
    Ok(())
}

/// 改文件大小：FAT 是目录项第 28 字节起的 4 字节；exFAT 是流扩展项的数据长度，有效长度不超过它。
fn set_size(
    patch: &mut Patch,
    at: &EntryAt,
    size: u64,
    sets: &mut BTreeSet<Vec<u64>>,
) -> Result<(), FsFixError> {
    match at {
        EntryAt::Fat(entry) => patch.set_le(entry + 28, 4, size.min(u64::from(u32::MAX))),
        EntryAt::Exfat(entries) => {
            let stream = *entries.get(1).ok_or(FsFixError::Unreadable(0))?;
            patch.set_le(stream + 24, 8, size)?;
            let valid = patch.get_le(stream + 8, 8)?;
            patch.set_le(stream + 8, 8, valid.min(size))?;
            sets.insert(entries.clone());
            Ok(())
        }
    }
}

/// 改起始簇：FAT 是目录项第 26 字节（FAT32 另有第 20 字节的高 16 位）；exFAT 是流扩展项第 20 字节。
fn set_first(
    patch: &mut Patch,
    layout: &FatLayout,
    at: &EntryAt,
    cluster: u32,
    sets: &mut BTreeSet<Vec<u64>>,
) -> Result<(), FsFixError> {
    match at {
        EntryAt::Fat(entry) => {
            patch.set_le(entry + 26, 2, u64::from(cluster & 0xFFFF))?;
            if layout.width == FatWidth::Fat32 {
                patch.set_le(entry + 20, 2, u64::from(cluster >> 16))?;
            }
            Ok(())
        }
        EntryAt::Exfat(entries) => {
            let stream = *entries.get(1).ok_or(FsFixError::Unreadable(0))?;
            patch.set_le(stream + 20, 4, u64::from(cluster))?;
            sets.insert(entries.clone());
            Ok(())
        }
    }
}

/// 按改后的内容重算一组 exFAT 目录项的校验和（次项个数以改后的文件项为准）。
fn reseal_set(patch: &mut Patch, entries: &[u64]) -> Result<(), FsFixError> {
    let primary = *entries.first().ok_or(FsFixError::Unreadable(0))?;
    let count = usize::from(patch.get(primary + 1)?);
    let mut raws = Vec::with_capacity(count + 1);
    for &at in entries.iter().take(count + 1) {
        let mut raw = [0u8; ENTRY as usize];
        for (index, byte) in raw.iter_mut().enumerate() {
            *byte = patch.get(at + index as u64)?;
        }
        raws.push(raw);
    }
    let checksum = exfat_set_checksum(&raws);
    patch.set_le(primary + 2, 2, u64::from(checksum))
}

/// 修交叉链接时从这里按簇号从小到大取空闲簇（检查得出的真正空闲的簇），用之前再核对一次表项与位图。
struct FreeClusters<'a> {
    runs: std::slice::Iter<'a, Run>,
    current: Option<(u32, u32)>,
}

impl<'a> FreeClusters<'a> {
    fn new(runs: &'a [Run]) -> Self {
        Self {
            runs: runs.iter(),
            current: None,
        }
    }

    fn take(&mut self, patch: &mut Patch, layout: &FatLayout) -> Result<u32, FsFixError> {
        loop {
            let (next, end) = match self.current {
                Some((next, end)) if next < end => (next, end),
                _ => {
                    let run = self.runs.next().ok_or(FsFixError::NoFreeClusters)?;
                    (run.first, run.first + run.count)
                }
            };
            self.current = Some((next + 1, end));
            // 表项为 0、位图里空闲（exFAT；位图本身坏了、正在重建时不看）才用
            if patch.fat_value(layout, next)? == 0
                && (layout.kind != FsKind::Exfat
                    || layout.bitmap.is_empty()
                    || !patch.bitmap_get(layout, next)?)
            {
                return Ok(next);
            }
        }
    }
}

/// 簇 `cluster` 在分区里的位置。
fn cluster_at(layout: &FatLayout, cluster: u32) -> u64 {
    layout.heap_offset + u64::from(cluster.saturating_sub(2)) * layout.cluster_size
}

/// 把 `data` 写进按顺序的这些簇（不满一簇的部分补 0），并把它们连成一条 FAT 链。
fn write_chain(
    patch: &mut Patch,
    layout: &FatLayout,
    clusters: &[u32],
    data: &[u8],
) -> Result<(), FsFixError> {
    for (index, &cluster) in clusters.iter().enumerate() {
        let at = cluster_at(layout, cluster);
        let start = index as u64 * layout.cluster_size;
        for byte in 0..layout.cluster_size {
            let value = data.get((start + byte) as usize).copied().unwrap_or(0);
            patch.set(at + byte, value)?;
        }
        let next = clusters
            .get(index + 1)
            .copied()
            .unwrap_or(layout.width.end_of_chain());
        patch.fat(layout, cluster, next)?;
    }
    Ok(())
}

/// 根目录里补一个系统目录项（位图项、大写表项）：类型、首簇、长度，另带大写表的校验和。
fn put_system_entry(
    patch: &mut Patch,
    at: u64,
    kind: u8,
    first: u32,
    length: u64,
    checksum: Option<u32>,
) -> Result<(), FsFixError> {
    for byte in 0..ENTRY {
        patch.set(at + byte, 0)?;
    }
    patch.set(at, kind)?;
    if let Some(checksum) = checksum {
        patch.set_le(at + 4, 4, u64::from(checksum))?;
    }
    patch.set_le(at + 20, 4, u64::from(first))?;
    patch.set_le(at + 24, 8, length)
}

/// 写回标准大写表。原来的簇够用就写回原处，否则另取空闲簇、释放原来的；表项改指它（找不到表项时补一项）。
fn write_upcase(
    patch: &mut Patch,
    layout: &FatLayout,
    findings: &Findings,
    free: &mut FreeClusters,
) -> Result<FixCount, FsFixError> {
    let fix = FsFix::Upcase;
    let upcase = findings
        .upcase
        .as_ref()
        .filter(|_| layout.kind == FsKind::Exfat)
        .ok_or(FsFixError::NothingToFix(fix))?;
    // 内容就是标准表：只改记的校验和
    if let (Some(entry), true) = (upcase.entry, upcase.standard) {
        patch.set_le(entry + 4, 4, u64::from(EXFAT_UPCASE_CHECKSUM))?;
        return Ok(FixCount {
            fix,
            items: 1,
            clusters: 0,
        });
    }
    if !findings.tracked {
        return Err(FsFixError::NotTracked);
    }
    let needed = (EXFAT_UPCASE_TABLE.len() as u64).div_ceil(layout.cluster_size.max(1)) as usize;
    let old: Vec<u32> = clusters_of(&upcase.runs).collect();
    let mut clusters: Vec<u32> = old.iter().copied().take(needed).collect();
    while clusters.len() < needed {
        let fresh = free.take(patch, layout)?;
        patch.bitmap(layout, fresh, true)?;
        clusters.push(fresh);
    }
    write_chain(patch, layout, &clusters, EXFAT_UPCASE_TABLE)?;
    // 原来的簇用不上的释放
    for &cluster in old.iter().filter(|cluster| !clusters.contains(cluster)) {
        patch.fat(layout, cluster, 0)?;
        patch.bitmap(layout, cluster, false)?;
    }
    let first = *clusters.first().ok_or(FsFixError::NoFreeClusters)?;
    let length = EXFAT_UPCASE_TABLE.len() as u64;
    match upcase.entry {
        Some(entry) => {
            patch.set_le(entry + 4, 4, u64::from(EXFAT_UPCASE_CHECKSUM))?;
            patch.set_le(entry + 20, 4, u64::from(first))?;
            patch.set_le(entry + 24, 8, length)?;
        }
        None => {
            let slot = findings.root_slot.ok_or(FsFixError::NoRootSlot)?;
            put_system_entry(
                patch,
                slot,
                EXFAT_TYPE_UPCASE,
                first,
                length,
                Some(EXFAT_UPCASE_CHECKSUM),
            )?;
        }
    }
    Ok(FixCount {
        fix,
        items: 1,
        clusters: needed as u64,
    })
}

/// 按目录树重建分配位图：有人认领的簇与新位图自己的簇标成占用，写到空闲簇上；位图项改指新簇（找不到时补一项），
/// 旧位图的簇释放。
fn rebuild_bitmap(
    patch: &mut Patch,
    layout: &FatLayout,
    findings: &Findings,
    free: &mut FreeClusters,
) -> Result<FixCount, FsFixError> {
    let fix = FsFix::RebuildBitmap;
    let rebuild = findings
        .rebuild
        .as_ref()
        .filter(|_| layout.kind == FsKind::Exfat)
        .ok_or(FsFixError::NothingToFix(fix))?;
    let length = u64::from(layout.cluster_count).div_ceil(8);
    let needed = length.div_ceil(layout.cluster_size.max(1)) as usize;
    let mut clusters = Vec::with_capacity(needed);
    while clusters.len() < needed {
        clusters.push(free.take(patch, layout)?);
    }
    let mut bits = vec![0u8; usize::try_from(length).unwrap_or(0)];
    let mut mark = |cluster: u32| {
        let bit = u64::from(cluster.saturating_sub(2));
        if let Some(byte) = bits.get_mut((bit / 8) as usize) {
            *byte |= 1 << (bit % 8);
        }
    };
    clusters_of(&rebuild.claimed).for_each(&mut mark);
    clusters.iter().copied().for_each(&mut mark);
    write_chain(patch, layout, &clusters, &bits)?;
    for cluster in clusters_of(&rebuild.old).filter(|cluster| !clusters.contains(cluster)) {
        patch.fat(layout, cluster, 0)?;
    }
    let first = *clusters.first().ok_or(FsFixError::NoFreeClusters)?;
    match rebuild.entry {
        Some(entry) => {
            // 第 1 字节是位图标志：第 0 位为 0 表示第一份位图
            patch.set(entry + 1, 0)?;
            patch.set_le(entry + 20, 4, u64::from(first))?;
            patch.set_le(entry + 24, 8, length)?;
        }
        None => {
            let slot = findings.root_slot.ok_or(FsFixError::NoRootSlot)?;
            put_system_entry(patch, slot, EXFAT_TYPE_BITMAP, first, length, None)?;
        }
    }
    Ok(FixCount {
        fix,
        items: 1,
        clusters: needed as u64,
    })
}

/// 交叉链接：把 `cross` 这个文件与别人共用的簇复制到空闲簇，链改指新簇（首簇变了时改目录项）。返回复制的簇数。
fn copy_shared(
    patch: &mut Patch,
    layout: &FatLayout,
    cross: &crate::check::CrossLinked,
    free: &mut FreeClusters,
    sets: &mut BTreeSet<Vec<u64>>,
) -> Result<u64, FsFixError> {
    let mut chain = cross.chain.clone();
    for &index in &cross.shared {
        let Some(slot) = chain.get_mut(index) else {
            continue;
        };
        let fresh = free.take(patch, layout)?;
        let from = layout.heap_offset + u64::from(*slot - 2) * layout.cluster_size;
        let to = layout.heap_offset + u64::from(fresh - 2) * layout.cluster_size;
        for byte in 0..layout.cluster_size {
            let value = patch.get(from + byte)?;
            patch.set(to + byte, value)?;
        }
        *slot = fresh;
        if layout.kind == FsKind::Exfat {
            patch.bitmap(layout, fresh, true)?;
        }
    }
    // 换过的簇与指向它的前一个簇都要重写表项；共用的原簇是别人的，不动
    for (index, &cluster) in chain.iter().enumerate() {
        let changed = cross.chain.get(index) != Some(&cluster);
        let next_changed = chain
            .get(index + 1)
            .is_some_and(|next| cross.chain.get(index + 1) != Some(next));
        if changed || next_changed {
            let value = chain
                .get(index + 1)
                .copied()
                .unwrap_or(layout.width.end_of_chain());
            patch.fat(layout, cluster, value)?;
        }
    }
    if let (Some(&first), Some(&old)) = (chain.first(), cross.chain.first())
        && first != old
    {
        set_first(patch, layout, &cross.at, first, sets)?;
    }
    Ok(cross.shared.len() as u64)
}

/// 坏簇：`bad` 里仍然读不出、又确实空闲的簇在 FAT 里标成坏簇（exFAT 另在位图里标成占用）。
fn mark_bad(
    patch: &mut Patch,
    layout: &FatLayout,
    findings: &Findings,
    bad: &[Run],
    device: &dyn BlockDevice,
    offset: u64,
) -> Result<FixCount, FsFixError> {
    let is_free = |cluster: u32| {
        findings
            .free
            .iter()
            .any(|run| (run.first..run.first + run.count).contains(&cluster))
    };
    let mut buf = vec![0u8; usize::try_from(layout.cluster_size).unwrap_or(0)];
    let (mut runs, mut clusters) = (Vec::new(), 0u64);
    for cluster in clusters_of(bad) {
        if cluster < 2 || cluster - 2 >= layout.cluster_count || !is_free(cluster) {
            continue;
        }
        if patch.fat_value(layout, cluster)? != 0 {
            continue;
        }
        let at = offset + layout.heap_offset + u64::from(cluster - 2) * layout.cluster_size;
        if device.read_exact_at(at, &mut buf).is_ok() {
            // 这次读得出：不是坏簇（或者已经好了），不标
            continue;
        }
        patch.fat(layout, cluster, layout.width.bad_cluster())?;
        if layout.kind == FsKind::Exfat {
            patch.bitmap(layout, cluster, true)?;
        }
        yisi_fs::format::push_run(&mut runs, cluster);
        clusters += 1;
    }
    Ok(FixCount {
        fix: FsFix::BadClusters,
        items: runs.len() as u64,
        clusters,
    })
}

/// 预演：叠加写入后再检查一遍。一修就是全部的几类必须都没了；其余几类至少少掉计划修的那么多；
/// 别的问题不能变多（统一两份 FAT 时不比，结论本来就会整个变；标坏簇时坏簇数当然会变多）；
/// 清除卸下标记时不能还有严重的问题。
fn rehearse(
    device: &Arc<dyn BlockDevice>,
    offset: u64,
    length: u64,
    fixes: &[FsFix],
    before: &Report,
    writes: &[SectorWrite],
    planned: &[IssueKind],
) -> Result<(), FsFixError> {
    let overlay: Arc<dyn BlockDevice> = Arc::new(Overlay::new(Arc::clone(device), writes));
    let (after, _, _) = inspect(&overlay, offset, length)?;
    let count = |report: &Report, kind: IssueKind| {
        report
            .issues
            .iter()
            .filter(|issue| issue.kind == kind)
            .count()
    };
    for fix in fixes.iter().filter(|fix| fix.complete()) {
        if let Some(&kind) = fix.kinds().iter().find(|&&kind| count(&after, kind) > 0) {
            return Err(FsFixError::Rehearsal(format!("仍有 {kind:?}")));
        }
    }
    let mut wanted: Vec<(IssueKind, usize)> = Vec::new();
    for &kind in planned {
        match wanted.iter_mut().find(|entry| entry.0 == kind) {
            Some(entry) => entry.1 += 1,
            None => wanted.push((kind, 1)),
        }
    }
    for &(kind, fixed) in &wanted {
        if count(&after, kind) + fixed > count(before, kind) {
            return Err(FsFixError::Rehearsal(format!(
                "{kind:?} 没有少掉计划修的 {fixed} 处"
            )));
        }
    }
    let targeted: Vec<IssueKind> = fixes
        .iter()
        .flat_map(|fix| fix.kinds().iter().copied())
        .collect();
    let unify = fixes.iter().any(|fix| fix.fat_source().is_some());
    let marks_bad = fixes.contains(&FsFix::BadClusters);
    if !unify
        && let Some(issue) = after.issues.iter().find(|issue| {
            !targeted.contains(&issue.kind)
                && !(marks_bad && issue.kind == IssueKind::BadClustersMarked)
                && count(&after, issue.kind) > count(before, issue.kind)
        })
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

    /// `at` 起 `len` 个字节的小端整数。
    fn get_le(&mut self, at: u64, len: u64) -> Result<u64, FsFixError> {
        let mut value = 0u64;
        for index in (0..len).rev() {
            value = (value << 8) | u64::from(self.get(at + index)?);
        }
        Ok(value)
    }

    fn set_le(&mut self, at: u64, len: u64, value: u64) -> Result<(), FsFixError> {
        for index in 0..len {
            self.set(at + index, (value >> (8 * index)) as u8)?;
        }
        Ok(())
    }

    /// 在用的那份（第一份）FAT 里表项 `cluster` 的值。
    fn fat_value(&mut self, layout: &FatLayout, cluster: u32) -> Result<u32, FsFixError> {
        let (at, len) = fat_entry_span(layout.width, cluster);
        let first = *layout.fats.first().ok_or(FsFixError::Unreadable(0))?;
        let mut bytes = [0u8; 4];
        for (index, slot) in bytes.iter_mut().take(len).enumerate() {
            *slot = self.get(first + at + index as u64)?;
        }
        Ok(fat_entry_raw(layout.width, cluster, &bytes).unwrap_or(u32::MAX))
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

    /// exFAT 分配位图里簇 `cluster` 那一位在分区里的位置与掩码。
    fn bitmap_at(&self, layout: &FatLayout, cluster: u32) -> Result<(u64, u8), FsFixError> {
        let bit = u64::from(cluster.saturating_sub(2));
        let mut byte = bit / 8;
        // 位图按簇存放：找出第 `byte` 个字节落在哪一段簇里
        for run in &layout.bitmap {
            let length = u64::from(run.count) * layout.cluster_size;
            if byte < length {
                let at = layout.heap_offset + u64::from(run.first - 2) * layout.cluster_size + byte;
                return Ok((at, 1u8 << (bit % 8)));
            }
            byte -= length;
        }
        Err(FsFixError::Unreadable(self.base))
    }

    fn bitmap_get(&mut self, layout: &FatLayout, cluster: u32) -> Result<bool, FsFixError> {
        let (at, mask) = self.bitmap_at(layout, cluster)?;
        Ok(self.get(at)? & mask != 0)
    }

    /// exFAT 分配位图里簇 `cluster` 那一位设为已占用或空闲。
    fn bitmap(&mut self, layout: &FatLayout, cluster: u32, used: bool) -> Result<(), FsFixError> {
        let (at, mask) = self.bitmap_at(layout, cluster)?;
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
                self.set(EXFAT_FLAGS, flags & !(EXFAT_FLAG_DIRTY as u8))
            }
            FatWidth::Fat16 | FatWidth::Fat32 => {
                let entry1 = self.fat_value(layout, 1)?;
                self.fat(layout, 1, entry1 | layout.width.clean_shutdown_bit())
            }
            // FAT12 没有这个标志
            FatWidth::Fat12 => Err(FsFixError::NothingToFix(FsFix::Dirty)),
        }
    }

    /// 改过的扇区按设备上的位置合并成连续的段；内容没变的扇区不写。
    fn writes(self) -> Vec<SectorWrite> {
        merge_writes(
            self.sectors
                .into_iter()
                .map(|(offset, (before, after))| SectorWrite {
                    offset,
                    before,
                    after,
                }),
        )
    }
}

#[cfg(test)]
mod tests;
