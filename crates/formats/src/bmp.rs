//! BMP（Windows 位图）：14 字节文件头（`BM`、文件总长度、两个保留字、像素数据的起点）之后是位图信息头，
//! 信息头的长度标明它是哪一版（OS/2 的 12、64 字节，Windows 的 40、52、56、108、124 字节）。
//! 总长度写在文件头里；其余各项只用来排除误识别：保留字为 0、位面数为 1、每像素位数是规定的几种、
//! 像素数据在文件里面，不压缩时一行行的像素正好装得下。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{KnownLength, Walker};
use yisi_part::bytes::{le16, le32};

const FILE_HEADER_LEN: u32 = 14;
const INFO_HEADER_LENS: [u32; 7] = [12, 40, 52, 56, 64, 108, 124];
const BITS_PER_PIXEL: [u16; 6] = [1, 4, 8, 16, 24, 32];
/// 信息头里的压缩方式：不压缩、16 / 32 位按位掩码（像素数据的大小都按行算得出来）。
const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;

pub struct Bmp;

static INFO: FormatInfo = FormatInfo {
    extension: "bmp",
    variants: &[],
    description: "Windows bitmap",
    max_size: 512 << 20,
    enabled_by_default: true,
    nests_files: false,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"BM",
}];

/// 文件头说的总长度，各项都合理时才给出。
fn file_length(block: &[u8]) -> Option<u64> {
    let size = le32(block, 2)?;
    let reserved = le32(block, 6)?;
    let pixels_at = le32(block, 10)?;
    let info_len = le32(block, 14)?;
    if reserved != 0 || !INFO_HEADER_LENS.contains(&info_len) {
        return None;
    }
    let headers = FILE_HEADER_LEN + info_len;
    if pixels_at < headers || pixels_at >= size {
        return None;
    }
    let (width, height, planes, bits, compression) = if info_len == 12 {
        // OS/2 1.x：宽高各 16 位，没有压缩方式
        let width = i64::from(le16(block, 18)?);
        let height = i64::from(le16(block, 20)?);
        (width, height, le16(block, 22)?, le16(block, 24)?, BI_RGB)
    } else {
        // 高度为负表示从上往下存
        let width = i64::from(le32(block, 18)? as i32);
        let height = i64::from(le32(block, 22)? as i32).abs();
        (
            width,
            height,
            le16(block, 26)?,
            le16(block, 28)?,
            le32(block, 30)?,
        )
    };
    if width <= 0 || height <= 0 || planes != 1 || !BITS_PER_PIXEL.contains(&bits) {
        return None;
    }
    if matches!(compression, BI_RGB | BI_BITFIELDS | BI_ALPHABITFIELDS) {
        // 每行按 4 字节对齐
        let row = (width.checked_mul(i64::from(bits))? + 31) / 32 * 4;
        let pixels = row.checked_mul(height)?;
        if i64::from(pixels_at) + pixels > i64::from(size) {
            return None;
        }
    }
    Some(u64::from(size))
}

impl FileFormat for Bmp {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let length = file_length(block)?;
        Some(Box::new(Walker::new(
            INFO.max_size,
            KnownLength::new(length),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    #[test]
    fn length_comes_from_the_file_header() {
        for (width, height, bits) in [(16, 8, 24), (33, 5, 1), (7, -9, 32), (100, 3, 8)] {
            let sample = samples::bmp(width, height, bits);
            let mut data = sample.clone();
            data.extend_from_slice(b"BM followed by something else");
            check_all_splits(
                || Bmp.header_check(&data).unwrap(),
                &data,
                Some(sample.len() as u64),
            );
        }
    }

    #[test]
    fn implausible_headers_are_rejected() {
        let sample = samples::bmp(16, 8, 24);
        let broken = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            Bmp.header_check(&data).is_none()
        };
        assert!(broken(6, &[1, 0, 0, 0]), "保留字不是 0");
        assert!(broken(14, &[41, 0, 0, 0]), "信息头长度不对");
        assert!(broken(26, &[2, 0]), "位面数不是 1");
        assert!(broken(28, &[7, 0]), "每像素位数不对");
        assert!(broken(2, &[60, 0, 0, 0]), "总长度装不下像素");
        assert!(Bmp.header_check(b"BM").is_none());
    }
}
