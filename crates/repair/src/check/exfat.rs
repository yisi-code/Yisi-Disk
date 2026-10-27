//! exFAT 的检查：主、备引导区（参数与校验和）、分配位图、大写表、目录项集合（校验和、次项个数、文件名长度）、
//! 每个文件的簇（FAT 链或连续存放）与分配位图是否一致、丢失簇。依据微软公开的 exFAT 规范。

use yisi_fs::boot::{exfat_boot_checksum, exfat_stored_checksum};
use yisi_part::bytes::{le16, le32, le64};

use crate::check::chains::{Layout, MAX_DEPTH, Volume};
use crate::check::disk::Span;
use crate::check::fat::join;
use crate::check::issue::{Area, Issue, IssueKind};
use crate::check::table::{FatTable, Owners, ROOT, Width};
use crate::check::{CheckItem, Ctx, FsType, Stage, Summary};

const ENTRY: usize = 32;
/// 目录项类型的第 7 位：1 为使用中。
const IN_USE: u8 = 0x80;
const TYPE_BITMAP: u8 = 0x81;
const TYPE_UPCASE: u8 = 0x82;
const TYPE_FILE: u8 = 0x85;
const TYPE_STREAM: u8 = 0xC0;
const TYPE_NAME: u8 = 0xC1;
const ATTR_DIRECTORY: u16 = 0x10;
/// 流扩展项的标志：第 1 位为「不走 FAT 链」（簇连续存放）。
const NO_FAT_CHAIN: u8 = 0x02;
/// VolumeFlags：第 0 位用第二份 FAT，第 1 位未正常卸下，第 2 位出过介质故障。
const FLAG_ACTIVE_FAT: u16 = 0x01;
const FLAG_DIRTY: u16 = 0x02;
const FLAG_MEDIA_FAILURE: u16 = 0x04;
/// 引导区校验和不计入的字节：VolumeFlags（106、107）与 PercentInUse（112）。
const CHECKSUM_SKIPPED: [usize; 3] = [106, 107, 112];
/// 大写表最多读这么多（规范里的完整大写表不到 6 KiB）。
const MAX_UPCASE_BYTES: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Geometry {
    sector_shift: u32,
    cluster_shift: u32,
    volume_length: u64,
    fat_offset: u64,
    fat_length: u64,
    heap_offset: u64,
    cluster_count: u64,
    root_cluster: u32,
    flags: u16,
    fats: u64,
}

impl Geometry {
    fn bytes_per_sector(&self) -> u64 {
        1 << self.sector_shift
    }

    fn cluster_size(&self) -> u64 {
        1 << (self.sector_shift + self.cluster_shift)
    }
}

/// 解析引导扇区；参数不合规范时返回出问题的那一项。
fn parse(sector: &[u8]) -> Result<Geometry, &'static str> {
    if sector.get(3..11) != Some(b"EXFAT   ".as_slice()) {
        return Err("fileSystemName");
    }
    if le16(sector, 510) != Some(0xAA55) {
        return Err("signature");
    }
    let sector_shift = u32::from(*sector.get(108).ok_or("bytesPerSectorShift")?);
    if !(9..=12).contains(&sector_shift) {
        return Err("bytesPerSectorShift");
    }
    let cluster_shift = u32::from(*sector.get(109).ok_or("sectorsPerClusterShift")?);
    if cluster_shift > 25 - sector_shift {
        return Err("sectorsPerClusterShift");
    }
    let fats = u64::from(*sector.get(110).ok_or("numberOfFats")?);
    if !matches!(fats, 1 | 2) {
        return Err("numberOfFats");
    }
    let number = |at, name| le32(sector, at).map(u64::from).ok_or(name);
    let geometry = Geometry {
        sector_shift,
        cluster_shift,
        volume_length: le64(sector, 72).ok_or("volumeLength")?,
        fat_offset: number(80, "fatOffset")?,
        fat_length: number(84, "fatLength")?,
        heap_offset: number(88, "clusterHeapOffset")?,
        cluster_count: number(92, "clusterCount")?,
        root_cluster: le32(sector, 96).ok_or("firstClusterOfRootDirectory")?,
        flags: le16(sector, 106).ok_or("volumeFlags")?,
        fats,
    };
    if geometry.fat_offset < 24 {
        return Err("fatOffset");
    }
    if geometry.fat_length == 0 {
        return Err("fatLength");
    }
    if geometry.heap_offset < geometry.fat_offset + geometry.fat_length * fats {
        return Err("clusterHeapOffset");
    }
    if geometry.cluster_count == 0 || geometry.cluster_count > 0xFFFF_FFF5 {
        return Err("clusterCount");
    }
    if geometry.root_cluster < 2 || u64::from(geometry.root_cluster) >= geometry.cluster_count + 2 {
        return Err("firstClusterOfRootDirectory");
    }
    Ok(geometry)
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
    let mut area = if from_backup {
        Area::BackupBoot
    } else {
        Area::Boot
    };
    let mut region = region.to_vec();
    let mut using_backup = from_backup;
    let geometry = match parse(&region) {
        Ok(geometry) => geometry,
        Err(field) => {
            ctx.issues
                .push(Issue::new(IssueKind::BootInvalid).area(area).field(field));
            // 主引导扇区坏了就试备份引导区
            let backup = (!from_backup).then(|| read_backup(ctx)).flatten();
            match backup.as_deref().map(|backup| (parse(backup), backup)) {
                Some((Ok(geometry), backup)) => {
                    region = backup.to_vec();
                    using_backup = true;
                    area = Area::BackupBoot;
                    geometry
                }
                _ => return Some(unknown()),
            }
        }
    };
    ctx.checked(CheckItem::Boot);
    let bps = geometry.bytes_per_sector();
    let cluster_size = geometry.cluster_size();
    let length = ctx.disk.length();

    if region
        .get(11..64)
        .is_some_and(|bytes| bytes.iter().any(|&byte| byte != 0))
    {
        ctx.issues.push(
            Issue::new(IssueKind::BootInvalid)
                .area(area)
                .field("mustBeZero"),
        );
    }
    if !checksum_matches(&region, bps as usize) {
        ctx.issues
            .push(Issue::new(IssueKind::BootChecksum).area(area));
    }
    ctx.checked(CheckItem::BootChecksum);
    if !using_backup {
        check_backup(ctx, &region, bps);
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
    let heap_end = heap_offset.saturating_add(geometry.cluster_count.saturating_mul(cluster_size));
    if heap_end > volume_bytes {
        ctx.issues.push(
            Issue::new(IssueKind::BootInvalid)
                .area(area)
                .field("clusterCount"),
        );
    }
    let fat_entries = geometry.fat_length.saturating_mul(bps) / 4;
    if fat_entries < geometry.cluster_count + 2 {
        ctx.issues.push(
            Issue::new(IssueKind::BootInvalid)
                .area(area)
                .field("fatLength"),
        );
    }
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

/// 备份引导区在第 12 个扇区起：主引导扇区坏了时用它。
fn read_backup(ctx: &mut Ctx) -> Option<Vec<u8>> {
    // 主引导扇区坏了，扇区大小也未必可信：先按备份自己的扇区大小试
    for shift in 9..=12u32 {
        let bps = 1u64 << shift;
        let mut backup = vec![0u8; 12 * bps as usize];
        let bad = ctx.read(12 * bps, &mut backup, Area::BackupBoot, None)?;
        if bad.is_empty()
            && backup.get(3..11) == Some(b"EXFAT   ".as_slice())
            && backup.get(108).copied() == Some(shift as u8)
        {
            return Some(backup);
        }
    }
    None
}

/// 备份引导区：校验和要对，参数（跳过 VolumeFlags 与 PercentInUse）要与主引导区一致。
fn check_backup(ctx: &mut Ctx, main: &[u8], bps: u64) {
    ctx.checked(CheckItem::BackupBoot);
    let mut backup = vec![0u8; main.len()];
    let readable = ctx
        .read(12 * bps, &mut backup, Area::BackupBoot, None)
        .is_some_and(|bad| bad.is_empty());
    if !readable || parse(&backup).is_err() {
        ctx.issues
            .push(Issue::new(IssueKind::BackupBootInvalid).area(Area::BackupBoot));
        return;
    }
    if !checksum_matches(&backup, bps as usize) {
        ctx.issues
            .push(Issue::new(IssueKind::BootChecksum).area(Area::BackupBoot));
        return;
    }
    let differs = main
        .iter()
        .zip(&backup)
        .take(11 * bps as usize)
        .enumerate()
        .any(|(index, (a, b))| a != b && !CHECKSUM_SKIPPED.contains(&index));
    if differs {
        ctx.issues
            .push(Issue::new(IssueKind::BackupBootMismatch).area(Area::BackupBoot));
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

/// 一组目录项（文件项 + 流扩展项 + 文件名项）里检查要用的内容。
struct FileSet {
    name: String,
    attributes: u16,
    first_cluster: u32,
    data_length: u64,
    valid_length: u64,
    no_fat_chain: bool,
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
                TYPE_BITMAP if bitmap_entry.is_none() => bitmap_entry = Some(*raw),
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
            if kind & IN_USE == 0 {
                index += 1;
                continue;
            }
            match kind {
                TYPE_FILE => {
                    let (consumed, set) =
                        self.file_set(ctx, &job.path, entries, index, &unreadable);
                    index += consumed;
                    if let Some(set) = set {
                        self.entry(ctx, job, set, stack)?;
                    }
                }
                TYPE_STREAM | TYPE_NAME => {
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

    /// 解析从 `index` 起的一组目录项，记下发现的问题。返回占了几项，以及能用来认领簇的内容
    /// （缺流扩展项、或者集合跨进了读不出的扇区时为 `None`）。
    fn file_set(
        &mut self,
        ctx: &mut Ctx,
        dir: &str,
        entries: &[[u8; ENTRY]],
        index: usize,
        unreadable: &dyn Fn(usize) -> bool,
    ) -> (usize, Option<FileSet>) {
        let Some(primary) = entries.get(index) else {
            return (1, None);
        };
        let recorded = usize::from(primary[1]);
        if !(2..=18).contains(&recorded) {
            ctx.issues.push(
                Issue::new(IssueKind::EntrySetCount)
                    .path(dir)
                    .expected(recorded as u64)
                    .actual(0),
            );
            return (1, None);
        }
        if (1..=recorded).any(|offset| unreadable(index + offset)) {
            // 读不出的部分已记成问题，这组项不再另算
            return (recorded + 1, None);
        }
        // 实际连着的次项：使用中、类型是次项（第 6 位为 1），第一项是流扩展项，文件名项紧随其后
        let mut found = 0;
        let mut stream = None;
        let mut names = 0usize;
        let mut units: Vec<u16> = Vec::new();
        for offset in 1..=recorded {
            let Some(raw) = entries.get(index + offset) else {
                break;
            };
            let kind = raw[0];
            if kind & IN_USE == 0 || kind & 0x40 == 0 {
                break;
            }
            match (offset, kind) {
                (1, TYPE_STREAM) => stream = Some(raw),
                (1, _) => break,
                (_, TYPE_NAME) if names + 2 == offset => {
                    names += 1;
                    units.extend((0..15).filter_map(|unit| le16(raw, 2 + unit * 2)));
                }
                (_, TYPE_NAME) => break,
                _ => {}
            }
            found = offset;
        }
        let name_length = stream.map_or(0, |raw| usize::from(raw[3]));
        let mut name: Vec<u16> = units.iter().copied().take(name_length).collect();
        if name.is_empty() {
            name = units
                .iter()
                .copied()
                .take_while(|&unit| unit != 0)
                .collect();
        }
        let display = String::from_utf16_lossy(&name);
        let path = if display.is_empty() {
            dir.to_owned()
        } else {
            join(dir, &display)
        };
        if found < recorded {
            ctx.issues.push(
                Issue::new(IssueKind::EntrySetCount)
                    .path(path.as_str())
                    .expected(recorded as u64)
                    .actual(found as u64),
            );
        } else {
            let mut checksum = 0u16;
            for (offset, raw) in entries.iter().skip(index).take(recorded + 1).enumerate() {
                for (position, &byte) in raw.iter().enumerate() {
                    if offset == 0 && (position == 2 || position == 3) {
                        continue;
                    }
                    checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
                }
            }
            let stored = le16(primary, 2).unwrap_or(0);
            if checksum != stored {
                ctx.issues.push(
                    Issue::new(IssueKind::EntrySetChecksum)
                        .path(path.as_str())
                        .expected(u64::from(stored))
                        .actual(u64::from(checksum)),
                );
            }
        }
        let Some(stream) = stream else {
            return (found + 1, None);
        };
        let wanted = name_length.div_ceil(15);
        if name_length == 0 || names != wanted {
            ctx.issues.push(
                Issue::new(IssueKind::EntryNameLength)
                    .path(path.as_str())
                    .expected(wanted as u64)
                    .actual(names as u64),
            );
        }
        let set = FileSet {
            name: if display.is_empty() {
                "?".to_owned()
            } else {
                display
            },
            attributes: le16(primary, 4).unwrap_or(0),
            first_cluster: le32(stream, 20).unwrap_or(0),
            data_length: le64(stream, 24).unwrap_or(0),
            valid_length: le64(stream, 8).unwrap_or(0),
            no_fat_chain: stream[1] & NO_FAT_CHAIN != 0,
        };
        (found + 1, Some(set))
    }

    /// 认领一个文件或目录的簇；目录压栈。
    fn entry(
        &mut self,
        ctx: &mut Ctx,
        job: &DirJob,
        set: FileSet,
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
