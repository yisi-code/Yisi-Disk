//! Ogg（RFC 3533）：文件是一页接一页，页头 27 字节（`OggS`、版本 0、标志、粒度位置、逻辑流序号、页序号、
//! CRC、段数），之后是段表与页内容，内容长度是段表各项之和。CRC 覆盖整页（CRC 字段按 0 算），多项式 0x04C11DB7、
//! 不反射、初值 0。每个逻辑流以带 BOS 标志的页开始、以带 EOS 标志的页结束；一个文件里的几个逻辑流
//! （音频加视频）的 BOS 页都在最前面。全部逻辑流都结束的那一页就是文件结尾；紧接着又是一组 BOS 页的
//! （首尾相接的多段 Ogg）接着往下走。
//!
//! 按第一个包的开头定扩展名：有 Theora / VP8 视频的存成 ogv；只有音频时第一个流是 Opus 存成 opus、
//! 是 FLAC 存成 oga，Vorbis 等其余的用 ogg。

use std::collections::BTreeSet;

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::le32;

const MAGIC: &[u8] = b"OggS";
const PAGE_HEADER_LEN: usize = 27;
const FLAG_CONTINUED: u8 = 0x01;
const FLAG_BOS: u8 = 0x02;
const FLAG_EOS: u8 = 0x04;
/// CRC 字段在页头里的位置。
const CRC_AT: usize = 22;
/// 同时打开的逻辑流最多这么多；更多多半是认错了。
const MAX_STREAMS: usize = 32;

const CRC_TABLE: [u32; 256] = crc_table();

#[expect(
    clippy::indexing_slicing,
    reason = "编译期求值，下标 n 由循环条件限定在 0..256"
)]
const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut value = (n as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 0x8000_0000 != 0 {
                (value << 1) ^ 0x04C1_1DB7
            } else {
                value << 1
            };
            bit += 1;
        }
        table[n] = value;
        n += 1;
    }
    table
}

/// 整页的 CRC，CRC 字段本身按 0 算。
fn page_crc(page: &[u8]) -> u32 {
    page.iter().enumerate().fold(0u32, |crc, (index, &byte)| {
        let byte = if (CRC_AT..CRC_AT + 4).contains(&index) {
            0
        } else {
            byte
        };
        let slot = ((crc >> 24) ^ u32::from(byte)) & 0xFF;
        (crc << 8) ^ CRC_TABLE.get(slot as usize).copied().unwrap_or(0)
    })
}

pub struct Ogg;

static INFO: FormatInfo = FormatInfo {
    extension: "ogg",
    variants: &["ogv", "opus", "oga"],
    description: "Ogg audio / video",
    max_size: 16 << 30,
    enabled_by_default: true,
    nests_files: false,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: MAGIC,
}];

/// 页头本身合规：魔数、版本 0、没有未定义的标志位。
fn page_header_ok(header: &[u8]) -> bool {
    header.get(..4) == Some(MAGIC)
        && header.get(4) == Some(&0)
        && header
            .get(5)
            .is_some_and(|flags| flags & !(FLAG_CONTINUED | FLAG_BOS | FLAG_EOS) == 0)
}

/// 一组 BOS 页的第一页：只带 BOS 标志、页序号为 0。
fn starts_streams(header: &[u8]) -> bool {
    page_header_ok(header) && header.get(5) == Some(&FLAG_BOS) && le32(header, 18) == Some(0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Codec {
    Video,
    Opus,
    Flac,
    Other,
}

/// 按逻辑流第一个包的开头认编码。
fn codec(packet: &[u8]) -> Codec {
    if packet.starts_with(b"\x80theora") || packet.starts_with(b"OVP80") {
        Codec::Video
    } else if packet.starts_with(b"OpusHead") {
        Codec::Opus
    } else if packet.starts_with(b"\x7FFLAC") {
        Codec::Flac
    } else {
        Codec::Other
    }
}

impl FileFormat for Ogg {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        if !starts_streams(block.get(..PAGE_HEADER_LEN)?) {
            return None;
        }
        Some(Box::new(Walker::new(
            INFO.max_size,
            OggStructure {
                phase: Phase::Header,
                open: BTreeSet::new(),
                link_has_data: false,
                closed_at: None,
                first_link: true,
                codecs: Vec::new(),
                pages: 0,
                last_page_end: 0,
            },
        )))
    }
}

#[derive(Clone, Copy)]
enum Phase {
    /// 等 27 字节的页头。
    Header,
    /// 等页头加段表。
    Segments,
    /// 等整页，核对 CRC。
    Page,
}

struct OggStructure {
    phase: Phase,
    /// 已经开始、还没结束的逻辑流。
    open: BTreeSet<u32>,
    /// 这一段（首尾相接的多段 Ogg 的一段）里已经有了 BOS 页以外的页：之后不能再开新的逻辑流。
    link_has_data: bool,
    /// 全部逻辑流都结束的那一页的结尾。
    closed_at: Option<u64>,
    first_link: bool,
    /// 第一段里各逻辑流的编码，按开始的顺序。
    codecs: Vec<Codec>,
    /// 核对过 CRC、接得上的页数，与最后一页的结尾。
    pages: u32,
    last_page_end: u64,
}

impl OggStructure {
    /// 一整页（CRC 已核对）是否接得上前面的页；接得上时更新逻辑流的状态。
    fn accept(&mut self, page: &[u8], segments: usize) -> bool {
        let (Some(&flags), Some(serial), Some(sequence)) =
            (page.get(5), le32(page, 14), le32(page, 18))
        else {
            return false;
        };
        if flags & FLAG_BOS != 0 {
            if self.link_has_data
                || flags & FLAG_CONTINUED != 0
                || sequence != 0
                || self.open.len() >= MAX_STREAMS
                || !self.open.insert(serial)
            {
                return false;
            }
            if self.first_link {
                let body = page.get(PAGE_HEADER_LEN + segments..).unwrap_or_default();
                self.codecs.push(codec(body));
            }
        } else {
            if !self.open.contains(&serial) {
                return false;
            }
            self.link_has_data = true;
        }
        if flags & FLAG_EOS != 0 {
            self.open.remove(&serial);
        }
        true
    }
}

impl Structure for OggStructure {
    fn first(&self) -> Step {
        Step::Need {
            at: 0,
            len: PAGE_HEADER_LEN,
        }
    }

    /// 部分恢复：至少两页核对过 CRC、接得上才保存，到最后一个这样的页为止。
    fn intact(&self, _cut: u64) -> Option<u64> {
        (self.pages >= 2).then_some(self.last_page_end)
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        match self.phase {
            Phase::Header => {
                if let Some(closed_at) = self.closed_at {
                    // 全部逻辑流都已结束：后面紧接着一组新的 BOS 页才接着走，否则文件到此为止
                    if !starts_streams(bytes) {
                        return Step::Done(closed_at);
                    }
                    self.closed_at = None;
                    self.first_link = false;
                    self.link_has_data = false;
                } else if !page_header_ok(bytes) {
                    return Step::Invalid;
                }
                let segments = bytes.get(26).copied().unwrap_or(0);
                self.phase = Phase::Segments;
                Step::Need {
                    at,
                    len: PAGE_HEADER_LEN + usize::from(segments),
                }
            }
            Phase::Segments => {
                let table = bytes.get(PAGE_HEADER_LEN..).unwrap_or_default();
                let body: usize = table.iter().map(|&lacing| usize::from(lacing)).sum();
                self.phase = Phase::Page;
                Step::Need {
                    at,
                    len: bytes.len() + body,
                }
            }
            Phase::Page => {
                let segments = usize::from(bytes.get(26).copied().unwrap_or(0));
                if le32(bytes, CRC_AT) != Some(page_crc(bytes)) || !self.accept(bytes, segments) {
                    return Step::Invalid;
                }
                let end = at + bytes.len() as u64;
                self.pages += 1;
                self.last_page_end = end;
                if self.open.is_empty() {
                    self.closed_at = Some(end);
                }
                self.phase = Phase::Header;
                Step::Need {
                    at: end,
                    len: PAGE_HEADER_LEN,
                }
            }
        }
    }

    /// 数据在全部逻辑流都结束之后用完（设备末尾）：文件到那一页为止。
    fn finish(&self, received: u64, _waiting_at: Option<u64>) -> Option<u64> {
        self.closed_at.filter(|&end| end <= received)
    }

    fn extension(&self) -> Option<&'static str> {
        if self.codecs.contains(&Codec::Video) {
            return Some("ogv");
        }
        match self.codecs.first() {
            Some(Codec::Opus) => Some("opus"),
            Some(Codec::Flac) => Some("oga"),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::{check_all_splits, run_keeping};

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Ogg.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn pages_are_followed_until_every_stream_has_ended() {
        type Case<'a> = (&'a [&'a [u8]], usize, Option<&'a str>);
        let cases: [Case; 4] = [
            (&[b"\x01vorbis"], 1, None),
            (&[b"OpusHead"], 2, Some("opus")),
            (&[b"\x80theora", b"\x01vorbis"], 1, Some("ogv")),
            (&[b"\x7FFLAC"], 1, Some("oga")),
        ];
        for (streams, links, extension) in cases {
            let sample = samples::ogg(streams, 4, links);
            let mut data = sample.clone();
            data.extend_from_slice(b"OggS\0\x00 not a page that follows");
            check(&data, Some(sample.len() as u64));
            check(&sample, Some(sample.len() as u64));
            let mut recovering = Ogg.header_check(&data).unwrap();
            run_keeping(&mut recovering, &data, 333);
            assert_eq!(recovering.extension(), extension);
        }
    }

    #[test]
    fn truncated_or_corrupted_pages_are_rejected() {
        let sample = samples::ogg(&[b"\x01vorbis"], 4, 1);
        check(&sample[..sample.len() - 1], None);
        let mut corrupted = sample.clone();
        corrupted[200] ^= 0x40;
        check(&corrupted, None);
        // 第一页不带 BOS 标志
        let mut no_bos = sample.clone();
        no_bos[5] = 0;
        assert!(Ogg.header_check(&no_bos).is_none());
    }

    #[test]
    fn crc_follows_the_ogg_polynomial() {
        // 只有页头、段数为 0 的页
        let mut page = b"OggS\0\x02".to_vec();
        page.extend_from_slice(&[0; 21]);
        let crc = page_crc(&page);
        page[CRC_AT..CRC_AT + 4].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(page_crc(&page), crc, "CRC 字段按 0 算");
        assert_eq!(crc, samples::ogg_crc(&page));
    }
}
