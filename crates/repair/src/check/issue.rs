//! 问题清单：每个问题的类型、严重程度与涉及的位置。说明与修法由界面按类型给出。

use crate::check::MAX_ISSUES;

/// 严重程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// 文件内容已经或可能读错、读不全，或者会被新写入的文件覆盖。
    DataLoss,
    /// 元数据前后不一致，暂时不影响读文件，放着不管可能越来越糟。
    Structure,
    /// 可以忽略：只是提示，或者系统自己会处理。
    Minor,
}

impl Severity {
    pub fn key(self) -> &'static str {
        match self {
            Self::DataLoss => "dataLoss",
            Self::Structure => "structure",
            Self::Minor => "minor",
        }
    }
}

/// 问题出在哪个结构上（不是某个文件时）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    Boot,
    BackupBoot,
    FsInfo,
    Fat,
    Bitmap,
    Upcase,
    RootDir,
    Directory,
    Mft,
    MftMirror,
}

impl Area {
    pub fn key(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::BackupBoot => "backupBoot",
            Self::FsInfo => "fsInfo",
            Self::Fat => "fat",
            Self::Bitmap => "bitmap",
            Self::Upcase => "upcase",
            Self::RootDir => "rootDir",
            Self::Directory => "directory",
            Self::Mft => "mft",
            Self::MftMirror => "mftMirror",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueKind {
    /// 主、备引导扇区都认不出是 FAT、exFAT 或 NTFS。
    UnknownFilesystem,
    /// 主引导扇区认不出，按备份引导扇区检查。
    BootUnrecognized,
    /// 引导扇区的参数不合规范；`field` 是哪一项。
    BootInvalid,
    /// exFAT 引导区校验和不对（`area` 区分主、备）。
    BootChecksum,
    /// 备份引导扇区读不出、认不出或参数不合规范。
    BackupBootInvalid,
    /// 备份引导扇区与主引导扇区的参数不一致。
    BackupBootMismatch,
    /// 文件系统记录的大小超出分区（分区被截短）：`expected` 是文件系统的字节数，`actual` 是分区的。
    VolumeTruncated,
    /// 卷标记为「未正常卸下」：拔盘前没有安全弹出，或者写到一半断电。
    VolumeDirty,
    /// 文件系统自己记录过读写错误（FAT 的硬错误位、exFAT 的介质故障位）。
    MediaFailure,
    /// FAT32 的 FSInfo 扇区签名不对。
    FsInfoInvalid,
    /// 两份 FAT 不一致：`cluster` 是第一处不同的簇，`count` 是不同的表项数。
    FatCopiesDiffer,
    /// FAT 第 0 项与引导扇区的介质描述符不符。
    FatMediaMismatch,
    /// FAT 里标记为坏簇的簇数（`count`）。
    BadClustersMarked,
    /// exFAT 根目录里没有可用的分配位图（`actual` 为位图字节数时是位图太短）。
    BitmapInvalid,
    /// exFAT 根目录里没有大写表。
    UpcaseMissing,
    /// 大写表的校验和不对：`expected` 是记录的，`actual` 是算出的。
    UpcaseChecksum,
    /// 文件或目录的起始簇不合法：`actual` 是记录的簇号。
    FirstClusterInvalid,
    /// FAT 链指向簇区之外：`cluster` 是出错的那一项，`actual` 是它的值。
    ChainOutOfRange,
    /// FAT 链成环：`cluster` 是回到的那个簇。
    ChainLoop,
    /// FAT 链在 `cluster` 处的表项是「空闲」，链在这里断开。
    ChainFree,
    /// FAT 链经过标记为坏簇的 `cluster`。
    ChainBadCluster,
    /// 链比文件大小需要的短：`expected` / `actual` 是簇数。
    ChainTooShort,
    /// 链比文件大小需要的长：`expected` / `actual` 是簇数。
    ChainTooLong,
    /// 两个文件（`path` 与 `other_path`）从 `cluster` 起共用 `count` 个簇。
    CrossLink,
    /// 已占用却没有文件引用的簇：从 `cluster` 起 `count` 个。
    LostClusters,
    /// 没有逐段列出的丢失簇：另外还有 `count` 个，分散在 `actual` 段里。
    LostClustersMore,
    /// exFAT：文件占用的簇在分配位图里是空闲的，从 `cluster` 起 `count` 个，可能被新文件覆盖。
    NotInBitmap,
    /// exFAT：有效数据长度（`actual`）大于文件长度（`expected`）。
    SizeInvalid,
    /// exFAT 目录项集合的校验和不对：`expected` 是记录的，`actual` 是算出的。
    EntrySetChecksum,
    /// exFAT 目录项集合的次项个数不对：`expected` 是记录的，`actual` 是实际有的。
    EntrySetCount,
    /// exFAT 文件名长度与文件名项的个数不符：`expected` 是按长度应有的项数，`actual` 是实际的。
    EntryNameLength,
    /// exFAT 目录里不属于任何文件的流扩展项、文件名项（`count` 个）。
    OrphanEntries,
    /// FAT 长文件名的校验和与短文件名对不上：长文件名会被忽略。
    LfnChecksum,
    /// FAT 长文件名各段的序号不对，或者有不属于任何文件的长文件名项。
    LfnOrder,
    /// FAT 子目录开头的 `.`、`..` 缺失或指向不对。
    DotEntries,
    /// 目录嵌套太深，没有再往下查。
    TooDeep,
    /// 读不出的区段：分区内 `offset` 起 `count` 字节。
    Unreadable,
    /// NTFS：`$MFT`（或 `$MFTMirr`）的位置在卷外：`actual` 是记录的簇号。
    MftOutOfRange,
    /// NTFS：`$MFT` 第一条记录不是有效的文件记录。
    MftInvalid,
    /// NTFS：`$MFTMirr` 与 `$MFT` 的前几条记录不一致（`count` 条）。
    MftMirrorMismatch,
}

impl IssueKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::UnknownFilesystem => "unknownFilesystem",
            Self::BootUnrecognized => "bootUnrecognized",
            Self::BootInvalid => "bootInvalid",
            Self::BootChecksum => "bootChecksum",
            Self::BackupBootInvalid => "backupBootInvalid",
            Self::BackupBootMismatch => "backupBootMismatch",
            Self::VolumeTruncated => "volumeTruncated",
            Self::VolumeDirty => "volumeDirty",
            Self::MediaFailure => "mediaFailure",
            Self::FsInfoInvalid => "fsInfoInvalid",
            Self::FatCopiesDiffer => "fatCopiesDiffer",
            Self::FatMediaMismatch => "fatMediaMismatch",
            Self::BadClustersMarked => "badClustersMarked",
            Self::BitmapInvalid => "bitmapInvalid",
            Self::UpcaseMissing => "upcaseMissing",
            Self::UpcaseChecksum => "upcaseChecksum",
            Self::FirstClusterInvalid => "firstClusterInvalid",
            Self::ChainOutOfRange => "chainOutOfRange",
            Self::ChainLoop => "chainLoop",
            Self::ChainFree => "chainFree",
            Self::ChainBadCluster => "chainBadCluster",
            Self::ChainTooShort => "chainTooShort",
            Self::ChainTooLong => "chainTooLong",
            Self::CrossLink => "crossLink",
            Self::LostClusters => "lostClusters",
            Self::LostClustersMore => "lostClustersMore",
            Self::NotInBitmap => "notInBitmap",
            Self::SizeInvalid => "sizeInvalid",
            Self::EntrySetChecksum => "entrySetChecksum",
            Self::EntrySetCount => "entrySetCount",
            Self::EntryNameLength => "entryNameLength",
            Self::OrphanEntries => "orphanEntries",
            Self::LfnChecksum => "lfnChecksum",
            Self::LfnOrder => "lfnOrder",
            Self::DotEntries => "dotEntries",
            Self::TooDeep => "tooDeep",
            Self::Unreadable => "unreadable",
            Self::MftOutOfRange => "mftOutOfRange",
            Self::MftInvalid => "mftInvalid",
            Self::MftMirrorMismatch => "mftMirrorMismatch",
        }
    }

    pub fn severity(self) -> Severity {
        match self {
            Self::UnknownFilesystem
            | Self::BootUnrecognized
            | Self::VolumeTruncated
            | Self::FirstClusterInvalid
            | Self::ChainOutOfRange
            | Self::ChainLoop
            | Self::ChainFree
            | Self::ChainBadCluster
            | Self::ChainTooShort
            | Self::CrossLink
            | Self::NotInBitmap
            | Self::Unreadable
            | Self::MftOutOfRange
            | Self::MftInvalid => Severity::DataLoss,
            Self::BootInvalid
            | Self::BootChecksum
            | Self::BackupBootInvalid
            | Self::MediaFailure
            | Self::FatCopiesDiffer
            | Self::BitmapInvalid
            | Self::UpcaseMissing
            | Self::UpcaseChecksum
            | Self::ChainTooLong
            | Self::LostClusters
            | Self::LostClustersMore
            | Self::SizeInvalid
            | Self::EntrySetChecksum
            | Self::EntrySetCount
            | Self::EntryNameLength
            | Self::OrphanEntries
            | Self::LfnChecksum
            | Self::LfnOrder
            | Self::DotEntries
            | Self::MftMirrorMismatch => Severity::Structure,
            Self::BackupBootMismatch
            | Self::VolumeDirty
            | Self::FsInfoInvalid
            | Self::FatMediaMismatch
            | Self::BadClustersMarked
            | Self::TooDeep => Severity::Minor,
        }
    }
}

/// 一个问题。没用到的位置信息为 `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub kind: IssueKind,
    pub severity: Severity,
    pub area: Option<Area>,
    /// 涉及的文件或目录，以 `/` 开头；`$Bitmap`、`$UpCase` 是 exFAT 的分配位图与大写表。
    pub path: Option<String>,
    /// 交叉链接的另一个文件。
    pub other_path: Option<String>,
    pub cluster: Option<u64>,
    pub count: Option<u64>,
    /// 分区内的字节偏移。
    pub offset: Option<u64>,
    pub expected: Option<u64>,
    pub actual: Option<u64>,
    /// 引导扇区里出问题的那一项（`bytesPerSector` 这样的名字）。
    pub field: Option<&'static str>,
}

impl Issue {
    pub fn new(kind: IssueKind) -> Self {
        Self {
            kind,
            severity: kind.severity(),
            area: None,
            path: None,
            other_path: None,
            cluster: None,
            count: None,
            offset: None,
            expected: None,
            actual: None,
            field: None,
        }
    }

    pub fn area(mut self, area: Area) -> Self {
        self.area = Some(area);
        self
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn other_path(mut self, path: impl Into<String>) -> Self {
        self.other_path = Some(path.into());
        self
    }

    pub fn cluster(mut self, cluster: impl Into<u64>) -> Self {
        self.cluster = Some(cluster.into());
        self
    }

    pub fn count(mut self, count: u64) -> Self {
        self.count = Some(count);
        self
    }

    pub fn offset(mut self, offset: u64) -> Self {
        self.offset = Some(offset);
        self
    }

    pub fn expected(mut self, expected: u64) -> Self {
        self.expected = Some(expected);
        self
    }

    pub fn actual(mut self, actual: u64) -> Self {
        self.actual = Some(actual);
        self
    }

    pub fn field(mut self, field: &'static str) -> Self {
        self.field = Some(field);
        self
    }
}

/// 各严重程度的问题数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Totals {
    pub data_loss: u64,
    pub structure: u64,
    pub minor: u64,
}

/// 收集问题：最多留 [`MAX_ISSUES`] 项，再多的只计数。
#[derive(Default)]
pub(crate) struct Issues {
    list: Vec<Issue>,
    omitted: u64,
    totals: Totals,
}

impl Issues {
    pub fn push(&mut self, issue: Issue) {
        let total = match issue.severity {
            Severity::DataLoss => &mut self.totals.data_loss,
            Severity::Structure => &mut self.totals.structure,
            Severity::Minor => &mut self.totals.minor,
        };
        *total = total.saturating_add(1);
        if self.list.len() < MAX_ISSUES {
            self.list.push(issue);
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }

    pub fn finish(self) -> (Vec<Issue>, u64, Totals) {
        (self.list, self.omitted, self.totals)
    }
}
