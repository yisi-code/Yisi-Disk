//! 字体文件（OpenType 规范的「字体文件的组织」一章，WOFF 1.0 与 WOFF 2.0 规范）。
//!
//! TrueType / OpenType：12 字节偏移表（sfnt 版本、表数、二分查找的三个参数），之后每张表一条 16 字节的记录
//! （标签、校验和、位置、长度）。文件没有总长度，取各表结尾的最大值，补齐到 4 字节（表按 4 字节对齐存放）。
//! sfnt 版本只有 4 个字节，容易在别的数据里撞上，所以要求二分查找的参数正是按表数算出来的值、标签都是可打印字符，
//! 再读 `head` 表核对它的魔数 0x5F0F3CF5。sfnt 版本是 `OTTO`（CFF 字形）的存成 otf。
//!
//! 字体集（TTC，`ttcf`）：头里是各字体偏移表的位置，2.0 版还有数字签名表的位置与长度；
//! 逐个读各字体的偏移表与表记录，取全部表（与签名表）结尾的最大值，存成 ttc。
//!
//! WOFF（`wOFF`）与 WOFF2（`wOF2`，存成 woff2）：头里写着整个文件的长度；其余各项只用来排除误识别。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, KnownLength, ReadQueue, Step, Structure, Walker};
use yisi_part::bytes::{be16, be32};

const OFFSET_TABLE_LEN: usize = 12;
const TABLE_RECORD_LEN: usize = 16;
const MAX_TABLES: u16 = 256;
const MAX_FONTS: u32 = 256;
/// `head` 表读到魔数为止。
const HEAD_LEN: usize = 16;
const HEAD_MAGIC: u32 = 0x5F0F_3CF5;
/// sfnt 版本：TrueType 字形（0x00010000，苹果的写成 `true`）、CFF 字形（`OTTO`）。
const SFNT_VERSIONS: [&[u8; 4]; 3] = [b"\0\x01\0\0", b"OTTO", b"true"];

pub struct Sfnt;

static SFNT_INFO: FormatInfo = FormatInfo {
    extension: "ttf",
    variants: &["otf", "ttc"],
    description: "TrueType / OpenType font",
    max_size: 512 << 20,
    enabled_by_default: true,
    // 彩色表情字体的表里存着一张张 PNG
    nests_files: true,
};

static SFNT_SIGNATURES: [Signature; 4] = [
    Signature {
        offset: 0,
        bytes: b"\0\x01\0\0",
    },
    Signature {
        offset: 0,
        bytes: b"OTTO",
    },
    Signature {
        offset: 0,
        bytes: b"true",
    },
    Signature {
        offset: 0,
        bytes: b"ttcf",
    },
];

/// 偏移表里的表数，二分查找的三个参数都是按表数算出来的值时才给出。
fn table_count(header: &[u8]) -> Option<u16> {
    let tables = be16(header, 4)?;
    if tables == 0 || tables > MAX_TABLES {
        return None;
    }
    let selector = 15 - tables.leading_zeros() as u16;
    let search_range = (1u16 << selector) * 16;
    let plausible = be16(header, 6)? == search_range
        && be16(header, 8)? == selector
        && be16(header, 10)? == tables * 16 - search_range;
    plausible.then_some(tables)
}

impl FileFormat for Sfnt {
    fn info(&self) -> &FormatInfo {
        &SFNT_INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SFNT_SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let version = block.get(..4)?;
        let (phase, extension) = if version == b"ttcf" {
            let major = be16(block, 4)?;
            let fonts = be32(block, 8)?;
            if !matches!(major, 1 | 2) || be16(block, 6)? != 0 || !(1..=MAX_FONTS).contains(&fonts)
            {
                return None;
            }
            let dsig = if major == 2 { 12 } else { 0 };
            let len = OFFSET_TABLE_LEN + 4 * fonts as usize + dsig;
            (Phase::Collection { len }, Some("ttc"))
        } else {
            let tables = table_count(block)?;
            let extension = (version == b"OTTO").then_some("otf");
            let len = OFFSET_TABLE_LEN + TABLE_RECORD_LEN * usize::from(tables);
            (Phase::Records { len }, extension)
        };
        Some(Box::new(Walker::new(
            SFNT_INFO.max_size,
            SfntStructure {
                phase,
                directories: ReadQueue::default(),
                end: 0,
                head_at: None,
                extension,
            },
        )))
    }
}

#[derive(Clone, Copy)]
enum Phase {
    /// 等字体集的头（`len` 字节）。
    Collection { len: usize },
    /// 等一个偏移表的前 12 字节。
    OffsetTable,
    /// 等偏移表连同全部表记录（`len` 字节）。
    Records { len: usize },
    /// 等 `head` 表的开头。
    Head,
}

struct SfntStructure {
    phase: Phase,
    /// 字体集里还没读的偏移表。
    directories: ReadQueue<()>,
    /// 已读的表结尾的最大值。
    end: u64,
    /// 单个字体的 `head` 表位置。
    head_at: Option<u64>,
    extension: Option<&'static str>,
}

impl SfntStructure {
    /// 在 `at` 读到一个偏移表连同表记录：更新结尾，记下 `head` 表。
    fn records(&mut self, at: u64, bytes: &[u8]) -> Option<()> {
        let tables = usize::from(table_count(bytes)?);
        let directory_end = at + (OFFSET_TABLE_LEN + TABLE_RECORD_LEN * tables) as u64;
        let records = bytes.get(OFFSET_TABLE_LEN..)?;
        let mut tags = Vec::with_capacity(tables);
        for record in records.as_chunks::<TABLE_RECORD_LEN>().0 {
            let tag = record.get(..4)?;
            let offset = u64::from(be32(record, 8)?);
            let length = u64::from(be32(record, 12)?);
            if !tag.iter().all(|byte| (0x20..=0x7E).contains(byte))
                || tags.contains(&tag)
                || offset < directory_end
            {
                return None;
            }
            tags.push(tag);
            self.end = self.end.max(offset + length);
            if tag == b"head" || tag == b"bhed" {
                self.head_at = Some(offset);
            }
        }
        Some(())
    }

    /// 读下一个字体的偏移表；都读完了时，单个字体还要看 `head` 表。
    fn next(&mut self) -> Step {
        if let Some((at, ())) = self.directories.pop() {
            self.phase = Phase::OffsetTable;
            return Step::Need {
                at,
                len: OFFSET_TABLE_LEN,
            };
        }
        let end = self.end.next_multiple_of(4);
        if self.extension == Some("ttc") {
            return Step::Done(end);
        }
        match self.head_at {
            Some(at) => {
                self.phase = Phase::Head;
                Step::Need { at, len: HEAD_LEN }
            }
            None => Step::Invalid,
        }
    }
}

impl Structure for SfntStructure {
    fn first(&self) -> Step {
        match self.phase {
            Phase::Collection { len } | Phase::Records { len } => Step::Need { at: 0, len },
            Phase::OffsetTable | Phase::Head => Step::Invalid,
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        match self.phase {
            Phase::Collection { len } => {
                let fonts = be32(bytes, 8).unwrap_or(0) as usize;
                for index in 0..fonts {
                    match be32(bytes, OFFSET_TABLE_LEN + 4 * index) {
                        Some(offset) if offset as usize >= len => {
                            self.directories.push(u64::from(offset), ());
                        }
                        _ => return Step::Invalid,
                    }
                }
                let dsig = OFFSET_TABLE_LEN + 4 * fonts;
                if bytes.len() > dsig && bytes.get(dsig..dsig + 4) == Some(b"DSIG".as_slice()) {
                    let (Some(length), Some(offset)) =
                        (be32(bytes, dsig + 4), be32(bytes, dsig + 8))
                    else {
                        return Step::Invalid;
                    };
                    self.end = u64::from(offset) + u64::from(length);
                }
                self.next()
            }
            Phase::OffsetTable => {
                let version_ok = SFNT_VERSIONS
                    .iter()
                    .any(|known| bytes.get(..4) == Some(&known[..]));
                match table_count(bytes) {
                    Some(tables) if version_ok => {
                        self.phase = Phase::Records {
                            len: OFFSET_TABLE_LEN + TABLE_RECORD_LEN * usize::from(tables),
                        };
                        Step::Need {
                            at,
                            len: OFFSET_TABLE_LEN + TABLE_RECORD_LEN * usize::from(tables),
                        }
                    }
                    _ => Step::Invalid,
                }
            }
            Phase::Records { .. } => match self.records(at, bytes) {
                Some(()) => self.next(),
                None => Step::Invalid,
            },
            Phase::Head => {
                if be32(bytes, 12) == Some(HEAD_MAGIC) && be16(bytes, 0) == Some(1) {
                    Step::Done(self.end.next_multiple_of(4))
                } else {
                    Step::Invalid
                }
            }
        }
    }

    fn extension(&self) -> Option<&'static str> {
        self.extension
    }
}

pub struct Woff;

static WOFF_INFO: FormatInfo = FormatInfo {
    extension: "woff",
    variants: &["woff2"],
    description: "WOFF web font",
    max_size: 256 << 20,
    enabled_by_default: true,
    nests_files: false,
};

static WOFF_SIGNATURES: [Signature; 2] = [
    Signature {
        offset: 0,
        bytes: b"wOFF",
    },
    Signature {
        offset: 0,
        bytes: b"wOF2",
    },
];

/// WOFF / WOFF2 头说的文件长度，各项都合规时才给出。
fn woff_length(block: &[u8]) -> Option<(u64, Option<&'static str>)> {
    let woff2 = block.get(..4)? == b"wOF2";
    let flavor = block.get(4..8)?;
    let length = u64::from(be32(block, 8)?);
    let tables = u64::from(be16(block, 12)?);
    let sfnt_size = u64::from(be32(block, 16)?);
    let (header_len, rest) = if woff2 { (48, 28) } else { (44, 24) };
    let flavor_ok =
        SFNT_VERSIONS.iter().any(|known| flavor == &known[..]) || (woff2 && flavor == b"ttcf");
    let mut plausible = flavor_ok
        && be16(block, 14)? == 0
        && tables >= 1
        && sfnt_size >= OFFSET_TABLE_LEN as u64 + TABLE_RECORD_LEN as u64 * tables;
    // 元数据块与私有数据块：位置为 0 表示没有，有的话要在文件之内
    for (offset_at, length_at) in [(rest, rest + 4), (rest + 12, rest + 16)] {
        let offset = u64::from(be32(block, offset_at)?);
        let size = u64::from(be32(block, length_at)?);
        plausible &= offset == 0 || (offset >= header_len && offset + size <= length);
    }
    if woff2 {
        let compressed = u64::from(be32(block, 20)?);
        plausible &= header_len + compressed <= length;
    } else {
        // 表目录紧跟在头后面，每项 20 字节；看第一项
        let tag = block.get(44..48)?;
        let offset = u64::from(be32(block, 48)?);
        let compressed = u64::from(be32(block, 52)?);
        let original = u64::from(be32(block, 56)?);
        plausible &= length >= header_len + 20 * tables
            && tag.iter().all(|byte| (0x20..=0x7E).contains(byte))
            && offset >= header_len + 20 * tables
            && compressed <= original
            && offset + compressed <= length;
    }
    plausible.then_some((length, woff2.then_some("woff2")))
}

impl FileFormat for Woff {
    fn info(&self) -> &FormatInfo {
        &WOFF_INFO
    }

    fn signatures(&self) -> &[Signature] {
        &WOFF_SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let (length, extension) = woff_length(block)?;
        Some(Box::new(Walker::new(
            WOFF_INFO.max_size,
            KnownLength::new(length).with_extension(extension),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::{check_all_splits, run_keeping};

    fn check(format: &dyn FileFormat, data: &[u8], expected: Option<u64>, extension: Option<&str>) {
        check_all_splits(|| format.header_check(data).unwrap(), data, expected);
        let mut recovering = format.header_check(data).unwrap();
        run_keeping(&mut recovering, data, 256);
        if expected.is_some() {
            assert_eq!(recovering.extension(), extension);
        }
    }

    #[test]
    fn single_fonts_end_at_the_last_table() {
        let tables: &[(&[u8; 4], usize)] = &[(b"cmap", 50), (b"glyf", 999), (b"head", 54)];
        for (version, extension) in [(b"\0\x01\0\0", None), (b"OTTO", Some("otf"))] {
            let sample = samples::sfnt(version, tables);
            assert_eq!(sample.len() % 4, 0);
            let mut data = sample.clone();
            data.extend_from_slice(b"\0\x01\0\0 and then something else");
            check(&Sfnt, &data, Some(sample.len() as u64), extension);
            check(&Sfnt, &sample[..sample.len() - 4], None, None);
        }
    }

    #[test]
    fn collections_cover_every_font_and_the_signature() {
        for signature in [false, true] {
            let sample = samples::ttc(2, signature);
            let mut data = sample.clone();
            data.extend_from_slice(&[0x77; 100]);
            check(&Sfnt, &data, Some(sample.len() as u64), Some("ttc"));
            check(&Sfnt, &sample[..sample.len() - 4], None, None);
        }
    }

    #[test]
    fn implausible_directories_are_rejected() {
        let sample = samples::sfnt(b"\0\x01\0\0", &[(b"cmap", 50), (b"head", 54)]);
        let changed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            data
        };
        assert!(
            Sfnt.header_check(&changed(6, &[0, 0x30])).is_none(),
            "二分查找参数"
        );
        // 标签不可打印
        check(&Sfnt, &changed(12, &[1]), None, None);
        // head 表的魔数不对
        let head_at = be32(&sample, 12 + 16 + 8).unwrap() as usize;
        check(&Sfnt, &changed(head_at + 12, &[0]), None, None);
        // 没有 head 表
        check(&Sfnt, &changed(12 + 16, b"hea_"), None, None);
    }

    #[test]
    fn woff_length_comes_from_the_header() {
        for (woff2, extension) in [(false, None), (true, Some("woff2"))] {
            let sample = samples::woff(woff2, 3000);
            let mut data = sample.clone();
            data.extend_from_slice(b"wOFF and more");
            check(&Woff, &data, Some(sample.len() as u64), extension);
            check(&Woff, &sample[..sample.len() - 1], None, None);
            let mut reserved = sample.clone();
            reserved[15] = 1;
            assert!(Woff.header_check(&reserved).is_none());
            let mut flavor = sample.clone();
            flavor[4..8].copy_from_slice(b"abcd");
            assert!(Woff.header_check(&flavor).is_none());
        }
    }
}
