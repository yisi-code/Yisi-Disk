//! ASF（Advanced Systems Format 规范 1.20，WMV / WMA 的容器）：文件由对象组成，对象是 16 字节 GUID、
//! 8 字节小端长度（含这 24 字节）、内容。第一个是头对象（30 字节：GUID、长度、子对象个数、保留字节 1 与 2），
//! 里面的「文件属性」对象记着整个文件的长度（第 40 字节起，广播标志置位时这个值无效，不认）；
//! 头对象之后紧跟着数据对象。按子对象逐个往后读到文件属性，核对数据对象在文件长度之内，文件就到那个长度。
//!
//! 「流属性」对象里的流类型说明有没有视频：有视频的存成 wmv，只有音频的存成 wma。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::{le32, le64};

/// GUID 按 ASF 的存法（前三段小端）写成字节。
pub(crate) const HEADER_OBJECT: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];
pub(crate) const DATA_OBJECT: [u8; 16] = [
    0x36, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];
pub(crate) const FILE_PROPERTIES: [u8; 16] = [
    0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65,
];
pub(crate) const STREAM_PROPERTIES: [u8; 16] = [
    0x91, 0x07, 0xDC, 0xB7, 0xB7, 0xA9, 0xCF, 0x11, 0x8E, 0xE6, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65,
];
pub(crate) const AUDIO_MEDIA: [u8; 16] = [
    0x40, 0x9E, 0x69, 0xF8, 0x4D, 0x5B, 0xCF, 0x11, 0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B,
];
pub(crate) const VIDEO_MEDIA: [u8; 16] = [
    0xC0, 0xEF, 0x19, 0xBC, 0x4D, 0x5B, 0xCF, 0x11, 0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B,
];

const HEADER_OBJECT_LEN: u64 = 30;
const OBJECT_HEAD: usize = 24;
const FILE_PROPERTIES_LEN: usize = 104;
/// 流属性对象读到流类型为止。
const STREAM_TYPE_END: usize = 40;
/// 数据对象自己的头：GUID、长度、文件 ID、包数、保留。
const DATA_OBJECT_HEAD: u64 = 50;
const BROADCAST_FLAG: u32 = 0x01;
/// 头对象最多这么长（带封面图片的也远小于此）。
const MAX_HEADER: u64 = 64 << 20;

pub struct Asf;

static INFO: FormatInfo = FormatInfo {
    extension: "asf",
    variants: &["wmv", "wma"],
    description: "Windows Media video / audio",
    max_size: 64 << 30,
    enabled_by_default: true,
    // 头对象的元数据里可能带着封面图片
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: &HEADER_OBJECT,
}];

impl FileFormat for Asf {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let header_end = le64(block, 16)?;
        let objects = le32(block, 24)?;
        let plausible = (HEADER_OBJECT_LEN + OBJECT_HEAD as u64..=MAX_HEADER).contains(&header_end)
            && objects >= 1
            && block.get(28..30) == Some([1, 2].as_slice());
        plausible.then(|| {
            Box::new(Walker::new(
                INFO.max_size,
                AsfStructure {
                    header_end,
                    objects_left: objects,
                    phase: Phase::Object,
                    file_size: None,
                    video: false,
                    audio: false,
                },
            )) as Box<dyn Recovering>
        })
    }
}

#[derive(Clone, Copy)]
enum Phase {
    /// 等一个子对象的 GUID 与长度。
    Object,
    /// 等文件属性对象的内容；`next` 是下一个子对象的位置。
    FileProperties {
        next: u64,
    },
    StreamProperties {
        next: u64,
    },
    /// 等头对象之后的数据对象。
    Data,
}

struct AsfStructure {
    header_end: u64,
    objects_left: u32,
    phase: Phase,
    file_size: Option<u64>,
    video: bool,
    audio: bool,
}

impl AsfStructure {
    /// 下一个子对象在 `at`；子对象正好排到头对象结尾时，接着看数据对象。
    fn next_object(&mut self, at: u64) -> Step {
        if at == self.header_end {
            self.phase = Phase::Data;
        } else if at > self.header_end || self.objects_left == 0 {
            return Step::Invalid;
        } else {
            self.phase = Phase::Object;
        }
        Step::Need {
            at,
            len: OBJECT_HEAD,
        }
    }
}

impl Structure for AsfStructure {
    fn first(&self) -> Step {
        Step::Need {
            at: HEADER_OBJECT_LEN,
            len: OBJECT_HEAD,
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        match self.phase {
            Phase::Object => {
                let (Some(guid), Some(size)) = (bytes.get(..16), le64(bytes, 16)) else {
                    return Step::Invalid;
                };
                let Some(next) = at.checked_add(size) else {
                    return Step::Invalid;
                };
                if size < OBJECT_HEAD as u64 || next > self.header_end {
                    return Step::Invalid;
                }
                self.objects_left -= 1;
                if guid == FILE_PROPERTIES {
                    if size < FILE_PROPERTIES_LEN as u64 {
                        return Step::Invalid;
                    }
                    self.phase = Phase::FileProperties { next };
                    Step::Need {
                        at,
                        len: FILE_PROPERTIES_LEN,
                    }
                } else if guid == STREAM_PROPERTIES {
                    if size < STREAM_TYPE_END as u64 {
                        return Step::Invalid;
                    }
                    self.phase = Phase::StreamProperties { next };
                    Step::Need {
                        at,
                        len: STREAM_TYPE_END,
                    }
                } else {
                    self.next_object(next)
                }
            }
            Phase::FileProperties { next } => {
                let (Some(file_size), Some(flags)) = (le64(bytes, 40), le32(bytes, 88)) else {
                    return Step::Invalid;
                };
                if flags & BROADCAST_FLAG != 0 || self.file_size.is_some() {
                    return Step::Invalid;
                }
                self.file_size = Some(file_size);
                self.next_object(next)
            }
            Phase::StreamProperties { next } => {
                match bytes.get(24..40) {
                    Some(kind) if kind == VIDEO_MEDIA => self.video = true,
                    Some(kind) if kind == AUDIO_MEDIA => self.audio = true,
                    _ => {}
                }
                self.next_object(next)
            }
            Phase::Data => {
                let (Some(file_size), Some(size)) = (self.file_size, le64(bytes, 16)) else {
                    return Step::Invalid;
                };
                let data_end = at.checked_add(size);
                if bytes.get(..16) != Some(DATA_OBJECT.as_slice())
                    || size < DATA_OBJECT_HEAD
                    || data_end.is_none_or(|end| end > file_size)
                {
                    return Step::Invalid;
                }
                Step::Done(file_size)
            }
        }
    }

    fn extension(&self) -> Option<&'static str> {
        if self.video {
            Some("wmv")
        } else if self.audio {
            Some("wma")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::{check_all_splits, run_keeping};

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Asf.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn length_comes_from_the_file_properties_object() {
        for (video, extension) in [(true, Some("wmv")), (false, Some("wma"))] {
            let sample = samples::asf(video, 3000);
            let mut data = sample.clone();
            data.extend_from_slice(&HEADER_OBJECT);
            check(&data, Some(sample.len() as u64));
            check(&sample[..sample.len() - 1], None);
            let mut recovering = Asf.header_check(&data).unwrap();
            run_keeping(&mut recovering, &data, 100);
            assert_eq!(recovering.extension(), extension);
        }
    }

    #[test]
    fn broadcast_files_and_broken_objects_are_rejected() {
        let sample = samples::asf(true, 500);
        let at = samples::ASF_FILE_PROPERTIES_AT;
        let mut broadcast = sample.clone();
        broadcast[at + 88] |= 1;
        check(&broadcast, None);
        // 文件长度装不下数据对象
        let mut short = sample.clone();
        short[at + 40..at + 48].copy_from_slice(&100u64.to_le_bytes());
        check(&short, None);
        // 子对象长度越出头对象
        let mut overflow = sample.clone();
        overflow[at + 16..at + 24].copy_from_slice(&100_000u64.to_le_bytes());
        check(&overflow, None);
        let mut reserved = sample.clone();
        reserved[29] = 0;
        assert!(Asf.header_check(&reserved).is_none());
    }
}
