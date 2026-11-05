//! RAR（RarLab 公开的格式说明）。两种版本：
//! - RAR 5：签名 `Rar!\x1A\x07\x01\x00`，之后是一串块：头 CRC32（4 字节）、头长度（变长整数）、类型、标志，
//!   标志说有时跟着附加区长度与数据长度。块长 = 4 + 头长度字段本身 + 头长度 + 数据长度。以「归档结束」块（类型 5）结束。
//!   头被加密的归档（有类型 4 的加密头）后面的头读不懂，不认。
//! - RAR 1.5–4.x：签名 `Rar!\x1A\x07\x00`（它本身就是 7 字节的标记块），之后每块是头 CRC（2 字节，CRC32 的低 16 位）、
//!   类型、标志、头长度（2 字节）；标志最高位置位时后面跟 4 字节的附加长度（文件块的压缩后大小，大文件另有高 32 位）。
//!   以「归档结束」块（0x7B）结束；老版本可能没有结束块，这时在最后一个能核对上的块之后结束。
//!
//! 每个头都核对 CRC，核对不上就不是这里的块。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::{le16, le32};
use yisi_part::crc32::crc32;

const RAR5_MAGIC: &[u8] = b"Rar!\x1A\x07\x01\x00";
const RAR4_MAGIC: &[u8] = b"Rar!\x1A\x07\x00";
/// RAR 5 的头最长 2 MiB，头长度字段最多 3 字节。
const RAR5_MAX_HEADER: u64 = 2 << 20;
const RAR5_ENCRYPTION: u64 = 4;
const RAR5_END: u64 = 5;
const RAR5_HAS_EXTRA: u64 = 0x0001;
const RAR5_HAS_DATA: u64 = 0x0002;
const RAR4_FILE: u8 = 0x74;
const RAR4_END: u8 = 0x7B;
/// RAR 4 块标志：后面有附加长度；文件块：压缩后大小有高 32 位。
const RAR4_LONG_BLOCK: u16 = 0x8000;
const RAR4_LARGE_FILE: u16 = 0x0100;

pub struct Rar;

static INFO: FormatInfo = FormatInfo {
    extension: "rar",
    variants: &[],
    description: "RAR archive",
    max_size: 64 << 30,
    enabled_by_default: true,
    nests_files: true,
};

static SIGNATURES: [Signature; 2] = [
    Signature {
        offset: 0,
        bytes: RAR5_MAGIC,
    },
    Signature {
        offset: 0,
        bytes: RAR4_MAGIC,
    },
];

/// RAR 5 的变长整数：每字节低 7 位，最高位表示后面还有。返回值与占的字节数。
fn vint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (index, &byte) in bytes.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7F).checked_shl(7 * index as u32)?;
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

impl FileFormat for Rar {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let structure = if block.starts_with(RAR5_MAGIC) {
            RarStructure::Rar5(Rar5::Start)
        } else if block.starts_with(RAR4_MAGIC) {
            RarStructure::Rar4 {
                phase: Rar4::Head,
                files: 0,
            }
        } else {
            return None;
        };
        Some(Box::new(Walker::new(INFO.max_size, structure)))
    }
}

#[derive(Clone, Copy)]
enum Rar5 {
    /// 等块开头的 CRC 与头长度的第一个字节。
    Start,
    /// 头长度不止一个字节，再读。
    SizeField,
    /// 等整个头（含 CRC 与头长度字段），`len` 是头长度字段的字节数。
    Header { len: usize },
}

#[derive(Clone, Copy)]
enum Rar4 {
    /// 等块的固定 7 字节。
    Head,
    /// 等整个头，核对 CRC。
    Header { crc: u16, kind: u8, flags: u16 },
}

enum RarStructure {
    Rar5(Rar5),
    Rar4 { phase: Rar4, files: u32 },
}

impl RarStructure {
    fn rar5(&mut self, at: u64, bytes: &[u8], phase: Rar5) -> Step {
        let need = |structure: &mut Self, phase, at, len| {
            *structure = RarStructure::Rar5(phase);
            Step::Need { at, len }
        };
        match phase {
            Rar5::Start => match bytes.get(4) {
                Some(&first) if first & 0x80 == 0 => need(
                    self,
                    Rar5::Header { len: 1 },
                    at,
                    4 + 1 + usize::from(first),
                ),
                Some(_) => need(self, Rar5::SizeField, at, 4 + 3),
                None => Step::Invalid,
            },
            Rar5::SizeField => match vint(bytes.get(4..).unwrap_or_default()) {
                Some((size, len)) if size <= RAR5_MAX_HEADER => {
                    need(self, Rar5::Header { len }, at, 4 + len + size as usize)
                }
                _ => Step::Invalid,
            },
            Rar5::Header { len } => {
                let (Some(crc), Some(header)) = (le32(bytes, 0), bytes.get(4..)) else {
                    return Step::Invalid;
                };
                if crc != crc32(&[header]) {
                    return Step::Invalid;
                }
                let Some(fields) = header.get(len..) else {
                    return Step::Invalid;
                };
                let Some((kind, a)) = vint(fields) else {
                    return Step::Invalid;
                };
                let Some((flags, b)) = vint(fields.get(a..).unwrap_or_default()) else {
                    return Step::Invalid;
                };
                let mut rest = fields.get(a + b..).unwrap_or_default();
                if flags & RAR5_HAS_EXTRA != 0 {
                    let Some((_, c)) = vint(rest) else {
                        return Step::Invalid;
                    };
                    rest = rest.get(c..).unwrap_or_default();
                }
                let data = if flags & RAR5_HAS_DATA != 0 {
                    match vint(rest) {
                        Some((data, _)) => data,
                        None => return Step::Invalid,
                    }
                } else {
                    0
                };
                let Some(end) = (at + bytes.len() as u64).checked_add(data) else {
                    return Step::Invalid;
                };
                match kind {
                    RAR5_END => Step::Done(end),
                    RAR5_ENCRYPTION => Step::Invalid,
                    1..=3 => need(self, Rar5::Start, end, 5),
                    _ => Step::Invalid,
                }
            }
        }
    }

    fn rar4(&mut self, at: u64, bytes: &[u8], phase: Rar4, files: u32) -> Step {
        match phase {
            Rar4::Head => {
                let (Some(crc), Some(&kind), Some(flags), Some(size)) =
                    (le16(bytes, 0), bytes.get(2), le16(bytes, 3), le16(bytes, 5))
                else {
                    return self.rar4_end(at, files);
                };
                // 块类型 0x72–0x7B；头至少 7 字节
                if !(0x72..=RAR4_END).contains(&kind) || size < 7 {
                    return self.rar4_end(at, files);
                }
                *self = RarStructure::Rar4 {
                    phase: Rar4::Header { crc, kind, flags },
                    files,
                };
                Step::Need {
                    at,
                    len: usize::from(size),
                }
            }
            Rar4::Header { crc, kind, flags } => {
                let checked = bytes
                    .get(2..)
                    .is_some_and(|covered| crc32(&[covered]) as u16 == crc);
                if !checked {
                    return self.rar4_end(at, files);
                }
                let mut extra = 0u64;
                if flags & RAR4_LONG_BLOCK != 0 {
                    extra = u64::from(le32(bytes, 7).unwrap_or(0));
                    if kind == RAR4_FILE && flags & RAR4_LARGE_FILE != 0 {
                        extra |= u64::from(le32(bytes, 32).unwrap_or(0)) << 32;
                    }
                }
                let end = at + bytes.len() as u64 + extra;
                if kind == RAR4_END {
                    return Step::Done(end);
                }
                *self = RarStructure::Rar4 {
                    phase: Rar4::Head,
                    files: files + u32::from(kind == RAR4_FILE),
                };
                Step::Need { at: end, len: 7 }
            }
        }
    }

    /// RAR 4 读到的不是能核对上的块：有文件时，归档到上一块为止。
    fn rar4_end(&self, at: u64, files: u32) -> Step {
        if files > 0 {
            Step::Done(at)
        } else {
            Step::Invalid
        }
    }
}

impl Structure for RarStructure {
    fn first(&self) -> Step {
        match self {
            RarStructure::Rar5(_) => Step::Need {
                at: RAR5_MAGIC.len() as u64,
                len: 5,
            },
            RarStructure::Rar4 { .. } => Step::Need {
                at: RAR4_MAGIC.len() as u64,
                len: 7,
            },
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        match *self {
            RarStructure::Rar5(phase) => self.rar5(at, bytes, phase),
            RarStructure::Rar4 { phase, files } => self.rar4(at, bytes, phase, files),
        }
    }

    /// RAR 4 没有结束块、数据正好在一个块之后用完（设备末尾）：有文件就到这里。
    fn finish(&self, received: u64, waiting_at: Option<u64>) -> Option<u64> {
        match self {
            RarStructure::Rar4 {
                phase: Rar4::Head,
                files,
            } if *files > 0 && waiting_at == Some(received) => Some(received),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Rar.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn rar5_blocks_are_followed_to_the_end_block() {
        for sizes in [&[100usize, 3000][..], &[0], &[70_000, 5, 200]] {
            let sample = samples::rar5(sizes);
            let mut data = sample.clone();
            data.extend_from_slice(b"Rar!\x1A\x07\x01\x00 another archive");
            check(&data, Some(sample.len() as u64));
        }
    }

    #[test]
    fn rar5_bad_crc_or_truncation_is_rejected() {
        let sample = samples::rar5(&[100, 200]);
        let mut bad = sample.clone();
        bad[8 + 6] ^= 0xFF;
        check(&bad, None);
        let truncated = &sample[..sample.len() - 2];
        check(truncated, None);
    }

    #[test]
    fn rar4_blocks_are_followed_with_or_without_an_end_block() {
        for end_block in [true, false] {
            let sample = samples::rar4(&[100, 2000, 0], end_block);
            let mut data = sample.clone();
            data.extend_from_slice(&[0x11; 40]);
            check(&data, Some(sample.len() as u64));
            check(&sample, Some(sample.len() as u64));
        }
    }

    #[test]
    fn rar4_without_files_or_with_bad_crc_is_rejected() {
        let sample = samples::rar4(&[300], true);
        let mut bad = sample.clone();
        bad[7] ^= 0xFF; // 归档头的 CRC
        check(&bad, None);
    }
}
