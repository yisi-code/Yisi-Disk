//! Windows 图元文件：EMF（[MS-EMF]）与 WMF（[MS-WMF]），都是一串记录，以结束记录收尾。
//!
//! EMF：第一条是 EMR_HEADER（类型 1、记录长度、外框、第 40 字节的签名 ` EMF`、版本 0x10000、
//! 整个文件的字节数、记录数……）。长度写在头里；最后一条记录是 EMR_EOF（类型 14），它的最后 4 字节重复记录长度，
//! 从文件末尾往前读就能找到它的开头，用来核对长度没错。
//!
//! WMF：可以先有 22 字节的「可放置」头（键 0x9AC6CDD7、保留 0、前 10 个字的异或校验和），之后是 META_HEADER
//! （类型 1 / 2、头长 9 个字、版本 0x0100 / 0x0300、整个图元文件的长度（以 16 位字计）……）。
//! 文件长度由此算出；最后一条记录是 META_EOF（长度 3 个字、函数 0），用来核对。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{CheckedLength, Walker};
use yisi_part::bytes::{le16, le32};

const EMR_HEADER: u32 = 1;
const EMR_EOF: u32 = 14;
const EMF_SIGNATURE_AT: u16 = 40;
const EMF_VERSION: u32 = 0x0001_0000;
/// EMR_HEADER 的最短长度（到 Millimeters 为止）。
const EMF_HEADER_MIN: u32 = 88;
/// EMR_EOF 不带调色板时的长度。
const EMF_EOF_MIN: u32 = 20;
/// 结束记录最多往前找这么远（带 256 色调色板的 EMR_EOF 也装得下）。
const EMF_TAIL: u32 = 4096;

const WMF_PLACEABLE_KEY: &[u8] = &[0xD7, 0xCD, 0xC6, 0x9A];
const WMF_PLACEABLE_LEN: u64 = 22;
const WMF_HEADER_WORDS: u16 = 9;
const META_EOF: &[u8] = &[3, 0, 0, 0, 0, 0];

pub struct Emf;

static EMF_INFO: FormatInfo = FormatInfo {
    extension: "emf",
    variants: &[],
    description: "Enhanced metafile",
    max_size: 256 << 20,
    enabled_by_default: true,
    // EMF+ 的图像记录里可以直接装着 PNG、JPEG
    nests_files: true,
};

static EMF_SIGNATURES: [Signature; 1] = [Signature {
    offset: EMF_SIGNATURE_AT,
    bytes: b" EMF",
}];

/// 文件末尾 `tail` 里的最后一条记录是不是 EMR_EOF：最后 4 字节是它的长度，往前那么多字节是它的开头。
fn ends_with_emf_eof(tail: &[u8]) -> bool {
    let Some(size) = tail.len().checked_sub(4).and_then(|at| le32(tail, at)) else {
        return false;
    };
    let Some(start) = tail.len().checked_sub(size as usize) else {
        return false;
    };
    size >= EMF_EOF_MIN
        && size % 4 == 0
        && le32(tail, start) == Some(EMR_EOF)
        && le32(tail, start + 4) == Some(size)
}

impl FileFormat for Emf {
    fn info(&self) -> &FormatInfo {
        &EMF_INFO
    }

    fn signatures(&self) -> &[Signature] {
        &EMF_SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let header_len = le32(block, 4)?;
        let bytes = le32(block, 48)?;
        let plausible = le32(block, 0)? == EMR_HEADER
            && header_len >= EMF_HEADER_MIN
            && header_len % 4 == 0
            && le32(block, 44)? == EMF_VERSION
            && u64::from(bytes) >= u64::from(header_len) + u64::from(EMF_EOF_MIN)
            && bytes % 4 == 0
            && le32(block, 52)? >= 2
            && le16(block, 58)? == 0;
        if !plausible {
            return None;
        }
        let tail_at = header_len.max(bytes - EMF_TAIL.min(bytes));
        Some(Box::new(Walker::new(
            EMF_INFO.max_size,
            CheckedLength {
                at: u64::from(tail_at),
                len: (bytes - tail_at) as usize,
                check: ends_with_emf_eof,
                length: u64::from(bytes),
            },
        )))
    }
}

pub struct Wmf;

static WMF_INFO: FormatInfo = FormatInfo {
    extension: "wmf",
    variants: &[],
    description: "Windows metafile",
    max_size: 256 << 20,
    enabled_by_default: true,
    nests_files: false,
};

/// 可放置头的键；没有可放置头时是 META_HEADER 的类型（内存 / 磁盘）、头长 9 与版本。
static WMF_SIGNATURES: [Signature; 5] = [
    Signature {
        offset: 0,
        bytes: WMF_PLACEABLE_KEY,
    },
    Signature {
        offset: 0,
        bytes: b"\x01\x00\x09\x00\x00\x03",
    },
    Signature {
        offset: 0,
        bytes: b"\x02\x00\x09\x00\x00\x03",
    },
    Signature {
        offset: 0,
        bytes: b"\x01\x00\x09\x00\x00\x01",
    },
    Signature {
        offset: 0,
        bytes: b"\x02\x00\x09\x00\x00\x01",
    },
];

/// WMF 的文件长度，可放置头与 META_HEADER 都合规时才给出。
fn wmf_length(block: &[u8]) -> Option<u64> {
    let base = if block.starts_with(WMF_PLACEABLE_KEY) {
        let checksum = (0..10).try_fold(0u16, |sum, word| Some(sum ^ le16(block, 2 * word)?))?;
        let placeable_ok =
            le16(block, 4)? == 0 && le32(block, 16)? == 0 && le16(block, 20)? == checksum;
        if !placeable_ok {
            return None;
        }
        WMF_PLACEABLE_LEN as usize
    } else {
        0
    };
    let words = le32(block, base + 6)?;
    let max_record = le32(block, base + 12)?;
    let plausible = matches!(le16(block, base)?, 1 | 2)
        && le16(block, base + 2)? == WMF_HEADER_WORDS
        && matches!(le16(block, base + 4)?, 0x0100 | 0x0300)
        && words >= u32::from(WMF_HEADER_WORDS) + 3
        && (3..=words).contains(&max_record)
        && le16(block, base + 16)? == 0;
    plausible.then(|| base as u64 + 2 * u64::from(words))
}

impl FileFormat for Wmf {
    fn info(&self) -> &FormatInfo {
        &WMF_INFO
    }

    fn signatures(&self) -> &[Signature] {
        &WMF_SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let length = wmf_length(block)?;
        Some(Box::new(Walker::new(
            WMF_INFO.max_size,
            CheckedLength {
                at: length - META_EOF.len() as u64,
                len: META_EOF.len(),
                check: |bytes: &[u8]| bytes == META_EOF,
                length,
            },
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn check(format: &dyn FileFormat, data: &[u8], expected: Option<u64>) {
        check_all_splits(|| format.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn emf_length_comes_from_the_header_and_ends_with_eof() {
        for (records, palette) in [(3, 0), (20, 16), (1, 0)] {
            let sample = samples::emf(records, palette);
            let mut data = sample.clone();
            data.extend_from_slice(b"\x01\0\0\0 and something else");
            check(&Emf, &data, Some(sample.len() as u64));
            check(&Emf, &sample[..sample.len() - 1], None);
        }
    }

    #[test]
    fn emf_without_eof_or_with_odd_headers_is_rejected() {
        let sample = samples::emf(3, 0);
        let mut no_eof = sample.clone();
        let eof_at = sample.len() - 20;
        no_eof[eof_at] = 13;
        check(&Emf, &no_eof, None);
        let changed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            Emf.header_check(&data).is_none()
        };
        assert!(changed(0, &[2]), "第一条不是头记录");
        assert!(changed(44, &[0, 0, 2, 0]), "版本");
        assert!(changed(48, &[2]), "字节数不是 4 的倍数");
        assert!(
            changed(4, &[0xFC, 0xFF, 0xFF, 0xFF]),
            "头记录长度接近上限也不溢出"
        );
    }

    #[test]
    fn wmf_length_comes_from_the_meta_header() {
        for placeable in [true, false] {
            let sample = samples::wmf(placeable, 5);
            let mut data = sample.clone();
            data.extend_from_slice(b"\xD7\xCD\xC6\x9A and more");
            check(&Wmf, &data, Some(sample.len() as u64));
            check(&Wmf, &sample[..sample.len() - 1], None);
            let mut no_eof = sample.clone();
            let last = sample.len() - 6;
            no_eof[last] = 4;
            check(&Wmf, &no_eof, None);
        }
        let sample = samples::wmf(true, 2);
        let mut checksum = sample.clone();
        checksum[20] ^= 1;
        assert!(Wmf.header_check(&checksum).is_none(), "可放置头的校验和");
        let mut header_words = sample.clone();
        header_words[24] = 8;
        assert!(Wmf.header_check(&header_words).is_none(), "头长不是 9 个字");
    }
}
