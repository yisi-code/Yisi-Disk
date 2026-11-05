//! 目录项与文件定位信息：FAT、exFAT、NTFS 共用。

/// 删除的文件还能不能完整找回，是从簇的占用情况推断的，不保证内容一定对。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// 没有删除。
    Live,
    /// 已删除，存放它的簇现在都没有被别的文件占用，长度也对得上，很可能完整。
    Intact,
    /// 已删除，存放它的簇里至少有一个已经分给了别的文件，内容至少部分被覆盖。
    Overwritten,
    /// 已删除，无法判断：例如首簇不在合法范围、FAT32 首簇的高 16 位可能被清零、簇链断了。
    Unknown,
}

/// 内容读不出来（或读出来不是原样）的存放方式。目前只有 NTFS 会有。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// NTFS 压缩（LZNT1），但压缩单元不认得、解不开。
    Compressed,
    /// EFS 加密：读出来是密文。
    Encrypted,
    /// Windows 的系统压缩（WOF），但算法不认得，或内容不在本文件里（WIMBoot 放在 WIM 文件中）。
    SystemCompressed,
    /// 其他重分析点（符号链接、云端占位文件等）：内容不在这里。
    Reparse,
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Compressed => "新技术文件系统（NTFS）压缩",
            Self::Encrypted => "加密文件系统（EFS）加密",
            Self::SystemCompressed => "系统压缩（WOF）",
            Self::Reparse => "重分析点（链接或云端占位文件）",
        })
    }
}

/// 卷上记的一个时间的原值：界面显示（[`Stamp::text`]）与复制时设输出文件的修改时间（[`Stamp::moment`]）都从它来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stamp {
    /// NTFS：1601-01-01 起的 100 纳秒数，UTC。
    Nt(u64),
    /// FAT / exFAT：DOS 格式，日期在高 16 位、时间在低 16 位，是写入时所在时区的本地时间。`utc_offset` 是 exFAT
    /// 另记的与 UTC 之差：第 7 位为 1 时有效，低 7 位是以 15 分钟为单位的有符号数；FAT 没有，记 0。
    Dos { stamp: u32, utc_offset: u8 },
}

/// 换算成的时刻。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// 1970-01-01 起的秒数与秒以下的纳秒，UTC。
    Utc { seconds: i64, nanos: u32 },
    /// 只有本地的年月日时分秒、不知道时区：按 UTC 算出的秒数，由调用方按本机时区换算。
    Local { seconds: i64 },
}

/// 1601-01-01 到 1970-01-01 的 100 纳秒数。
const NT_UNIX_EPOCH: u64 = yisi_io::NT_UNIX_EPOCH_SECONDS.unsigned_abs() * 10_000_000;

/// 年月日时分秒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Civil {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
}

impl Civil {
    fn text(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// 当作 UTC、1970-01-01 起的秒数。
    fn seconds(&self) -> i64 {
        days_from_civil(self.year, self.month, self.day) * 86_400
            + self.hour * 3600
            + self.minute * 60
            + self.second
    }
}

/// DOS 格式的日期与时间（FAT、exFAT）拆成各项；月、日、时、分、秒不合法时为 `None`。
fn dos_civil(date: u16, time: u16) -> Option<Civil> {
    let civil = Civil {
        year: 1980 + i64::from(date >> 9),
        month: i64::from((date >> 5) & 0x0F),
        day: i64::from(date & 0x1F),
        hour: i64::from(time >> 11),
        minute: i64::from((time >> 5) & 0x3F),
        second: i64::from(time & 0x1F) * 2,
    };
    let valid = (1..=12).contains(&civil.month)
        && (1..=31).contains(&civil.day)
        && civil.hour < 24
        && civil.minute < 60
        && civil.second < 60;
    valid.then_some(civil)
}

impl Stamp {
    /// `yyyy-MM-dd HH:mm:ss`，按卷上记的样子（NTFS 是 UTC，FAT / exFAT 是本地时间）；不合法时为 `None`。
    pub fn text(&self) -> Option<String> {
        match *self {
            Self::Nt(value) => nt_time(value),
            Self::Dos { stamp, .. } => dos_time((stamp >> 16) as u16, stamp as u16),
        }
    }

    pub fn moment(&self) -> Option<Moment> {
        match *self {
            Self::Nt(value) => {
                self.text()?;
                let since = i128::from(value) - i128::from(NT_UNIX_EPOCH);
                Some(Moment::Utc {
                    seconds: i64::try_from(since.div_euclid(10_000_000)).ok()?,
                    nanos: u32::try_from(since.rem_euclid(10_000_000) * 100).ok()?,
                })
            }
            Self::Dos { stamp, utc_offset } => {
                let seconds = dos_civil((stamp >> 16) as u16, stamp as u16)?.seconds();
                if utc_offset & 0x80 == 0 {
                    return Some(Moment::Local { seconds });
                }
                // 低 7 位按有符号数读：0x40 以上是负数
                let quarters =
                    i64::from(utc_offset & 0x7F) - if utc_offset & 0x40 != 0 { 0x80 } else { 0 };
                Some(Moment::Utc {
                    seconds: seconds - quarters * 15 * 60,
                    nanos: 0,
                })
            }
        }
    }

    fn token(&self) -> String {
        match *self {
            Self::Nt(value) => format!("tn{value}"),
            Self::Dos { stamp, utc_offset } => format!("td{stamp}o{utc_offset}"),
        }
    }

    fn from_token(part: &str) -> Option<Self> {
        if let Some(value) = part.strip_prefix("tn") {
            return Some(Self::Nt(value.parse().ok()?));
        }
        let (stamp, offset) = part.strip_prefix("td")?.split_once('o')?;
        Some(Self::Dos {
            stamp: stamp.parse().ok()?,
            utc_offset: offset.parse().ok()?,
        })
    }
}

/// 1970-01-01 起第 `days` 天的年月日（以 3 月为一年之始，闰日落在年末）；[`days_from_civil`] 的反算。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

/// 公历日期距 1970-01-01 的天数。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// FAT / exFAT 文件的位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterRef {
    /// 首簇；0 表示没有数据（空文件），或 FAT12/16 的根目录（[`FileRef::is_fixed_root`]）。
    pub first_cluster: u32,
    /// exFAT 的有效数据长度，之后的部分按规范读作 0；FAT 与长度相同。
    pub valid_size: u64,
    /// 簇连续存放、不查 FAT：exFAT 的「不走 FAT 链」标记，或 FAT 里已删除（簇链已清零）的文件。
    pub contiguous: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Clusters(ClusterRef),
    /// NTFS 的 MFT 记录号与记录的序号（记录每被重用一次，序号就变一次）。
    Mft {
        record: u64,
        sequence: u16,
    },
    /// NTFS：原来的目录已找不到的已删除项集中列在这个虚拟目录里，它不在卷上。
    Orphans,
}

/// 文件在卷里的位置，足以重新找到并读出它，不依赖目录缓存。
///
/// 界面拿到的是 [`FileRef::to_token`] 编出的字符串，原样传回；解码后的每个数值在使用前都会检查范围，
/// 伪造的定位信息最多读到这个卷里的其他簇或其他 MFT 记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRef {
    /// 文件长度；目录在 FAT 里没有长度，记 0。NTFS 取目录索引里的长度，读文件时以 MFT 记录为准。
    pub size: u64,
    pub is_dir: bool,
    pub deleted: bool,
    pub location: Location,
    /// 修改时间：复制时设到输出文件上。根目录、虚拟目录与没记时间的项为 `None`。
    pub modified: Option<Stamp>,
}

impl FileRef {
    /// FAT12/16 的根目录在固定区域，不在簇里。
    pub fn is_fixed_root(&self) -> bool {
        matches!(self.location, Location::Clusters(at) if at.first_cluster == 0)
            && self.is_dir
            && !self.deleted
    }

    /// NTFS 的 MFT 记录号。
    pub fn record(&self) -> Option<u64> {
        match self.location {
            Location::Mft { record, .. } => Some(record),
            _ => None,
        }
    }

    /// 是不是「找不到原目录的文件」这个虚拟目录。
    pub fn is_orphans(&self) -> bool {
        self.location == Location::Orphans
    }

    pub(crate) fn clusters(&self) -> Result<ClusterRef, crate::FsError> {
        match self.location {
            Location::Clusters(at) => Ok(at),
            _ => Err(crate::FsError::BadRef),
        }
    }

    /// 编成界面原样传回的字符串：FAT / exFAT 如 `r1-d0-x0-c2-s0-v0-k0`，NTFS 如 `n1-d0-x1-m42-q3-s100`，
    /// 虚拟目录为 `n1-o`；有修改时间时末尾另有一段（`tn…` 或 `td…o…`）。
    pub fn to_token(&self) -> String {
        let (dir, deleted) = (u8::from(self.is_dir), u8::from(self.deleted));
        let token = match self.location {
            Location::Clusters(at) => format!(
                "r1-d{dir}-x{deleted}-c{}-s{}-v{}-k{}",
                at.first_cluster,
                self.size,
                at.valid_size,
                u8::from(at.contiguous),
            ),
            Location::Mft { record, sequence } => {
                format!("n1-d{dir}-x{deleted}-m{record}-q{sequence}-s{}", self.size)
            }
            Location::Orphans => "n1-o".to_owned(),
        };
        match self.modified {
            Some(stamp) => format!("{token}-{}", stamp.token()),
            None => token,
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        if token == "n1-o" {
            return Some(Self::orphans());
        }
        let mut parts = token.split('-');
        let version = parts.next()?;
        let mut field =
            |prefix: char| -> Option<u64> { parts.next()?.strip_prefix(prefix)?.parse().ok() };
        let flag = |value: u64| match value {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };
        let is_dir = flag(field('d')?)?;
        let deleted = flag(field('x')?)?;
        let file = match version {
            "r1" => {
                let first_cluster = u32::try_from(field('c')?).ok()?;
                let size = field('s')?;
                let valid_size = field('v')?;
                let contiguous = flag(field('k')?)?;
                if valid_size > size {
                    return None;
                }
                Self {
                    size,
                    is_dir,
                    deleted,
                    location: Location::Clusters(ClusterRef {
                        first_cluster,
                        valid_size,
                        contiguous,
                    }),
                    modified: None,
                }
            }
            "n1" => {
                let record = field('m')?;
                let sequence = u16::try_from(field('q')?).ok()?;
                let size = field('s')?;
                Self {
                    size,
                    is_dir,
                    deleted,
                    location: Location::Mft { record, sequence },
                    modified: None,
                }
            }
            _ => return None,
        };
        let modified = match parts.next() {
            Some(part) => Some(Stamp::from_token(part)?),
            None => None,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Self { modified, ..file })
    }

    pub(crate) fn orphans() -> Self {
        Self {
            size: 0,
            is_dir: true,
            deleted: false,
            location: Location::Orphans,
            modified: None,
        }
    }
}

/// 文件属性里与显示有关的两位：隐藏、系统（资源管理器默认不显示带这两位的回收站、`System Volume Information` 等）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileAttributes {
    pub hidden: bool,
    pub system: bool,
}

impl FileAttributes {
    /// 从卷上记的属性取（FAT 的属性字节、exFAT 的 16 位属性、NTFS 的 32 位文件属性，低几位含义相同）。
    pub(crate) fn from_bits(bits: u32) -> Self {
        Self {
            hidden: bits & u32::from(crate::format::ATTR_HIDDEN) != 0,
            system: bits & u32::from(crate::format::ATTR_SYSTEM) != 0,
        }
    }
}

/// 目录里的一项（不含 `.`、`..` 和卷标）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// FAT 的 8.3 短文件名；NTFS 另有 DOS 名时也给出；exFAT 没有。
    pub short_name: Option<String>,
    /// 名字有字符无法确定（FAT 删除后丢了首字母、又没有长文件名可以推算）。
    pub name_uncertain: bool,
    pub size: u64,
    /// `yyyy-MM-dd HH:mm:ss`，按卷上记录的时间：FAT / exFAT 是写入时的本地时间，NTFS 是 UTC
    /// （[`crate::Volume::times_utc`]）。
    pub modified: Option<String>,
    pub created: Option<String>,
    pub state: State,
    /// 数据在分区里的起始字节偏移；没有数据、或还不知道（NTFS 扫描 MFT 之前的现存文件）时为 `None`。
    /// NTFS 内容存放在 MFT 记录里的小文件，给的是记录的位置。
    pub position: Option<u64>,
    /// 内容的存放方式暂不支持，读出来不是原样。
    pub unsupported: Option<Unsupported>,
    pub attributes: FileAttributes,
    pub file: FileRef,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.file.is_dir
    }

    pub fn deleted(&self) -> bool {
        self.file.deleted
    }
}

/// 一个目录的全部项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// 目录项数超过上限，没有全部列出。
    pub truncated: bool,
    /// 已删除的目录的内容已被覆盖（开头不再是 `.` 与 `..`），列不出东西。
    pub overwritten: bool,
}

/// 把 NTFS 的时间（UTC，自 1601-01-01 起的 100 纳秒数）格式化成 `yyyy-MM-dd HH:mm:ss`；为 0 或超出 9999 年时返回 `None`。
pub(crate) fn nt_time(value: u64) -> Option<String> {
    if value == 0 {
        return None;
    }
    let unix = i64::try_from(value / 10_000_000).ok()? - yisi_io::NT_UNIX_EPOCH_SECONDS;
    let (days, rest) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    if !(1601..=9999).contains(&year) {
        return None;
    }
    Some(
        Civil {
            year,
            month,
            day,
            hour: rest / 3600,
            minute: rest / 60 % 60,
            second: rest % 60,
        }
        .text(),
    )
}

/// 把 FAT / exFAT 的日期时间（年从 1980 起）格式化成 `yyyy-MM-dd HH:mm:ss`，取值不合法时返回 `None`。
pub(crate) fn dos_time(date: u16, time: u16) -> Option<String> {
    dos_civil(date, time).map(|civil| civil.text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip_and_reject_garbage() {
        let file = FileRef {
            size: 99_999_999_999,
            is_dir: false,
            deleted: true,
            location: Location::Clusters(ClusterRef {
                first_cluster: 1234,
                valid_size: 5,
                contiguous: true,
            }),
            modified: None,
        };
        assert_eq!(FileRef::from_token(&file.to_token()), Some(file));
        let record = FileRef {
            size: 7,
            is_dir: true,
            deleted: false,
            location: Location::Mft {
                record: 1 << 40,
                sequence: 65535,
            },
            modified: None,
        };
        assert_eq!(record.to_token(), "n1-d1-x0-m1099511627776-q65535-s7");
        assert_eq!(FileRef::from_token(&record.to_token()), Some(record));
        // 带修改时间的两种写法
        for modified in [
            Stamp::Nt(133_000_000_000_000_000),
            Stamp::Dos {
                stamp: 0x5865_6B4F,
                utc_offset: 0xA0,
            },
        ] {
            let timed = FileRef {
                modified: Some(modified),
                ..record
            };
            assert_eq!(FileRef::from_token(&timed.to_token()), Some(timed));
        }
        assert_eq!(
            FileRef {
                modified: Some(Stamp::Nt(5)),
                ..record
            }
            .to_token(),
            "n1-d1-x0-m1099511627776-q65535-s7-tn5"
        );
        assert_eq!(FileRef::from_token("n1-o"), Some(FileRef::orphans()));
        for bad in [
            "",
            "n1",
            "n1-o-x",
            "n1-d0-x0-m5-q65536-s0",
            "n1-d0-x0-m5-q1",
            "n1-d0-x0-m5-q1-s0-k0",
            "r2-d0-x0-c2-s0-v0-k0",
            "r1-d2-x0-c2-s0-v0-k0",
            "r1-d0-x0-c99999999999-s0-v0-k0",
            "r1-d0-x0-c2-s1-v2-k0",
            "r1-d0-x0-c2-s0-v0-k0-z",
            "r1-d0-x0-c2-s0-v0",
            "r1-d0-x0-c-1-s0-v0-k0",
            "r1-d0-x0-c2-s0-v0-k0-tx5",
            "r1-d0-x0-c2-s0-v0-k0-td5",
            "r1-d0-x0-c2-s0-v0-k0-td5o256",
            "r1-d0-x0-c2-s0-v0-k0-tn5-tn5",
        ] {
            assert_eq!(FileRef::from_token(bad), None, "{bad}");
        }
    }

    #[test]
    fn nt_times_are_formatted_in_utc() {
        // 1970-01-01 00:00:00 与 2024-02-29 23:59:59（闰日）
        assert_eq!(
            nt_time(116_444_736_000_000_000).as_deref(),
            Some("1970-01-01 00:00:00")
        );
        let leap = 116_444_736_000_000_000 + 1_709_251_199 * 10_000_000;
        assert_eq!(nt_time(leap).as_deref(), Some("2024-02-29 23:59:59"));
        assert_eq!(nt_time(1).as_deref(), Some("1601-01-01 00:00:00"));
        assert_eq!(nt_time(0), None);
        assert_eq!(nt_time(u64::MAX), None);
    }

    #[test]
    fn stamps_become_utc_moments() {
        // NTFS：1970-01-01 00:00:00.5 UTC
        assert_eq!(
            Stamp::Nt(NT_UNIX_EPOCH + 5_000_000).moment(),
            Some(Moment::Utc {
                seconds: 0,
                nanos: 500_000_000
            })
        );
        // 1601 年是 1970 年之前：秒数为负
        assert_eq!(
            Stamp::Nt(1).moment(),
            Some(Moment::Utc {
                seconds: -11_644_473_600,
                nanos: 100
            })
        );
        assert_eq!(Stamp::Nt(0).moment(), None);
        // DOS：2024-03-05 13:45:30，2024-03-05 是 1970 年起第 19787 天
        let date = ((2024 - 1980) << 9) | (3 << 5) | 5;
        let time = (13 << 11) | (45 << 5) | 15;
        let wall = 19_787 * 86_400 + 13 * 3600 + 45 * 60 + 30;
        let dos = |utc_offset| Stamp::Dos {
            stamp: (date << 16) | time,
            utc_offset,
        };
        assert_eq!(dos(0).moment(), Some(Moment::Local { seconds: wall }));
        assert_eq!(dos(0).text().as_deref(), Some("2024-03-05 13:45:30"));
        // exFAT 记了 UTC+8（32 个 15 分钟）与 UTC-5（-20，七位补码 0x6C）
        assert_eq!(
            dos(0x80 | 32).moment(),
            Some(Moment::Utc {
                seconds: wall - 8 * 3600,
                nanos: 0
            })
        );
        assert_eq!(
            dos(0x80 | 0x6C).moment(),
            Some(Moment::Utc {
                seconds: wall + 5 * 3600,
                nanos: 0
            })
        );
        // 闰日与世纪年
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(
            Stamp::Dos {
                stamp: 0,
                utc_offset: 0
            }
            .moment(),
            None
        );
    }

    #[test]
    fn dos_times_are_formatted_or_rejected() {
        // 2024-03-05 13:45:30
        let date = ((2024 - 1980) << 9) | (3 << 5) | 5;
        let time = (13 << 11) | (45 << 5) | 15;
        assert_eq!(dos_time(date, time).as_deref(), Some("2024-03-05 13:45:30"));
        assert_eq!(dos_time(0, 0), None);
        assert_eq!(dos_time(date, 24 << 11), None);
    }
}
