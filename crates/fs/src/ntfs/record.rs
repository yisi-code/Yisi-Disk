//! MFT 记录：更新序列修正、记录头、属性遍历；`$FILE_NAME` 与运行表的解码。

use yisi_part::bytes::{le16, le32, le64};

/// 属性类型。
pub(crate) const STANDARD_INFORMATION: u32 = 0x10;
pub(crate) const ATTRIBUTE_LIST: u32 = 0x20;
pub(crate) const FILE_NAME: u32 = 0x30;
pub(crate) const VOLUME_NAME: u32 = 0x60;
pub(crate) const DATA: u32 = 0x80;
pub(crate) const INDEX_ROOT: u32 = 0x90;
pub(crate) const INDEX_ALLOCATION: u32 = 0xA0;
pub(crate) const BITMAP: u32 = 0xB0;
pub(crate) const REPARSE_POINT: u32 = 0xC0;
const END: u32 = 0xFFFF_FFFF;

/// 记录头的标志。
const IN_USE: u16 = 0x0001;
const DIRECTORY: u16 = 0x0002;

/// 属性头的标志。
const ATTR_COMPRESSED: u16 = 0x00FF;
const ATTR_ENCRYPTED: u16 = 0x4000;

/// `$FILE_NAME` 的命名空间：只有 DOS 8.3 名字的那份。
pub(crate) const NAMESPACE_DOS: u8 = 2;

/// 文件引用：低 48 位是记录号，高 16 位是序号。
pub(crate) fn split_ref(value: u64) -> (u64, u16) {
    (value & 0xFFFF_FFFF_FFFF, (value >> 48) as u16)
}

/// 更新序列修正：每个扇区（步长由数组长度算出，一般是 512 字节）最后两个字节写盘时换成了更新序列号，
/// 原值存在数组里。末尾对不上说明这一块没写完整（写到一半断电），返回 `false`。
pub(crate) fn apply_fixup(block: &mut [u8]) -> bool {
    let (Some(offset), Some(count)) = (le16(block, 4), le16(block, 6)) else {
        return false;
    };
    let (offset, count) = (usize::from(offset), usize::from(count));
    let Some(sectors) = count.checked_sub(1).filter(|&sectors| sectors > 0) else {
        return false;
    };
    if !block.len().is_multiple_of(sectors) {
        return false;
    }
    let stride = block.len() / sectors;
    let Some(usn) = le16(block, offset) else {
        return false;
    };
    if stride < 2 || offset.checked_add(count * 2).is_none_or(|end| end > stride) {
        return false;
    }
    for index in 0..sectors {
        let at = (index + 1) * stride - 2;
        let Some(original) = le16(block, offset + 2 + index * 2) else {
            return false;
        };
        if le16(block, at) != Some(usn) {
            return false;
        }
        let Some(slot) = block.get_mut(at..at + 2) else {
            return false;
        };
        slot.copy_from_slice(&original.to_le_bytes());
    }
    true
}

/// 一条修正过的 MFT 记录。
pub(crate) struct Record {
    bytes: Vec<u8>,
    pub sequence: u16,
    flags: u16,
    /// 扩展记录所属的基本记录（文件引用）；基本记录为 0。
    pub base: u64,
    first_attribute: usize,
    used: usize,
}

impl Record {
    /// 解析记录 `number`；签名、更新序列、记录头不对时返回 `None`（没用过的记录、写坏的记录）。
    pub fn parse(mut bytes: Vec<u8>, number: u64) -> Option<Self> {
        if bytes.get(..4) != Some(b"FILE".as_slice()) || !apply_fixup(&mut bytes) {
            return None;
        }
        let usa_offset = usize::from(le16(&bytes, 4)?);
        let first_attribute = usize::from(le16(&bytes, 0x14)?);
        let used = (le32(&bytes, 0x18)? as usize).min(bytes.len());
        if first_attribute < 0x2A || first_attribute >= used {
            return None;
        }
        // Windows XP 起记录头里还有记录号的低 32 位，对不上说明这条记录不在它该在的位置
        if usa_offset >= 0x30 && le32(&bytes, 0x2C)? != number as u32 {
            return None;
        }
        Some(Self {
            sequence: le16(&bytes, 0x10)?,
            flags: le16(&bytes, 0x16)?,
            base: le64(&bytes, 0x20)?,
            first_attribute,
            used,
            bytes,
        })
    }

    pub fn in_use(&self) -> bool {
        self.flags & IN_USE != 0
    }

    pub fn is_dir(&self) -> bool {
        self.flags & DIRECTORY != 0
    }

    pub fn is_base(&self) -> bool {
        split_ref(self.base).0 == 0
    }

    /// 这条记录在分区里的偏移加上属性的偏移，就是常驻属性内容的位置；这里给出属性内容在记录里的偏移。
    pub fn attributes(&self) -> Attributes<'_> {
        Attributes {
            bytes: self.bytes.get(..self.used).unwrap_or_default(),
            offset: self.first_attribute,
        }
    }

    /// 第一个类型为 `kind`、名字为 `name` 的属性。
    pub fn find(&self, kind: u32, name: &[u16]) -> Option<Attribute<'_>> {
        self.attributes()
            .find(|attribute| attribute.kind == kind && attribute.name_is(name))
    }
}

pub(crate) struct Attributes<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for Attributes<'a> {
    type Item = Attribute<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.offset;
        let kind = le32(self.bytes, start)?;
        if kind == END {
            return None;
        }
        let length = le32(self.bytes, start + 4)? as usize;
        let raw = self
            .bytes
            .get(start..start.checked_add(length)?)
            .filter(|_| length >= 0x18 && length.is_multiple_of(8))?;
        self.offset = start + length;
        Attribute::parse(kind, raw, start)
    }
}

/// 一个属性。名字是 UTF-16 小端的原始字节。
pub(crate) struct Attribute<'a> {
    pub kind: u32,
    pub id: u16,
    flags: u16,
    name: &'a [u8],
    pub body: Body<'a>,
}

pub(crate) enum Body<'a> {
    /// 常驻：内容就在记录里，`offset` 是内容在记录里的偏移。
    Resident {
        value: &'a [u8],
        offset: usize,
    },
    NonResident(NonResident<'a>),
}

pub(crate) struct NonResident<'a> {
    pub lowest_vcn: u64,
    pub highest_vcn: u64,
    pub runs: &'a [u8],
    pub size: u64,
    pub initialized: u64,
}

impl<'a> Attribute<'a> {
    fn parse(kind: u32, raw: &'a [u8], at: usize) -> Option<Self> {
        let non_resident = *raw.get(8)? != 0;
        let name_length = usize::from(*raw.get(9)?);
        let name_offset = usize::from(le16(raw, 0x0A)?);
        let name = raw.get(name_offset..name_offset.checked_add(name_length * 2)?)?;
        let body = if non_resident {
            let runs_offset = usize::from(le16(raw, 0x20)?);
            Body::NonResident(NonResident {
                lowest_vcn: le64(raw, 0x10)?,
                highest_vcn: le64(raw, 0x18)?,
                runs: raw.get(runs_offset..)?,
                size: le64(raw, 0x30)?,
                initialized: le64(raw, 0x38)?,
            })
        } else {
            let length = le32(raw, 0x10)? as usize;
            let offset = usize::from(le16(raw, 0x14)?);
            Body::Resident {
                value: raw.get(offset..offset.checked_add(length)?)?,
                offset: at + offset,
            }
        };
        Some(Self {
            kind,
            id: le16(raw, 0x0E)?,
            flags: le16(raw, 0x0C)?,
            name,
            body,
        })
    }

    pub fn name_is(&self, name: &[u16]) -> bool {
        self.name.len() == name.len() * 2
            && self
                .name
                .as_chunks::<2>()
                .0
                .iter()
                .zip(name)
                .all(|(unit, &want)| u16::from_le_bytes(*unit) == want)
    }

    pub fn compressed(&self) -> bool {
        self.flags & ATTR_COMPRESSED != 0
    }

    pub fn encrypted(&self) -> bool {
        self.flags & ATTR_ENCRYPTED != 0
    }

    pub fn resident_value(&self) -> Option<&'a [u8]> {
        match self.body {
            Body::Resident { value, .. } => Some(value),
            Body::NonResident(_) => None,
        }
    }
}

/// 把 ASCII 写的属性名换成 UTF-16，用于比较。
pub(crate) fn utf16(name: &str) -> Vec<u16> {
    name.encode_utf16().collect()
}

/// `$FILE_NAME` 属性（也是目录索引的键）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileName {
    pub parent: u64,
    pub parent_sequence: u16,
    pub created: u64,
    pub modified: u64,
    pub size: u64,
    /// 文件属性（只读、隐藏、压缩、加密、重分析点等），目录另有 0x1000_0000。
    pub flags: u32,
    /// 是重分析点时为重分析标记。
    pub reparse_tag: u32,
    pub namespace: u8,
    pub name: String,
}

pub(crate) fn parse_file_name(value: &[u8]) -> Option<FileName> {
    let (parent, parent_sequence) = split_ref(le64(value, 0)?);
    let length = usize::from(*value.get(0x40)?);
    let units: Vec<u16> = value
        .get(0x42..0x42 + length * 2)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|unit| u16::from_le_bytes(*unit))
        .collect();
    if units.is_empty() {
        return None;
    }
    Some(FileName {
        parent,
        parent_sequence,
        created: le64(value, 0x08)?,
        modified: le64(value, 0x10)?,
        size: le64(value, 0x30)?,
        flags: le32(value, 0x38)?,
        reparse_tag: le32(value, 0x3C)?,
        namespace: *value.get(0x41)?,
        name: String::from_utf16_lossy(&units),
    })
}

/// 运行表里的一段：从虚拟簇号 `vcn` 起的 `length` 个簇，`lcn` 为空表示稀疏（没有分配空间，读作 0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extent {
    pub vcn: u64,
    pub length: u64,
    pub lcn: Option<u64>,
}

/// 解码运行表。每一项开头一个字节：低 4 位是长度字段的字节数，高 4 位是偏移字段的字节数；
/// 偏移是相对上一段起点的有符号数，偏移字段为 0 字节表示稀疏。格式不对时返回 `None`。
pub(crate) fn decode_runs(runs: &[u8], first_vcn: u64) -> Option<Vec<Extent>> {
    let mut extents = Vec::new();
    let mut position = 0usize;
    let mut vcn = first_vcn;
    let mut lcn = 0i64;
    loop {
        let header = *runs.get(position)?;
        if header == 0 {
            return Some(extents);
        }
        let length_size = usize::from(header & 0x0F);
        let offset_size = usize::from(header >> 4);
        if length_size == 0 || length_size > 8 || offset_size > 8 {
            return None;
        }
        let length_bytes = runs.get(position + 1..position + 1 + length_size)?;
        let length = length_bytes
            .iter()
            .rev()
            .fold(0u64, |value, &byte| (value << 8) | u64::from(byte));
        let offset_bytes =
            runs.get(position + 1 + length_size..position + 1 + length_size + offset_size)?;
        position += 1 + length_size + offset_size;
        if length == 0 {
            return None;
        }
        let target = if offset_size == 0 {
            None
        } else {
            let mut delta = offset_bytes
                .iter()
                .rev()
                .fold(0i64, |value, &byte| (value << 8) | i64::from(byte));
            // 按字段宽度做符号扩展
            if offset_size < 8 && offset_bytes.last().is_some_and(|&byte| byte & 0x80 != 0) {
                delta -= 1i64 << (offset_size * 8);
            }
            lcn = lcn.checked_add(delta)?;
            Some(u64::try_from(lcn).ok()?)
        };
        extents.push(Extent {
            vcn,
            length,
            lcn: target,
        });
        vcn = vcn.checked_add(length)?;
        if extents.len() > 1_000_000 {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_decode_with_negative_offsets_and_sparse_holes() {
        // 8 簇在 0x1234；稀疏 4 簇；3 簇在 0x1234 - 0x34 = 0x1200；结束
        let runs = [0x21, 0x08, 0x34, 0x12, 0x01, 0x04, 0x11, 0x03, 0xCC, 0x00];
        assert_eq!(
            decode_runs(&runs, 0),
            Some(vec![
                Extent {
                    vcn: 0,
                    length: 8,
                    lcn: Some(0x1234)
                },
                Extent {
                    vcn: 8,
                    length: 4,
                    lcn: None
                },
                Extent {
                    vcn: 12,
                    length: 3,
                    lcn: Some(0x1200)
                },
            ])
        );
        // 没有结束标记、长度为 0、落到负的簇号
        assert_eq!(decode_runs(&[0x11, 0x01], 0), None);
        assert_eq!(decode_runs(&[0x11, 0x00, 0x05, 0x00], 0), None);
        assert_eq!(decode_runs(&[0x11, 0x01, 0xFF, 0x00], 0), None);
    }

    #[test]
    fn fixup_restores_sector_tails_and_rejects_torn_blocks() {
        let mut block = vec![0u8; 1024];
        block[..4].copy_from_slice(b"FILE");
        block[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        block[6..8].copy_from_slice(&3u16.to_le_bytes());
        block[0x30..0x32].copy_from_slice(&7u16.to_le_bytes());
        block[0x32..0x34].copy_from_slice(&[0xAA, 0xBB]);
        block[0x34..0x36].copy_from_slice(&[0xCC, 0xDD]);
        block[510..512].copy_from_slice(&7u16.to_le_bytes());
        block[1022..1024].copy_from_slice(&7u16.to_le_bytes());
        let mut good = block.clone();
        assert!(apply_fixup(&mut good));
        assert_eq!(
            (&good[510..512], &good[1022..1024]),
            (&[0xAA, 0xBB][..], &[0xCC, 0xDD][..])
        );
        block[1022] = 8;
        assert!(!apply_fixup(&mut block));
    }
}
