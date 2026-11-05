//! RIFF（微软的资源交换文件格式）：`RIFF`、4 字节小端长度、4 字节形式类型，之后是一串块。
//! 总长度写在开头（长度不含前 8 字节，奇数时后面补一个字节）。按形式类型分成三种格式：
//! - AVI（`AVI `）：第一块是 `LIST`。超过 1 GiB 的 AVI（OpenDML）在第一个 RIFF 之后紧跟着若干个
//!   形式为 `AVIX` 的 RIFF，一并算进文件；
//! - WAV（`WAVE`）：第一块通常是 `fmt `，也有先放 `JUNK`、`bext`、`LIST` 的；
//! - WebP（`WEBP`）：第一块是 `VP8 `、`VP8L` 或 `VP8X`。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::le32;

/// RIFF 头：`RIFF`、长度、形式类型。
const HEADER_LEN: u64 = 12;

/// 一种 RIFF 格式：形式类型与它允许的第一块。
#[derive(Clone, Copy)]
pub struct RiffFormat {
    info: &'static FormatInfo,
    form: &'static [u8; 4],
    first_chunks: &'static [&'static [u8; 4]],
    /// 后面可以接扩展用的 RIFF（AVI 的 `AVIX`）。
    extension_form: Option<&'static [u8; 4]>,
}

static AVI_INFO: FormatInfo = FormatInfo {
    extension: "avi",
    variants: &[],
    description: "AVI video",
    max_size: 64 << 30,
    enabled_by_default: true,
    nests_files: false,
};

static WAV_INFO: FormatInfo = FormatInfo {
    extension: "wav",
    variants: &[],
    description: "WAVE audio",
    max_size: 4 << 30,
    enabled_by_default: true,
    nests_files: false,
};

static WEBP_INFO: FormatInfo = FormatInfo {
    extension: "webp",
    variants: &[],
    description: "WebP image",
    max_size: 64 << 20,
    enabled_by_default: true,
    nests_files: false,
};

pub const AVI: RiffFormat = RiffFormat {
    info: &AVI_INFO,
    form: b"AVI ",
    first_chunks: &[b"LIST"],
    extension_form: Some(b"AVIX"),
};

pub const WAV: RiffFormat = RiffFormat {
    info: &WAV_INFO,
    form: b"WAVE",
    first_chunks: &[b"fmt ", b"JUNK", b"bext", b"LIST", b"fact", b"PAD "],
    extension_form: None,
};

pub const WEBP: RiffFormat = RiffFormat {
    info: &WEBP_INFO,
    form: b"WEBP",
    first_chunks: &[b"VP8 ", b"VP8L", b"VP8X"],
    extension_form: None,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"RIFF",
}];

/// 从 `at` 起的这个 RIFF 一共多长（含补齐的字节）。RIFF 沿用 IFF 的块：4 字节类型、4 字节长度、内容，
/// 奇数长度补一个字节；IFF 的 `FORM`（AIFF、DjVu）也按它算结尾，只是长度是大端。
pub(crate) fn riff_end(at: u64, size: u32) -> u64 {
    at + 8 + u64::from(size) + u64::from(size & 1)
}

impl FileFormat for RiffFormat {
    fn info(&self) -> &FormatInfo {
        self.info
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let size = le32(block, 4)?;
        let first: &[u8; 4] = block.get(12..16)?.try_into().ok()?;
        let plausible = block.get(8..12) == Some(self.form.as_slice())
            && size >= 4 + 8
            && self.first_chunks.contains(&first);
        plausible.then(|| {
            Box::new(Walker::new(
                self.info.max_size,
                RiffStructure {
                    end: riff_end(0, size),
                    extension_form: self.extension_form,
                },
            )) as Box<dyn Recovering>
        })
    }
}

struct RiffStructure {
    /// 已知部分的结尾。
    end: u64,
    extension_form: Option<&'static [u8; 4]>,
}

impl Structure for RiffStructure {
    fn first(&self) -> Step {
        match self.extension_form {
            Some(_) => Step::Need {
                at: self.end,
                len: HEADER_LEN as usize,
            },
            None => Step::Done(self.end),
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        let continues = bytes.get(..4) == Some(b"RIFF".as_slice())
            && self
                .extension_form
                .is_some_and(|form| bytes.get(8..12) == Some(form.as_slice()));
        match le32(bytes, 4) {
            Some(size) if continues && size >= 4 => {
                self.end = riff_end(at, size);
                Step::Need {
                    at: self.end,
                    len: HEADER_LEN as usize,
                }
            }
            _ => Step::Done(at),
        }
    }

    /// 只会停在已知部分的结尾、等下一个扩展 RIFF 的头：数据已经到了那里（设备末尾、或后面不足一个头），
    /// 文件就到这里。
    fn finish(&self, received: u64, _waiting_at: Option<u64>) -> Option<u64> {
        (received >= self.end).then_some(self.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn check(format: RiffFormat, data: &[u8], expected: Option<u64>) {
        check_all_splits(|| format.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn length_comes_from_the_riff_header_padded_to_even() {
        for (format, form, first) in [(WAV, b"WAVE", b"fmt "), (WEBP, b"WEBP", b"VP8L")] {
            for data_len in [100, 101] {
                let sample = samples::riff(form, &[(first, 16), (b"data", data_len)]);
                let mut data = sample.clone();
                data.extend_from_slice(b"RIFF....WAVEfmt ");
                check(format, &data, Some(sample.len() as u64));
                check(format, &sample, Some(sample.len() as u64));
            }
        }
    }

    #[test]
    fn avi_includes_the_following_avix_parts() {
        let first = samples::riff(b"AVI ", &[(b"LIST", 200), (b"LIST", 3000), (b"idx1", 64)]);
        let second = samples::riff(b"AVIX", &[(b"LIST", 5001)]);
        let third = samples::riff(b"AVIX", &[(b"LIST", 77)]);
        let mut avi = [first.clone(), second, third].concat();
        let whole = avi.len() as u64;
        check(AVI, &avi, Some(whole));
        avi.extend_from_slice(&samples::riff(b"WAVE", &[(b"fmt ", 16)]));
        check(AVI, &avi, Some(whole));
        check(AVI, &first, Some(first.len() as u64));
        // 后面只剩不到一个 RIFF 头的数据
        let mut short_tail = first.clone();
        short_tail.extend_from_slice(b"RIFF");
        check(AVI, &short_tail, Some(first.len() as u64));
        // AVI 本身没完
        let cut = &first[..first.len() - 10];
        check(AVI, cut, None);
    }

    #[test]
    fn form_and_first_chunk_pick_the_format() {
        let wav = samples::riff(b"WAVE", &[(b"fmt ", 16), (b"data", 10)]);
        assert!(WAV.header_check(&wav).is_some());
        assert!(AVI.header_check(&wav).is_none() && WEBP.header_check(&wav).is_none());
        let odd = samples::riff(b"WAVE", &[(b"zzzz", 16)]);
        assert!(WAV.header_check(&odd).is_none());
    }
}
