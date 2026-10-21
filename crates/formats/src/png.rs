//! PNG：8 字节签名之后是一串块（长度、类型、数据、CRC），以 IEND 块结束。

use crate::bytes::be32;
use crate::crc32::crc32;
use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};

const MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
/// 不超过这个长度的块读入整块校验 CRC；更大的块（通常是图像数据）只按长度跳过。
const CRC_CHECK_LIMIT: u32 = 64 * 1024;

pub struct Png;

static INFO: FormatInfo = FormatInfo {
    extension: "png",
    description: "Portable Network Graphics image",
    max_size: 256 << 20,
    enabled_by_default: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: MAGIC,
}];

impl FileFormat for Png {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        // 第一个块必须是 13 字节的 IHDR
        let is_png = block.starts_with(MAGIC)
            && be32(block, 8) == Some(13)
            && block.get(12..16) == Some(b"IHDR".as_slice());
        is_png.then(|| Box::new(Walker::new(PngStructure::Header)) as Box<dyn Recovering>)
    }
}

enum PngStructure {
    /// 等块头：4 字节长度 + 4 字节类型。
    Header,
    /// 等块数据与 CRC，用来校验。
    Body { kind: [u8; 4], length: u32 },
}

impl Structure for PngStructure {
    fn first(&self) -> Step {
        Step::Need { at: 8, len: 8 }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        match *self {
            PngStructure::Header => {
                let (Some(length), Some(kind)) = (be32(bytes, 0), bytes.get(4..8)) else {
                    return Step::Invalid;
                };
                let Ok(kind) = <[u8; 4]>::try_from(kind) else {
                    return Step::Invalid;
                };
                // 规范要求长度不超过 2^31-1，类型是四个 ASCII 字母
                if length > 0x7FFF_FFFF || !kind.iter().all(u8::is_ascii_alphabetic) {
                    return Step::Invalid;
                }
                let data_start = at + 8;
                if length <= CRC_CHECK_LIMIT {
                    *self = PngStructure::Body { kind, length };
                    Step::Need {
                        at: data_start,
                        len: length as usize + 4,
                    }
                } else {
                    Step::Need {
                        at: data_start + u64::from(length) + 4,
                        len: 8,
                    }
                }
            }
            PngStructure::Body { kind, length } => {
                let data = bytes.get(..length as usize).unwrap_or_default();
                if be32(bytes, length as usize) != Some(crc32(&[&kind, data])) {
                    return Step::Invalid;
                }
                let next = at + u64::from(length) + 4;
                if &kind == b"IEND" {
                    return Step::Done(next);
                }
                *self = PngStructure::Header;
                Step::Need { at: next, len: 8 }
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
        Png.header_check(data).unwrap()
    }

    #[test]
    fn finds_the_end_with_small_and_large_chunks() {
        let sample = samples::png(&[10, 70_000, 5]);
        let mut data = sample.clone();
        data.extend_from_slice(b"trailing garbage after IEND");
        check_all_splits(|| recover(&data), &data, Some(sample.len() as u64));
    }

    #[test]
    fn corrupted_crc_or_truncation_is_rejected() {
        let mut corrupted = samples::png(&[10]);
        let crc_byte = corrupted.len() - 1 - 12; // IDAT 的 CRC 最后一字节
        corrupted[crc_byte] ^= 0xFF;
        check_all_splits(|| recover(&corrupted), &corrupted, None);

        let sample = samples::png(&[10]);
        let truncated = &sample[..sample.len() - 4];
        check_all_splits(|| recover(truncated), truncated, None);
    }

    #[test]
    fn header_requires_ihdr() {
        let mut data = samples::png(&[1]);
        data[12] = b'X';
        assert!(Png.header_check(&data).is_none());
    }
}
