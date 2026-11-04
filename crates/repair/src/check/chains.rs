//! FAT 与 exFAT 共用的部分：沿 FAT 链认领每个文件的簇（越界、成环、断链、链长、交叉链接），
//! 读目录内容，最后逐簇找出丢失簇。

use std::collections::{HashMap, HashSet};

use yisi_fs::format::cluster_offset;
pub(crate) use yisi_fs::format::{Run, push_run};

use crate::check::disk::Span;
use crate::check::issue::{Area, Issue, IssueKind};
use crate::check::table::{FatTable, Link, Owners};
use crate::check::{BrokenChain, CheckItem, CrossLinked, Ctx, EntryRef, LongChain, Stage};

/// 一个目录最多读这么多字节（FAT 规范的上限是 2 MiB，exFAT 是 256 MiB）。
pub(crate) const MAX_DIR_BYTES: u64 = 256 << 20;
/// 目录最多嵌套这么多层。
pub(crate) const MAX_DEPTH: usize = 256;
/// 一条链每走这么多簇过一次检查点。
const STEP_CHECKPOINT: u64 = 1 << 16;
/// 丢失簇最多逐段列出这么多段，其余的合成一项。
const MAX_LOST_RUNS: usize = 100;

/// 目录内容在分区里的位置：FAT12/16 的根目录区，或者按顺序拼起来的一串簇。
#[derive(Clone, Copy)]
pub(crate) enum Place<'a> {
    Fixed(u64),
    Runs {
        runs: &'a [Run],
        count: u32,
        cluster_size: u64,
        heap_offset: u64,
    },
}

impl Place<'_> {
    /// 内容里第 `byte` 个字节在分区里的位置。
    pub fn locate(&self, byte: u64) -> Option<u64> {
        match *self {
            Self::Fixed(offset) => offset.checked_add(byte),
            Self::Runs {
                runs,
                count,
                cluster_size,
                heap_offset,
            } => {
                let mut rest = byte;
                for run in runs {
                    let length = u64::from(run.count) * cluster_size;
                    if rest < length {
                        return cluster_offset(run.first, count, cluster_size, heap_offset)?
                            .checked_add(rest);
                    }
                    rest -= length;
                }
                None
            }
        }
    }
}

/// 文件的簇怎么找。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    /// 沿 FAT 链；`needed` 是按大小应有的簇数，不知道大小（目录）时为 `None`。
    Chain { needed: Option<u64> },
    /// exFAT「不走 FAT 链」：从起始簇起连续 `needed` 个。
    Contiguous { needed: u64 },
}

/// 认领的结果。
pub(crate) struct Walk {
    /// 认领到的簇（只在要求时记下，用来读目录）。
    pub runs: Vec<Run>,
    /// 起始簇已被别的文件认领：目录不再往下查，免得绕回已经查过的地方。
    pub first_taken: bool,
}

/// FAT / exFAT 检查的共用状态。
pub(crate) struct Volume {
    pub fat: FatTable,
    pub owners: Owners,
    pub cluster_size: u64,
    /// 2 号簇在分区里的字节偏移。
    pub heap_offset: u64,
    /// 已认领的簇数与已占用的簇数，用于报告目录阶段的进度。
    pub claimed: u64,
    pub used: u64,
    pub files: u64,
    pub directories: u64,
}

impl Volume {
    pub fn cluster_offset(&self, cluster: u32) -> Option<u64> {
        cluster_offset(cluster, self.fat.count, self.cluster_size, self.heap_offset)
    }

    pub fn clusters_for(&self, bytes: u64) -> u64 {
        bytes.div_ceil(self.cluster_size)
    }

    /// 目录阶段的进度。
    pub fn report_progress(&self, ctx: &Ctx) {
        let total = self.used.max(1);
        ctx.control
            .progress(Stage::Directories, self.claimed.min(total), total);
    }

    /// 目录阶段结束：报一次完成，各阶段的最后一次进度总会送到界面。
    pub fn finish_directories(&self, ctx: &Ctx) {
        let total = self.used.max(1);
        ctx.control.progress(Stage::Directories, total, total);
    }

    /// 认领 `id` 的簇，发现的问题记在 `path` 名下；`entry` 是它的目录项（系统结构没有），修复要用。
    /// 被取消时返回 `None`。
    #[allow(clippy::too_many_arguments)]
    pub fn walk(
        &mut self,
        ctx: &mut Ctx,
        id: u32,
        path: &str,
        first: u32,
        layout: Layout,
        keep_runs: bool,
        entry: Option<&EntryRef>,
    ) -> Option<Walk> {
        let mut walk = Walk {
            runs: Vec::new(),
            first_taken: false,
        };
        let needed = match layout {
            Layout::Chain { needed } => needed,
            Layout::Contiguous { needed } => Some(needed),
        };
        if needed == Some(0) && first == 0 {
            return Some(walk);
        }
        if !self.fat.is_valid(first) {
            ctx.issues.push(
                Issue::new(IssueKind::FirstClusterInvalid)
                    .path(path)
                    .actual(u64::from(first)),
            );
            self.broken(
                ctx,
                IssueKind::FirstClusterInvalid,
                path,
                entry,
                None,
                0,
                true,
            );
            return Some(walk);
        }
        let mut claim = Claim::new(id);
        let mut steps = 0u64;
        let mut cluster = first;
        let mut previous = None;
        // 修复要用：整条链（按顺序）、与别人共用的位置；链过长时按大小应保留的最后一簇与之后只属于这个文件的簇
        let mut chain = Vec::new();
        let mut shared = Vec::new();
        let mut last_kept = None;
        let mut extra = Vec::new();
        // 链中途出了问题（已记下）：链长就不再另算；能修时记下修法（问题类型、在哪写链尾、保留几个簇）
        let mut broken = false;
        let mut cut: Option<(IssueKind, Option<u32>, u64)> = None;
        loop {
            let taken = claim.take(self, ctx, path, cluster);
            let mine = matches!(taken, Taken::Mine);
            if entry.is_some() {
                chain.push(cluster);
            }
            match taken {
                Taken::Mine => {}
                Taken::Shared => {
                    if steps == 0 {
                        walk.first_taken = true;
                    }
                    shared.push(chain.len().saturating_sub(1));
                }
                Taken::Loop => broken = true,
            }
            steps += 1;
            if broken {
                // 前一个簇指回了走过的簇：在前一个簇处结束
                cut = Some((IssueKind::ChainLoop, previous, steps - 1));
            }
            if let Layout::Chain {
                needed: Some(needed),
            } = layout
            {
                if steps == needed {
                    last_kept = Some(cluster);
                } else if steps > needed && mine {
                    extra.push(cluster);
                }
            }
            self.claimed = self.claimed.saturating_add(1);
            if keep_runs {
                push_run(&mut walk.runs, cluster);
            }
            if steps.is_multiple_of(STEP_CHECKPOINT) {
                self.report_progress(ctx);
                if !ctx.control.checkpoint() {
                    return None;
                }
            }
            if broken {
                break;
            }
            match layout {
                Layout::Contiguous { needed } => {
                    if steps >= needed {
                        break;
                    }
                    match cluster
                        .checked_add(1)
                        .filter(|&next| self.fat.is_valid(next))
                    {
                        Some(next) => cluster = next,
                        None => {
                            ctx.issues.push(
                                Issue::new(IssueKind::ChainOutOfRange)
                                    .path(path)
                                    .cluster(cluster)
                                    .actual(u64::from(cluster) + 1),
                            );
                            broken = true;
                            cut = Some((IssueKind::ChainOutOfRange, None, steps));
                            break;
                        }
                    }
                }
                Layout::Chain { .. } => {
                    // 链比簇总数还长，一定绕进了别人的环里（自己的环在认领时就发现了）
                    if steps > u64::from(self.fat.count) {
                        ctx.issues
                            .push(Issue::new(IssueKind::ChainLoop).path(path).cluster(cluster));
                        broken = true;
                        cut = Some((IssueKind::ChainLoop, Some(cluster), steps));
                        break;
                    }
                    let (issue, fix) = match self.fat.link(ctx, cluster) {
                        Link::Next(next) => {
                            previous = Some(cluster);
                            cluster = next;
                            continue;
                        }
                        Link::End => break,
                        // 表项读不出：已记为读不出的区段，链长无从核对，也不修
                        Link::Unknown => {
                            broken = true;
                            break;
                        }
                        // 表项是空闲或不合法的值：这个簇留着，在它这里结束
                        Link::Free => (
                            Issue::new(IssueKind::ChainFree).cluster(cluster),
                            (IssueKind::ChainFree, Some(cluster), steps),
                        ),
                        Link::Invalid(value) => (
                            Issue::new(IssueKind::ChainOutOfRange)
                                .cluster(cluster)
                                .actual(u64::from(value)),
                            (IssueKind::ChainOutOfRange, Some(cluster), steps),
                        ),
                        // 走进了标成坏簇的簇：不要它，在前一个簇处结束
                        Link::Bad => (
                            Issue::new(IssueKind::ChainBadCluster).cluster(cluster),
                            (IssueKind::ChainBadCluster, previous, steps - 1),
                        ),
                    };
                    ctx.issues.push(issue.path(path));
                    broken = true;
                    cut = Some(fix);
                    break;
                }
            }
        }
        claim.finish(self, ctx, path);
        if let Some((kind, end_at, kept)) = cut {
            // 连续存放的文件没有 FAT 链，只改大小
            let end_at = end_at.filter(|_| matches!(layout, Layout::Chain { .. }));
            self.broken(ctx, kind, path, entry, end_at, kept, kept == 0);
        }
        if let (Some(needed), false) = (needed, broken) {
            if steps < needed {
                ctx.issues.push(
                    Issue::new(IssueKind::ChainTooShort)
                        .path(path)
                        .expected(needed)
                        .actual(steps),
                );
                self.broken(
                    ctx,
                    IssueKind::ChainTooShort,
                    path,
                    entry,
                    None,
                    steps,
                    false,
                );
            } else if steps > needed {
                ctx.issues.push(
                    Issue::new(IssueKind::ChainTooLong)
                        .path(path)
                        .expected(needed)
                        .actual(steps),
                );
                if let (Some(last), Layout::Chain { .. }) = (last_kept, layout) {
                    ctx.findings.too_long.push(LongChain {
                        path: path.to_owned(),
                        last,
                        extra,
                    });
                }
            }
        }
        if let Some(entry) = entry
            && !shared.is_empty()
            && !broken
            && !entry.directory
            && matches!(layout, Layout::Chain { .. })
        {
            ctx.findings.cross.push(CrossLinked {
                path: path.to_owned(),
                at: entry.at.clone(),
                chain,
                shared,
            });
        }
        Some(walk)
    }

    /// 记下断链的修法：保留 `kept` 个簇，在 `end_at` 写链尾。没有目录项（系统结构）、FAT 的目录要清空时不修。
    #[allow(clippy::too_many_arguments)]
    fn broken(
        &self,
        ctx: &mut Ctx,
        kind: IssueKind,
        path: &str,
        entry: Option<&EntryRef>,
        end_at: Option<u32>,
        kept: u64,
        empty: bool,
    ) {
        let Some(entry) = entry else {
            return;
        };
        // FAT 的目录没有大小，清空不了（起始簇为 0 表示根目录）
        if empty && entry.size.is_none() {
            return;
        }
        ctx.findings.broken.push(BrokenChain {
            kind,
            path: path.to_owned(),
            at: entry.at.clone(),
            end_at: end_at.filter(|_| !empty),
            size: entry
                .size
                .map(|size| size.min(kept.saturating_mul(self.cluster_size))),
            empty,
        });
    }

    /// 目录内容里第 `byte` 个字节在分区里的位置（[`Place`]）。
    pub fn locate(&self, runs: &[Run], byte: u64) -> Option<u64> {
        self.place(runs).locate(byte)
    }

    pub fn place<'a>(&self, runs: &'a [Run]) -> Place<'a> {
        Place::Runs {
            runs,
            count: self.fat.count,
            cluster_size: self.cluster_size,
            heap_offset: self.heap_offset,
        }
    }

    /// 读一串簇的内容（至多 [`MAX_DIR_BYTES`]），返回内容与读不出的区段（相对内容起点）。
    pub fn read_runs(&self, ctx: &mut Ctx, runs: &[Run], path: &str) -> (Vec<u8>, Vec<Span>) {
        let mut data = Vec::new();
        let mut bad = Vec::new();
        for run in runs {
            let Some(offset) = self.cluster_offset(run.first) else {
                break;
            };
            let room = MAX_DIR_BYTES.saturating_sub(data.len() as u64);
            let length = (u64::from(run.count) * self.cluster_size).min(room);
            if length == 0 {
                break;
            }
            let start = data.len();
            data.resize(start + length as usize, 0);
            let Some(target) = data.get_mut(start..) else {
                break;
            };
            match ctx.read(offset, target, Area::Directory, Some(path)) {
                Some(spans) => bad.extend(spans.into_iter().map(|span| Span {
                    offset: span.offset + start as u64,
                    length: span.length,
                })),
                // 簇在分区之外（分区被截短）：当作读不出，已由引导区检查报告
                None => bad.push(Span {
                    offset: start as u64,
                    length,
                }),
            }
        }
        (data, bad)
    }

    /// 逐簇汇总：`allocated` 说明一个簇在 FAT 或分配位图里是否已占用（读不出时为 `None`），`marked_bad` 说明
    /// 已占用、没人认领的簇是不是标成了坏簇（exFAT 的坏簇在位图里占用）。已占用却没人认领的是丢失簇；
    /// `check_bitmap` 时，有人认领却未占用的按文件汇总成「不在位图里」。
    pub fn sweep(
        &mut self,
        ctx: &mut Ctx,
        allocated: &mut dyn FnMut(&mut Ctx, &mut FatTable, u32) -> Option<bool>,
        marked_bad: &mut dyn FnMut(&mut Ctx, &mut FatTable, u32) -> bool,
        check_bitmap: bool,
    ) -> Option<()> {
        if !self.owners.tracking() {
            ctx.skipped(CheckItem::LostClusters);
            ctx.skipped(CheckItem::CrossLinks);
            if check_bitmap {
                ctx.skipped(CheckItem::Bitmap);
            }
            return Some(());
        }
        let total = u64::from(self.fat.count);
        let mut lost: Vec<Run> = Vec::new();
        // 认领者 → 第一个不在位图里的簇与个数，按发现的顺序
        let mut missing: Vec<(u32, u32, u64)> = Vec::new();
        let mut missing_at: HashMap<u32, usize> = HashMap::new();
        for index in 0..self.fat.count {
            let cluster = index + 2;
            if u64::from(index).is_multiple_of(STEP_CHECKPOINT) {
                ctx.control
                    .progress(Stage::Clusters, u64::from(index), total);
                if !ctx.control.checkpoint() {
                    return None;
                }
            }
            let Some(used) = allocated(ctx, &mut self.fat, cluster) else {
                continue;
            };
            match (used, self.owners.owner(cluster)) {
                // 占用而标成坏簇的（exFAT 标坏簇时位图里也占用）：不算丢失
                (true, None) if marked_bad(ctx, &mut self.fat, cluster) => {}
                (true, None) => push_run(&mut lost, cluster),
                (false, None) => push_run(&mut ctx.findings.free, cluster),
                (false, Some(owner)) if check_bitmap => {
                    push_run(&mut ctx.findings.not_in_bitmap, cluster);
                    match missing_at.get(&owner) {
                        Some(&at) => {
                            if let Some(entry) = missing.get_mut(at) {
                                entry.2 += 1;
                            }
                        }
                        None => {
                            missing_at.insert(owner, missing.len());
                            missing.push((owner, cluster, 1));
                        }
                    }
                }
                _ => {}
            }
        }
        ctx.control.progress(Stage::Clusters, total, total);
        ctx.findings.tracked = true;
        ctx.findings.lost = lost.clone();
        for (owner, cluster, count) in missing {
            ctx.issues.push(
                Issue::new(IssueKind::NotInBitmap)
                    .path(self.owners.path(owner))
                    .cluster(cluster)
                    .count(count),
            );
        }
        for run in lost.iter().take(MAX_LOST_RUNS) {
            ctx.issues.push(
                Issue::new(IssueKind::LostClusters)
                    .cluster(run.first)
                    .count(u64::from(run.count)),
            );
        }
        if lost.len() > MAX_LOST_RUNS {
            let rest = lost.get(MAX_LOST_RUNS..).unwrap_or_default();
            let clusters = rest.iter().map(|run| u64::from(run.count)).sum();
            ctx.issues.push(
                Issue::new(IssueKind::LostClustersMore)
                    .count(clusters)
                    .actual(rest.len() as u64),
            );
        }
        ctx.checked(CheckItem::LostClusters);
        ctx.checked(CheckItem::CrossLinks);
        if check_bitmap {
            ctx.checked(CheckItem::Bitmap);
        }
        Some(())
    }
}

enum Taken {
    Mine,
    /// 已被别的文件认领：交叉链接。
    Shared,
    Loop,
}

/// 一个文件认领簇的过程：发现与别人共用的簇时按对方汇总，最后各记一项交叉链接。
struct Claim {
    id: u32,
    /// 与哪个文件共用：第一个共用的簇与共用的簇数，按发现的顺序。
    shared: Vec<(u32, u32, u64)>,
    /// 走过的、属于别人的簇：再次走到就是在别人的链里绕圈。
    foreign: HashSet<u32>,
}

impl Claim {
    fn new(id: u32) -> Self {
        Self {
            id,
            shared: Vec::new(),
            foreign: HashSet::new(),
        }
    }

    /// 认领一个簇。别人的簇记作交叉链接，链照常往下走（合并进别人链里的部分也算这个文件的）；
    /// 走回自己认领过的簇，或者再次走到同一个别人的簇，就是成环。
    fn take(&mut self, volume: &mut Volume, ctx: &mut Ctx, path: &str, cluster: u32) -> Taken {
        // 不跟踪认领时只能靠步数上限发现成环
        if !volume.owners.tracking() {
            return Taken::Mine;
        }
        match volume.owners.owner(cluster) {
            None => {
                volume.owners.set(cluster, self.id);
                Taken::Mine
            }
            Some(owner) if owner != self.id && self.foreign.insert(cluster) => {
                match self.shared.iter_mut().find(|entry| entry.0 == owner) {
                    Some(entry) => entry.2 += 1,
                    None => self.shared.push((owner, cluster, 1)),
                }
                Taken::Shared
            }
            Some(_) => {
                ctx.issues
                    .push(Issue::new(IssueKind::ChainLoop).path(path).cluster(cluster));
                Taken::Loop
            }
        }
    }

    fn finish(self, volume: &Volume, ctx: &mut Ctx, path: &str) {
        for (owner, cluster, count) in self.shared {
            ctx.issues.push(
                Issue::new(IssueKind::CrossLink)
                    .path(path)
                    .other_path(volume.owners.path(owner))
                    .cluster(cluster)
                    .count(count),
            );
        }
    }
}
