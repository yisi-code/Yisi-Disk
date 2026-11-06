//! IFF（EA IFF 85）的 `FORM`：`FORM`、4 字节大端长度、4 字节形式类型，之后是一串块（4 字节类型、大端长度、内容，
//! 奇数长度补一个字节）。总长度写在开头（不含前 8 字节），与 RIFF 同样按 [`riff_end`] 算结尾。按形式类型分两种格式：
//! - AIFF（Audio IFF 1.3，形式 `AIFF`）与 AIFF-C（形式 `AIFC`，存成 aifc）：第一块通常是 `COMM`，AIFF-C 是 `FVER`；
//! - DjVu（DjVu3 规范）：文件开头多 4 字节 `AT&T`，形式是 `DJVU`（单页，第一块 `INFO`）或 `DJVM`（多页，第一块 `DIRM`）。
//!
//! 其余各项只用来排除误识别：形式类型与第一块是规定的几种，第一块装得进 `FORM`。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::riff::riff_end;
use crate::walker::{KnownLength, Walker};
use yisi_part::bytes::be32;

/// 一种形式：形式类型、存成的扩展名（`None` 用格式本身的）、允许的第一块。
type Form = (
    &'static [u8; 4],
    Option<&'static str>,
    &'static [&'static [u8; 4]],
);

/// 一种 IFF 格式：`FORM` 前面的前缀与它的几种形式。
pub struct IffFormat {
    info: &'static FormatInfo,
    signatures: &'static [Signature],
    prefix: &'static [u8],
    forms: &'static [Form],
    /// 截短时保存断点之前的一段（AIFF 的前半段照样能放；DjVu 不做）。
    partial: bool,
}

static AIFF_INFO: FormatInfo = FormatInfo {
    extension: "aif",
    variants: &["aifc"],
    description: "AIFF audio",
    max_size: 4 << 30,
    enabled_by_default: true,
    nests_files: false,
};

static DJVU_INFO: FormatInfo = FormatInfo {
    extension: "djvu",
    variants: &[],
    description: "DjVu document",
    max_size: 1 << 30,
    enabled_by_default: true,
    nests_files: false,
};

/// AIFF 与 AIFF-C 规定的块；文件可以从其中任何一块开始。
const AIFF_CHUNKS: &[&[u8; 4]] = &[
    b"COMM", b"FVER", b"SSND", b"MARK", b"INST", b"COMT", b"NAME", b"AUTH", b"(c) ", b"ANNO",
    b"APPL", b"MIDI", b"AESD", b"ID3 ", b"CHAN",
];

pub const AIFF: IffFormat = IffFormat {
    info: &AIFF_INFO,
    signatures: &[Signature {
        offset: 0,
        bytes: b"FORM",
    }],
    prefix: b"",
    forms: &[
        (b"AIFF", None, AIFF_CHUNKS),
        (b"AIFC", Some("aifc"), AIFF_CHUNKS),
    ],
    partial: true,
};

pub const DJVU: IffFormat = IffFormat {
    info: &DJVU_INFO,
    signatures: &[Signature {
        offset: 0,
        bytes: b"AT&TFORM",
    }],
    prefix: b"AT&T",
    forms: &[(b"DJVU", None, &[b"INFO"]), (b"DJVM", None, &[b"DIRM"])],
    partial: false,
};

impl FileFormat for IffFormat {
    fn info(&self) -> &FormatInfo {
        self.info
    }

    fn signatures(&self) -> &[Signature] {
        self.signatures
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let at = self.prefix.len();
        if !block.starts_with(self.prefix) || block.get(at..at + 4) != Some(b"FORM".as_slice()) {
            return None;
        }
        let size = be32(block, at + 4)?;
        let form = block.get(at + 8..at + 12)?;
        let first = block.get(at + 12..at + 16)?;
        let first_size = be32(block, at + 16)?;
        let (_, extension, first_chunks) = self
            .forms
            .iter()
            .find(|(candidate, ..)| candidate.as_slice() == form)?;
        let plausible = first_chunks.iter().any(|chunk| chunk.as_slice() == first)
            && u64::from(first_size) + 8 + 4 <= u64::from(size);
        plausible.then(|| {
            let length = riff_end(at as u64, size);
            let mut structure = KnownLength::new(length).with_extension(*extension);
            if self.partial {
                // 第一个块的头之后才有真正的内容
                structure = structure.intact_after(at as u64 + 12 + 8);
            }
            Box::new(Walker::new(self.info.max_size, structure)) as Box<dyn Recovering>
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::{check_all_splits, run_keeping};

    fn check(format: &IffFormat, data: &[u8], expected: Option<u64>) {
        check_all_splits(|| format.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn length_comes_from_the_form_header() {
        for (format, prefix, form, first, extension) in [
            (&AIFF, b"".as_slice(), b"AIFF", b"COMM", None),
            (&AIFF, b"", b"AIFC", b"FVER", Some("aifc")),
            (&DJVU, b"AT&T", b"DJVU", b"INFO", None),
            (&DJVU, b"AT&T", b"DJVM", b"DIRM", None),
        ] {
            for data_len in [100, 101] {
                let sample = samples::iff(prefix, form, &[(first, 18), (b"SSND", data_len)]);
                let mut data = sample.clone();
                data.extend_from_slice(b"FORM\0\0\0\x10AIFFCOMM");
                check(format, &data, Some(sample.len() as u64));
                check(format, &sample[..sample.len() - 1], None);
                let mut recovering = format.header_check(&data).unwrap();
                run_keeping(&mut recovering, &data, 64);
                assert_eq!(recovering.extension(), extension);
            }
        }
    }

    #[test]
    fn other_forms_or_first_chunks_are_rejected() {
        let wrong_form = samples::iff(b"", b"ILBM", &[(b"BMHD", 20)]);
        assert!(AIFF.header_check(&wrong_form).is_none());
        let wrong_first = samples::iff(b"", b"AIFF", &[(b"zzzz", 20)]);
        assert!(AIFF.header_check(&wrong_first).is_none());
        let djvu_without_prefix = samples::iff(b"", b"DJVU", &[(b"INFO", 10)]);
        assert!(DJVU.header_check(&djvu_without_prefix).is_none());
        // 第一块比整个 FORM 还长
        let mut too_long = samples::iff(b"", b"AIFF", &[(b"COMM", 18)]);
        too_long[16..20].copy_from_slice(&1000u32.to_be_bytes());
        assert!(AIFF.header_check(&too_long).is_none());
    }
}
