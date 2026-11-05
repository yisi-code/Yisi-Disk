//! MP3（MPEG-1 / 2 / 2.5 音频，ISO/IEC 11172-3、13818-3）：只认以 ID3v2 标签开头的文件——
//! 没有标签的 MP3 只能靠 11 位帧同步认，太容易认错。
//!
//! ID3v2 头 10 字节（`ID3`、版本、标志、4 字节同步安全的长度），有页脚时再加 10 字节；之后是一帧帧音频。
//! 帧头 4 字节，从版本、层、码率、采样率、填充位算出这一帧的长度，一帧接一帧往下走；
//! 版本、层、采样率与第一帧不同、或不是帧头时音频结束，后面是 128 字节的 ID3v1 标签（`TAG`）就一并算上。
//! 标签之后有一段 0 填充的，往后找第一个帧同步。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Scanner, Step, Structure, Walker};

const ID3V2_HEADER: u64 = 10;
const ID3V2_FOOTER_FLAG: u8 = 0x10;
const ID3V1_LEN: u64 = 128;
/// 至少这么多帧才算一个音频文件。
const MIN_FRAMES: u32 = 4;

/// 码率（kbps），按 [MPEG-1 / MPEG-2 与 2.5][层 1、2、3][码率序号]。
const BITRATES: [[[u16; 15]; 3]; 2] = [
    [
        [
            0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ],
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
        ],
        [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ],
    ],
    [
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
        ],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ],
];
/// 采样率，按 [MPEG-1、MPEG-2、MPEG-2.5][序号]。
const SAMPLE_RATES: [[u32; 3]; 3] = [
    [44100, 48000, 32000],
    [22050, 24000, 16000],
    [11025, 12000, 8000],
];

/// 标签后面的 0 填充里找帧同步。
const FRAME_SYNC: Scanner = Scanner {
    find: |window| {
        window
            .windows(2)
            .position(|pair| matches!(pair, [0xFF, next] if next & 0xE0 == 0xE0))
    },
    overlap: 1,
};

pub struct Mp3;

static INFO: FormatInfo = FormatInfo {
    extension: "mp3",
    variants: &[],
    description: "MP3 audio",
    max_size: 1 << 30,
    enabled_by_default: true,
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"ID3",
}];

/// 一个帧头：（版本、层、采样率序号）用来比较前后帧是否一致，以及这一帧的长度。
pub(crate) fn frame(header: &[u8]) -> Option<((u8, u8, u8), u64)> {
    let [sync, b1, b2, _] = *header.get(..4)? else {
        return None;
    };
    if sync != 0xFF || b1 & 0xE0 != 0xE0 {
        return None;
    }
    let version = (b1 >> 3) & 0x03; // 0 MPEG-2.5，1 保留，2 MPEG-2，3 MPEG-1
    let layer = (b1 >> 1) & 0x03; // 1 层 3，2 层 2，3 层 1，0 保留
    let bitrate_index = usize::from(b2 >> 4);
    let rate_index = (b2 >> 2) & 0x03;
    let padding = u64::from((b2 >> 1) & 0x01);
    if version == 1 || layer == 0 || bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
        return None;
    }
    let mpeg1 = version == 3;
    let layer_index = usize::from(3 - layer); // 0 层 1，1 层 2，2 层 3
    let bitrate = u64::from(
        *BITRATES
            .get(usize::from(!mpeg1))?
            .get(layer_index)?
            .get(bitrate_index)?,
    ) * 1000;
    let rate_row = match version {
        3 => 0,
        2 => 1,
        _ => 2,
    };
    let rate = u64::from(*SAMPLE_RATES.get(rate_row)?.get(usize::from(rate_index))?);
    let length = match layer_index {
        0 => (12 * bitrate / rate + padding) * 4,
        2 if !mpeg1 => 72 * bitrate / rate + padding,
        _ => 144 * bitrate / rate + padding,
    };
    (length >= 4).then_some(((version, layer, rate_index), length))
}

impl FileFormat for Mp3 {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let major = *block.get(3)?;
        let flags = *block.get(5)?;
        let size = block.get(6..10)?;
        if !(2..=4).contains(&major) || size.iter().any(|&byte| byte & 0x80 != 0) {
            return None;
        }
        let tag_len = size
            .iter()
            .fold(0u64, |total, &byte| (total << 7) | u64::from(byte));
        let footer = if flags & ID3V2_FOOTER_FLAG != 0 {
            10
        } else {
            0
        };
        Some(Box::new(Walker::new(
            INFO.max_size,
            Mp3Structure {
                first_frame: ID3V2_HEADER + tag_len + footer,
                kind: None,
                frames: 0,
                phase: Phase::AfterTag,
            },
        )))
    }
}

#[derive(Clone, Copy)]
enum Phase {
    /// 等标签之后的 4 字节：帧头，或 0 填充。
    AfterTag,
    /// 等下一个帧头。
    Frame,
    /// 音频结束处，看是不是 ID3v1 标签。
    Trailer,
}

struct Mp3Structure {
    first_frame: u64,
    kind: Option<(u8, u8, u8)>,
    frames: u32,
    phase: Phase,
}

impl Mp3Structure {
    fn enough(&self) -> bool {
        self.frames >= MIN_FRAMES
    }

    /// 在 `at` 读到一个帧头：接着读下一帧；不是帧头（或与前面的帧不一致）时音频到此为止。
    fn frame_at(&mut self, at: u64, bytes: &[u8]) -> Step {
        match frame(bytes) {
            Some((kind, length)) if self.kind.is_none_or(|first| first == kind) => {
                self.kind = Some(kind);
                self.frames += 1;
                self.phase = Phase::Frame;
                Step::Need {
                    at: at + length,
                    len: 4,
                }
            }
            _ if self.enough() => {
                self.phase = Phase::Trailer;
                Step::Need { at, len: 3 }
            }
            _ => Step::Invalid,
        }
    }
}

impl Structure for Mp3Structure {
    fn first(&self) -> Step {
        Step::Need {
            at: self.first_frame,
            len: 4,
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let (at, bytes) = match event {
            Event::Found { at } => {
                // 找到帧同步：从这里读帧头
                self.phase = Phase::Frame;
                return Step::Need { at, len: 4 };
            }
            Event::Bytes { at, bytes } => (at, bytes),
        };
        match self.phase {
            Phase::AfterTag if bytes == [0, 0, 0, 0] => Step::Scan {
                from: at,
                scanner: FRAME_SYNC,
            },
            Phase::AfterTag | Phase::Frame => self.frame_at(at, bytes),
            Phase::Trailer => {
                if bytes == b"TAG" {
                    Step::Done(at + ID3V1_LEN)
                } else {
                    Step::Done(at)
                }
            }
        }
    }

    /// 数据在帧与帧之间、或在音频末尾用完（设备末尾）：帧够多就到这里。
    fn finish(&self, received: u64, waiting_at: Option<u64>) -> Option<u64> {
        (matches!(self.phase, Phase::Frame | Phase::Trailer)
            && waiting_at == Some(received)
            && self.enough())
        .then_some(received)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Mp3.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn frames_are_followed_to_the_end_with_or_without_id3v1() {
        for (version, layer, id3v1, padding) in [
            (3, 1, false, 0), // MPEG-1 层 3
            (3, 1, true, 0),
            (2, 1, false, 0),   // MPEG-2 层 3
            (3, 2, true, 0),    // MPEG-1 层 2
            (3, 1, false, 300), // 标签后面有 0 填充
        ] {
            let sample = samples::mp3(version, layer, 30, id3v1, padding);
            let mut data = sample.clone();
            data.extend_from_slice(b"ID3 and not a frame");
            check(&data, Some(sample.len() as u64));
            check(&sample, Some(sample.len() as u64));
        }
    }

    #[test]
    fn too_few_frames_or_broken_tags_are_rejected() {
        let short = samples::mp3(3, 1, 2, false, 0);
        let mut data = short.clone();
        data.extend_from_slice(&[0x11; 64]);
        check(&data, None);

        let mut bad_size = samples::mp3(3, 1, 30, false, 0);
        bad_size[6] = 0x80;
        assert!(Mp3.header_check(&bad_size).is_none());
    }

    #[test]
    fn frame_lengths_follow_the_standard() {
        // MPEG-1 层 3、128 kbps、44.1 kHz：417 字节，有填充位时 418
        assert_eq!(
            frame(&[0xFF, 0xFB, 0x90, 0x00]).map(|(_, length)| length),
            Some(417)
        );
        assert_eq!(
            frame(&[0xFF, 0xFB, 0x92, 0x00]).map(|(_, length)| length),
            Some(418)
        );
        // MPEG-2 层 3、64 kbps、22.05 kHz：208 字节
        assert_eq!(
            frame(&[0xFF, 0xF3, 0x80, 0x00]).map(|(_, length)| length),
            Some(208)
        );
        // 码率序号 15、采样率序号 3 都是保留值
        assert!(frame(&[0xFF, 0xFB, 0xF0, 0x00]).is_none());
        assert!(frame(&[0xFF, 0xFB, 0x9C, 0x00]).is_none());
    }
}
