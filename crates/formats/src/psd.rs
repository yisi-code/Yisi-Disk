//! Photoshop 文档（Adobe Photoshop 文件格式说明）：PSD 与大文档 PSB。26 字节文件头（`8BPS`、版本 1 / 2、
//! 6 字节保留 0、通道数、高、宽、每通道位数、颜色模式），之后依次是四段：颜色模式数据、图像资源、
//! 图层与蒙版信息（各以 4 字节长度开头，PSB 的图层段是 8 字节），最后是合并后的图像数据：
//! 2 字节压缩方式，不压缩时是通道数 × 高 × 每行字节数；RLE（PackBits）时先是每行压缩后的字节数
//! （PSD 每项 2 字节、PSB 4 字节，共通道数 × 高项），之后是各行数据，总长度是这些字节数之和。
//! 图像数据是 ZIP 压缩的（很少见）算不出结尾，不认。
//!
//! 其余各项只用来排除误识别：保留字节为 0、各项取值在规定范围内、图像资源以 `8BIM` 开头、
//! 每行的 RLE 字节数不超过 PackBits 最坏情况下的长度。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::{be16, be32, be64};

const HEADER_LEN: u64 = 26;
const COMPRESSION_RAW: u16 = 0;
const COMPRESSION_RLE: u16 = 1;
/// RLE 的行字节数表一次最多读这么多项。
const COUNTS_PER_READ: u64 = 16 * 1024;
const DEPTHS: [u16; 4] = [1, 8, 16, 32];
/// 位图、灰度、索引、RGB、CMYK、多通道、双色调、Lab。
const COLOR_MODES: [u16; 8] = [0, 1, 2, 3, 4, 7, 8, 9];
const MAX_CHANNELS: u16 = 56;

pub struct Psd;

static INFO: FormatInfo = FormatInfo {
    extension: "psd",
    variants: &["psb"],
    description: "Photoshop document",
    max_size: 64 << 30,
    enabled_by_default: true,
    nests_files: false,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"8BPS",
}];

/// 合并图像的几何：通道数、行数（通道数 × 高）、每行字节数；是否 PSB。
#[derive(Clone, Copy)]
struct Geometry {
    big: bool,
    rows: u64,
    row_bytes: u64,
}

fn geometry(block: &[u8]) -> Option<Geometry> {
    let version = be16(block, 4)?;
    let channels = be16(block, 12)?;
    let height = be32(block, 14)?;
    let width = be32(block, 18)?;
    let depth = be16(block, 22)?;
    let mode = be16(block, 24)?;
    let max_side = match version {
        1 => 30_000,
        2 => 300_000,
        _ => return None,
    };
    let plausible = block.get(6..12) == Some([0; 6].as_slice())
        && (1..=MAX_CHANNELS).contains(&channels)
        && (1..=max_side).contains(&height)
        && (1..=max_side).contains(&width)
        && DEPTHS.contains(&depth)
        && COLOR_MODES.contains(&mode);
    plausible.then(|| Geometry {
        big: version == 2,
        rows: u64::from(channels) * u64::from(height),
        row_bytes: (u64::from(width) * u64::from(depth)).div_ceil(8),
    })
}

impl FileFormat for Psd {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let geometry = geometry(block)?;
        Some(Box::new(Walker::new(
            INFO.max_size,
            PsdStructure {
                geometry,
                phase: Phase::ColorMode,
            },
        )))
    }
}

#[derive(Clone, Copy)]
enum Phase {
    /// 等颜色模式数据段的长度。
    ColorMode,
    /// 等图像资源段的长度。
    Resources,
    /// 等图像资源的第一个 `8BIM`；图层段在 `layers_at`。
    ResourceSignature { layers_at: u64 },
    /// 等图层与蒙版信息段的长度。
    Layers,
    /// 等图像数据的压缩方式。
    Compression,
    /// 读 RLE 的行字节数表：还剩 `left` 项，表之后的数据已累计 `total` 字节，表在 `table_end` 结束。
    Counts {
        left: u64,
        total: u64,
        table_end: u64,
    },
}

struct PsdStructure {
    geometry: Geometry,
    phase: Phase,
}

impl PsdStructure {
    fn count_len(&self) -> u64 {
        if self.geometry.big { 4 } else { 2 }
    }

    /// 接着读行字节数表的下一段，读完了就到文件结尾。
    fn next_counts(&mut self, at: u64, left: u64, total: u64, table_end: u64) -> Step {
        if left == 0 {
            return match table_end.checked_add(total) {
                Some(end) => Step::Done(end),
                None => Step::Invalid,
            };
        }
        self.phase = Phase::Counts {
            left,
            total,
            table_end,
        };
        Step::Need {
            at,
            len: (left.min(COUNTS_PER_READ) * self.count_len()) as usize,
        }
    }
}

impl Structure for PsdStructure {
    fn first(&self) -> Step {
        Step::Need {
            at: HEADER_LEN,
            len: 4,
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        let length = |bytes: &[u8]| be32(bytes, 0).map(u64::from);
        match self.phase {
            Phase::ColorMode => {
                let Some(len) = length(bytes) else {
                    return Step::Invalid;
                };
                self.phase = Phase::Resources;
                Step::Need {
                    at: at + 4 + len,
                    len: 4,
                }
            }
            Phase::Resources => {
                let Some(len) = length(bytes) else {
                    return Step::Invalid;
                };
                let layers_at = at + 4 + len;
                if len == 0 {
                    self.phase = Phase::Layers;
                    return Step::Need {
                        at: layers_at,
                        len: if self.geometry.big { 8 } else { 4 },
                    };
                }
                self.phase = Phase::ResourceSignature { layers_at };
                Step::Need { at: at + 4, len: 4 }
            }
            Phase::ResourceSignature { layers_at } => {
                if bytes != b"8BIM" {
                    return Step::Invalid;
                }
                self.phase = Phase::Layers;
                Step::Need {
                    at: layers_at,
                    len: if self.geometry.big { 8 } else { 4 },
                }
            }
            Phase::Layers => {
                let len = if self.geometry.big {
                    be64(bytes, 0)
                } else {
                    length(bytes)
                };
                let Some(image_at) = len.and_then(|len| (at + bytes.len() as u64).checked_add(len))
                else {
                    return Step::Invalid;
                };
                self.phase = Phase::Compression;
                Step::Need {
                    at: image_at,
                    len: 2,
                }
            }
            Phase::Compression => {
                let Geometry {
                    rows, row_bytes, ..
                } = self.geometry;
                let data_at = at + 2;
                match be16(bytes, 0) {
                    Some(COMPRESSION_RAW) => {
                        match rows
                            .checked_mul(row_bytes)
                            .and_then(|len| data_at.checked_add(len))
                        {
                            Some(end) => Step::Done(end),
                            None => Step::Invalid,
                        }
                    }
                    Some(COMPRESSION_RLE) => {
                        let table_end = data_at + rows * self.count_len();
                        self.next_counts(data_at, rows, 0, table_end)
                    }
                    _ => Step::Invalid,
                }
            }
            Phase::Counts {
                left,
                total,
                table_end,
            } => {
                let row_bytes = self.geometry.row_bytes;
                // PackBits 最坏的情况：每 128 字节多一个长度字节
                let worst = row_bytes + row_bytes.div_ceil(128);
                let mut sum = total;
                for count in bytes.chunks_exact(self.count_len() as usize) {
                    let value = count
                        .iter()
                        .fold(0u64, |value, &byte| (value << 8) | u64::from(byte));
                    if value > worst {
                        return Step::Invalid;
                    }
                    sum += value;
                }
                let read = bytes.len() as u64 / self.count_len();
                self.next_counts(at + bytes.len() as u64, left - read, sum, table_end)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples::{self, PsdLayout};
    use crate::walker::tests::check_all_splits;

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Psd.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn length_is_the_end_of_the_merged_image() {
        let layouts = [
            PsdLayout::default(),
            PsdLayout {
                rle: true,
                ..PsdLayout::default()
            },
            PsdLayout {
                big: true,
                rle: true,
                channels: 4,
                ..PsdLayout::default()
            },
            PsdLayout {
                big: true,
                resources: false,
                ..PsdLayout::default()
            },
        ];
        for layout in layouts {
            let sample = samples::psd(&layout);
            let mut data = sample.clone();
            data.extend_from_slice(b"8BPS and something else");
            check(&data, Some(sample.len() as u64));
            check(&sample[..sample.len() - 1], None);
        }
    }

    #[test]
    fn implausible_headers_and_sections_are_rejected() {
        let sample = samples::psd(&PsdLayout {
            rle: true,
            ..PsdLayout::default()
        });
        let changed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            data
        };
        assert!(Psd.header_check(&changed(4, &[0, 3])).is_none(), "版本");
        assert!(Psd.header_check(&changed(8, &[1])).is_none(), "保留字节");
        assert!(Psd.header_check(&changed(22, &[0, 7])).is_none(), "位数");
        assert!(
            Psd.header_check(&changed(24, &[0, 5])).is_none(),
            "颜色模式"
        );
        // 图像资源不以 8BIM 开头
        check(&changed(26 + 4 + 4, b"8BIX"), None);
        // ZIP 压缩的图像数据
        let compression_at = samples::psd_image_at(&sample);
        check(&changed(compression_at, &[0, 2]), None);
        // 一行的 RLE 字节数比最坏情况还长
        check(&changed(compression_at + 2, &[0x7F, 0xFF]), None);
    }
}
