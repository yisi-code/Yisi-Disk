//! SWF（SWF 文件格式说明第 19 版第 2 章）：未压缩的 SWF 以 `FWS` 开头，之后是版本、4 字节小端的文件长度，
//! 再是舞台大小（RECT，位长度可变：前 5 位是每个坐标的位数 n，共 5 + 4n 位，按字节补齐）、帧率、帧数，
//! 然后是一串标签：2 字节的「代码 << 6 | 长度」，长度为 0x3F 时后面再跟 4 字节的长度。End 标签（代码 0）结束文件。
//! 一个标签接一个走到 End 标签，它的结尾必须正好是文件头说的长度，否则不认。
//!
//! 压缩的 SWF（`CWS` 用 zlib、`ZWS` 用 LZMA）头里只有解压后的长度，算不出压缩数据在哪里结束，不认。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::{le16, le32};

/// 标签头里表示「长度在后面 4 字节里」的值。
const LONG_TAG: u16 = 0x3F;
const SHORT_TAG_HEADER: u64 = 2;
const LONG_TAG_HEADER: usize = 6;
const MAX_VERSION: u8 = 64;

pub struct Swf;

static INFO: FormatInfo = FormatInfo {
    extension: "swf",
    variants: &[],
    description: "Flash movie (uncompressed)",
    max_size: 256 << 20,
    enabled_by_default: true,
    nests_files: false,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"FWS",
}];

/// 第一个标签的位置：8 字节文件头、舞台大小、2 字节帧率与 2 字节帧数之后。
fn first_tag(block: &[u8]) -> Option<u64> {
    let bits = u64::from(*block.get(8)? >> 3);
    let rect = (5 + 4 * bits).div_ceil(8);
    Some(8 + rect + 4)
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
        let tags_at = first_tag(block)?;
        if !(1..=MAX_VERSION).contains(&version) || length < tags_at + SHORT_TAG_HEADER {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Swf.header_check(data).unwrap(), data, expected);
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
}
