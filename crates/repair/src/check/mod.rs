//! 文件系统一致性检查（只读）：exFAT、FAT12/16/32 做完整检查，NTFS 只查引导扇区与 `$MFT` 的位置。
//!
//! 只读源盘，输出问题清单：每项有类型、严重程度、涉及的路径或簇，界面据此给出说明与以后的修法。
//! 读不出的扇区记成问题、补 0 继续，不中断检查。依据微软公开的 exFAT 与 FAT 文件系统规范。

mod chains;
mod disk;
mod exfat;
mod fat;
mod issue;
mod ntfs;
mod table;
#[cfg(test)]
mod tests;

use yisi_fs::FsKind;
use yisi_fs::boot::{BackupFamily, backup_candidates, backup_kind, remnant_kind, usable_kind};
use yisi_fs::bpb::{self, Problem, Rule};
use yisi_io::BlockDevice;

pub use issue::{Area, Issue, IssueKind, Severity, Totals};

use crate::check::disk::Disk;
use crate::check::issue::Issues;

/// 一次检查最多列出这么多个问题，再多的只计数：结果要放进一条消息里送给界面。
pub const MAX_ISSUES: usize = 1000;

/// 检查分几个阶段，进度按阶段报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// 引导区；`done` / `total` 是扇区数。
    Boot,
    /// 读一遍 FAT（比对各份 FAT、统计已占用的簇）；单位是簇。
    Fat,
    /// 遍历目录树、沿 FAT 链认领每个文件的簇；单位是簇（已认领 / 已占用）。
    Directories,
    /// 逐簇汇总：丢失簇、分配位图与占用是否一致；单位是簇。
    Clusters,
    /// NTFS 的 `$MFT` 与 `$MFTMirr`；单位是记录。
    Mft,
}

impl Stage {
    /// 协议里的阶段名。
    pub fn key(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::Fat => "fat",
            Self::Directories => "directories",
            Self::Clusters => "clusters",
            Self::Mft => "mft",
        }
    }
}

/// 检查过程中的暂停、取消与进度。
pub trait Control {
    /// 检查点：暂停时在这里等到继续；返回 `false` 表示已取消，检查随即结束。
    fn checkpoint(&self) -> bool;
    fn progress(&self, stage: Stage, done: u64, total: u64);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsType {
    Fat12,
    Fat16,
    Fat32,
    Exfat,
    Ntfs,
    /// 主、备引导扇区都认不出。
    Unknown,
}

impl FsType {
    pub fn key(self) -> &'static str {
        match self {
            Self::Fat12 => "fat12",
            Self::Fat16 => "fat16",
            Self::Fat32 => "fat32",
            Self::Exfat => "exfat",
            Self::Ntfs => "ntfs",
            Self::Unknown => "unknown",
        }
    }
}

/// 检查过的项目；做不了的列在 [`Report::skipped`] 里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckItem {
    /// 引导扇区的参数（含 exFAT 的扩展引导区、FAT32 的 FSInfo）。
    Boot,
    /// 备份引导扇区。
    BackupBoot,
    /// exFAT 引导区校验和。
    BootChecksum,
    /// 各份 FAT 是否一致。
    FatCopies,
    /// FAT 链：越界、成环、指向空闲簇、链长与文件大小。
    Chains,
    /// 目录项：exFAT 的目录项集合，FAT 的长文件名与 `.`、`..`。
    Entries,
    /// 交叉链接。
    CrossLinks,
    /// 丢失簇。
    LostClusters,
    /// exFAT 分配位图与实际占用。
    Bitmap,
    /// exFAT 大写表校验和。
    Upcase,
    /// NTFS `$MFT` 的位置与第一条记录。
    Mft,
    /// NTFS `$MFTMirr` 与 `$MFT` 前几条记录是否一致。
    MftMirror,
}

impl CheckItem {
    pub fn key(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::BackupBoot => "backupBoot",
            Self::BootChecksum => "bootChecksum",
            Self::FatCopies => "fatCopies",
            Self::Chains => "chains",
            Self::Entries => "entries",
            Self::CrossLinks => "crossLinks",
            Self::LostClusters => "lostClusters",
            Self::Bitmap => "bitmap",
            Self::Upcase => "upcase",
            Self::Mft => "mft",
            Self::MftMirror => "mftMirror",
        }
    }
}

/// 检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub fs: FsType,
    pub cluster_size: u64,
    pub cluster_count: u64,
    /// FAT 或分配位图里记为已占用的簇数（NTFS 为 0）。
    pub used_clusters: u64,
    /// 遍历到的文件与目录数（不含根目录）。
    pub files: u64,
    pub directories: u64,
    /// 至多 [`MAX_ISSUES`] 项；按发现的顺序。
    pub issues: Vec<Issue>,
    /// 超出上限、没有列出的问题数。
    pub omitted: u64,
    /// 各严重程度的问题数，含没有列出的。
    pub totals: Totals,
    /// 读不出的字节数（分区内，相同位置只算一次）。
    pub unreadable_bytes: u64,
    pub checked: Vec<CheckItem>,
    /// 因为结构损坏、卷太大等原因没能做的检查。
    pub skipped: Vec<CheckItem>,
}

/// 检查设备上 `offset` 起 `length` 字节的分区。被取消时返回 `None`。
pub fn check(
    device: &dyn BlockDevice,
    offset: u64,
    length: u64,
    control: &dyn Control,
) -> Option<Report> {
    let mut ctx = Ctx {
        disk: Disk::new(device, offset, length),
        issues: Issues::default(),
        control,
        checked: Vec::new(),
        skipped: Vec::new(),
    };
    control.progress(Stage::Boot, 0, 1);
    let detected = detect(&mut ctx);
    control.progress(Stage::Boot, 1, 1);
    let summary = match detected {
        Detected::Fat {
            sector,
            from_backup,
        } => fat::check(&mut ctx, &sector, from_backup)?,
        Detected::Exfat {
            region,
            from_backup,
        } => exfat::check(&mut ctx, &region, from_backup)?,
        Detected::Ntfs {
            sector,
            from_backup,
        } => ntfs::check(&mut ctx, &sector, from_backup)?,
        Detected::Unknown => {
            ctx.issues
                .push(Issue::new(IssueKind::UnknownFilesystem).area(Area::Boot));
            Summary::unknown()
        }
    };
    let Ctx {
        disk,
        issues,
        mut checked,
        mut skipped,
        ..
    } = ctx;
    checked.sort_unstable();
    checked.dedup();
    skipped.sort_unstable();
    skipped.dedup();
    skipped.retain(|item| !checked.contains(item));
    let (issues, omitted, totals) = issues.finish();
    Some(Report {
        fs: summary.fs,
        cluster_size: summary.cluster_size,
        cluster_count: summary.cluster_count,
        used_clusters: summary.used_clusters,
        files: summary.files,
        directories: summary.directories,
        issues,
        omitted,
        totals,
        unreadable_bytes: disk.unreadable_bytes(),
        checked,
        skipped,
    })
}

/// 各文件系统的检查交回的概况。
pub(crate) struct Summary {
    pub fs: FsType,
    pub cluster_size: u64,
    pub cluster_count: u64,
    pub used_clusters: u64,
    pub files: u64,
    pub directories: u64,
}

impl Summary {
    fn unknown() -> Self {
        Self {
            fs: FsType::Unknown,
            cluster_size: 0,
            cluster_count: 0,
            used_clusters: 0,
            files: 0,
            directories: 0,
        }
    }
}

/// 检查时共用的状态：只读视图、问题清单、暂停与取消。
pub(crate) struct Ctx<'a> {
    pub disk: Disk<'a>,
    pub issues: Issues,
    pub control: &'a dyn Control,
    pub checked: Vec<CheckItem>,
    pub skipped: Vec<CheckItem>,
}

impl Ctx<'_> {
    /// 读分区内 `at` 起的 `buf`；读不出的扇区补 0，第一次遇到时记成问题（标明在读什么）。
    /// 超出分区时返回 `None`，`buf` 不变。返回读不出的区段（相对 `buf` 起点）。
    pub fn read(
        &mut self,
        at: u64,
        buf: &mut [u8],
        area: Area,
        path: Option<&str>,
    ) -> Option<Vec<disk::Span>> {
        let bad = self.disk.read(at, buf)?;
        let mut spans = Vec::with_capacity(bad.len());
        for (span, fresh) in bad {
            if fresh {
                let mut issue = Issue::new(IssueKind::Unreadable)
                    .area(area)
                    .offset(at.saturating_add(span.offset))
                    .count(span.length);
                if let Some(path) = path {
                    issue = issue.path(path);
                }
                self.issues.push(issue);
            }
            spans.push(span);
        }
        Some(spans)
    }

    pub fn checked(&mut self, item: CheckItem) {
        self.checked.push(item);
    }

    /// 引导扇区按 [`yisi_fs::bpb`] 核对出的问题（规则与打开卷、引导扇区诊断共用），一项记一条，字段名用规范里的叫法。
    /// 跳转指令不标准、NTFS 的旧字段不为 0、FAT32 记的备份位置不在保留区（备份另按常见位置找）只是形式上不合规范，
    /// 记成可以忽略的 [`IssueKind::BootNote`]。
    pub fn boot_problems(&mut self, problems: &[Problem], area: Area) {
        for problem in problems {
            let cosmetic = matches!(problem.rule, Rule::Jump | Rule::BackupBoot)
                || (problem.rule == Rule::LegacyFields && !problem.fatal);
            let kind = if cosmetic {
                IssueKind::BootNote
            } else {
                IssueKind::BootInvalid
            };
            self.issues
                .push(Issue::new(kind).area(area).field(problem.field));
        }
    }

    pub fn skipped(&mut self, item: CheckItem) {
        self.skipped.push(item);
    }
}

enum Detected {
    Fat {
        sector: Vec<u8>,
        from_backup: bool,
    },
    /// exFAT 的整个引导区（12 个扇区）。
    Exfat {
        region: Vec<u8>,
        from_backup: bool,
    },
    Ntfs {
        sector: Vec<u8>,
        from_backup: bool,
    },
    Unknown,
}

/// 按主引导扇区识别（[`usable_kind`]，与打开卷相同）；不能用时依次试备份引导扇区，位置与认法同打开卷、引导扇区诊断
/// （[`backup_candidates`]、[`backup_kind`]）。主引导扇区不能用而备份能用时记一个问题，按备份接着检查；
/// 主引导扇区还认得出是哪种文件系统时，再列出它哪几项不对。
fn detect(ctx: &mut Ctx) -> Detected {
    let mut sector = vec![0u8; 512];
    let readable = ctx.read(0, &mut sector, Area::Boot, None).is_some();
    if readable
        && let Some(kind) = usable_kind(&sector)
        && let Some(found) = load(ctx, kind, &sector, 0, false)
    {
        return found;
    }
    if let Some(kind) = readable.then(|| remnant_kind(&sector)).flatten() {
        let problems = match kind {
            FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 => {
                bpb::check_fat(&sector, Some(kind)).problems
            }
            FsKind::Exfat => bpb::check_exfat(&sector).problems,
            FsKind::Ntfs => bpb::check_ntfs(&sector).problems,
            _ => Vec::new(),
        };
        ctx.boot_problems(&problems, Area::Boot);
    }
    let device_sector = u32::try_from(ctx.disk.sector_size()).unwrap_or(512);
    for candidate in backup_candidates(&sector, device_sector, ctx.disk.length()) {
        let mut backup = vec![0u8; 512];
        if ctx
            .read(candidate.offset, &mut backup, Area::BackupBoot, None)
            .is_none_or(|bad| !bad.is_empty())
        {
            continue;
        }
        if let Some(kind) = backup_kind(&candidate, &backup)
            && let Some(found) = load(ctx, kind, &backup, candidate.offset, true)
        {
            ctx.issues
                .push(Issue::new(IssueKind::BootUnrecognized).area(Area::Boot));
            return found;
        }
    }
    Detected::Unknown
}

/// 读齐检查要用的内容：exFAT 是 `at` 起的整个引导区（12 个扇区），其余是这一个扇区。
fn load(
    ctx: &mut Ctx,
    kind: FsKind,
    sector: &[u8],
    at: u64,
    from_backup: bool,
) -> Option<Detected> {
    match kind {
        FsKind::Exfat => {
            let bytes_per_sector = bpb::check_exfat(sector).bytes_per_sector?;
            let mut region = vec![0u8; usize::try_from(12 * bytes_per_sector).ok()?];
            let area = if from_backup {
                Area::BackupBoot
            } else {
                Area::Boot
            };
            ctx.read(at, &mut region, area, None)?;
            Some(Detected::Exfat {
                region,
                from_backup,
            })
        }
        FsKind::Ntfs => Some(Detected::Ntfs {
            sector: sector.to_vec(),
            from_backup,
        }),
        FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 => Some(Detected::Fat {
            sector: sector.to_vec(),
            from_backup,
        }),
        _ => None,
    }
}

/// 找备份引导扇区并与主引导扇区比对：位置与认法同打开卷、引导扇区诊断（[`backup_candidates`]、[`backup_kind`]），
/// 比哪些字段同引导扇区诊断（[`bpb::differing_fields`]），一个字段记一条。找到时返回它在分区里的位置。
pub(crate) fn check_backup(ctx: &mut Ctx, kind: FsKind, main: &[u8]) -> Option<u64> {
    let family = BackupFamily::of(kind)?;
    ctx.checked(CheckItem::BackupBoot);
    let device_sector = u32::try_from(ctx.disk.sector_size()).unwrap_or(512);
    let candidates = backup_candidates(main, device_sector, ctx.disk.length());
    for candidate in candidates
        .into_iter()
        .filter(|candidate| candidate.family == family)
    {
        let mut backup = vec![0u8; 512];
        if ctx
            .read(candidate.offset, &mut backup, Area::BackupBoot, None)
            .is_none_or(|bad| !bad.is_empty())
            || backup_kind(&candidate, &backup) != Some(kind)
        {
            continue;
        }
        for difference in bpb::differing_fields(kind, main, &backup) {
            ctx.issues.push(
                Issue::new(IssueKind::BackupBootMismatch)
                    .area(Area::BackupBoot)
                    .field(difference.field)
                    .expected(difference.primary)
                    .actual(difference.backup),
            );
        }
        return Some(candidate.offset);
    }
    ctx.issues
        .push(Issue::new(IssueKind::BackupBootInvalid).area(Area::BackupBoot));
    None
}
