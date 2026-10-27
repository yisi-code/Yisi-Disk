//! FAT12 / FAT16 / FAT32 的检查：引导扇区、备份引导扇区与 FSInfo、两份 FAT 是否一致、
//! 目录树（长文件名的校验和与序号、`.` 与 `..`）、每个文件的 FAT 链、丢失簇。依据微软公开的 FAT 规范。

use yisi_fs::{fat_short_checksum as short_checksum, fat_short_display};
use yisi_part::bytes::{le16, le32};

use crate::check::chains::{Layout, MAX_DEPTH, Volume};
use crate::check::disk::Span;
use crate::check::issue::{Area, Issue, IssueKind};
use crate::check::table::{FatTable, Link, Owners, ROOT, Width};
use crate::check::{CheckItem, Ctx, FsType, Stage, Summary};

const ENTRY: usize = 32;
const ATTR_LFN: u8 = 0x0F;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const DELETED: u8 = 0xE5;
/// 长文件名最多 20 段（255 个字符）。
const MAX_LFN_PARTS: usize = 20;
/// 比对两份 FAT 时每次读这么多。
const COMPARE_BLOCK: u64 = 1 << 20;

/// 引导扇区里的几何参数。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Geometry {
    width: Width,
    bytes_per_sector: u64,
    sectors_per_cluster: u64,
    reserved: u64,
    fats: u64,
    root_entries: u64,
    total_sectors: u64,
    fat_sectors: u64,
    media: u8,
    root_cluster: u32,
    fs_info: u64,
    backup_boot: u64,
    /// FAT32 关掉了镜像、只用 `active_fat` 那一份。
    mirroring: bool,
    active_fat: u64,
    /// 按数据区算出的簇数（未按 FAT 长度与分区长度截短）。
    cluster_count: u64,
}

impl Geometry {
    fn root_dir_sectors(&self) -> u64 {
        (self.root_entries * ENTRY as u64).div_ceil(self.bytes_per_sector)
    }

    fn first_data_sector(&self) -> u64 {
        self.reserved + self.fats * self.fat_sectors + self.root_dir_sectors()
    }

    fn bits(&self) -> u64 {
        match self.width {
            Width::Fat12 => 12,
            Width::Fat16 => 16,
            Width::Fat32 | Width::Exfat => 32,
        }
    }
}

/// 解析引导扇区；参数不自洽时返回出问题的那一项。类型按簇数判定（规范第 3.5 节），
/// 只是 BPB 里 16 位的 FAT 扇区数为 0 时按 FAT32 处理（与浏览时的识别一致）。
fn parse(sector: &[u8]) -> Result<Geometry, &'static str> {
    let field = |value: Option<u64>, name| value.ok_or(name);
    if le16(sector, 510) != Some(0xAA55) {
        return Err("signature");
    }
    let bytes_per_sector = field(le16(sector, 11).map(u64::from), "bytesPerSector")?;
    if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
        return Err("bytesPerSector");
    }
    let sectors_per_cluster = field(sector.get(13).copied().map(u64::from), "sectorsPerCluster")?;
    if !sectors_per_cluster.is_power_of_two() || sectors_per_cluster > 128 {
        return Err("sectorsPerCluster");
    }
    let reserved = field(le16(sector, 14).map(u64::from), "reservedSectors")?;
    if reserved == 0 {
        return Err("reservedSectors");
    }
    let fats = field(sector.get(16).copied().map(u64::from), "fatCount")?;
    if !matches!(fats, 1 | 2) {
        return Err("fatCount");
    }
    let media = *sector.get(21).ok_or("mediaDescriptor")?;
    if !(media == 0xF0 || media >= 0xF8) {
        return Err("mediaDescriptor");
    }
    let root_entries = field(le16(sector, 17).map(u64::from), "rootEntries")?;
    let total16 = field(le16(sector, 19).map(u64::from), "totalSectors")?;
    let total32 = field(le32(sector, 32).map(u64::from), "totalSectors")?;
    let total_sectors = if total16 != 0 { total16 } else { total32 };
    if total_sectors == 0 {
        return Err("totalSectors");
    }
    let fat16_sectors = field(le16(sector, 22).map(u64::from), "fatSize")?;
    let is_fat32 = fat16_sectors == 0;
    let fat_sectors = if is_fat32 {
        field(le32(sector, 36).map(u64::from), "fatSize")?
    } else {
        fat16_sectors
    };
    if fat_sectors == 0 {
        return Err("fatSize");
    }
    if is_fat32 == (root_entries != 0) {
        return Err("rootEntries");
    }
    let mut geometry = Geometry {
        width: Width::Fat32,
        bytes_per_sector,
        sectors_per_cluster,
        reserved,
        fats,
        root_entries,
        total_sectors,
        fat_sectors,
        media,
        root_cluster: 0,
        fs_info: 0,
        backup_boot: 0,
        mirroring: true,
        active_fat: 0,
        cluster_count: 0,
    };
    let data_sectors = total_sectors
        .checked_sub(geometry.first_data_sector())
        .ok_or("totalSectors")?;
    let clusters = data_sectors / sectors_per_cluster;
    geometry.width = if is_fat32 {
        Width::Fat32
    } else if clusters < 4085 {
        Width::Fat12
    } else if clusters < 65525 {
        Width::Fat16
    } else {
        return Err("fatSize");
    };
    if clusters == 0 || clusters > 0x0FFF_FFF5 {
        return Err("totalSectors");
    }
    geometry.cluster_count = clusters;
    if is_fat32 {
        let flags = le16(sector, 40).ok_or("extFlags")?;
        if flags & 0x80 != 0 {
            geometry.mirroring = false;
            geometry.active_fat = u64::from(flags & 0x0F);
            if geometry.active_fat >= fats {
                return Err("extFlags");
            }
        }
        geometry.root_cluster = le32(sector, 44).ok_or("rootCluster")?;
        if geometry.root_cluster < 2 || u64::from(geometry.root_cluster) >= clusters + 2 {
            return Err("rootCluster");
        }
        geometry.fs_info = u64::from(le16(sector, 48).ok_or("fsInfo")?);
        geometry.backup_boot = u64::from(le16(sector, 50).ok_or("backupBootSector")?);
    }
    Ok(geometry)
}

/// 识别用：认得出时给出是不是 FAT32、扇区大小。
pub(crate) fn looks_valid(sector: &[u8]) -> Option<(bool, u64)> {
    let geometry = parse(sector).ok()?;
    Some((geometry.width == Width::Fat32, geometry.bytes_per_sector))
}

/// 检查一个 FAT 卷。`sector` 是认出来的引导扇区（`from_backup` 时来自备份）。被取消时返回 `None`。
pub(crate) fn check(ctx: &mut Ctx, sector: &[u8], from_backup: bool) -> Option<Summary> {
    let geometry = match parse(sector) {
        Ok(geometry) => geometry,
        Err(field) => {
            ctx.issues.push(
                Issue::new(IssueKind::BootInvalid)
                    .area(Area::Boot)
                    .field(field),
            );
            return Some(unknown());
        }
    };
    ctx.checked(CheckItem::Boot);
    let bps = geometry.bytes_per_sector;
    let cluster_size = geometry.sectors_per_cluster * bps;
    let length = ctx.disk.length();

    let volume_bytes = geometry.total_sectors.saturating_mul(bps);
    if volume_bytes > length {
        ctx.issues.push(
            Issue::new(IssueKind::VolumeTruncated)
                .area(Area::Boot)
                .expected(volume_bytes)
                .actual(length),
        );
    }
    // 簇数受 FAT 能装下的表项数限制，分区被截短时只算到分区末尾
    let heap_offset = geometry.first_data_sector().saturating_mul(bps);
    let fat_entries = geometry.fat_sectors.saturating_mul(bps).saturating_mul(8) / geometry.bits();
    if fat_entries < geometry.cluster_count + 2 {
        ctx.issues.push(
            Issue::new(IssueKind::BootInvalid)
                .area(Area::Boot)
                .field("fatSize"),
        );
    }
    let fits = length.saturating_sub(heap_offset) / cluster_size;
    let count = geometry
        .cluster_count
        .min(fat_entries.saturating_sub(2))
        .min(fits);
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    let fs = match geometry.width {
        Width::Fat12 => FsType::Fat12,
        Width::Fat16 => FsType::Fat16,
        Width::Fat32 | Width::Exfat => FsType::Fat32,
    };

    if geometry.width == Width::Fat32 && !from_backup {
        check_backup_boot(ctx, &geometry, sector);
    }
    // FSInfo 里的空闲簇数只是提示，Windows 正常卸下的卷也常对不上，不核对，只查签名
    if geometry.width == Width::Fat32 {
        check_fs_info(ctx, &geometry);
    }

    let fat_start = (geometry.reserved + geometry.active_fat * geometry.fat_sectors) * bps;
    let fat = FatTable::new(fat_start, geometry.fat_sectors * bps, geometry.width, count);
    let mut volume = Volume {
        fat,
        owners: Owners::new(u64::from(count) + 2),
        cluster_size,
        heap_offset,
        claimed: 0,
        used: 0,
        files: 0,
        directories: 0,
    };
    if geometry.mirroring && geometry.fats > 1 {
        compare_copies(ctx, &geometry, count)?;
    }
    scan_table(ctx, &mut volume, &geometry)?;

    let mut checker = FatChecker {
        volume,
        is_fat32: geometry.width == Width::Fat32,
        root_cluster: geometry.root_cluster,
    };
    let root = if checker.is_fat32 {
        DirAt::Clusters(geometry.root_cluster)
    } else {
        DirAt::Fixed {
            offset: (geometry.reserved + geometry.fats * geometry.fat_sectors) * bps,
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
        &mut |ctx, fat, cluster| match fat.link(ctx, cluster) {
            Link::Unknown => None,
            link => Some(link.is_used()),
        },
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
        fs: FsType::Unknown,
        cluster_size: 0,
        cluster_count: 0,
        used_clusters: 0,
        files: 0,
        directories: 0,
    }
}

/// FAT32 的备份引导扇区（位置写在 BPB 里，通常是第 6 扇区；为 0 表示没有）：几何参数应与主引导扇区一致。
fn check_backup_boot(ctx: &mut Ctx, geometry: &Geometry, main: &[u8]) {
    let at = geometry.backup_boot;
    if at == 0 || at == 0xFFFF {
        return;
    }
    ctx.checked(CheckItem::BackupBoot);
    let mut backup = vec![0u8; 512];
    let readable = at < geometry.reserved
        && ctx
            .read(
                at * geometry.bytes_per_sector,
                &mut backup,
                Area::BackupBoot,
                None,
            )
            .is_some_and(|bad| bad.is_empty());
    if !readable || parse(&backup).is_err() {
        ctx.issues
            .push(Issue::new(IssueKind::BackupBootInvalid).area(Area::BackupBoot));
        return;
    }
    // 只比几何参数（到卷序列号之前）：卷标、序列号、引导代码不同不影响使用
    if backup.get(11..67) != main.get(11..67) {
        ctx.issues
            .push(Issue::new(IssueKind::BackupBootMismatch).area(Area::BackupBoot));
    }
}

/// FAT32 的 FSInfo 扇区（位置写在 BPB 里，为 0 表示没有）：三处签名要对。
fn check_fs_info(ctx: &mut Ctx, geometry: &Geometry) -> Option<()> {
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
    if le32(&sector, 0) != Some(0x4161_5252)
        || le32(&sector, 484) != Some(0x6141_7272)
        || le32(&sector, 508) != Some(0xAA55_0000)
    {
        ctx.issues
            .push(Issue::new(IssueKind::FsInfoInvalid).area(Area::FsInfo));
    }
    Some(())
}

/// 比对各份 FAT（只比到最后一个簇的表项），不同之处按表项计数。读不出的部分不比。被取消时返回 `None`。
fn compare_copies(ctx: &mut Ctx, geometry: &Geometry, count: u32) -> Option<()> {
    let bps = geometry.bytes_per_sector;
    let bits = geometry.bits();
    let length = (u64::from(count) + 2)
        .saturating_mul(bits)
        .div_ceil(8)
        .min(geometry.fat_sectors * bps);
    let first_copy = geometry.reserved * bps;
    for copy in 1..geometry.fats {
        let other_copy = (geometry.reserved + copy * geometry.fat_sectors) * bps;
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
        }
    }
    ctx.checked(CheckItem::FatCopies);
    Some(())
}

/// 读一遍 FAT：第 0、1 项（介质描述符、卷的脏标记与硬错误标记），统计已占用、坏簇与空闲的簇。
/// 被取消时返回 `None`。
fn scan_table(ctx: &mut Ctx, volume: &mut Volume, geometry: &Geometry) -> Option<()> {
    let fat = &mut volume.fat;
    if let Some(entry0) = fat.raw(ctx, 0)
        && (entry0 & 0xFF) as u8 != geometry.media
    {
        ctx.issues.push(
            Issue::new(IssueKind::FatMediaMismatch)
                .area(Area::Fat)
                .expected(u64::from(geometry.media))
                .actual(u64::from(entry0 & 0xFF)),
        );
    }
    if let Some(entry1) = fat.raw(ctx, 1) {
        // FAT16 第 15 位、FAT32 第 27 位为 1 表示正常卸下；FAT16 第 14 位、FAT32 第 26 位为 0 表示出过读写错误
        let (clean, no_error) = match geometry.width {
            Width::Fat16 => (0x8000, 0x4000),
            Width::Fat32 => (0x0800_0000, 0x0400_0000),
            Width::Fat12 | Width::Exfat => (0, 0),
        };
        if clean != 0 && entry1 & clean == 0 {
            ctx.issues
                .push(Issue::new(IssueKind::VolumeDirty).area(Area::Fat));
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
        }];
        while let Some(job) = stack.pop() {
            if !ctx.control.checkpoint() {
                return None;
            }
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
                    )?;
                    if walk.first_taken {
                        // 起始簇是别人的（已记为交叉链接）：不再往下查，免得在目录之间绕圈
                        continue;
                    }
                    self.volume.read_runs(ctx, &walk.runs, &job.path)
                }
            };
            self.volume.report_progress(ctx);
            self.directory(ctx, &job, &data, &bad, &mut stack)?;
        }
        Some(())
    }

    /// 查一个目录的内容：目录项、长文件名、`.` 与 `..`；文件当场认领簇，子目录压栈。
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
        if let DirAt::Clusters(first) = job.at
            && job.id != ROOT
        {
            self.check_dots(ctx, job, first, entries, &unreadable);
        }
        let mut pending: Vec<&[u8; ENTRY]> = Vec::new();
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
                    pending.clear();
                }
                if !starts && pending.is_empty() {
                    ctx.issues
                        .push(Issue::new(IssueKind::LfnOrder).path(job.path.as_str()));
                    continue;
                }
                pending.push(raw);
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
                self.long_name(ctx, &job.path, &short_name, &short, &parts)
            };
            let name = long.unwrap_or(short_name);
            let path = join(&job.path, &name);
            let cluster_low = u32::from(le16(raw, 26).unwrap_or(0));
            let cluster_high = if self.is_fat32 {
                u32::from(le16(raw, 20).unwrap_or(0))
            } else {
                0
            };
            let first = cluster_high << 16 | cluster_low;
            let id = self.volume.owners.add(Some(job.id), &name);
            if attr & ATTR_DIRECTORY != 0 {
                self.volume.directories += 1;
                if first == 0 {
                    ctx.issues.push(
                        Issue::new(IssueKind::FirstClusterInvalid)
                            .path(path)
                            .actual(0),
                    );
                } else if job.depth + 1 > MAX_DEPTH {
                    ctx.issues.push(Issue::new(IssueKind::TooDeep).path(path));
                } else {
                    stack.push(DirJob {
                        id,
                        path,
                        at: DirAt::Clusters(first),
                        parent_cluster: match job.at {
                            DirAt::Clusters(cluster) if job.id != ROOT => cluster,
                            _ => 0,
                        },
                        depth: job.depth + 1,
                    });
                }
            } else {
                self.volume.files += 1;
                let size = u64::from(le32(raw, 28).unwrap_or(0));
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
                )?;
            }
        }
        if !pending.is_empty() {
            ctx.issues
                .push(Issue::new(IssueKind::LfnOrder).path(job.path.as_str()));
        }
        Some(())
    }

    /// 子目录开头应是 `.`（指向自己）与 `..`（指向上级，上级是根目录时为 0；FAT32 也接受根目录的簇号）。
    fn check_dots(
        &self,
        ctx: &mut Ctx,
        job: &DirJob,
        first: u32,
        entries: &[[u8; ENTRY]],
        unreadable: &dyn Fn(usize) -> bool,
    ) {
        if unreadable(0) || unreadable(1) {
            return;
        }
        let cluster_of = |raw: &[u8; ENTRY]| {
            let high = if self.is_fat32 {
                u32::from(le16(raw, 20).unwrap_or(0))
            } else {
                0
            };
            high << 16 | u32::from(le16(raw, 26).unwrap_or(0))
        };
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
        }
    }

    /// 核对长文件名各段：序号从 `0x40 | n` 递减到 1、段数合理，校验和与短文件名一致。都对时返回长文件名。
    fn long_name(
        &self,
        ctx: &mut Ctx,
        dir: &str,
        short_name: &str,
        short: &[u8; 11],
        parts: &[&[u8; ENTRY]],
    ) -> Option<String> {
        let path = join(dir, short_name);
        let count = parts.len();
        let ordered = count <= MAX_LFN_PARTS
            && parts
                .first()
                .is_some_and(|first| first[0] & 0x40 != 0 && usize::from(first[0] & 0x1F) == count)
            && parts
                .iter()
                .enumerate()
                .all(|(index, part)| usize::from(part[0] & 0x1F) == count - index);
        if !ordered {
            ctx.issues.push(Issue::new(IssueKind::LfnOrder).path(path));
            return None;
        }
        let checksum = short_checksum(short);
        if parts.iter().any(|part| part[13] != checksum) {
            ctx.issues
                .push(Issue::new(IssueKind::LfnChecksum).path(path));
            return None;
        }
        let mut units = Vec::with_capacity(count * 13);
        for part in parts.iter().rev() {
            let positions = (1..11)
                .step_by(2)
                .chain((14..26).step_by(2))
                .chain((28..32).step_by(2));
            units.extend(positions.filter_map(|at| le16(part.as_slice(), at)));
        }
        if let Some(end) = units.iter().position(|&unit| unit == 0) {
            units.truncate(end);
        }
        while units.last() == Some(&0xFFFF) {
            units.pop();
        }
        let name = String::from_utf16_lossy(&units);
        (!name.is_empty()).then_some(name)
    }
}

pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}
