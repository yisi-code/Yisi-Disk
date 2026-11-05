//! 标准 MIDI 文件（Standard MIDI Files 1.0）：头块 `MThd`（长度 6：格式 0 / 1 / 2、音轨数、时间单位），
//! 之后是音轨块 `MTrk`；块是 4 字节类型、4 字节大端长度、内容。按块往后走，数到头块说的音轨数就是文件结尾，
//! 中间可以夹着不认识的块（规范要求读者跳过）。每个音轨以「音轨结束」元事件 `FF 2F 00` 收尾，
//! 用它确认长度没算错、没有认错。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::{be16, be32};

/// 头块：`MThd` 与长度 6。
const HEADER: &[u8] = b"MThd\0\0\0\x06";
const HEADER_LEN: u64 = 14;
const CHUNK_HEAD: usize = 8;
const END_OF_TRACK: &[u8] = &[0xFF, 0x2F, 0x00];
/// 最多跳过这么多个不认识的块。
const MAX_ALIEN_CHUNKS: u32 = 16;

pub struct Midi;

static INFO: FormatInfo = FormatInfo {
    extension: "mid",
    variants: &[],
    description: "MIDI music",
    max_size: 64 << 20,
    enabled_by_default: true,
    nests_files: false,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: HEADER,
}];

/// 头块里的音轨数，格式与时间单位都合规时才给出。
fn track_count(block: &[u8]) -> Option<u16> {
    let format = be16(block, 8)?;
    let tracks = be16(block, 10)?;
    let division = be16(block, 12)?;
    let division_ok = if division & 0x8000 != 0 {
        // SMPTE 时间码：高字节是负的每秒帧数
        matches!((division >> 8) as u8 as i8, -24 | -25 | -29 | -30)
    } else {
        division > 0
    };
    let tracks_ok = match format {
        0 => tracks == 1,
        1 | 2 => tracks >= 1,
        _ => false,
    };
    (division_ok && tracks_ok).then_some(tracks)
}

impl FileFormat for Midi {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let tracks = track_count(block)?;
        Some(Box::new(Walker::new(
            INFO.max_size,
            MidiStructure {
                tracks_left: tracks,
                aliens: 0,
                track_end: None,
            },
        )))
    }
}

struct MidiStructure {
    tracks_left: u16,
    aliens: u32,
    /// 正在核对结尾的音轨块在哪里结束。
    track_end: Option<u64>,
}

impl Structure for MidiStructure {
    fn first(&self) -> Step {
        Step::Need {
            at: HEADER_LEN,
            len: CHUNK_HEAD,
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        if let Some(end) = self.track_end.take() {
            if bytes != END_OF_TRACK {
                return Step::Invalid;
            }
            self.tracks_left -= 1;
            return if self.tracks_left == 0 {
                Step::Done(end)
            } else {
                Step::Need {
                    at: end,
                    len: CHUNK_HEAD,
                }
            };
        }
        let (Some(kind), Some(size)) = (bytes.get(..4), be32(bytes, 4)) else {
            return Step::Invalid;
        };
        let end = at + CHUNK_HEAD as u64 + u64::from(size);
        if kind == b"MTrk" {
            // 最短的音轨：一个时间差字节加 FF 2F 00
            if size < 4 {
                return Step::Invalid;
            }
            self.track_end = Some(end);
            return Step::Need {
                at: end - END_OF_TRACK.len() as u64,
                len: END_OF_TRACK.len(),
            };
        }
        if !kind.iter().all(u8::is_ascii_alphanumeric) || self.aliens >= MAX_ALIEN_CHUNKS {
            return Step::Invalid;
        }
        self.aliens += 1;
        Step::Need {
            at: end,
            len: CHUNK_HEAD,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Midi.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn length_is_the_end_of_the_last_track() {
        for (tracks, alien) in [(&[40usize][..], false), (&[10, 300, 7], true)] {
            let sample = samples::midi(tracks, alien);
            let mut data = sample.clone();
            data.extend_from_slice(b"MTrk\0\0\0\x04\0\xFF\x2F\0 more");
            check(&data, Some(sample.len() as u64));
            check(&sample[..sample.len() - 1], None);
        }
    }

    #[test]
    fn broken_tracks_or_headers_are_rejected() {
        let sample = samples::midi(&[30, 30], false);
        // 第一个音轨没有以 FF 2F 00 结尾
        let mut no_end = sample.clone();
        no_end[14 + 8 + 30 + 1] = 0x00;
        check(&no_end, None);
        let changed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            Midi.header_check(&data).is_none()
        };
        assert!(changed(8, &[0, 3]), "格式只有 0、1、2");
        assert!(changed(8, &[0, 0]), "格式 0 只能有一个音轨");
        assert!(changed(12, &[0, 0]), "时间单位为 0");
        assert!(changed(12, &[0xE5, 0x28]), "SMPTE 帧数不对");
    }
}
