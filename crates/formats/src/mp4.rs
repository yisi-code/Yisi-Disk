//! MP4 / MOV（ISO 基础媒体文件格式）：一串顶层 box（4 字节大端长度 + 4 字节类型），以 ftyp 开头。
//! 文件没有结束标志：遇到不认识的顶层 box、或数据结束时，在最后一个完整 box 之后结束。

use crate::bytes::{be32, be64};
use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};

/// 视为合法的顶层 box 类型；遇到其他类型时文件结束。
const TOP_LEVEL_BOXES: [&[u8; 4]; 17] = [
    b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"uuid", b"meta", b"pdin", b"moof",
    b"mfra", b"styp", b"sidx", b"ssix", b"prft", b"emsg", b"pnot",
];

pub struct Mp4;

static INFO: FormatInfo = FormatInfo {
    extension: "mp4",
    description: "MPEG-4 / QuickTime video",
    max_size: 64 << 30,
    enabled_by_default: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 4,
    bytes: b"ftyp",
}];

impl FileFormat for Mp4 {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let size = be32(block, 0)?;
        let brand = block.get(8..12)?;
        let plausible = (16..=4096).contains(&size)
            && block.get(4..8) == Some(b"ftyp".as_slice())
            && brand
                .iter()
                .all(|byte| byte.is_ascii_graphic() || *byte == b' ');
        plausible.then(|| {
            Box::new(Walker::new(Mp4Structure {
                phase: Phase::Header,
                seen_moov: false,
                seen_mdat: false,
            })) as Box<dyn Recovering>
        })
    }
}

enum Phase {
    /// 等 box 头：长度 + 类型。
    Header,
    /// 长度字段为 1，等后面的 8 字节 64 位长度。
    LargeSize { start: u64 },
}

struct Mp4Structure {
    phase: Phase,
    seen_moov: bool,
    seen_mdat: bool,
}

impl Mp4Structure {
    fn complete(&self) -> bool {
        self.seen_moov && self.seen_mdat
    }

    fn end_at(&self, at: u64) -> Step {
        if self.complete() {
            Step::Done(at)
        } else {
            Step::Invalid
        }
    }
}

impl Structure for Mp4Structure {
    fn first(&self) -> Step {
        Step::Need { at: 0, len: 8 }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        match self.phase {
            Phase::Header => {
                let (Some(size), Some(kind)) = (be32(bytes, 0), bytes.get(4..8)) else {
                    return Step::Invalid;
                };
                let Some(known) = TOP_LEVEL_BOXES
                    .iter()
                    .find(|known| known.as_slice() == kind)
                else {
                    // 不认识的类型：文件在这里结束（后面多半是别的数据）
                    return self.end_at(at);
                };
                match *known {
                    b"moov" => self.seen_moov = true,
                    b"mdat" => self.seen_mdat = true,
                    _ => {}
                }
                match size {
                    1 => {
                        self.phase = Phase::LargeSize { start: at };
                        Step::Need { at: at + 8, len: 8 }
                    }
                    // 0 表示 box 一直延伸到文件末尾，无法知道文件在哪里结束
                    0..8 => Step::Invalid,
                    _ => Step::Need {
                        at: at + u64::from(size),
                        len: 8,
                    },
                }
            }
            Phase::LargeSize { start } => {
                let Some(size) = be64(bytes, 0).filter(|&size| size >= 16) else {
                    return Step::Invalid;
                };
                let Some(next) = start.checked_add(size) else {
                    return Step::Invalid;
                };
                self.phase = Phase::Header;
                Step::Need { at: next, len: 8 }
            }
        }
    }

    /// 数据恰好在一个 box 结束处用完（例如在设备末尾）：只要 moov、mdat 都已出现，就算完整。
    fn finish(&self, received: u64, waiting_at: Option<u64>) -> Option<u64> {
        (matches!(self.phase, Phase::Header) && waiting_at == Some(received) && self.complete())
            .then_some(received)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn recover(data: &[u8]) -> Box<dyn Recovering> {
        Mp4.header_check(data).unwrap()
    }

    #[test]
    fn ends_before_an_unknown_box_or_at_end_of_data() {
        let sample = samples::mp4(5000, false);
        let mut followed = sample.clone();
        followed.extend_from_slice(b"\xFF\xD8\xFF\xE0garbage that is not a box");
        check_all_splits(|| recover(&followed), &followed, Some(sample.len() as u64));
        // 数据正好在最后一个 box 结束处用完
        check_all_splits(|| recover(&sample), &sample, Some(sample.len() as u64));
    }

    #[test]
    fn large_size_boxes_are_followed() {
        let sample = samples::mp4(3000, true);
        let mut followed = sample.clone();
        followed.extend_from_slice(&[0u8; 32]);
        check_all_splits(|| recover(&followed), &followed, Some(sample.len() as u64));
    }

    #[test]
    fn missing_moov_or_mdat_is_rejected() {
        let mut only_ftyp = samples::mp4(100, false);
        only_ftyp.truncate(24);
        only_ftyp.extend_from_slice(&[0u8; 16]);
        check_all_splits(|| recover(&only_ftyp), &only_ftyp, None);
    }
}
