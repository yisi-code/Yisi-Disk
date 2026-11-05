//! TIFF（TIFF 6.0 规范）与基于它的相机 RAW：8 字节文件头（字节序 `II` / `MM`、42、第一个 IFD 的位置），
//! IFD 是 2 字节项数、每项 12 字节（标签、类型、个数、值或值的位置）、4 字节下一个 IFD 的位置。
//!
//! 文件没有结束标志，长度取各处登记的数据的结尾的最大值：IFD 本身、放在 IFD 外面的值、
//! 条带与图块（偏移数组配长度数组）、内嵌的 JPEG 预览；子 IFD、EXIF、GPS、互操作 IFD 与下一个 IFD 都跟着看。
//! 遍历器只能往后读，所以要读的东西按位置从小到大依次读；IFD 写在文件末尾时，它引用的数组已经过去了，
//! 那些数据也在 IFD 之前，不影响长度。
//!
//! 按内容改用 RAW 的扩展名：文件头是佳能 CR2（第 8 字节起 `CR` 2）、奥林巴斯 ORF（`IIRO` / `IIRS` / `MMOR`）、
//! 松下 RW2（`IIU\0`）；IFD0 里有 DNGVersion 是 DNG；否则按 IFD0 的厂商名（Make）：尼康 NEF、索尼 ARW、
//! 宾得 / 理光 PEF、三星 SRW。

use std::collections::BTreeSet;

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, MAX_NEED, ReadQueue, Step, Structure, Walker};
use yisi_part::bytes::{be16, be32, le16, le32};

const TAG_MAKE: u16 = 271;
const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_TILE_OFFSETS: u16 = 324;
const TAG_TILE_BYTE_COUNTS: u16 = 325;
const TAG_SUB_IFDS: u16 = 330;
const TAG_JPEG_OFFSET: u16 = 513;
const TAG_JPEG_LENGTH: u16 = 514;
const TAG_EXIF_IFD: u16 = 34665;
const TAG_GPS_IFD: u16 = 34853;
const TAG_INTEROP_IFD: u16 = 40965;
const TAG_DNG_VERSION: u16 = 50706;
/// 类型 13（IFD）是 TIFF 技术说明 1 加的，值是一个 IFD 的位置。
const TYPE_IFD: u16 = 13;
/// 一个 IFD 最多这么多项；更多多半是认错了。
const MAX_ENTRIES: u16 = 1000;
/// 最多看这么多个 IFD，防止 IFD 互相指着绕圈。
const MAX_IFDS: usize = 256;
/// 厂商名最多读这么长。
const MAX_MAKE: u64 = 64;

pub struct Tiff;

static INFO: FormatInfo = FormatInfo {
    extension: "tif",
    variants: &["cr2", "nef", "arw", "dng", "orf", "rw2", "pef", "srw"],
    description: "TIFF image / camera raw",
    max_size: 4 << 30,
    enabled_by_default: true,
    nests_files: true,
};

static SIGNATURES: [Signature; 6] = [
    Signature {
        offset: 0,
        bytes: b"II*\0",
    },
    Signature {
        offset: 0,
        bytes: b"MM\0*",
    },
    Signature {
        offset: 0,
        bytes: b"IIRO",
    },
    Signature {
        offset: 0,
        bytes: b"IIRS",
    },
    Signature {
        offset: 0,
        bytes: b"MMOR",
    },
    Signature {
        offset: 0,
        bytes: b"IIU\0",
    },
];

/// 各类型一个值的字节数；不认识的类型为 `None`。
fn type_size(kind: u16) -> Option<u64> {
    match kind {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 | TYPE_IFD => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

/// 按文件头定的扩展名。
fn header_extension(block: &[u8]) -> Option<&'static str> {
    match block.get(..4)? {
        b"IIRO" | b"IIRS" | b"MMOR" => Some("orf"),
        b"IIU\0" => Some("rw2"),
        b"II*\0" if block.get(8..11) == Some(b"CR\x02".as_slice()) => Some("cr2"),
        _ => None,
    }
}

/// 按厂商名定的扩展名。
fn make_extension(make: &[u8]) -> Option<&'static str> {
    let make = make.trim_ascii_start().to_ascii_uppercase();
    [
        (b"NIKON".as_slice(), "nef"),
        (b"SONY", "arw"),
        (b"PENTAX", "pef"),
        (b"RICOH", "pef"),
        (b"SAMSUNG", "srw"),
    ]
    .into_iter()
    .find(|(prefix, _)| make.starts_with(prefix))
    .map(|(_, extension)| extension)
}

impl FileFormat for Tiff {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let big = block.first() == Some(&b'M');
        let first_ifd = if big {
            be32(block, 4)?
        } else {
            le32(block, 4)?
        };
        if first_ifd < 8 || u64::from(first_ifd) >= INFO.max_size {
            return None;
        }
        let mut structure = TiffStructure {
            big,
            pending: ReadQueue::default(),
            current: None,
            end: 8,
            has_image: false,
            visited: BTreeSet::new(),
            groups: Vec::new(),
            header_extension: header_extension(block),
            make_extension: None,
            dng: false,
        };
        structure.visited.insert(u64::from(first_ifd));
        structure.schedule(u64::from(first_ifd), Item::IfdCount { first: true });
        Some(Box::new(Walker::new(INFO.max_size, structure)))
    }
}

/// 条带或图块：偏移数组配长度数组，两个都拿到才算得出结尾。
#[derive(Default)]
struct Group {
    offsets: Option<Vec<u64>>,
    counts: Option<Vec<u64>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Offsets,
    Counts,
}

/// 要读的一样东西。
#[derive(Clone, Copy)]
enum Item {
    /// IFD 的项数（2 字节）；`first` 是 IFD0。
    IfdCount { first: bool },
    /// IFD 的各项与下一个 IFD 的位置。
    IfdEntries { count: u16, first: bool },
    /// 放在 IFD 外面的数组：条带或图块的偏移或长度。
    Array {
        group: usize,
        which: Which,
        kind: u16,
        count: u32,
    },
    /// 放在 IFD 外面的子 IFD 位置数组。
    IfdList { kind: u16, count: u32 },
    /// 厂商名。
    Make { len: u64 },
}

impl Item {
    /// 要读多少字节。
    fn len(&self) -> u64 {
        match *self {
            Item::IfdCount { .. } => 2,
            Item::IfdEntries { count, .. } => u64::from(count) * 12 + 4,
            Item::Array { kind, count, .. } | Item::IfdList { kind, count } => {
                type_size(kind).unwrap_or(4) * u64::from(count)
            }
            Item::Make { len } => len,
        }
    }
}

struct TiffStructure {
    big: bool,
    /// 还要读的东西，按位置排。
    pending: ReadQueue<Item>,
    /// 正在读的那一样。
    current: Option<Item>,
    /// 目前知道的文件结尾。
    end: u64,
    /// 见到过图像数据（条带、图块或 JPEG）。
    has_image: bool,
    visited: BTreeSet<u64>,
    groups: Vec<Group>,
    header_extension: Option<&'static str>,
    make_extension: Option<&'static str>,
    dng: bool,
}

impl TiffStructure {
    fn u16_at(&self, bytes: &[u8], at: usize) -> Option<u16> {
        if self.big {
            be16(bytes, at)
        } else {
            le16(bytes, at)
        }
    }

    fn u32_at(&self, bytes: &[u8], at: usize) -> Option<u32> {
        if self.big {
            be32(bytes, at)
        } else {
            le32(bytes, at)
        }
    }

    /// 一串 SHORT / LONG / IFD 值。
    fn values(&self, bytes: &[u8], kind: u16, count: u32) -> Option<Vec<u64>> {
        (0..count as usize)
            .map(|index| match kind {
                3 => self.u16_at(bytes, index * 2).map(u64::from),
                4 | TYPE_IFD => self.u32_at(bytes, index * 4).map(u64::from),
                _ => None,
            })
            .collect()
    }

    fn schedule(&mut self, at: u64, item: Item) {
        self.pending.push(at, item);
    }

    fn extend_to(&mut self, end: u64) {
        self.end = self.end.max(end);
    }

    fn group_values(&mut self, group: usize, which: Which, values: Vec<u64>) {
        let Some(slot) = self.groups.get_mut(group) else {
            return;
        };
        match which {
            Which::Offsets => slot.offsets = Some(values),
            Which::Counts => slot.counts = Some(values),
        }
        if let (Some(offsets), Some(counts)) = (&slot.offsets, &slot.counts) {
            let end = offsets
                .iter()
                .zip(counts)
                .filter_map(|(offset, count)| offset.checked_add(*count))
                .max();
            if let Some(end) = end {
                self.has_image = true;
                self.extend_to(end);
            }
        }
    }

    fn ifd_at(&mut self, at: u64) {
        if at >= 8 && self.visited.len() < MAX_IFDS && self.visited.insert(at) {
            self.schedule(at, Item::IfdCount { first: false });
        }
    }

    /// 解析一个 IFD 的各项。IFD0 里有不认识的类型时判为认错了。
    fn entries(&mut self, at: u64, bytes: &[u8], count: u16, first: bool) -> bool {
        let mut strips = [None, None];
        let mut tiles = [None, None];
        let mut jpeg = [None, None];
        for index in 0..usize::from(count) {
            let Some(entry) = bytes.get(index * 12..index * 12 + 12) else {
                return false;
            };
            let (Some(tag), Some(kind), Some(n), Some(value)) = (
                self.u16_at(entry, 0),
                self.u16_at(entry, 2),
                self.u32_at(entry, 4),
                self.u32_at(entry, 8),
            ) else {
                return false;
            };
            let Some(size) = type_size(kind) else {
                if first {
                    return false;
                }
                continue;
            };
            let bytes_len = size * u64::from(n);
            let inline = bytes_len <= 4;
            if !inline {
                self.extend_to(u64::from(value) + bytes_len);
            }
            // 值或值的位置：放在项里的，按类型从项的第 8 字节起读
            let field = (
                tag,
                kind,
                n,
                value,
                inline,
                entry.get(8..12).unwrap_or_default(),
            );
            match tag {
                TAG_MAKE if first => {
                    if inline {
                        self.make_extension = make_extension(field.5);
                    } else {
                        self.schedule(
                            u64::from(value),
                            Item::Make {
                                len: bytes_len.min(MAX_MAKE),
                            },
                        );
                    }
                }
                TAG_DNG_VERSION if first => self.dng = true,
                TAG_STRIP_OFFSETS => strips[0] = Some(field),
                TAG_STRIP_BYTE_COUNTS => strips[1] = Some(field),
                TAG_TILE_OFFSETS => tiles[0] = Some(field),
                TAG_TILE_BYTE_COUNTS => tiles[1] = Some(field),
                TAG_JPEG_OFFSET => jpeg[0] = Some(value),
                TAG_JPEG_LENGTH => jpeg[1] = Some(value),
                TAG_EXIF_IFD | TAG_GPS_IFD | TAG_INTEROP_IFD if inline => {
                    self.ifd_at(u64::from(value));
                }
                TAG_SUB_IFDS => {
                    if inline {
                        for sub in self.values(field.5, kind, n).unwrap_or_default() {
                            self.ifd_at(sub);
                        }
                    } else if bytes_len <= MAX_NEED as u64 {
                        self.schedule(u64::from(value), Item::IfdList { kind, count: n });
                    }
                }
                _ => {}
            }
        }
        for pair in [strips, tiles] {
            let [Some(offsets), Some(counts)] = pair else {
                continue;
            };
            let group = self.groups.len();
            self.groups.push(Group::default());
            for ((_, kind, n, value, inline, raw), which) in
                [(offsets, Which::Offsets), (counts, Which::Counts)]
            {
                if inline {
                    let values = self.values(raw, kind, n).unwrap_or_default();
                    self.group_values(group, which, values);
                } else if (kind == 3 || kind == 4)
                    && type_size(kind).unwrap_or(4) * u64::from(n) <= MAX_NEED as u64
                {
                    self.schedule(
                        u64::from(value),
                        Item::Array {
                            group,
                            which,
                            kind,
                            count: n,
                        },
                    );
                }
            }
        }
        if let [Some(offset), Some(length)] = jpeg
            && length > 0
        {
            self.has_image = true;
            self.extend_to(u64::from(offset) + u64::from(length));
        }
        let next_at = usize::from(count) * 12;
        if let Some(next) = self.u32_at(bytes, next_at)
            && next != 0
        {
            self.ifd_at(u64::from(next));
        }
        self.extend_to(at + bytes.len() as u64);
        true
    }

    /// 读下一样还读得到的东西（位置不早于 `cursor`）；都读完了就结束。
    fn next(&mut self, cursor: u64) -> Step {
        while let Some((at, item)) = self.pending.pop() {
            let Ok(len) = usize::try_from(item.len()) else {
                continue;
            };
            if at < cursor || !(1..=MAX_NEED).contains(&len) {
                // 条带或图块的数组已经过去了（IFD 写在后面）：数据也在前面，图像是有的
                if matches!(item, Item::Array { .. }) {
                    self.has_image = true;
                }
                continue;
            }
            self.current = Some(item);
            return Step::Need { at, len };
        }
        if self.has_image {
            Step::Done(self.end.max(cursor))
        } else {
            Step::Invalid
        }
    }
}

impl Structure for TiffStructure {
    fn first(&self) -> Step {
        match self.pending.first_at() {
            Some(at) => Step::Need { at, len: 2 },
            None => Step::Invalid,
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        // 第一步是 header_check 排进去的 IFD0
        let item = match self.current.take() {
            Some(item) => item,
            None => {
                self.pending.pop();
                Item::IfdCount { first: true }
            }
        };
        match item {
            Item::IfdCount { first } => {
                let Some(count) = self.u16_at(bytes, 0) else {
                    return Step::Invalid;
                };
                if count == 0 || count > MAX_ENTRIES {
                    if first {
                        return Step::Invalid;
                    }
                    return self.next(at);
                }
                self.current = Some(Item::IfdEntries { count, first });
                Step::Need {
                    at: at + 2,
                    len: usize::from(count) * 12 + 4,
                }
            }
            Item::IfdEntries { count, first } => {
                if !self.entries(at, bytes, count, first) && first {
                    return Step::Invalid;
                }
                self.next(at)
            }
            Item::Array {
                group,
                which,
                kind,
                count,
            } => {
                let values = self.values(bytes, kind, count).unwrap_or_default();
                self.group_values(group, which, values);
                self.next(at)
            }
            Item::IfdList { kind, count } => {
                for sub in self.values(bytes, kind, count).unwrap_or_default() {
                    self.ifd_at(sub);
                }
                self.next(at)
            }
            Item::Make { .. } => {
                let name = bytes.split(|&byte| byte == 0).next().unwrap_or_default();
                self.make_extension = make_extension(name);
                self.next(at)
            }
        }
    }

    fn extension(&self) -> Option<&'static str> {
        self.header_extension
            .or(self.dng.then_some("dng"))
            .or(self.make_extension)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples::{self, TiffLayout};
    use crate::walker::tests::{check_all_splits, run_keeping};

    fn read(data: &[u8]) -> Option<(u64, Option<&'static str>)> {
        let mut recovering = Tiff.header_check(data)?;
        let length = run_keeping(&mut recovering, data, 211)?;
        Some((length, recovering.extension()))
    }

    #[test]
    fn length_covers_strips_and_ifds_wherever_the_ifd_is() {
        for big in [false, true] {
            for ifd_last in [false, true] {
                let layout = TiffLayout {
                    big,
                    ifd_last,
                    ..TiffLayout::default()
                };
                let sample = samples::tiff(&layout);
                let mut data = sample.clone();
                data.extend_from_slice(b"II*\0 then another file");
                check_all_splits(
                    || Tiff.header_check(&data).unwrap(),
                    &data,
                    Some(sample.len() as u64),
                );
                assert_eq!(read(&sample), Some((sample.len() as u64, None)));
            }
        }
    }

    #[test]
    fn exif_preview_and_sub_ifds_extend_the_file() {
        let layout = TiffLayout {
            make: Some(b"NIKON CORPORATION"),
            preview: true,
            sub_ifd: true,
            ..TiffLayout::default()
        };
        let sample = samples::tiff(&layout);
        assert_eq!(read(&sample), Some((sample.len() as u64, Some("nef"))));
        check_all_splits(
            || Tiff.header_check(&sample).unwrap(),
            &sample,
            Some(sample.len() as u64),
        );
    }

    #[test]
    fn raw_kinds_come_from_the_header_dng_tag_or_make() {
        let cases: [(TiffLayout, Option<&str>); 6] = [
            (
                TiffLayout {
                    make: Some(b"SONY"),
                    ..TiffLayout::default()
                },
                Some("arw"),
            ),
            (
                TiffLayout {
                    make: Some(b"PENTAX"),
                    ..TiffLayout::default()
                },
                Some("pef"),
            ),
            (
                TiffLayout {
                    make: Some(b"Canon"),
                    ..TiffLayout::default()
                },
                None,
            ),
            (
                TiffLayout {
                    make: Some(b"NIKON"),
                    dng: true,
                    ..TiffLayout::default()
                },
                Some("dng"),
            ),
            (
                TiffLayout {
                    magic: *b"IIRO",
                    ..TiffLayout::default()
                },
                Some("orf"),
            ),
            (
                TiffLayout {
                    magic: *b"IIU\0",
                    ..TiffLayout::default()
                },
                Some("rw2"),
            ),
        ];
        for (layout, expected) in cases {
            let sample = samples::tiff(&layout);
            assert_eq!(read(&sample), Some((sample.len() as u64, expected)));
        }
        // CR2 的文件头是 16 字节，第 8 字节起是 `CR` 与版本；这里第 8 字节起是条带数据，可以改
        let mut cr2 = samples::tiff(&TiffLayout {
            ifd_last: true,
            ..TiffLayout::default()
        });
        cr2[8..11].copy_from_slice(b"CR\x02");
        assert_eq!(
            read(&cr2).map(|(_, extension)| extension),
            Some(Some("cr2"))
        );
    }

    #[test]
    fn truncated_or_imageless_files_are_rejected() {
        // 截在 IFD 上：读不全 IFD。截在条带数据里时 IFD 已经说明了总长度，由深度恢复按长度取数据时发现不够
        let sample = samples::tiff(&TiffLayout {
            ifd_last: true,
            ..TiffLayout::default()
        });
        let truncated = &sample[..sample.len() - 3];
        check_all_splits(|| Tiff.header_check(truncated).unwrap(), truncated, None);

        let mut no_image = samples::tiff(&TiffLayout {
            strips: 0,
            ..TiffLayout::default()
        });
        no_image.extend_from_slice(&[0; 64]);
        check_all_splits(|| Tiff.header_check(&no_image).unwrap(), &no_image, None);

        let mut bad_type = samples::tiff(&TiffLayout::default());
        // IFD0 第一项的类型改成不存在的 99
        bad_type[8 + 2 + 2] = 99;
        bad_type[8 + 2 + 3] = 0;
        check_all_splits(|| Tiff.header_check(&bad_type).unwrap(), &bad_type, None);
    }
}
