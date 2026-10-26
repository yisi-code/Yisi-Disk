//! 目录项与文件定位信息：FAT 与 exFAT 共用。

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

/// 文件在卷里的位置，足以重新找到并读出它，不依赖目录缓存。
///
/// 界面拿到的是 [`FileRef::to_token`] 编出的字符串，原样传回；解码后的每个数值在使用前都会检查范围，
/// 伪造的定位信息最多读到这个卷里的其他簇。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRef {
    /// 首簇；0 表示没有数据（空文件），或 FAT12/16 的根目录（[`FileRef::is_fixed_root`]）。
    pub first_cluster: u32,
    /// 文件长度；目录在 FAT 里没有长度，记 0。
    pub size: u64,
    /// exFAT 的有效数据长度，之后的部分按规范读作 0；FAT 与 `size` 相同。
    pub valid_size: u64,
    pub is_dir: bool,
    pub deleted: bool,
    /// 簇连续存放、不查 FAT：exFAT 的「不走 FAT 链」标记，或 FAT 里已删除（簇链已清零）的文件。
    pub contiguous: bool,
}

impl FileRef {
    /// FAT12/16 的根目录在固定区域，不在簇里。
    pub fn is_fixed_root(&self) -> bool {
        self.is_dir && self.first_cluster == 0 && !self.deleted
    }

    /// 编成界面原样传回的字符串，例如 `r1-d0-x0-c2-s0-v0-k0`。
    pub fn to_token(&self) -> String {
        format!(
            "r1-d{}-x{}-c{}-s{}-v{}-k{}",
            u8::from(self.is_dir),
            u8::from(self.deleted),
            self.first_cluster,
            self.size,
            self.valid_size,
            u8::from(self.contiguous),
        )
    }

    pub fn from_token(token: &str) -> Option<Self> {
        let mut parts = token.split('-');
        if parts.next()? != "r1" {
            return None;
        }
        let mut field =
            |prefix: char| -> Option<u64> { parts.next()?.strip_prefix(prefix)?.parse().ok() };
        let flag = |value: u64| match value {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };
        let is_dir = flag(field('d')?)?;
        let deleted = flag(field('x')?)?;
        let first_cluster = u32::try_from(field('c')?).ok()?;
        let size = field('s')?;
        let valid_size = field('v')?;
        let contiguous = flag(field('k')?)?;
        if parts.next().is_some() || valid_size > size {
            return None;
        }
        Some(Self {
            first_cluster,
            size,
            valid_size,
            is_dir,
            deleted,
            contiguous,
        })
    }
}

/// 目录里的一项（不含 `.`、`..` 和卷标）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// FAT 的 8.3 短文件名；exFAT 没有。
    pub short_name: Option<String>,
    /// 名字有字符无法确定（FAT 删除后丢了首字母、又没有长文件名可以推算）。
    pub name_uncertain: bool,
    pub size: u64,
    /// `yyyy-MM-dd HH:mm:ss`，按卷上记录的时间（FAT 记的是写入时的本地时间）。
    pub modified: Option<String>,
    pub created: Option<String>,
    pub state: State,
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

/// 把 FAT / exFAT 的日期时间（年从 1980 起）格式化成 `yyyy-MM-dd HH:mm:ss`，取值不合法时返回 `None`。
pub(crate) fn dos_time(date: u16, time: u16) -> Option<String> {
    let year = 1980 + u32::from(date >> 9);
    let month = u32::from((date >> 5) & 0x0F);
    let day = u32::from(date & 0x1F);
    let hour = u32::from(time >> 11);
    let minute = u32::from((time >> 5) & 0x3F);
    let second = u32::from(time & 0x1F) * 2;
    let valid = (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour < 24
        && minute < 60
        && second < 60;
    valid.then(|| format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip_and_reject_garbage() {
        let file = FileRef {
            first_cluster: 1234,
            size: 99_999_999_999,
            valid_size: 5,
            is_dir: false,
            deleted: true,
            contiguous: true,
        };
        assert_eq!(FileRef::from_token(&file.to_token()), Some(file));
        for bad in [
            "",
            "r2-d0-x0-c2-s0-v0-k0",
            "r1-d2-x0-c2-s0-v0-k0",
            "r1-d0-x0-c99999999999-s0-v0-k0",
            "r1-d0-x0-c2-s1-v2-k0",
            "r1-d0-x0-c2-s0-v0-k0-z",
            "r1-d0-x0-c2-s0-v0",
            "r1-d0-x0-c-1-s0-v0-k0",
        ] {
            assert_eq!(FileRef::from_token(bad), None, "{bad}");
        }
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
