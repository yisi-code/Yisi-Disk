//! FAT / exFAT：找出原来所在的文件夹已找不到的已删除项。
//!
//! 沿目录树找得到的已删除文件夹，浏览时直接列得出；上级目录里记它的那一项已被清掉或覆盖时，它就从树上断了。
//! 这里读一遍目录树没用到的空闲簇，按内容认出目录（规则在 `fat` 与 `exfat` 各自的 `look`），相邻的连成一组，
//! 被别的组里的目录项指着的是那一组的子目录，其余的列进虚拟目录「找不到原目录的文件」（[`Location::Orphans`]）。
//! 每一组按已删除的目录打开，里面的项照常判断可信度、读出。

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use crate::clusters::{Clusters, Run};
use crate::entry::{ClusterRef, Entry, FileAttributes, FileRef, Listing, Location, Stamp, State};
use crate::{FsError, MAX_DIR_BYTES, MAX_ENTRIES, ScanSummary};

/// 沿目录树最多走这么多个目录（含已删除的）。更多时不再往下走，没走到的已删除文件夹可能在虚拟目录里重复出现一次。
const MAX_WALK_DIRS: usize = 100_000;
/// 最多认出这么多组目录内容，更多的不再收集（虚拟目录一次也只列 [`MAX_ENTRIES`] 项）。
const MAX_GROUPS: usize = 1_000_000;
/// 一次读这么多字节的连续空闲簇（簇比它大时一次读一个簇）。
const BLOCK: u64 = 1 << 20;
/// 数空闲簇、走目录树时，每这么多步问一次要不要停下。
const CHECK_EVERY: u32 = 1 << 16;
const CHECK_DIRS_EVERY: usize = 256;

/// 一个空闲簇的内容看起来是什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Look {
    /// 不像目录内容。
    Other,
    /// FAT 子目录的第一个簇：以 `.`、`..` 两项开头，`.` 指向这个簇自己。
    Head {
        /// 簇里有「此后再无目录项」的标记。
        ended: bool,
        /// 文件与子目录项的个数（不含 `.`、`..`）。
        entries: u32,
        /// `.` 项的修改时间（就是这个目录建立的时间）。
        modified: Option<Stamp>,
    },
    /// 整簇都是格式正确的目录项。`strong` 为 `false` 时证据不足（FAT 只有一项），只能接在相邻的目录内容后面。
    Body {
        ended: bool,
        entries: u32,
        strong: bool,
    },
}

/// FAT 与 exFAT 各自提供的部分。
pub(crate) trait FreeSpace {
    fn clusters(&self) -> &Clusters;
    fn root(&self) -> FileRef;
    /// 列目录，并给出列它时读了哪些簇（FAT12/16 的根目录不在簇里，为空）。
    fn list_runs(&self, dir: &FileRef) -> Result<(Listing, Vec<Run>), FsError>;
    /// 簇在 FAT（exFAT 是分配位图）里是空闲的。
    fn is_free(&self, cluster: u32) -> Result<bool, FsError>;
    fn look(&self, cluster: u32, bytes: &[u8]) -> Look;
    /// 一组目录内容里各子目录占的簇（FAT 只知道首簇，给一个簇）。
    fn subdirs(&self, bytes: &[u8]) -> Vec<Run>;
}

/// 认出的一组目录内容：相邻的若干个空闲簇。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Group {
    pub first: u32,
    pub clusters: u32,
    /// 以 FAT 子目录的第一个簇开头。
    pub headed: bool,
    pub modified: Option<Stamp>,
}

impl Group {
    /// 在虚拟目录里的一项：按已删除的目录打开，长度是这一组的字节数（FAT 据此只读这几个簇，见 `fat::dir_contents`）。
    fn entry(&self, clusters: &Clusters) -> Entry {
        let size = u64::from(self.clusters)
            .saturating_mul(clusters.cluster_size)
            .min(MAX_DIR_BYTES);
        let file = FileRef {
            size,
            is_dir: true,
            deleted: true,
            location: Location::Clusters(ClusterRef {
                first_cluster: self.first,
                valid_size: size,
                contiguous: true,
            }),
            modified: self.modified,
        };
        Entry {
            // 原来的名字记在上级目录里，已经找不到了
            name: format!("dir-{}", self.first),
            short_name: None,
            name_uncertain: false,
            size: 0,
            modified: self.modified.and_then(|stamp| stamp.text()),
            created: None,
            // 只有 FAT 子目录的第一个簇能确认这是一个目录的开头；其余的可能只是目录的一段
            state: if self.headed {
                State::Intact
            } else {
                State::Unknown
            },
            position: clusters.offset(self.first),
            unsupported: None,
            attributes: FileAttributes::default(),
            file,
        }
    }
}

pub(crate) struct OrphanScan {
    pub summary: ScanSummary,
    /// 列进虚拟目录的组，按首簇排序。
    pub groups: Vec<Group>,
}

impl OrphanScan {
    /// 虚拟目录的内容。
    pub fn listing(&self, clusters: &Clusters) -> Listing {
        Listing {
            entries: self
                .groups
                .iter()
                .take(MAX_ENTRIES)
                .map(|group| group.entry(clusters))
                .collect(),
            truncated: self.groups.len() > MAX_ENTRIES,
            overwritten: false,
        }
    }
}

/// 一个卷的扫描结果：扫过一次之后复用；同一时间只扫一遍。
#[derive(Default)]
pub(crate) struct ScanSlot {
    result: Mutex<Option<Arc<OrphanScan>>>,
    running: Mutex<()>,
}

impl ScanSlot {
    pub fn get(&self) -> Option<Arc<OrphanScan>> {
        self.result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 扫一遍（扫过的直接给出上次的结果）。`progress` 返回 `false` 时停下，结果为 `None`，下次重扫。
    pub fn run(
        &self,
        volume: &dyn FreeSpace,
        progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<Option<ScanSummary>, FsError> {
        let _guard = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(done) = self.get() {
            return Ok(Some(done.summary));
        }
        let Some(result) = scan(volume, progress)? else {
            return Ok(None);
        };
        let summary = result.summary;
        *self.result.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(result));
        Ok(Some(summary))
    }

    /// 虚拟目录的内容；还没扫过时为空。
    pub fn listing(&self, clusters: &Clusters) -> Listing {
        self.get().map_or_else(
            || Listing {
                entries: Vec::new(),
                truncated: false,
                overwritten: false,
            },
            |scan| scan.listing(clusters),
        )
    }

    /// 根目录里列出「找不到原目录的文件」：扫过、且找到了东西。
    pub fn root_entry(&self) -> Option<Entry> {
        self.get()
            .filter(|scan| !scan.groups.is_empty())
            .map(|_| Entry::orphans_dir())
    }
}

/// 扫描的三步：沿目录树记下已删除文件夹占的簇；读其余的空闲簇、认出目录内容；分出哪些组是别的组的子目录。
fn scan(
    volume: &dyn FreeSpace,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Option<OrphanScan>, FsError> {
    let Some(used) = reachable_clusters(volume, progress)? else {
        return Ok(None);
    };
    let clusters = volume.clusters();
    let candidate = |cluster: u32| -> Result<bool, FsError> {
        Ok(!used.contains(&cluster) && volume.is_free(cluster)?)
    };
    let first = 2u32;
    let end = clusters.count.saturating_add(2);

    // 先数出要读多少，进度才有意义
    let mut free = 0u64;
    for cluster in first..end {
        if (cluster - first).is_multiple_of(CHECK_EVERY) && !progress(0, 0) {
            return Ok(None);
        }
        if candidate(cluster)? {
            free += 1;
        }
    }
    let cs = clusters.cluster_size;
    let total = free.saturating_mul(cs);
    let per_block = u32::try_from((BLOCK / cs).max(1)).unwrap_or(1);

    let mut grouper = Grouper::new(volume);
    let mut summary = ScanSummary::default();
    let mut done = 0u64;
    let mut cluster = first;
    let mut since_check = 0u32;
    while cluster < end && !grouper.full() {
        if !candidate(cluster)? {
            grouper.close();
            cluster += 1;
            since_check += 1;
            if since_check >= CHECK_EVERY {
                since_check = 0;
                if !progress(done, total) {
                    return Ok(None);
                }
            }
            continue;
        }
        // 从这里起连续的、要读的簇，一次读出
        let mut count = 1u32;
        while count < per_block
            && cluster.checked_add(count).is_some_and(|next| next < end)
            && candidate(cluster + count)?
        {
            count += 1;
        }
        let bytes = read_clusters(clusters, cluster, count);
        for step in 0..count {
            let at = cluster + step;
            let slice = bytes.as_ref().and_then(|bytes| {
                let start = usize::try_from(u64::from(step) * cs).ok()?;
                bytes.get(start..start.checked_add(usize::try_from(cs).ok()?)?)
            });
            let single;
            let content = match slice {
                Some(content) => Some(content),
                // 整段读不出时逐个簇重读，只放弃读不出的簇
                None => {
                    single = read_clusters(clusters, at, 1);
                    single.as_deref()
                }
            };
            match content {
                Some(content) => grouper.push(at, volume.look(at, content), content),
                None => {
                    summary.unreadable += 1;
                    grouper.close();
                }
            }
        }
        cluster += count;
        done = done.saturating_add(u64::from(count).saturating_mul(cs));
        if !progress(done.min(total), total) {
            return Ok(None);
        }
    }
    grouper.close();
    summary.free_bytes = done.min(total);
    let found = grouper.finish();
    summary.deleted_names = found.entries;
    let groups = top_level(found.groups, &found.links);
    summary.orphans = groups.len() as u64;
    Ok(Some(OrphanScan { summary, groups }))
}

/// 沿目录树（连同已删除、还列得出的文件夹）走一遍，记下已删除文件夹列目录时读的簇：这些簇浏览时已经看得到。
/// 未删除的目录占的簇在 FAT（分配位图）里已被占用，本来就不读。
fn reachable_clusters(
    volume: &dyn FreeSpace,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Option<HashSet<u32>>, FsError> {
    let mut used = HashSet::new();
    let root = volume.root();
    let mut seen = HashSet::new();
    if let Location::Clusters(at) = root.location {
        seen.insert(at.first_cluster);
    }
    let mut pending = vec![root];
    let mut walked = 0usize;
    while let Some(dir) = pending.pop() {
        if walked >= MAX_WALK_DIRS {
            break;
        }
        walked += 1;
        if walked.is_multiple_of(CHECK_DIRS_EVERY) && !progress(0, 0) {
            return Ok(None);
        }
        // 读不出的目录跳过：它下面的已删除文件夹也许会在虚拟目录里出现
        let Ok((listing, runs)) = volume.list_runs(&dir) else {
            continue;
        };
        if dir.deleted {
            for run in &runs {
                used.extend((0..run.count).filter_map(|step| run.first.checked_add(step)));
            }
        }
        for entry in listing.entries {
            if let Location::Clusters(at) = entry.file.location
                && entry.file.is_dir
                && seen.insert(at.first_cluster)
            {
                pending.push(entry.file);
            }
        }
    }
    Ok(Some(used))
}

/// 读从 `first` 起连续 `count` 个簇；读不出时为 `None`。
fn read_clusters(clusters: &Clusters, first: u32, count: u32) -> Option<Vec<u8>> {
    let offset = clusters.offset(first)?;
    let len = usize::try_from(u64::from(count).checked_mul(clusters.cluster_size)?).ok()?;
    let mut bytes = vec![0u8; len];
    clusters.region.read_exact(offset, &mut bytes).ok()?;
    Some(bytes)
}

/// 正在连的一组。
struct Open {
    group: Group,
    ended: bool,
    bytes: Vec<u8>,
    entries: u32,
}

struct Found {
    groups: Vec<Group>,
    /// 组 `.0` 里有指向组 `.1` 的子目录项。
    links: Vec<(usize, usize)>,
    /// 各组里文件与子目录项的总数。
    entries: u64,
}

/// 按簇号从小到大把认出的簇连成组：子目录的第一个簇另起一组；其余的接在紧挨着的、还没结束的组后面，接不上时
/// 证据足够才另起一组。
struct Grouper<'a> {
    volume: &'a dyn FreeSpace,
    open: Option<Open>,
    groups: Vec<Group>,
    /// 各组里子目录占的簇。
    subdirs: Vec<Vec<Run>>,
    entries: u64,
}

impl<'a> Grouper<'a> {
    fn new(volume: &'a dyn FreeSpace) -> Self {
        Self {
            volume,
            open: None,
            groups: Vec::new(),
            subdirs: Vec::new(),
            entries: 0,
        }
    }

    fn full(&self) -> bool {
        self.groups.len() >= MAX_GROUPS
    }

    fn push(&mut self, cluster: u32, look: Look, bytes: &[u8]) {
        let (ended, entries) = match look {
            Look::Other => {
                self.close();
                return;
            }
            Look::Head {
                ended,
                entries,
                modified,
            } => {
                self.close();
                self.start(cluster, true, modified);
                (ended, entries)
            }
            Look::Body {
                ended,
                entries,
                strong,
            } => {
                let continues = self.open.as_ref().is_some_and(|open| {
                    !open.ended
                        && open.group.first.checked_add(open.group.clusters) == Some(cluster)
                        && (open.bytes.len() as u64).saturating_add(bytes.len() as u64)
                            <= MAX_DIR_BYTES
                });
                if !continues {
                    self.close();
                    if !strong {
                        return;
                    }
                    self.start(cluster, false, None);
                }
                (ended, entries)
            }
        };
        if let Some(open) = &mut self.open {
            open.group.clusters += 1;
            open.ended = ended;
            open.entries = open.entries.saturating_add(entries);
            open.bytes.extend_from_slice(bytes);
        }
    }

    fn start(&mut self, cluster: u32, headed: bool, modified: Option<Stamp>) {
        self.open = Some(Open {
            group: Group {
                first: cluster,
                clusters: 0,
                headed,
                modified,
            },
            ended: false,
            bytes: Vec::new(),
            entries: 0,
        });
    }

    fn close(&mut self) {
        let Some(open) = self.open.take() else {
            return;
        };
        if self.full() {
            return;
        }
        self.entries = self.entries.saturating_add(u64::from(open.entries));
        self.subdirs.push(self.volume.subdirs(&open.bytes));
        self.groups.push(open.group);
    }

    fn finish(mut self) -> Found {
        self.close();
        let mut links = Vec::new();
        for (from, runs) in self.subdirs.iter().enumerate() {
            for run in runs {
                // 首簇落在这个子目录占的簇里的组（组按首簇排好了序）
                let end = u64::from(run.first) + u64::from(run.count);
                let start = self.groups.partition_point(|group| group.first < run.first);
                for (to, group) in self.groups.iter().enumerate().skip(start) {
                    if u64::from(group.first) >= end {
                        break;
                    }
                    if to != from {
                        links.push((from, to));
                    }
                }
            }
        }
        Found {
            groups: self.groups,
            links,
            entries: self.entries,
        }
    }
}

/// 列进虚拟目录的组：没有被别的组指着的；成环、谁也走不到的也列出来。
fn top_level(groups: Vec<Group>, links: &[(usize, usize)]) -> Vec<Group> {
    let mut nested = vec![false; groups.len()];
    for &(_, to) in links {
        if let Some(flag) = nested.get_mut(to) {
            *flag = true;
        }
    }
    let mut reached = nested.iter().map(|&flag| !flag).collect::<Vec<_>>();
    let mut pending: Vec<usize> = (0..groups.len())
        .filter(|&index| reached.get(index) == Some(&true))
        .collect();
    loop {
        while let Some(from) = pending.pop() {
            for &(source, to) in links {
                if source == from
                    && let Some(flag) = reached.get_mut(to)
                    && !*flag
                {
                    *flag = true;
                    pending.push(to);
                }
            }
        }
        // 只在环里的组：取环上最小的一个列出来，再接着走
        match reached.iter().position(|&flag| !flag) {
            Some(index) => {
                if let Some(flag) = nested.get_mut(index) {
                    *flag = false;
                }
                if let Some(flag) = reached.get_mut(index) {
                    *flag = true;
                }
                pending.push(index);
            }
            None => break,
        }
    }
    groups
        .into_iter()
        .zip(nested)
        .filter_map(|(group, nested)| (!nested).then_some(group))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(first: u32) -> Group {
        Group {
            first,
            clusters: 1,
            headed: true,
            modified: None,
        }
    }

    #[test]
    fn nested_groups_are_hidden_and_cycles_still_show_up() {
        let groups = vec![group(10), group(20), group(30), group(40), group(50)];
        // 10 → 20 → 30；40 ⇄ 50 成环
        let links = [(0, 1), (1, 2), (3, 4), (4, 3)];
        let top: Vec<u32> = top_level(groups, &links)
            .iter()
            .map(|group| group.first)
            .collect();
        assert_eq!(top, [10, 40]);
    }
}
