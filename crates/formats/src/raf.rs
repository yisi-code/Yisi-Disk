//! 富士相机原片 RAF：文件头以 16 字节的 `FUJIFILMCCD-RAW ` 开头，之后是 4 位数字的格式版本、8 字节相机编号、
//! 32 字节相机名、4 字节目录版本与 20 字节保留；第 0x54 字节起是三段数据的位置与长度（大端）：
//! 内嵌的 JPEG 预览、CFA 头（相机参数的记录）、CFA 数据（传感器原始数据）。文件长度取三段结尾的最大值。
//!
//! 其余各项只用来排除误识别：版本是数字、相机名是可打印字符、三段都在文件头之后、JPEG 预览以 `FF D8 FF` 开头。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{CheckedLength, Walker};
use yisi_part::bytes::be32;

const MAGIC: &[u8] = b"FUJIFILMCCD-RAW ";
/// 三段（JPEG 预览、CFA 头、CFA 数据）的位置与长度从这里开始，各 8 字节。
const SECTIONS_AT: usize = 0x54;
const HEADER_LEN: u32 = 0x6C;
const JPEG_START: &[u8] = &[0xFF, 0xD8, 0xFF];

pub struct Raf;

static INFO: FormatInfo = FormatInfo {
    extension: "raf",
    variants: &[],
    description: "Fujifilm raw photo",
    max_size: 1 << 30,
    enabled_by_default: true,
    // 里面带着 JPEG 预览
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: MAGIC,
}];

impl FileFormat for Raf {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let version = block.get(0x10..0x14)?;
        let name = block.get(0x1C..0x3C)?;
        let name_ok = name.first().is_some_and(u8::is_ascii_graphic)
            && name
                .iter()
                .all(|&byte| byte == 0 || (0x20..=0x7E).contains(&byte));
        if !version.iter().all(u8::is_ascii_digit) || !name_ok {
            return None;
        }
        let mut end = 0u64;
        let mut jpeg_at = 0;
        for index in 0..3 {
            let at = be32(block, SECTIONS_AT + 8 * index)?;
            let len = be32(block, SECTIONS_AT + 8 * index + 4)?;
            if at < HEADER_LEN || len == 0 {
                return None;
            }
            if index == 0 {
                jpeg_at = at;
            }
            end = end.max(u64::from(at) + u64::from(len));
        }
        Some(Box::new(Walker::new(
            INFO.max_size,
            CheckedLength {
                at: u64::from(jpeg_at),
                len: JPEG_START.len(),
                check: |bytes: &[u8]| bytes == JPEG_START,
                length: end,
            },
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Raf.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn length_is_the_end_of_the_last_section() {
        for (jpeg, cfa) in [(2000, 9000), (5000, 100)] {
            let sample = samples::raf(jpeg, cfa);
            let mut data = sample.clone();
            data.extend_from_slice(b"FUJIFILMCCD-RAW and more");
            check(&data, Some(sample.len() as u64));
            check(&sample[..sample.len() - 1], None);
        }
    }

    #[test]
    fn implausible_headers_are_rejected() {
        let sample = samples::raf(500, 500);
        let changed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            data
        };
        assert!(Raf.header_check(&changed(0x10, b"02x1")).is_none(), "版本");
        assert!(
            Raf.header_check(&changed(0x1C, &[0x01])).is_none(),
            "相机名"
        );
        assert!(
            Raf.header_check(&changed(0x58, &[0; 4])).is_none(),
            "长度为 0"
        );
        let jpeg_at = be32(&sample, SECTIONS_AT).unwrap() as usize;
        check(&changed(jpeg_at, &[0xFF, 0xD9]), None);
    }
}
