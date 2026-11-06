//! SWF（SWF 文件格式说明第 19 版第 2 章）：未压缩的 SWF 以 `FWS` 开头，之后是版本、4 字节小端的文件长度，
//! 再是舞台大小（RECT，位长度可变：前 5 位是每个坐标的位数 n，共 5 + 4n 位，按字节补齐）、帧率、帧数，
//! 然后是一串标签：2 字节的「代码 << 6 | 长度」，长度为 0x3F 时后面再跟 4 字节的长度。End 标签（代码 0）结束文件。
//! 一个标签接一个走到 End 标签，它的结尾必须正好是文件头说的长度，否则不认。
//!
//! 压缩的 SWF 以 `CWS` 开头，版本与长度同上（长度是解压后的总长，含这 8 字节），之后是 zlib 流（RFC 1950），
//! 解出的就是未压缩时第 8 字节起的内容。头里没有压缩后的长度，结尾要把 zlib 流解到结束才知道（共用的流式解压，
//! [`crate::inflate`]，核对 Adler-32），解出的字节数加 8 必须正好是头里的长度。部分恢复：流断了或对不上时
//! 存到解到的位置为止，至少要已经解过 [`INTACT_INPUT`] 字节的压缩数据（断点可能偏后，同 gzip）；未压缩的不做部分恢复。
//!
//! `ZWS`（LZMA 压缩）不支持：没有 LZMA 解压，算不出压缩数据在哪里结束。

use crate::inflate::{INTACT_INPUT, Inflated, Inflater};
use crate::plugin::{DataCheck, FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::{le16, le32};

/// 标签头里表示「长度在后面 4 字节里」的值。
const LONG_TAG: u16 = 0x3F;
const SHORT_TAG_HEADER: u64 = 2;
const LONG_TAG_HEADER: usize = 6;
const MAX_VERSION: u8 = 64;
/// 文件头：签名、版本、长度。
const FILE_HEADER: u64 = 8;
/// 最短的 SWF：文件头、只有 5 位的舞台大小（1 字节）、帧率与帧数、End 标签。
const MIN_LENGTH: u64 = FILE_HEADER + 1 + 4 + SHORT_TAG_HEADER;

pub struct Swf;

static INFO: FormatInfo = FormatInfo {
    extension: "swf",
    variants: &[],
    description: "Flash movie (uncompressed or zlib-compressed)",
    max_size: 256 << 20,
    enabled_by_default: true,
    nests_files: false,
};

static SIGNATURES: [Signature; 2] = [
    Signature {
        offset: 0,
        bytes: b"FWS",
    },
    Signature {
        offset: 0,
        bytes: b"CWS",
    },
];

/// 第一个标签的位置：8 字节文件头、舞台大小、2 字节帧率与 2 字节帧数之后。
fn first_tag(block: &[u8]) -> Option<u64> {
    let bits = u64::from(*block.get(8)? >> 3);
    let rect = (5 + 4 * bits).div_ceil(8);
    Some(FILE_HEADER + rect + 4)
}

impl FileFormat for Swf {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let version = *block.get(3)?;
        let length = u64::from(le32(block, 4)?);
        if !(1..=MAX_VERSION).contains(&version) {
            return None;
        }
        if block.starts_with(b"CWS") {
            return (length >= MIN_LENGTH).then(|| {
                Box::new(Compressed {
                    length,
                    inflater: Inflater::zlib(),
                    received: 0,
                    end: None,
                    broken: false,
                    good: 0,
                }) as Box<dyn Recovering>
            });
        }
        let tags_at = first_tag(block)?;
        if length < tags_at + SHORT_TAG_HEADER {
            return None;
        }
        Some(Box::new(Walker::new(
            INFO.max_size,
            SwfStructure { tags_at, length },
        )))
    }
}

struct SwfStructure {
    tags_at: u64,
    length: u64,
}

impl Structure for SwfStructure {
    fn first(&self) -> Step {
        Step::Need {
            at: self.tags_at,
            len: SHORT_TAG_HEADER as usize,
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        let Some(code_and_length) = le16(bytes, 0) else {
            return Step::Invalid;
        };
        let short = u64::from(code_and_length & LONG_TAG);
        let (header, size) = if short == u64::from(LONG_TAG) {
            match le32(bytes, 2) {
                Some(size) => (LONG_TAG_HEADER as u64, u64::from(size)),
                // 先读到了 2 字节，再从同一处读整个长标签头
                None => {
                    return Step::Need {
                        at,
                        len: LONG_TAG_HEADER,
                    };
                }
            }
        } else {
            (SHORT_TAG_HEADER, short)
        };
        let end = at + header + size;
        if code_and_length >> 6 == 0 {
            // End 标签
            return if end == self.length {
                Step::Done(end)
            } else {
                Step::Invalid
            };
        }
        if end + SHORT_TAG_HEADER > self.length {
            return Step::Invalid;
        }
        Step::Need {
            at: end,
            len: SHORT_TAG_HEADER as usize,
        }
    }
}

/// 压缩的 SWF（`CWS`）。不经结构遍历器：每块到了就解，流一结束就给出结尾（同 gzip，见 [`crate::inflate`]）。
struct Compressed {
    /// 头里说的解压后的总长。
    length: u64,
    inflater: Inflater,
    /// 已经收到的字节数，下一块从这里开始。
    received: u64,
    /// 流的结尾（解出的长度也对上了）。
    end: Option<u64>,
    broken: bool,
    /// 部分恢复的断点：解到的位置或流的结尾。
    good: u64,
}

impl Recovering for Compressed {
    fn data_check(&mut self, offset: u64, block: &[u8]) -> DataCheck {
        if let Some(end) = self.end {
            return DataCheck::Complete(end);
        }
        let received = offset
            .checked_add(block.len() as u64)
            .filter(|_| offset == self.received && !self.broken);
        let Some(received) = received else {
            self.broken = true;
            return DataCheck::Invalid;
        };
        self.received = received;
        // zlib 流从文件头之后开始
        let skip = FILE_HEADER.saturating_sub(offset).min(block.len() as u64);
        let start = offset + skip;
        let rest = block.get(skip as usize..).unwrap_or_default();
        match self.inflater.feed(rest) {
            Inflated::More => {
                self.good = received;
                // 解出的已经比头里说的多
                if self.inflater.output_len().saturating_add(FILE_HEADER) > self.length {
                    self.broken = true;
                    return DataCheck::Invalid;
                }
                DataCheck::Continue
            }
            Inflated::End { used } => {
                let end = start + used as u64;
                self.good = end;
                if self.inflater.output_len().saturating_add(FILE_HEADER) == self.length {
                    self.end = Some(end);
                    DataCheck::Complete(end)
                } else {
                    self.broken = true;
                    DataCheck::Invalid
                }
            }
            Inflated::Broken { used } => {
                self.good = start + used as u64;
                self.broken = true;
                DataCheck::Invalid
            }
        }
    }

    fn finish(&mut self, received: u64) -> Option<u64> {
        self.end.filter(|&end| end <= received)
    }

    fn intact(&mut self, received: u64) -> Option<u64> {
        if self.inflater.input_used() < INTACT_INPUT {
            return None;
        }
        Some(self.good.min(received)).filter(|&length| length > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::{check_all_splits, check_intact_splits};

    fn make(data: &[u8]) -> impl Fn() -> Box<dyn Recovering> + '_ {
        move || Swf.header_check(data).unwrap()
    }

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(make(data), data, expected);
    }

    #[test]
    fn tags_lead_to_the_end_tag_at_the_declared_length() {
        for tags in [&[3usize][..], &[0, 70, 5000, 62, 63]] {
            let sample = samples::swf(tags);
            let mut data = sample.clone();
            data.extend_from_slice(b"FWS and more");
            check(&data, Some(sample.len() as u64));
            check(&sample[..sample.len() - 1], None);
        }
    }

    #[test]
    fn declared_length_must_match_the_end_tag() {
        let sample = samples::swf(&[10, 20]);
        let mut longer = sample.clone();
        longer[4..8].copy_from_slice(&(sample.len() as u32 + 4).to_le_bytes());
        longer.extend_from_slice(&[0; 4]);
        check(&longer, None);
        let mut shorter = sample.clone();
        shorter[4..8].copy_from_slice(&(sample.len() as u32 - 1).to_le_bytes());
        check(&shorter, None);
        let mut version = sample.clone();
        version[3] = 0;
        assert!(Swf.header_check(&version).is_none());
    }

    #[test]
    fn compressed_movies_end_with_their_zlib_stream() {
        for tags in [&[3usize][..], &[0, 70, 5000, 62, 63], &[]] {
            let sample = samples::cws(tags);
            let mut data = sample.clone();
            data.extend_from_slice(b"CWS and more");
            check(&data, Some(sample.len() as u64));
            // 少了 Adler-32 的最后一个字节
            check(&sample[..sample.len() - 1], None);
        }
    }

    #[test]
    fn compressed_length_and_checksum_must_match() {
        let sample = samples::cws(&[10, 20]);
        let declared = u32::from_le_bytes(sample[4..8].try_into().unwrap());
        for wrong in [declared - 1, declared + 1] {
            let mut data = sample.clone();
            data[4..8].copy_from_slice(&wrong.to_le_bytes());
            check(&data, None);
        }
        let mut adler = sample.clone();
        *adler.last_mut().unwrap() ^= 1;
        check(&adler, None);
        let mut version = sample.clone();
        version[3] = 0;
        assert!(Swf.header_check(&version).is_none());
        let mut tiny = sample.clone();
        tiny[4..8].copy_from_slice(&(MIN_LENGTH as u32 - 1).to_le_bytes());
        assert!(Swf.header_check(&tiny).is_none());
    }

    #[test]
    fn a_cut_compressed_movie_keeps_what_was_decoded() {
        let sample = samples::cws(&[30_000]);
        let cut = &sample[..sample.len() / 2];
        check(cut, None);
        check_intact_splits(make(cut), cut, Some(cut.len() as u64));
        // 只解出一点点：说不出可靠的一段
        let small = samples::cws(&[2000]);
        check_intact_splits(make(&small), &small[..small.len() / 2], None);
        // 未压缩的照旧不做部分恢复
        let plain = samples::swf(&[30_000]);
        check_intact_splits(make(&plain), &plain[..plain.len() / 2], None);
    }
}
