//! FAT12 / FAT16 / FAT32 的检查：引导扇区、备份引导扇区与 FSInfo、两份 FAT 是否一致、
//! 目录树（长文件名的校验和与序号、`.` 与 `..`）、每个文件的 FAT 链、丢失簇。依据微软公开的 FAT 规范。

use yisi_fs::FsKind;
use yisi_fs::boot_write::fat32_fs_info_ok;
use yisi_fs::bpb::{self, FatBpb, Problem};
use yisi_fs::format::{
    ATTR_DIRECTORY, ATTR_LONG_NAME as ATTR_LFN, ATTR_VOLUME_ID, DIR_ENTRY_SIZE as ENTRY,
    FAT_DELETED as DELETED, LfnPart, fat_first_cluster, fat_media_ok, lfn_name, lfn_ordered,
    lfn_part,
};
use yisi_fs::{fat_short_checksum as short_checksum, fat_short_display};
use yisi_part::bytes::le32;

use crate::check::chains::{Layout, MAX_DEPTH, Place, Volume};
use crate::check::disk::Span;
use crate::check::issue::{Area, Issue, IssueKind};
use crate::check::table::{FatTable, Link, Owners, ROOT};
use crate::check::{
    CheckItem, Ctx, EmptyDir, EntryAt, EntryChange, EntryFix, EntryRef, FatCopies, FatLayout,
    Stage, Summary, check_backup,
};

/// 比对两份 FAT 时每次读这么多。
const COMPARE_BLOCK: u64 = 1 << 20;

/// 解析引导扇区：规则在 [`yisi_fs::bpb`]（与打开卷、引导扇区诊断共用）。有致命问题时参数为 `None`。
fn parse(sector: &[u8]) -> (Option<FatBpb>, Vec<Problem>) {
    let checked = bpb::check_fat(sector, None);
    (checked.geometry, checked.problems)
}

/// 检查一个 FAT 卷。`sector` 是认出来的引导扇区（`from_backup` 时来自备份）。被取消时返回 `None`。
pub(crate) fn check(ctx: &mut Ctx, sector: &[u8], from_backup: bool) -> Option<Summary> {
    let (geometry, problems) = parse(sector);
    let area = if from_backup {
        Area::BackupBoot
    } else {
        Area::Boot
    };
    ctx.boot_problems(&problems, area);
    let Some(geometry) = geometry else {
        return Some(unknown());
    };
    ctx.checked(CheckItem::Boot);
    let cluster_size = geometry.cluster_size();
    let length = ctx.disk.length();

    let volume_bytes = geometry.volume_bytes();
    if volume_bytes > length {
        ctx.issues.push(
            Issue::new(IssueKind::VolumeTruncated)
                .area(Area::Boot)
                .expected(volume_bytes)
                .actual(length),
        );
    }
    // 簇数与打开卷时算的相同：受 FAT 能装下的表项数限制，分区被截短时只算到分区末尾
    let count = u32::try_from(geometry.usable_clusters(length)).unwrap_or(u32::MAX);
    let is_fat32 = geometry.kind == FsKind::Fat32;
    let fs = geometry.kind;

    if !from_backup {
        check_backup(ctx, geometry.kind, sector);
    }
    // FSInfo 里的空闲簇数只是提示，Windows 正常卸下的卷也常对不上，不核对，只查签名
    if is_fat32 {
        check_fs_info(ctx, &geometry);
    }

    let fs_info = (is_fat32 && geometry.fs_info != 0 && geometry.fs_info < geometry.reserved)
        .then(|| geometry.fs_info * geometry.bytes_per_sector);
    ctx.findings.from_backup = from_backup;
    ctx.findings.layout = Some(FatLayout {
        kind: fs,
        width: geometry.width(),
        bytes_per_sector: geometry.bytes_per_sector,
        fats: match geometry.active_fat {
            Some(copy) => vec![geometry.fat_offset(copy)],
            None => (0..geometry.fats)
                .map(|copy| geometry.fat_offset(copy))
                .collect(),
        },
        cluster_count: count,
        cluster_size,
        heap_offset: geometry.heap_offset(),
        fs_info,
        bitmap: Vec::new(),
    });
    let fat = FatTable::new(
        geometry.active_fat_offset(),
        geometry.fat_bytes(),
        geometry.width(),
        count,
    );
    let mut volume = Volume {
        fat,
        owners: Owners::new(u64::from(count) + 2),
        cluster_size,
        heap_offset: geometry.heap_offset(),
        claimed: 0,
        used: 0,
        files: 0,
        directories: 0,
    };
    if geometry.active_fat.is_none() && geometry.fats > 1 {
        compare_copies(ctx, &geometry, count)?;
    }
    scan_table(ctx, &mut volume, &geometry)?;

    let mut checker = FatChecker {
        volume,
        is_fat32,
        root_cluster: geometry.root_cluster,
    };
    let root = if is_fat32 {
        DirAt::Clusters(geometry.root_cluster)
    } else {
        DirAt::Fixed {
            offset: geometry.root_dir_offset(),
            length: geometry.root_entries * ENTRY as u64,
        }
    };
    checker.tree(ctx, root)?;
    checker.volume.finish_directories(ctx);
    ctx.checked(CheckItem::Chains);
    ctx.checked(CheckItem::Entries);

    let mut volume = checker.volume;
    volume.sweep(
        ctx,
        // 坏簇按占用算、再由下一条认出来：既不是丢失簇，也不算空闲
        &mut |ctx, fat, cluster| match fat.link(ctx, cluster) {
            Link::Unknown => None,
            link => Some(link.is_used() || link == Link::Bad),
        },
        &mut |ctx, fat, cluster| fat.link(ctx, cluster) == Link::Bad,
        false,
    )?;
    Some(Summary {
        fs,
        cluster_size,
        cluster_count: u64::from(count),
        used_clusters: volume.used,
        files: volume.files,
        directories: volume.directories,
    })
}

fn unknown() -> Summary {
    Summary {
        fs: FsKind::Unknown,
        cluster_size: 0,
        cluster_count: 0,
        used_clusters: 0,
        files: 0,
        directories: 0,
    }
}

/// FAT32 的 FSInfo 扇区（位置写在 BPB 里，为 0 表示没有）：三处签名要对。
fn check_fs_info(ctx: &mut Ctx, geometry: &FatBpb) -> Option<()> {
    let at = geometry.fs_info;
    if at == 0 || at == 0xFFFF || at >= geometry.reserved {
        return None;
    }
    let mut sector = vec![0u8; 512];
    let bad = ctx.read(
        at * geometry.bytes_per_sector,
        &mut sector,
        Area::FsInfo,
        None,
    )?;
    if !bad.is_empty() {
        return None;
    }
    if !fat32_fs_info_ok(&sector) {
        ctx.issues
            .push(Issue::new(IssueKind::FsInfoInvalid).area(Area::FsInfo));
        ctx.findings.fs_info_invalid = true;
    }
    Some(())
}

/// 比对各份 FAT（只比到最后一个簇的表项），不同之处按表项计数。读不出的部分不比。被取消时返回 `None`。
fn compare_copies(ctx: &mut Ctx, geometry: &FatBpb, count: u32) -> Option<()> {
    let bits = geometry.width().bits();
    let length = (u64::from(count) + 2)
        .saturating_mul(bits)
        .div_ceil(8)
        .min(geometry.fat_bytes());
    let first_copy = geometry.fat_offset(0);
    let sector = geometry.bytes_per_sector.max(1);
    // 两份时记下不同的扇区（相对 FAT 起点），修复时只改这些扇区
    let mut sectors: Vec<(u64, u64)> = Vec::new();
    for copy in 1..geometry.fats {
        let other_copy = geometry.fat_offset(copy);
        let mut differing = 0u64;
        let mut first_entry = None;
        let mut last_entry = None;
        let mut position = 0u64;
        while position < length {
            if !ctx.control.checkpoint() {
                return None;
            }
            ctx.control.progress(Stage::Fat, position, length);
            let take = COMPARE_BLOCK.min(length - position);
            let mut left = vec![0u8; take as usize];
            let mut right = vec![0u8; take as usize];
            let bad_left = ctx.read(first_copy + position, &mut left, Area::Fat, None)?;
            let bad_right = ctx.read(other_copy + position, &mut right, Area::Fat, None)?;
            for (index, (a, b)) in left.iter().zip(&right).enumerate() {
                let at = index as u64;
                if a == b
                    || bad_left
                        .iter()
                        .chain(&bad_right)
                        .any(|span| span.overlaps(at, 1))
                {
                    continue;
                }
                let start = (position + at) / sector * sector;
                match sectors.last_mut() {
                    Some(last) if last.0 + last.1 == start => last.1 += sector,
                    Some(last) if last.0 + last.1 > start => {}
                    _ => sectors.push((start, sector)),
                }
                let entry = (position + at) * 8 / bits;
                if last_entry != Some(entry) {
                    differing += 1;
                    last_entry = Some(entry);
                    first_entry.get_or_insert(entry);
                }
            }
            position += take;
        }
        if let Some(entry) = first_entry {
            ctx.issues.push(
                Issue::new(IssueKind::FatCopiesDiffer)
                    .area(Area::Fat)
                    .cluster(entry)
                    .count(differing),
            );
            ctx.findings.fat_copies_differ = true;
        }
    }
    if geometry.fats == 2 && !sectors.is_empty() {
        ctx.findings.fat_copies = Some(FatCopies {
            copies: [first_copy, geometry.fat_offset(1)],
            differing: sectors,
        });
    }
    ctx.checked(CheckItem::FatCopies);
    Some(())
}

/// 读一遍 FAT：第 0、1 项（介质描述符、卷的脏标记与硬错误标记），统计已占用、坏簇与空闲的簇。
/// 被取消时返回 `None`。
fn scan_table(ctx: &mut Ctx, volume: &mut Volume, geometry: &FatBpb) -> Option<()> {
    let fat = &mut volume.fat;
    if let Some(entry0) = fat.raw(ctx, 0)
        && !fat_media_ok(fat.width, geometry.media, entry0)
    {
        ctx.issues.push(
            Issue::new(IssueKind::FatMediaMismatch)
                .area(Area::Fat)
                .expected(u64::from(geometry.media))
                .actual(u64::from(entry0)),
        );
    }
    if let Some(entry1) = fat.raw(ctx, 1) {
        let (clean, no_error) = (fat.width.clean_shutdown_bit(), fat.width.no_error_bit());
        if clean != 0 && entry1 & clean == 0 {
            ctx.issues
                .push(Issue::new(IssueKind::VolumeDirty).area(Area::Fat));
            ctx.findings.dirty = true;
        }
        if no_error != 0 && entry1 & no_error == 0 {
            ctx.issues
                .push(Issue::new(IssueKind::MediaFailure).area(Area::Fat));
        }
    }
    let total = u64::from(fat.count);
    let (mut used, mut bad) = (0u64, 0u64);
    for index in 0..fat.count {
        if index.is_multiple_of(1 << 16) {
            ctx.control.progress(Stage::Fat, u64::from(index), total);
            if !ctx.control.checkpoint() {
                return None;
            }
        }
        match fat.link(ctx, index + 2) {
            Link::Free | Link::Unknown => {}
            Link::Bad => bad += 1,
            _ => used += 1,
        }
    }
    ctx.control.progress(Stage::Fat, total, total);
    if bad > 0 {
        ctx.issues.push(
            Issue::new(IssueKind::BadClustersMarked)
                .area(Area::Fat)
                .count(bad),
        );
    }
    volume.used = used;
    Some(())
}

/// 目录的位置：FAT12/16 的根目录区，或者从某个簇起的链。
#[derive(Debug, Clone, Copy)]
enum DirAt {
    Fixed { offset: u64, length: u64 },
    Clusters(u32),
}

/// 待查的目录。
struct DirJob {
    id: u32,
    path: String,
    at: DirAt,
    /// 上级目录的起始簇（上级是根目录时为 0），用来核对 `..`。
    parent_cluster: u32,
    depth: usize,
    /// 这个目录自己的目录项（根目录没有）。
    entry: Option<EntryRef>,
}

struct FatChecker {
    volume: Volume,
    is_fat32: bool,
    root_cluster: u32,
}

impl FatChecker {
    /// 从根目录起逐个目录检查。被取消时返回 `None`。
    fn tree(&mut self, ctx: &mut Ctx, root: DirAt) -> Option<()> {
        let mut stack = vec![DirJob {
            id: ROOT,
            path: "/".to_owned(),
            at: root,
            parent_cluster: 0,
            depth: 0,
            entry: None,
        }];
        while let Some(job) = stack.pop() {
            if !ctx.control.checkpoint() {
                return None;
            }
            let mut runs = Vec::new();
            let (data, bad) = match job.at {
                DirAt::Fixed { offset, length } => {
                    let mut data = vec![0u8; length as usize];
                    let bad = ctx
                        .read(offset, &mut data, Area::RootDir, Some(&job.path))
                        .unwrap_or_else(|| vec![Span { offset: 0, length }]);
                    (data, bad)
                }
                DirAt::Clusters(first) => {
                    let walk = self.volume.walk(
                        ctx,
                        job.id,
                        &job.path,
                        first,
                        Layout::Chain { needed: None },
                        true,
                        job.entry.as_ref(),
                    )?;
                    if walk.first_taken {
                        // 起始簇是别人的（已记为交叉链接）：不再往下查，免得在目录之间绕圈
                        continue;
                    }
                    runs = walk.runs;
                    self.volume.read_runs(ctx, &runs, &job.path)
                }
            };
            let place = match job.at {
                DirAt::Fixed { offset, .. } => Place::Fixed(offset),
                DirAt::Clusters(_) => self.volume.place(&runs),
            };
            self.volume.report_progress(ctx);
            self.directory(ctx, &job, &place, &data, &bad, &mut stack)?;
        }
        Some(())
    }

    /// 查一个目录的内容：目录项、长文件名、`.` 与 `..`；文件当场认领簇，子目录压栈。
    fn directory(
        &mut self,
        ctx: &mut Ctx,
        job: &DirJob,
        place: &Place,
        data: &[u8],
        bad: &[Span],
        stack: &mut Vec<DirJob>,
    ) -> Option<()> {
        let entries = data.as_chunks::<ENTRY>().0;
        let unreadable = |index: usize| {
            bad.iter()
                .any(|span| span.overlaps((index * ENTRY) as u64, ENTRY as u64))
        };
        let place = *place;
        let at = |index: usize| place.locate((index * ENTRY) as u64);
        if let DirAt::Clusters(first) = job.at
            && job.id != ROOT
        {
            self.check_dots(ctx, job, first, entries, &unreadable, &at);
        }
        // 长文件名各段（序号、内容）还没等到它的短文件名项
        let mut pending: Vec<(usize, &[u8; ENTRY])> = Vec::new();
        // 孤立的长文件名项：标为已删除，短文件名项不受影响
        let orphans = |ctx: &mut Ctx, indexes: &[usize]| {
            let offsets: Option<Vec<u64>> = indexes.iter().map(|&index| at(index)).collect();
            if let Some(offsets) = offsets.filter(|offsets| !offsets.is_empty()) {
                ctx.findings.entries.push(EntryFix {
                    kinds: vec![IssueKind::LfnOrder],
                    path: job.path.clone(),
                    change: EntryChange::Delete {
                        at: offsets,
                        exfat: false,
                    },
                });
            }
        };
        for (index, raw) in entries.iter().enumerate() {
            if unreadable(index) {
                pending.clear();
                continue;
            }
            let first_byte = raw[0];
            if first_byte == 0 {
                break;
            }
            if first_byte == DELETED {
                pending.clear();
                continue;
            }
            let attr = raw[11];
            if attr & 0x3F == ATTR_LFN {
                let starts = first_byte & 0x40 != 0;
                if starts && !pending.is_empty() {
                    // 上一组长文件名后面没有短文件名项
                    ctx.issues
                        .push(Issue::new(IssueKind::LfnOrder).path(job.path.as_str()));
                    let indexes: Vec<usize> = pending.iter().map(|part| part.0).collect();
                    orphans(ctx, &indexes);
                    pending.clear();
                }
                if !starts && pending.is_empty() {
                    ctx.issues
                        .push(Issue::new(IssueKind::LfnOrder).path(job.path.as_str()));
                    orphans(ctx, &[index]);
                    continue;
                }
                pending.push((index, raw));
                continue;
            }
            let parts = std::mem::take(&mut pending);
            let short: [u8; 11] = raw.first_chunk::<11>().copied().unwrap_or([b' '; 11]);
            if &short == b".          " || &short == b"..         " {
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 || attr & 0xC0 != 0 {
                continue;
            }
            let short_name = fat_short_display(&short, raw[12]).0;
            let long = if parts.is_empty() {
                None
            } else {
                let raws: Vec<&[u8; ENTRY]> = parts.iter().map(|part| part.1).collect();
                match self.long_name(ctx, &job.path, &short_name, &short, &raws) {
                    Ok(long) => long,
                    Err(kind) => {
                        // 长文件名对不上短文件名项：标为已删除，文件留下短文件名
                        let offsets: Option<Vec<u64>> =
                            parts.iter().map(|part| at(part.0)).collect();
                        if let Some(offsets) = offsets {
                            ctx.findings.entries.push(EntryFix {
                                kinds: vec![kind],
                                path: join(&job.path, &short_name),
                                change: EntryChange::Delete {
                                    at: offsets,
                                    exfat: false,
                                },
                            });
                        }
                        None
                    }
                }
            };
            let name = long.unwrap_or(short_name);
            let path = join(&job.path, &name);
            let first = fat_first_cluster(raw, self.is_fat32);
            let id = self.volume.owners.add(Some(job.id), &name);
            let directory = attr & ATTR_DIRECTORY != 0;
            let size = u64::from(le32(raw, 28).unwrap_or(0));
            let entry = at(index).map(|offset| EntryRef {
                at: EntryAt::Fat(offset),
                size: (!directory).then_some(size),
                directory,
            });
            if directory {
                self.volume.directories += 1;
                let parent_cluster = match job.at {
                    DirAt::Clusters(cluster) if job.id != ROOT => cluster,
                    _ => 0,
                };
                if first == 0 || !self.volume.fat.is_valid(first) {
                    ctx.issues.push(
                        Issue::new(IssueKind::FirstClusterInvalid)
                            .path(path.as_str())
                            .actual(u64::from(first)),
                    );
                    // 原来的内容找不到了：改成空目录，名字留着
                    if let Some(at) = at(index) {
                        ctx.findings.empty_dirs.push(EmptyDir {
                            path,
                            at,
                            parent: parent_cluster,
                        });
                    }
                } else if job.depth + 1 > MAX_DEPTH {
                    ctx.issues.push(Issue::new(IssueKind::TooDeep).path(path));
                } else {
                    stack.push(DirJob {
                        id,
                        path,
                        at: DirAt::Clusters(first),
                        parent_cluster,
                        depth: job.depth + 1,
                        entry,
                    });
                }
            } else {
                self.volume.files += 1;
                let needed = self.volume.clusters_for(size);
                self.volume.walk(
                    ctx,
                    id,
                    &path,
                    first,
                    Layout::Chain {
                        needed: Some(needed),
                    },
                    false,
                    entry.as_ref(),
                )?;
            }
        }
        if !pending.is_empty() {
            ctx.issues
                .push(Issue::new(IssueKind::LfnOrder).path(job.path.as_str()));
            let indexes: Vec<usize> = pending.iter().map(|part| part.0).collect();
            orphans(ctx, &indexes);
        }
        Some(())
    }

    /// 子目录开头应是 `.`（指向自己）与 `..`（指向上级，上级是根目录时为 0；FAT32 也接受根目录的簇号）。
    /// 不对时，这两个位置本来就是 `.`、`..` 或空着才修（重写这两项），放着别的文件就不动。
    fn check_dots(
        &self,
        ctx: &mut Ctx,
        job: &DirJob,
        first: u32,
        entries: &[[u8; ENTRY]],
        unreadable: &dyn Fn(usize) -> bool,
        at: &dyn Fn(usize) -> Option<u64>,
    ) {
        if unreadable(0) || unreadable(1) {
            return;
        }
        let cluster_of = |raw: &[u8; ENTRY]| fat_first_cluster(raw, self.is_fat32);
        let dot_ok = entries.first().is_some_and(|raw| {
            raw.starts_with(b".          ")
                && raw[11] & ATTR_DIRECTORY != 0
                && cluster_of(raw) == first
        });
        let dotdot_ok = entries.get(1).is_some_and(|raw| {
            let parent = cluster_of(raw);
            raw.starts_with(b"..         ")
                && raw[11] & ATTR_DIRECTORY != 0
                && (parent == job.parent_cluster
                    || (job.parent_cluster == 0 && self.is_fat32 && parent == self.root_cluster))
        });
        if !dot_ok || !dotdot_ok {
            ctx.issues
                .push(Issue::new(IssueKind::DotEntries).path(job.path.as_str()));
            let slot_ok = |index: usize, name: &[u8; 11]| {
                entries
                    .get(index)
                    .is_some_and(|raw| matches!(raw[0], 0 | DELETED) || raw.starts_with(name))
            };
            if slot_ok(0, b".          ")
                && slot_ok(1, b"..         ")
                && let (Some(start), Some(second)) = (at(0), at(1))
                && second == start + ENTRY as u64
            {
                ctx.findings.entries.push(EntryFix {
                    kinds: vec![IssueKind::DotEntries],
                    path: job.path.clone(),
                    change: EntryChange::Dots {
                        at: start,
                        cluster: first,
                        parent: job.parent_cluster,
                    },
                });
            }
        }
    }

    /// 核对长文件名各段：序号从 `0x40 | n` 递减到 1、段数合理，校验和与短文件名一致。都对时返回长文件名
    /// （解不出时为 `None`）；不对时记下问题，返回它的类型。规则在 [`yisi_fs::format`]，与浏览共用。
    fn long_name(
        &self,
        ctx: &mut Ctx,
        dir: &str,
        short_name: &str,
        short: &[u8; 11],
        parts: &[&[u8; ENTRY]],
    ) -> Result<Option<String>, IssueKind> {
        let path = join(dir, short_name);
        let parts: Vec<LfnPart> = parts
            .iter()
            .filter_map(|part| lfn_part(part.as_slice()))
            .collect();
        if !lfn_ordered(&parts) {
            ctx.issues.push(Issue::new(IssueKind::LfnOrder).path(path));
            return Err(IssueKind::LfnOrder);
        }
        let checksum = short_checksum(short);
        if parts.iter().any(|part| part.checksum != checksum) {
            ctx.issues
                .push(Issue::new(IssueKind::LfnChecksum).path(path));
            return Err(IssueKind::LfnChecksum);
        }
        Ok(lfn_name(&parts))
    }
}

pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}
