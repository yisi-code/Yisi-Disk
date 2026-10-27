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

impl FatWidth {
    /// 每个表项的位数。
    pub fn bits(self) -> u64 {
        match self {
            Self::Fat12 => 12,
            Self::Fat16 => 16,
            Self::Fat32 | Self::Exfat => 32,
        }
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
    let full = match width {
        FatWidth::Fat12 => 0x0FFF,
        FatWidth::Fat16 => 0xFFFF,
        FatWidth::Fat32 => 0x0FFF_FFFF,
        FatWidth::Exfat => 0xFFFF_FFFF,
    };
    entry0 & full == (full & !0xFF) | u32::from(media)
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
