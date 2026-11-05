//! FAT12/16/32 与 exFAT 磁盘格式里的纯规则：FAT 表项的位置与含义、介质描述符、簇段、目录项的首簇、
//! 长文件名、exFAT 目录项集合。依据微软公开的 FAT 与 exFAT 规范。
//!
//! 浏览（本 crate）与文件系统检查（`yisi-repair`）共用这一份：检查不经过本 crate 打开卷（要逐项说清哪里坏了），
//! 但判读同一个字节的规则只有一套，两边对同一个卷不会有两种说法。

use yisi_part::bytes::{le16, le32, le64};

/// FAT 的种类：决定表项多宽、标记值是多少。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatWidth {
    Fat12,
    Fat16,
    Fat32,
    Exfat,
}

/// 目录项的大小（FAT 与 exFAT 都是 32 字节）。
pub const DIR_ENTRY_SIZE: usize = 32;
/// FAT 目录项：已删除项的首字节；属性里的卷标位、目录位；长文件名项的属性（只读、隐藏、系统、卷标四位全为 1）。
/// exFAT 文件项的属性同样第 4 位表示目录。
pub const FAT_DELETED: u8 = 0xE5;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_LONG_NAME: u8 = 0x0F;
/// exFAT 引导扇区里卷标志的位置（16 位）；第 1 位「未正常卸下」，第 2 位「出过介质故障」。
pub const EXFAT_VOLUME_FLAGS_AT: usize = 106;
pub const EXFAT_FLAG_DIRTY: u16 = 0x02;
pub const EXFAT_FLAG_MEDIA_FAILURE: u16 = 0x04;

/// 介质描述符的规则在最底层（认分区表也要用）。
pub use yisi_part::boot_sector::fat_media_byte_ok;

impl FatWidth {
    /// 第 1 项里「正常卸下」的位（FAT16 第 15 位、FAT32 第 27 位，为 1 表示正常卸下）与「没出过读写错误」的位
    /// （FAT16 第 14 位、FAT32 第 26 位）；FAT12 与 exFAT 没有，为 0。检查读它，修复写它。
    pub fn clean_shutdown_bit(self) -> u32 {
        match self {
            Self::Fat16 => 0x8000,
            Self::Fat32 => 0x0800_0000,
            Self::Fat12 | Self::Exfat => 0,
        }
    }

    pub fn no_error_bit(self) -> u32 {
        match self {
            Self::Fat16 => 0x4000,
            Self::Fat32 => 0x0400_0000,
            Self::Fat12 | Self::Exfat => 0,
        }
    }

    /// 每个表项的位数。
    pub fn bits(self) -> u64 {
        match self {
            Self::Fat12 => 12,
            Self::Fat16 => 16,
            Self::Fat32 | Self::Exfat => 32,
        }
    }

    /// 写链尾时用的值（全部位为 1）。
    pub fn end_of_chain(self) -> u32 {
        match self {
            Self::Fat12 => 0xFFF,
            Self::Fat16 => 0xFFFF,
            Self::Fat32 => 0x0FFF_FFFF,
            Self::Exfat => 0xFFFF_FFFF,
        }
    }

    /// 标成坏簇时写的值。
    pub fn bad_cluster(self) -> u32 {
        self.markers().0
    }

    /// 坏簇标记与链尾标记（不小于它的都是链尾）。
    fn markers(self) -> (u32, u32) {
        match self {
            Self::Fat12 => (0xFF7, 0xFF8),
            Self::Fat16 => (0xFFF7, 0xFFF8),
            Self::Fat32 => (0x0FFF_FFF7, 0x0FFF_FFF8),
            Self::Exfat => (0xFFFF_FFF7, 0xFFFF_FFF8),
        }
    }
}

/// 一个 FAT 表项的含义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatEntry {
    Free,
    Next(u32),
    End,
    Bad,
    /// 不是合法的簇号，也不是标记值。
    Invalid(u32),
}

/// 表项 `cluster` 在 FAT 里的字节偏移与要读的字节数（FAT12 一项跨两个字节）。
pub fn fat_entry_span(width: FatWidth, cluster: u32) -> (u64, usize) {
    let cluster = u64::from(cluster);
    match width {
        FatWidth::Fat12 => (cluster + cluster / 2, 2),
        FatWidth::Fat16 => (cluster * 2, 2),
        FatWidth::Fat32 | FatWidth::Exfat => (cluster * 4, 4),
    }
}

/// 从 [`fat_entry_span`] 给出的那几个字节里取出表项 `cluster` 的原始值；字节不够时为 `None`。
pub fn fat_entry_raw(width: FatWidth, cluster: u32, bytes: &[u8]) -> Option<u32> {
    Some(match width {
        FatWidth::Fat12 => {
            let pair = le16(bytes, 0)?;
            u32::from(if cluster % 2 == 1 {
                pair >> 4
            } else {
                pair & 0x0FFF
            })
        }
        FatWidth::Fat16 => u32::from(le16(bytes, 0)?),
        FatWidth::Fat32 | FatWidth::Exfat => le32(bytes, 0)?,
    })
}

/// 把表项 `cluster` 改成 `value`：`bytes` 是 [`fat_entry_span`] 给出的那几个字节，原地改。FAT12 只改自己那 12 位
/// （同一对字节的另一半属于相邻的表项），FAT32 保留高 4 位（规范要求写时不动）。字节不够时什么也不改。
pub fn put_fat_entry(width: FatWidth, cluster: u32, value: u32, bytes: &mut [u8]) {
    let (word, keep): (u32, u32) = match width {
        FatWidth::Fat12 => {
            let Some(pair) = le16(bytes, 0) else {
                return;
            };
            let value = value & 0x0FFF;
            let pair = u32::from(pair);
            let word = if cluster % 2 == 1 {
                (pair & 0x000F) | (value << 4)
            } else {
                (pair & 0xF000) | value
            };
            (word, 0)
        }
        FatWidth::Fat16 => (value & 0xFFFF, 0),
        FatWidth::Fat32 => (value & 0x0FFF_FFFF, 0xF000_0000),
        FatWidth::Exfat => (value, 0),
    };
    let len = fat_entry_span(width, cluster).1;
    let Some(slot) = bytes.get_mut(..len) else {
        return;
    };
    let old = slot
        .iter()
        .rev()
        .fold(0u32, |acc, &byte| (acc << 8) | u32::from(byte));
    let word = (old & keep) | word;
    for (index, byte) in slot.iter_mut().enumerate() {
        *byte = (word >> (8 * index)) as u8;
    }
}

/// 表项的含义。`count` 是簇数，合法的簇号是 2 到 `count + 1`。FAT32 只看低 28 位。
/// 规范把链尾标记之前的几个值列为保留，但簇数到上限时它们也是合法的簇号，所以只按范围判断。
pub fn decode_fat_entry(width: FatWidth, raw: u32, count: u32) -> FatEntry {
    let value = if width == FatWidth::Fat32 {
        raw & 0x0FFF_FFFF
    } else {
        raw
    };
    let (bad, end) = width.markers();
    if value == 0 {
        FatEntry::Free
    } else if value == bad {
        FatEntry::Bad
    } else if value >= end {
        FatEntry::End
    } else if is_cluster(value, count) {
        FatEntry::Next(value)
    } else {
        FatEntry::Invalid(value)
    }
}

/// `cluster` 是不是合法的簇号（2 到 `count + 1`）。
pub fn is_cluster(cluster: u32, count: u32) -> bool {
    cluster >= 2 && u64::from(cluster) < u64::from(count) + 2
}

/// FAT 第 0 项：低 8 位是介质描述符，其余位全为 1（FAT32 只看低 28 位）。
pub fn fat_media_ok(width: FatWidth, media: u8, entry0: u32) -> bool {
    let full = width.end_of_chain();
    entry0 & full == (full & !0xFF) | u32::from(media)
}

/// 簇 `cluster` 在分区里的字节偏移：簇区从 2 号簇起，`heap_offset` 是 2 号簇的偏移。不是合法簇号或溢出时为 `None`。
pub fn cluster_offset(
    cluster: u32,
    count: u32,
    cluster_size: u64,
    heap_offset: u64,
) -> Option<u64> {
    if !is_cluster(cluster, count) {
        return None;
    }
    u64::from(cluster - 2)
        .checked_mul(cluster_size)?
        .checked_add(heap_offset)
}

/// 连续的若干簇。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub first: u32,
    pub count: u32,
}

/// 把簇接到最后一段上，接不上就另起一段。
pub fn push_run(runs: &mut Vec<Run>, cluster: u32) {
    match runs.last_mut() {
        Some(run) if run.first.checked_add(run.count) == Some(cluster) => run.count += 1,
        _ => runs.push(Run {
            first: cluster,
            count: 1,
        }),
    }
}

/// FAT 目录项的首簇：低 16 位在第 26 字节；只有 FAT32 的高 16 位在第 20 字节（FAT12/16 那两个字节另有用途）。
pub fn fat_first_cluster(entry: &[u8], is_fat32: bool) -> u32 {
    let high = if is_fat32 {
        u32::from(le16(entry, 20).unwrap_or(0))
    } else {
        0
    };
    high << 16 | u32::from(le16(entry, 26).unwrap_or(0))
}

/// 长文件名最多 20 段（255 个字符）。
pub const MAX_LFN_PARTS: usize = 20;

/// 长文件名的一段：序号（第 0 字节）、短文件名的校验和（第 13 字节）、13 个 UTF-16 单元。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LfnPart {
    pub order: u8,
    pub checksum: u8,
    pub units: [u16; 13],
}

/// 解析一个长文件名目录项。
pub fn lfn_part(entry: &[u8]) -> Option<LfnPart> {
    let mut units = [0u16; 13];
    let positions = (1..11)
        .step_by(2)
        .chain((14..26).step_by(2))
        .chain((28..32).step_by(2));
    for (slot, at) in units.iter_mut().zip(positions) {
        *slot = le16(entry, at)?;
    }
    Some(LfnPart {
        order: *entry.first()?,
        checksum: *entry.get(13)?,
        units,
    })
}

/// 未删除的各段（按在目录里的次序）：第一段的序号带 0x40，序号从段数递减到 1，段数不超过 20。
pub fn lfn_ordered(parts: &[LfnPart]) -> bool {
    let count = parts.len();
    count <= MAX_LFN_PARTS
        && parts.first().is_some_and(|first| {
            first.order & 0x40 != 0 && usize::from(first.order & 0x1F) == count
        })
        && parts
            .iter()
            .enumerate()
            .all(|(index, part)| usize::from(part.order & 0x1F) == count - index)
}

/// 拼出长文件名：各段在目录里是倒着存的（最后一段在前），到 0 为止，去掉结尾的 `0xFFFF` 填充。
pub fn lfn_name(parts: &[LfnPart]) -> Option<String> {
    let mut units = Vec::with_capacity(parts.len() * 13);
    for part in parts.iter().rev() {
        units.extend_from_slice(&part.units);
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

/// exFAT 目录项类型的第 7 位：1 为使用中，删除时清零。
pub const EXFAT_IN_USE: u8 = 0x80;
pub const EXFAT_TYPE_BITMAP: u8 = 0x81;
pub const EXFAT_TYPE_UPCASE: u8 = 0x82;
pub const EXFAT_TYPE_LABEL: u8 = 0x83;
pub const EXFAT_TYPE_FILE: u8 = 0x85;
pub const EXFAT_TYPE_STREAM: u8 = 0xC0;
pub const EXFAT_TYPE_NAME: u8 = 0xC1;
/// 流扩展项的标志：第 1 位为「不走 FAT 链」（簇连续存放）。
const NO_FAT_CHAIN: u8 = 0x02;

/// exFAT 一组目录项（文件项、流扩展项、文件名项、其他次项）按规范核对出的问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetProblem {
    /// 文件项记的次项个数不在 2 到 18 之间。
    SecondaryCount {
        recorded: usize,
    },
    /// 连着的次项比记的少：第一项不是流扩展项、文件名项没有紧跟在后面、类型或使用中标记不对、到了目录末尾。
    Missing {
        recorded: usize,
        found: usize,
    },
    Checksum {
        stored: u16,
        computed: u16,
    },
    /// 名字长度为 0，或文件名项的个数与名字长度不符。
    NameLength {
        expected: usize,
        found: usize,
    },
}

/// 一组 exFAT 目录项的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExfatSet {
    /// 这一组实际连着的项数（至少 1），下一组从这之后开始。
    pub entries: usize,
    pub deleted: bool,
    /// 没有问题才可以放心使用；有问题时下面各项尽量给出，供检查说明是哪一个文件。
    pub problems: Vec<SetProblem>,
    /// 流扩展项在不在（不在时首簇、长度都为 0）。
    pub has_stream: bool,
    pub attributes: u16,
    /// 名字：按记录的长度截取；长度为 0 时取到第一个 0 为止。
    pub name: String,
    pub first_cluster: u32,
    pub data_length: u64,
    pub valid_length: u64,
    pub no_fat_chain: bool,
    /// 创建与修改时间的原始值：日期在高 16 位、时间在低 16 位，各按 DOS 格式。
    pub created: u32,
    pub modified: u32,
    /// 修改时间与 UTC 之差的原始字节（第 7 位为 1 时有效，见 [`crate::Stamp::Dos`]）。
    pub modified_utc_offset: u8,
}

/// 从 `entries[index]`（一个文件项）起解析一组目录项。次项要与文件项的使用中标记一致；校验和按「使用中」的
/// 类型字节计算，所以已删除的组也能核对（删除只清掉类型的第 7 位）。
pub fn exfat_entry_set(entries: &[[u8; 32]], index: usize) -> ExfatSet {
    let primary = entries.get(index).copied().unwrap_or([0u8; 32]);
    let deleted = primary[0] & EXFAT_IN_USE == 0;
    let recorded = usize::from(primary[1]);
    let mut set = ExfatSet {
        entries: 1,
        deleted,
        problems: Vec::new(),
        has_stream: false,
        attributes: le16(&primary, 4).unwrap_or(0),
        name: String::new(),
        first_cluster: 0,
        data_length: 0,
        valid_length: 0,
        no_fat_chain: false,
        created: le32(&primary, 8).unwrap_or(0),
        modified: le32(&primary, 12).unwrap_or(0),
        modified_utc_offset: primary[23],
    };
    if !(2..=18).contains(&recorded) {
        set.problems.push(SetProblem::SecondaryCount { recorded });
        return set;
    }
    let mut found = 0;
    let mut names = 0usize;
    let mut units: Vec<u16> = Vec::new();
    for offset in 1..=recorded {
        let Some(raw) = entries.get(index + offset) else {
            break;
        };
        let kind = raw[0];
        // 次项：类型第 6 位为 1，使用中标记与文件项相同
        if (kind & EXFAT_IN_USE == 0) != deleted || kind & 0x40 == 0 {
            break;
        }
        match (offset, kind | EXFAT_IN_USE) {
            (1, EXFAT_TYPE_STREAM) => {
                set.has_stream = true;
                set.no_fat_chain = raw[1] & NO_FAT_CHAIN != 0;
                set.valid_length = le64(raw, 8).unwrap_or(0);
                set.first_cluster = le32(raw, 20).unwrap_or(0);
                set.data_length = le64(raw, 24).unwrap_or(0);
            }
            (1, _) => break,
            // 文件名项要紧跟在流扩展项后面
            (_, EXFAT_TYPE_NAME) if names + 2 == offset => {
                names += 1;
                units.extend((0..15).filter_map(|unit| le16(raw, 2 + unit * 2)));
            }
            (_, EXFAT_TYPE_NAME) => break,
            // 厂商扩展项等：只参与校验和
            _ => {}
        }
        found = offset;
    }
    set.entries = found + 1;
    if found < recorded {
        set.problems.push(SetProblem::Missing { recorded, found });
    } else {
        let stored = le16(&primary, 2).unwrap_or(0);
        let computed =
            exfat_set_checksum(entries.get(index..=index + recorded).unwrap_or_default());
        if computed != stored {
            set.problems.push(SetProblem::Checksum { stored, computed });
        }
    }
    let name_length = if set.has_stream {
        entries
            .get(index + 1)
            .map_or(0, |stream| usize::from(stream[3]))
    } else {
        0
    };
    let wanted = name_length.div_ceil(15);
    if set.has_stream && (name_length == 0 || names != wanted) {
        set.problems.push(SetProblem::NameLength {
            expected: wanted,
            found: names,
        });
    }
    let name: Vec<u16> = if name_length > 0 {
        units.iter().copied().take(name_length).collect()
    } else {
        units
            .iter()
            .copied()
            .take_while(|&unit| unit != 0)
            .collect()
    };
    set.name = String::from_utf16_lossy(&name);
    set
}

/// exFAT 卷标目录项（`0x83`）里的卷标；空的卷标为 `None`。
pub fn exfat_label(raw: &[u8]) -> Option<String> {
    let count = usize::from(raw.get(1).copied().unwrap_or(0)).min(11);
    let units: Vec<u16> = (0..count)
        .filter_map(|index| le16(raw, 2 + index * 2))
        .collect();
    let label = String::from_utf16_lossy(&units);
    (!label.is_empty()).then_some(label)
}

/// exFAT 大写表的校验和：逐字节循环右移一位再加（大写表目录项里记的 `TableChecksum`）。
pub fn exfat_table_checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |sum, &byte| {
        sum.rotate_right(1).wrapping_add(u32::from(byte))
    })
}

/// exFAT 规范推荐的压缩大写表，Windows 格式化 exFAT 时写的就是它：5836 字节，校验和 [`EXFAT_UPCASE_CHECKSUM`]。
/// 取自 Windows 格式化的卷，按规范公布的长度与校验和核对过。修复大写表时写回它。
pub const EXFAT_UPCASE_TABLE: &[u8] = include_bytes!("exfat_upcase.bin");
pub const EXFAT_UPCASE_CHECKSUM: u32 = 0xE619_D30D;

/// 目录项集合的校验和：逐字节循环右移一位再加，跳过文件项里存校验和的第 2、3 字节；每项的类型字节按使用中计算。
pub fn exfat_set_checksum(entries: &[[u8; 32]]) -> u16 {
    let mut checksum = 0u16;
    for (offset, raw) in entries.iter().enumerate() {
        for (position, &byte) in raw.iter().enumerate() {
            if offset == 0 && (position == 2 || position == 3) {
                continue;
            }
            let byte = if position == 0 {
                byte | EXFAT_IN_USE
            } else {
                byte
            };
            checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
        }
    }
    checksum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_upcase_table_matches_the_published_checksum() {
        assert_eq!(EXFAT_UPCASE_TABLE.len(), 5836);
        assert_eq!(
            exfat_table_checksum(EXFAT_UPCASE_TABLE),
            EXFAT_UPCASE_CHECKSUM
        );
        // 压缩格式：开头 a 到 z 映射成 A 到 Z
        let unit = |index: usize| {
            u16::from_le_bytes([
                EXFAT_UPCASE_TABLE[index * 2],
                EXFAT_UPCASE_TABLE[index * 2 + 1],
            ])
        };
        assert_eq!((unit(0x61), unit(0x7A)), (0x41, 0x5A));
    }

    #[test]
    fn fat_entries_decode_by_width() {
        // FAT12：两项共用三个字节
        let bytes = [0x03, 0x40, 0x00];
        assert_eq!(fat_entry_raw(FatWidth::Fat12, 0, &bytes[0..2]), Some(0x003));
        assert_eq!(fat_entry_raw(FatWidth::Fat12, 1, &bytes[1..3]), Some(0x004));
        assert_eq!(fat_entry_span(FatWidth::Fat12, 3), (4, 2));
        assert_eq!(
            decode_fat_entry(FatWidth::Fat12, 0xFF7, 4000),
            FatEntry::Bad
        );
        assert_eq!(
            decode_fat_entry(FatWidth::Fat12, 0xFFF, 4000),
            FatEntry::End
        );
        // 簇数到上限时，保留值也是合法簇号
        assert_eq!(
            decode_fat_entry(FatWidth::Fat12, 0xFF0, 4084),
            FatEntry::Next(0xFF0)
        );
        assert_eq!(
            decode_fat_entry(FatWidth::Fat12, 0xFF0, 100),
            FatEntry::Invalid(0xFF0)
        );
        assert_eq!(
            decode_fat_entry(FatWidth::Fat32, 0xF000_0005, 10),
            FatEntry::Next(5)
        );
        assert_eq!(
            decode_fat_entry(FatWidth::Exfat, 0xFFFF_FFFF, 10),
            FatEntry::End
        );
        assert_eq!(decode_fat_entry(FatWidth::Fat16, 0, 10), FatEntry::Free);
    }

    #[test]
    fn fat_media_needs_all_other_bits_set() {
        assert!(fat_media_ok(FatWidth::Fat12, 0xF8, 0xFF8));
        assert!(fat_media_ok(FatWidth::Fat16, 0xF8, 0xFFF8));
        assert!(fat_media_ok(FatWidth::Fat32, 0xF8, 0xFFFF_FFF8));
        assert!(fat_media_ok(FatWidth::Fat32, 0xF8, 0x0FFF_FFF8));
        assert!(!fat_media_ok(FatWidth::Fat16, 0xF8, 0x00F8));
        assert!(!fat_media_ok(FatWidth::Fat16, 0xF0, 0xFFF8));
        assert!(fat_media_ok(FatWidth::Exfat, 0xF8, 0xFFFF_FFF8));
    }

    #[test]
    fn first_cluster_high_half_is_fat32_only() {
        let mut entry = [0u8; 32];
        entry[20..22].copy_from_slice(&1u16.to_le_bytes());
        entry[26..28].copy_from_slice(&5u16.to_le_bytes());
        assert_eq!(fat_first_cluster(&entry, true), 0x1_0005);
        assert_eq!(fat_first_cluster(&entry, false), 5);
    }

    fn lfn(order: u8, text: &str) -> LfnPart {
        let mut units = [0xFFFFu16; 13];
        for (slot, unit) in units.iter_mut().zip(text.encode_utf16().chain([0])) {
            *slot = unit;
        }
        LfnPart {
            order,
            checksum: 7,
            units,
        }
    }

    #[test]
    fn long_names_are_ordered_and_assembled() {
        let parts = [lfn(0x42, "tail.txt"), lfn(0x01, "a long name, ")];
        assert!(lfn_ordered(&parts));
        assert_eq!(lfn_name(&parts).as_deref(), Some("a long name, tail.txt"));
        assert!(!lfn_ordered(&[lfn(0x01, "x"), lfn(0x42, "y")]));
        assert!(!lfn_ordered(&[lfn(0x02, "x")]));
    }
}
