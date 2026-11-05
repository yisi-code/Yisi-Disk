//! 文件系统一致性检查（只读）：exFAT、FAT12/16/32 做完整检查；NTFS 查引导扇区、`$MFT` 与 `$MFTMirr`，
//! 再按 [`yisi_fs::audit_ntfs`] 核对记录、目录索引与簇的分配。
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

use std::sync::Arc;

use yisi_fs::boot::{BackupFamily, backup_candidates, backup_kind, remnant_kind, usable_kind};
use yisi_fs::boot_write::EXFAT_BOOT_REGION_SECTORS;
use yisi_fs::bpb::{self, Problem, Rule};
use yisi_fs::format::{FatWidth, Run};
use yisi_fs::{FsKind, Region};
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
    /// NTFS 的 `$MFT` 与 `$MFTMirr`，以及逐条读 MFT 记录；单位是记录。
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
    /// NTFS 每条 MFT 记录、记录位图、文件的上级目录。
    MftRecords,
    /// NTFS 目录索引：项指向的记录、文件在不在上级目录的索引里。
    Indexes,
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
            Self::MftRecords => "mftRecords",
            Self::Indexes => "indexes",
        }
    }
}

/// 检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub fs: FsKind,
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

/// 修复要用、检查顺带得出的结论（FAT / exFAT；其他文件系统为空）。修复只按这些结论生成写入，
/// 与检查用同一套规则，不另判一遍。卷太大、不跟踪认领时（`tracked` 为 `false`）丢失簇、位图漏标、
/// 链过长多出来的簇这几项为空。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Findings {
    /// 按备份引导扇区检查的（主引导扇区不能用）。
    pub from_backup: bool,
    pub layout: Option<FatLayout>,
    pub tracked: bool,
    /// 全部丢失簇（已占用却没人认领），按簇号排序。
    pub lost: Vec<Run>,
    /// exFAT：有人认领、分配位图里却是空闲的簇。
    pub not_in_bitmap: Vec<Run>,
    pub too_long: Vec<LongChain>,
    /// 卷标记为未正常卸下。
    pub dirty: bool,
    /// FAT32 的 FSInfo 签名不对。
    pub fs_info_invalid: bool,
    /// 各份 FAT 不一致。
    pub fat_copies_differ: bool,
    /// 两份 FAT 不一致时哪几处不同（只有两份时给出；三份以上不修）。
    pub fat_copies: Option<FatCopies>,
    /// 断了的链：在哪里截断、文件大小改成多少。
    pub broken: Vec<BrokenChain>,
    /// 交叉链接里后认领的那个文件（只收沿 FAT 链存放、链本身没断的文件）。
    pub cross: Vec<CrossLinked>,
    /// 目录项要改的地方。
    pub entries: Vec<EntryFix>,
    /// 真正空闲的簇（FAT 或分配位图里空闲、也没人认领），按簇号排序：修交叉链接时从这里取新簇，
    /// 找坏簇时只读这些簇。FAT 里标成坏簇的也在里面，用之前要再看一眼表项。分配位图本身坏了时，
    /// 是没人认领的簇（重建位图时从这里取簇）。
    pub free: Vec<Run>,
    /// exFAT 大写表坏了或找不到。
    pub upcase: Option<UpcaseFix>,
    /// exFAT 分配位图本身坏了（找不到位图项、比簇数短）、又跟踪了每个簇的归属时：按目录树重建要用的。
    pub rebuild: Option<BitmapRebuild>,
    /// exFAT 根目录里第一个空着的项（类型为 0），补位图项、大写表项时写在这里。
    pub root_slot: Option<u64>,
    /// FAT 里起始簇不合法的子目录：改成空目录。
    pub empty_dirs: Vec<EmptyDir>,
}

/// exFAT 大写表的修法：`kind` 是 `UpcaseChecksum` 或 `UpcaseMissing`；`entry` 是大写表项的位置（找不到时为空，
/// 补在根目录的空项里）；`runs` 是表现在占的簇；`standard` 表示表的内容就是标准大写表、只是记的校验和不对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpcaseFix {
    pub kind: IssueKind,
    pub entry: Option<u64>,
    pub runs: Vec<Run>,
    pub standard: bool,
}

/// 按目录树重建 exFAT 分配位图：`claimed` 是有人认领的簇（位图自己的旧簇不算），`entry` 是位图项的位置
/// （找不到时为空，补在根目录的空项里），`old` 是旧位图占的簇（重建后释放）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapRebuild {
    pub entry: Option<u64>,
    pub claimed: Vec<Run>,
    pub old: Vec<Run>,
}

/// FAT 里起始簇不合法的子目录：`at` 是它的目录项，`parent` 是上级目录的起始簇（上级是根目录时为 0，用来写 `..`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyDir {
    pub path: String,
    pub at: u64,
    pub parent: u32,
}

/// 两份 FAT 的位置与不同之处（分区内字节偏移，按扇区对齐的区段，相对每份 FAT 的起点）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FatCopies {
    pub copies: [u64; 2],
    pub differing: Vec<(u64, u64)>,
}

/// 一个文件或目录的目录项在分区里的位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryAt {
    /// FAT：短文件名那一项。
    Fat(u64),
    /// exFAT：整组项，第一项是文件项、第二项是流扩展项。
    Exfat(Vec<u64>),
}

/// 认领簇的是哪个目录项；系统结构（exFAT 的分配位图、大写表）没有。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRef {
    pub at: EntryAt,
    /// 记录的大小；FAT 的目录没有大小，为 `None`。
    pub size: Option<u64>,
    pub directory: bool,
}

/// 断了的链（起始簇不合法、越界、成环、指向空闲簇或坏簇、比大小短）。修法与 Windows 磁盘检查相同：
/// 在 `end_at` 写链尾（为 `None` 时不改链），大小改成 `size`（目录没有大小时为 `None`）；
/// `empty` 时起始簇与大小都改成 0。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokenChain {
    pub kind: IssueKind,
    pub path: String,
    pub at: EntryAt,
    pub end_at: Option<u32>,
    pub size: Option<u64>,
    pub empty: bool,
}

/// 交叉链接：`path` 是后认领的文件，`chain` 是它的整条链（按顺序），`shared` 是其中与别人共用的位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossLinked {
    pub path: String,
    pub at: EntryAt,
    pub chain: Vec<u32>,
    pub shared: Vec<usize>,
}

/// 目录项的一处修改；`kinds` 是它修的那几类问题（一组 exFAT 目录项可能同时有几类）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryFix {
    pub kinds: Vec<IssueKind>,
    pub path: String,
    pub change: EntryChange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryChange {
    /// exFAT：重算这组项的校验和（`entries` 是改后的整组项，次项个数、文件名长度一并改成给出的值）。
    ExfatSet {
        entries: Vec<u64>,
        count: Option<u8>,
        name_length: Option<u8>,
        valid_length: Option<u64>,
    },
    /// 标为已删除：FAT 首字节改成 0xE5，exFAT 清掉类型的第 7 位。
    Delete { at: Vec<u64>, exfat: bool },
    /// FAT 子目录开头的 `.` 与 `..`：`at` 是第一项的位置，`parent` 为 0 表示上级是根目录。
    Dots { at: u64, cluster: u32, parent: u32 },
}

/// FAT / exFAT 的结构在分区里的位置（字节偏移）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FatLayout {
    pub kind: FsKind,
    pub width: FatWidth,
    pub bytes_per_sector: u64,
    /// 要一起改的各份 FAT 的起点：镜像时是全部几份，不镜像时只有在用的那份。
    pub fats: Vec<u64>,
    pub cluster_count: u32,
    pub cluster_size: u64,
    /// 2 号簇的位置。
    pub heap_offset: u64,
    /// FAT32 的 FSInfo 扇区。
    pub fs_info: Option<u64>,
    /// exFAT 分配位图所在的簇。
    pub bitmap: Vec<Run>,
}

/// 链比文件大小需要的长：保留到 `last` 为止，`extra` 是多出来、只属于这个文件的簇（被别的文件认领的不在里面）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LongChain {
    pub path: String,
    pub last: u32,
    pub extra: Vec<u32>,
}

/// 检查设备上 `offset` 起 `length` 字节的分区。被取消时返回 `None`。
pub fn check(
    device: &Arc<dyn BlockDevice>,
    offset: u64,
    length: u64,
    control: &dyn Control,
) -> Option<Report> {
    check_for_repair(device, offset, length, control).map(|(report, _)| report)
}

/// 同 [`check`]，另外交出修复要用的结论。
pub fn check_for_repair(
    device: &Arc<dyn BlockDevice>,
    offset: u64,
    length: u64,
    control: &dyn Control,
) -> Option<(Report, Findings)> {
    let mut ctx = Ctx {
        disk: Disk::new(device.as_ref(), offset, length),
        region: Region::new(Arc::clone(device), offset, length),
        issues: Issues::default(),
        control,
        checked: Vec::new(),
        skipped: Vec::new(),
        findings: Findings::default(),
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
        findings,
        ..
    } = ctx;
    checked.sort_unstable();
    checked.dedup();
    skipped.sort_unstable();
    skipped.dedup();
    skipped.retain(|item| !checked.contains(item));
    let (issues, omitted, totals) = issues.finish();
    let report = Report {
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
    };
    Some((report, findings))
}

/// 各文件系统的检查交回的概况。
pub(crate) struct Summary {
    pub fs: FsKind,
    pub cluster_size: u64,
    pub cluster_count: u64,
    pub used_clusters: u64,
    pub files: u64,
    pub directories: u64,
}

impl Summary {
    fn unknown() -> Self {
        Self {
            fs: FsKind::Unknown,
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
    /// 同一个分区，给 [`yisi_fs`] 里按结构读卷的核对用（读不出时它自己处理，不记进 `disk`）。
    pub region: Region,
    pub issues: Issues,
    pub control: &'a dyn Control,
    pub checked: Vec<CheckItem>,
    pub skipped: Vec<CheckItem>,
    pub findings: Findings,
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
            let mut region =
                vec![0u8; usize::try_from(EXFAT_BOOT_REGION_SECTORS * bytes_per_sector).ok()?];
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
