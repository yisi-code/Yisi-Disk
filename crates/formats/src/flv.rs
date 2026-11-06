//! FLV（Flash Video 文件格式说明 10.1 版附录 E）：9 字节文件头（`FLV`、版本 1、音视频标志、头长度 9），
//! 之后「前一个标签的长度」与标签交替出现，第一个长度是 0。标签头 11 字节：类型（8 音频、9 视频、18 脚本数据，
//! 第 5 位是加密标志）、3 字节数据长度、4 字节时间戳、3 字节流 ID（总是 0）；数据之后的「前一个标签的长度」
//! 等于 11 加数据长度。
//!
//! 文件没有结束标志：一个标签接一个往下走，走到不是标签头的地方就是结尾。标签后面的长度对不上说明这个标签坏了
//! （被截短或被覆盖），不认。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::be32;

const FILE_HEADER_LEN: u32 = 9;
const TAG_HEADER_LEN: usize = 11;
const TRAILER_LEN: usize = 4;
/// 文件头里的音频、视频标志；其余位保留为 0。
const FLAGS_KNOWN: u8 = 0x05;
const TAG_AUDIO: u8 = 8;
const TAG_VIDEO: u8 = 9;
const TAG_SCRIPT: u8 = 18;
const TAG_FILTER: u8 = 0x20;

pub struct Flv;

static INFO: FormatInfo = FormatInfo {
    extension: "flv",
    variants: &[],
    description: "Flash video",
    max_size: 8 << 30,
    enabled_by_default: true,
    nests_files: false,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"FLV\x01",
}];

/// 标签头里的数据长度；不是合规的标签头时为 `None`。
fn tag_data_len(header: &[u8]) -> Option<u32> {
    let kind = *header.first()?;
    let size = be32(header, 0)? & 0x00FF_FFFF;
    let plausible = matches!(kind & !TAG_FILTER, TAG_AUDIO | TAG_VIDEO | TAG_SCRIPT)
        && size > 0
        && header.get(8..11) == Some([0, 0, 0].as_slice());
    plausible.then_some(size)
}

impl FileFormat for Flv {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let flags = *block.get(4)?;
        let plausible =
            flags & !FLAGS_KNOWN == 0 && be32(block, 5)? == FILE_HEADER_LEN && be32(block, 9)? == 0;
        plausible.then(|| {
            Box::new(Walker::new(
                INFO.max_size,
                FlvStructure {
                    tags: 0,
                    last_end: None,
                    data_len: None,
                },
            )) as Box<dyn Recovering>
        })
    }
}

struct FlvStructure {
    tags: u32,
    /// 最后一个完整标签（连同它后面的长度）的结尾。
    last_end: Option<u64>,
    /// 正在核对后面长度的标签的数据长度。
    data_len: Option<u32>,
}

impl Structure for FlvStructure {
    fn first(&self) -> Step {
        Step::Need {
            at: u64::from(FILE_HEADER_LEN) + TRAILER_LEN as u64,
            len: TAG_HEADER_LEN,
        }
    }

    /// 部分恢复：到最后一个完整的标签（后面的长度核对过）为止。
    fn intact(&self, _cut: u64) -> Option<u64> {
        self.last_end.filter(|_| self.tags > 0)
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        if let Some(data_len) = self.data_len.take() {
            if be32(bytes, 0) != Some(TAG_HEADER_LEN as u32 + data_len) {
                return Step::Invalid;
            }
            self.tags += 1;
            let end = at + TRAILER_LEN as u64;
            self.last_end = Some(end);
            return Step::Need {
                at: end,
                len: TAG_HEADER_LEN,
            };
        }
        match tag_data_len(bytes) {
            Some(data_len) => {
                self.data_len = Some(data_len);
                Step::Need {
                    at: at + TAG_HEADER_LEN as u64 + u64::from(data_len),
                    len: TRAILER_LEN,
                }
            }
            None if self.tags > 0 => Step::Done(at),
            None => Step::Invalid,
        }
    }

    /// 数据在两个标签之间用完（设备末尾）：文件到最后一个完整的标签为止。
    fn finish(&self, received: u64, _waiting_at: Option<u64>) -> Option<u64> {
        if self.data_len.is_some() {
            return None;
        }
        self.last_end.filter(|&end| end <= received)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Flv.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn tags_are_followed_to_the_end() {
        for tags in [1, 6] {
            let sample = samples::flv(tags);
            let mut data = sample.clone();
            data.extend_from_slice(&[0; 40]);
            check(&data, Some(sample.len() as u64));
            check(&sample, Some(sample.len() as u64));
        }
    }

    #[test]
    fn broken_tags_or_headers_are_rejected() {
        let sample = samples::flv(3);
        // 最后一个标签被截短
        check(&sample[..sample.len() - 2], None);
        // 第一个标签后面的长度不对
        let mut wrong_trailer = sample.clone();
        let first_len = (be32(&sample, 13).unwrap() & 0x00FF_FFFF) as usize;
        wrong_trailer[13 + 11 + first_len + 3] ^= 1;
        check(&wrong_trailer, None);
        // 文件头之后不是标签
        let mut no_tag = sample.clone();
        no_tag[13] = 0x07;
        check(&no_tag, None);
        let mut header = sample.clone();
        header[4] = 0x08;
        assert!(Flv.header_check(&header).is_none(), "保留的标志位");
    }
}
