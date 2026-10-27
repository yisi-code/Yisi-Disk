//! exFAT 的检查：主、备引导区（参数与校验和）、分配位图、大写表、目录项集合（校验和、次项个数、文件名长度）、
//! 每个文件的簇（FAT 链或连续存放）与分配位图是否一致、丢失簇。依据微软公开的 exFAT 规范。

use yisi_fs::FsKind;
use yisi_fs::boot::{exfat_boot_checksum, exfat_stored_checksum};
use yisi_fs::bpb::{self, ExfatBpb as Geometry, Problem};
use yisi_fs::format::{
    EXFAT_IN_USE, EXFAT_TYPE_BITMAP, EXFAT_TYPE_FILE, EXFAT_TYPE_NAME, EXFAT_TYPE_STREAM, ExfatSet,
    SetProblem, exfat_entry_set,
};
use yisi_part::bytes::{le32, le64};

use crate::check::chains::{Layout, MAX_DEPTH, Volume};
use crate::check::disk::Span;
use crate::check::fat::join;
use crate::check::issue::{Area, Issue, IssueKind};
use crate::check::table::{FatTable, Owners, ROOT, Width};
use crate::check::{CheckItem, Ctx, FsType, Stage, Summary, check_backup};

const ENTRY: usize = 32;
const TYPE_UPCASE: u8 = 0x82;
const ATTR_DIRECTORY: u16 = 0x10;
/// VolumeFlags：第 0 位用第二份 FAT，第 1 位未正常卸下，第 2 位出过介质故障。
const FLAG_ACTIVE_FAT: u16 = 0x01;
const FLAG_DIRTY: u16 = 0x02;
const FLAG_MEDIA_FAILURE: u16 = 0x04;
/// 大写表最多读这么多（规范里的完整大写表不到 6 KiB）。
const MAX_UPCASE_BYTES: u64 = 1 << 20;

/// 解析引导扇区：规则在 [`yisi_fs::bpb`]（与打开卷、引导扇区诊断共用）。有致命问题时参数为 `None`。
fn parse(sector: &[u8]) -> (Option<Geometry>, Vec<Problem>) {
    let checked = bpb::check_exfat(sector);
    (checked.geometry, checked.problems)
}

/// 第 12 个扇区里每 4 个字节都应等于前 11 个扇区的校验和（[`exfat_boot_checksum`]）。
fn checksum_matches(region: &[u8], bytes_per_sector: usize) -> bool {
    let covered = region.get(..11 * bytes_per_sector);
    let stored = region.get(11 * bytes_per_sector..12 * bytes_per_sector);
    match (covered, stored) {
        (Some(covered), Some(stored)) => {
            exfat_stored_checksum(stored) == Some(exfat_boot_checksum(covered))
        }
        _ => false,
    }
}

/// 检查一个 exFAT 卷。`region` 是认出来的引导区（12 个扇区，`from_backup` 时来自备份）。被取消时返回 `None`。
pub(crate) fn check(ctx: &mut Ctx, region: &[u8], from_backup: bool) -> Option<Summary> {
    let area = if from_backup {
        Area::BackupBoot
    } else {
        Area::Boot
    };
    let (parsed, problems) = parse(region);
    ctx.boot_problems(&problems, area);
    let Some(geometry) = parsed else {
        return Some(unknown());
    };
    ctx.checked(CheckItem::Boot);
    let bps = geometry.bytes_per_sector();
    let cluster_size = geometry.cluster_size();
    let length = ctx.disk.length();

    if !checksum_matches(region, bps as usize) {
        ctx.issues
            .push(Issue::new(IssueKind::BootChecksum).area(area));
    }
    ctx.checked(CheckItem::BootChecksum);
    // 备份引导区：参数按诊断的字段比对，校验和另算
    if !from_backup && let Some(at) = check_backup(ctx, FsKind::Exfat, region) {
        let mut backup = vec![0u8; region.len()];
        if ctx
            .read(at, &mut backup, Area::BackupBoot, None)
            .is_some_and(|bad| bad.is_empty())
            && !checksum_matches(&backup, bps as usize)
        {
            ctx.issues
                .push(Issue::new(IssueKind::BootChecksum).area(Area::BackupBoot));
        }
    }

    let volume_bytes = geometry.volume_length.saturating_mul(bps);
    if volume_bytes > length {
        ctx.issues.push(
            Issue::new(IssueKind::VolumeTruncated)
                .area(Area::Boot)
                .expected(volume_bytes)
                .actual(length),
        );
    }
    let heap_offset = geometry.heap_offset.saturating_mul(bps);
    let fat_entries = geometry.fat_length.saturating_mul(bps) / 4;
    if geometry.flags & FLAG_DIRTY != 0 {
        ctx.issues
            .push(Issue::new(IssueKind::VolumeDirty).area(area));
    }
    if geometry.flags & FLAG_MEDIA_FAILURE != 0 {
        ctx.issues
            .push(Issue::new(IssueKind::MediaFailure).area(area));
    }

    let fits = length.saturating_sub(heap_offset) / cluster_size;
    let count = geometry
        .cluster_count
        .min(fat_entries.saturating_sub(2))
        .min(fits);
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    let active = u64::from(geometry.fats == 2 && geometry.flags & FLAG_ACTIVE_FAT != 0);
    let fat_start = (geometry.fat_offset + active * geometry.fat_length).saturating_mul(bps);
    let volume = Volume {
        fat: FatTable::new(
            fat_start,
            geometry.fat_length.saturating_mul(bps),
            Width::Exfat,
            count,
        ),
        owners: Owners::new(u64::from(count) + 2),
        cluster_size,
        heap_offset,
        claimed: 0,
        used: 0,
        files: 0,
        directories: 0,
    };
    let mut checker = ExfatChecker {
        volume,
        bitmap: None,
    };
    checker.tree(ctx, geometry.root_cluster)?;
    checker.volume.finish_directories(ctx);
    ctx.checked(CheckItem::Chains);
    ctx.checked(CheckItem::Entries);

    let bitmap = checker.bitmap.take();
    let mut volume = checker.volume;
    match &bitmap {
        Some(bitmap) => {
            volume.sweep(ctx, &mut |_, _, cluster| bitmap.get(cluster), true)?;
        }
        None => {
            ctx.skipped(CheckItem::Bitmap);
            ctx.skipped(CheckItem::LostClusters);
            ctx.skipped(CheckItem::CrossLinks);
        }
    }
    Some(Summary {
        fs: FsType::Exfat,
        cluster_size,
        cluster_count: u64::from(count),
        used_clusters: volume.used,
        files: volume.files,
        directories: volume.directories,
    })
}

fn unknown() -> Summary {
    Summary {
        fs: FsType::Exfat,
        cluster_size: 0,
        cluster_count: 0,
        used_clusters: 0,
        files: 0,
        directories: 0,
    }
}

/// 读进内存的分配位图：第 n 位（从 2 号簇算起）为 1 表示已分配。读不出的部分不作判断。
struct Bitmap {
    bytes: Vec<u8>,
    bad: Vec<Span>,
}

impl Bitmap {
    fn get(&self, cluster: u32) -> Option<bool> {
        let bit = u64::from(cluster.checked_sub(2)?);
        let at = bit / 8;
        if self.bad.iter().any(|span| span.overlaps(at, 1)) {
            return None;
        }
        let byte = self.bytes.get(at as usize)?;
        Some(byte & (1 << (bit % 8)) != 0)
    }
}

/// 待查的目录。
struct DirJob {
    id: u32,
    path: String,
    first: u32,
    layout: Layout,
    depth: usize,
}

struct ExfatChecker {
    volume: Volume,
    bitmap: Option<Bitmap>,
}

impl ExfatChecker {
    /// 从根目录起逐个目录检查；根目录里先取出分配位图与大写表。被取消时返回 `None`。
    fn tree(&mut self, ctx: &mut Ctx, root_cluster: u32) -> Option<()> {
        let mut stack = vec![DirJob {
            id: ROOT,
            path: "/".to_owned(),
            first: root_cluster,
            layout: Layout::Chain { needed: None },
            depth: 0,
        }];
        while let Some(job) = stack.pop() {
            if !ctx.control.checkpoint() {
                return None;
            }
            let walk = self
                .volume
                .walk(ctx, job.id, &job.path, job.first, job.layout, true)?;
            if walk.first_taken {
                continue;
            }
            let (data, bad) = self.volume.read_runs(ctx, &walk.runs, &job.path);
            if job.id == ROOT {
                self.system_entries(ctx, &data, &bad)?;
            }
            self.volume.report_progress(ctx);
            self.directory(ctx, &job, &data, &bad, &mut stack)?;
        }
        Some(())
    }

    /// 根目录里的分配位图项与大写表项。
    fn system_entries(&mut self, ctx: &mut Ctx, data: &[u8], bad: &[Span]) -> Option<()> {
        let mut bitmap_entry = None;
        let mut upcase_entry = None;
        for (index, raw) in data.as_chunks::<ENTRY>().0.iter().enumerate() {
            if bad
                .iter()
                .any(|span| span.overlaps((index * ENTRY) as u64, ENTRY as u64))
            {
                continue;
            }
            match raw[0] {
                0 => break,
                EXFAT_TYPE_BITMAP if bitmap_entry.is_none() => bitmap_entry = Some(*raw),
                TYPE_UPCASE if upcase_entry.is_none() => upcase_entry = Some(*raw),
                _ => {}
            }
        }
        match bitmap_entry {
            Some(raw) => self.load_bitmap(ctx, &raw)?,
            None => ctx
                .issues
                .push(Issue::new(IssueKind::BitmapInvalid).area(Area::Bitmap)),
        }
        match upcase_entry {
            Some(raw) => self.check_upcase(ctx, &raw)?,
            None => {
                ctx.issues
                    .push(Issue::new(IssueKind::UpcaseMissing).area(Area::Upcase));
                ctx.skipped(CheckItem::Upcase);
            }
        }
        Some(())
    }

    /// 认领分配位图的簇、读进内存，统计已分配的簇数。位图比簇数短时不用它。
    fn load_bitmap(&mut self, ctx: &mut Ctx, raw: &[u8; ENTRY]) -> Option<()> {
        let first = le32(raw, 20).unwrap_or(0);
        let length = le64(raw, 24).unwrap_or(0);
        let needed_bytes = u64::from(self.volume.fat.count).div_ceil(8);
        let id = self.volume.owners.add(None, "$Bitmap");
        let needed = self.volume.clusters_for(length);
        let walk = self.volume.walk(
            ctx,
            id,
            "$Bitmap",
            first,
            Layout::Chain {
                needed: Some(needed),
            },
            true,
        )?;
        if length < needed_bytes {
            ctx.issues.push(
                Issue::new(IssueKind::BitmapInvalid)
                    .area(Area::Bitmap)
                    .expected(needed_bytes)
                    .actual(length),
            );
            return Some(());
        }
        let (mut bytes, bad) = self.volume.read_runs(ctx, &walk.runs, "$Bitmap");
        if (bytes.len() as u64) < needed_bytes {
            ctx.issues.push(
                Issue::new(IssueKind::BitmapInvalid)
                    .area(Area::Bitmap)
                    .expected(needed_bytes)
                    .actual(bytes.len() as u64),
            );
            return Some(());
        }
        bytes.truncate(needed_bytes as usize);
        let bitmap = Bitmap { bytes, bad };
        let total = u64::from(self.volume.fat.count);
        let mut used = 0u64;
        for index in 0..self.volume.fat.count {
            if index.is_multiple_of(1 << 20) {
                ctx.control.progress(Stage::Fat, u64::from(index), total);
            }
            if bitmap.get(index + 2) == Some(true) {
                used += 1;
            }
        }
        ctx.control.progress(Stage::Fat, total, total);
        self.volume.used = used;
        self.bitmap = Some(bitmap);
        Some(())
    }

    /// 认领大写表的簇，核对它的校验和。
    fn check_upcase(&mut self, ctx: &mut Ctx, raw: &[u8; ENTRY]) -> Option<()> {
        let recorded = le32(raw, 4).unwrap_or(0);
        let first = le32(raw, 20).unwrap_or(0);
        let length = le64(raw, 24).unwrap_or(0);
        let id = self.volume.owners.add(None, "$UpCase");
        let needed = self.volume.clusters_for(length);
        let walk = self.volume.walk(
            ctx,
            id,
            "$UpCase",
            first,
            Layout::Chain {
                needed: Some(needed),
            },
            true,
        )?;
        let (mut bytes, bad) = self.volume.read_runs(ctx, &walk.runs, "$UpCase");
        let wanted = length.min(MAX_UPCASE_BYTES);
        if !bad.is_empty() || (bytes.len() as u64) < wanted {
            ctx.skipped(CheckItem::Upcase);
            return Some(());
        }
        bytes.truncate(wanted as usize);
        let computed = bytes.iter().fold(0u32, |sum, &byte| {
            sum.rotate_right(1).wrapping_add(u32::from(byte))
        });
        if computed != recorded {
            ctx.issues.push(
                Issue::new(IssueKind::UpcaseChecksum)
                    .area(Area::Upcase)
                    .expected(u64::from(recorded))
                    .actual(u64::from(computed)),
            );
        }
        ctx.checked(CheckItem::Upcase);
        Some(())
    }

    /// 查一个目录里的目录项集合；文件当场认领簇，子目录压栈。
    fn directory(
        &mut self,
        ctx: &mut Ctx,
        job: &DirJob,
        data: &[u8],
        bad: &[Span],
        stack: &mut Vec<DirJob>,
    ) -> Option<()> {
        let entries = data.as_chunks::<ENTRY>().0;
        let unreadable = |index: usize| {
            bad.iter()
                .any(|span| span.overlaps((index * ENTRY) as u64, ENTRY as u64))
        };
        let mut orphans = 0u64;
        let mut index = 0;
        while let Some(raw) = entries.get(index) {
            if unreadable(index) {
                index += 1;
                continue;
            }
            let kind = raw[0];
            if kind == 0 {
                break;
            }
            if kind & EXFAT_IN_USE == 0 {
                index += 1;
                continue;
            }
            match kind {
                EXFAT_TYPE_FILE => {
                    let (consumed, set) =
                        self.file_set(ctx, &job.path, entries, index, &unreadable);
                    index += consumed;
                    if let Some(set) = set {
                        self.entry(ctx, job, set, stack)?;
                    }
                }
                EXFAT_TYPE_STREAM | EXFAT_TYPE_NAME => {
                    orphans += 1;
                    index += 1;
                }
                _ => index += 1,
            }
        }
        if orphans > 0 {
            ctx.issues.push(
                Issue::new(IssueKind::OrphanEntries)
                    .path(job.path.as_str())
                    .count(orphans),
            );
        }
        Some(())
    }

    /// 解析从 `index` 起的一组目录项（规则在 [`yisi_fs::format`]，与浏览共用），记下发现的问题。返回占了几项，
    /// 以及能用来认领簇的内容（缺流扩展项、或者集合跨进了读不出的扇区时为 `None`）。
    fn file_set(
        &mut self,
        ctx: &mut Ctx,
        dir: &str,
        entries: &[[u8; ENTRY]],
        index: usize,
        unreadable: &dyn Fn(usize) -> bool,
    ) -> (usize, Option<ExfatSet>) {
        let recorded = entries
            .get(index)
            .map_or(0, |primary| usize::from(primary[1]));
        if (2..=18).contains(&recorded) && (1..=recorded).any(|offset| unreadable(index + offset)) {
            // 读不出的部分已记成问题，这组项不再另算
            return (recorded + 1, None);
        }
        let mut set = exfat_entry_set(entries, index);
        let path = if set.name.is_empty() {
            dir.to_owned()
        } else {
            join(dir, &set.name)
        };
        for problem in &set.problems {
            let (kind, expected, actual) = match *problem {
                SetProblem::SecondaryCount { recorded } => {
                    (IssueKind::EntrySetCount, recorded as u64, 0)
                }
                SetProblem::Missing { recorded, found } => {
                    (IssueKind::EntrySetCount, recorded as u64, found as u64)
                }
                SetProblem::Checksum { stored, computed } => (
                    IssueKind::EntrySetChecksum,
                    u64::from(stored),
                    u64::from(computed),
                ),
                SetProblem::NameLength { expected, found } => {
                    (IssueKind::EntryNameLength, expected as u64, found as u64)
                }
            };
            ctx.issues.push(
                Issue::new(kind)
                    .path(path.as_str())
                    .expected(expected)
                    .actual(actual),
            );
        }
        if !set.has_stream {
            return (set.entries, None);
        }
        if set.name.is_empty() {
            set.name = "?".to_owned();
        }
        (set.entries, Some(set))
    }

    /// 认领一个文件或目录的簇；目录压栈。
    fn entry(
        &mut self,
        ctx: &mut Ctx,
        job: &DirJob,
        set: ExfatSet,
        stack: &mut Vec<DirJob>,
    ) -> Option<()> {
        let path = join(&job.path, &set.name);
        let id = self.volume.owners.add(Some(job.id), &set.name);
        if set.valid_length > set.data_length {
            ctx.issues.push(
                Issue::new(IssueKind::SizeInvalid)
                    .path(path.as_str())
                    .expected(set.data_length)
                    .actual(set.valid_length),
            );
        }
        let needed = self.volume.clusters_for(set.data_length);
        let layout = if set.no_fat_chain {
            Layout::Contiguous { needed }
        } else {
            Layout::Chain {
                needed: Some(needed),
            }
        };
        if set.attributes & ATTR_DIRECTORY != 0 {
            self.volume.directories += 1;
            if set.first_cluster == 0 && needed == 0 {
                return Some(());
            }
            if job.depth + 1 > MAX_DEPTH {
                ctx.issues.push(Issue::new(IssueKind::TooDeep).path(path));
                return Some(());
            }
            stack.push(DirJob {
                id,
                path,
                first: set.first_cluster,
                layout,
                depth: job.depth + 1,
            });
            return Some(());
        }
        self.volume.files += 1;
        // 长度为 0 的文件不占簇，起始簇记什么都不算数
        if needed == 0 {
            return Some(());
        }
        self.volume
            .walk(ctx, id, &path, set.first_cluster, layout, false)?;
        Some(())
    }
}
