//! JPEG：SOI 之后是一串标记段；SOS 之后是熵编码数据，其中的 0xFF 后面跟 0x00（填充）或 RST 标记，
//! 遇到其他标记时熵编码数据结束；以 EOI（FF D9）结束。缩略图嵌在 APP 段里，随段长度整段跳过。

use crate::bytes::be16;
use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Scanner, Step, Structure, Walker};

const SOI: &[u8] = b"\xFF\xD8\xFF";
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;

/// SOI 之后第一个标记的常见取值：APPn、DQT、SOFn、DHT、DRI、COM。
const FIRST_MARKERS: [u8; 20] = [
    0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xEB, 0xEC, 0xED, 0xEE, 0xEF,
    0xDB, 0xC0, 0xC4, 0xFE,
];

/// 熵编码数据里，0xFF 后面不是 0x00、不是 RST0–RST7、也不是填充用的 0xFF 时，就是下一个标记。
const ENTROPY_END: Scanner = Scanner {
    find: |window| {
        window.windows(2).position(|pair| {
            matches!(pair, [0xFF, next] if *next != 0x00 && *next != 0xFF && !(0xD0..=0xD7).contains(next))
        })
    },
    overlap: 1,
};

pub struct Jpg;

static INFO: FormatInfo = FormatInfo {
    extension: "jpg",
    description: "JPEG image",
    max_size: 256 << 20,
    enabled_by_default: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: SOI,
}];

impl FileFormat for Jpg {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let marker = *block.get(3)?;
        (block.starts_with(SOI) && FIRST_MARKERS.contains(&marker)).then(|| {
            Box::new(Walker::new(JpgStructure {
                phase: Phase::Marker,
                seen_scan: false,
            })) as Box<dyn Recovering>
        })
    }
}

#[derive(Clone, Copy)]
enum Phase {
    /// 等两个字节的标记。
    Marker,
    /// 等标记段的两字节长度。
    Length { marker: u8 },
}

struct JpgStructure {
    phase: Phase,
    seen_scan: bool,
}

impl Structure for JpgStructure {
    fn first(&self) -> Step {
        Step::Need { at: 2, len: 2 }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let (at, bytes) = match event {
            // 熵编码数据结束于一个标记
            Event::Found { at } => {
                self.phase = Phase::Marker;
                return Step::Need { at, len: 2 };
            }
            Event::Bytes { at, bytes } => (at, bytes),
        };
        match self.phase {
            Phase::Marker => {
                let (Some(&0xFF), Some(&marker)) = (bytes.first(), bytes.get(1)) else {
                    return Step::Invalid;
                };
                match marker {
                    // 填充字节：标记前可以有任意多个 0xFF
                    0xFF => Step::Need { at: at + 1, len: 2 },
                    EOI if self.seen_scan => Step::Done(at + 2),
                    // 没有图像数据就结束，或在顶层又出现 SOI：不是有效文件
                    EOI | 0xD8 | 0x00 => Step::Invalid,
                    // 不带长度的标记
                    0x01 | 0xD0..=0xD7 => Step::Need { at: at + 2, len: 2 },
                    _ => {
                        self.phase = Phase::Length { marker };
                        Step::Need { at: at + 2, len: 2 }
                    }
                }
            }
            Phase::Length { marker } => {
                let Some(length) = be16(bytes, 0).filter(|&length| length >= 2) else {
                    return Step::Invalid;
                };
                let segment_end = at + u64::from(length);
                if marker == SOS {
                    self.seen_scan = true;
                    Step::Scan {
                        from: segment_end,
                        scanner: ENTROPY_END,
                    }
                } else {
                    self.phase = Phase::Marker;
                    Step::Need {
                        at: segment_end,
                        len: 2,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn recover(data: &[u8]) -> Box<dyn Recovering> {
        Jpg.header_check(data).unwrap()
    }

    #[test]
    fn skips_thumbnail_stuffing_and_restart_markers() {
        let sample = samples::jpg(4000);
        let mut data = sample.clone();
        data.extend_from_slice(b"\xFF\xD9 trailing bytes after EOI");
        check_all_splits(|| recover(&data), &data, Some(sample.len() as u64));
    }

    #[test]
    fn progressive_scans_continue_after_entropy_data() {
        let sample = samples::jpg_progressive(1500);
        check_all_splits(|| recover(&sample), &sample, Some(sample.len() as u64));
    }

    #[test]
    fn missing_eoi_or_scan_is_incomplete() {
        let sample = samples::jpg(500);
        let truncated = &sample[..sample.len() - 2];
        check_all_splits(|| recover(truncated), truncated, None);

        let no_scan = b"\xFF\xD8\xFF\xE0\x00\x04ab\xFF\xD9";
        check_all_splits(|| recover(no_scan), no_scan, None);
    }
}
