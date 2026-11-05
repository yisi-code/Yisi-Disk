//! Outlook 数据文件（[MS-PST] 2.2.2.6 节的文件头）：PST 与脱机的 OST。文件头以 `!BDN` 开头，
//! 之后是 4 字节的 CRC（覆盖第 8 字节起的 471 字节）、客户端标识（PST 是 `SM`，OST 是 `SO`）、格式版本
//! （14、15 是 ANSI 格式；23 起是 Unicode 格式）。文件头里的 ROOT 结构记着文件结尾的位置 ibFileEof：
//! ANSI 格式在第 168 字节（4 字节），Unicode 格式在第 184 字节（8 字节）。
//!
//! 这个 CRC 用的是 CRC-32 的表，但初值为 0、最后不取反；按线性关系由通用的 CRC-32 换算（见 [`partial_crc`]）。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{KnownLength, Walker};
use yisi_part::bytes::{le16, le32, le64};
use yisi_part::crc32::crc32;

/// CRC 覆盖的范围：第 8 字节起的 471 字节。
const CRC_FROM: usize = 8;
const CRC_LEN: usize = 471;
const CLIENT_PST: u16 = 0x4D53; // "SM"
const CLIENT_OST: u16 = 0x4F53; // "SO"
const ANSI_VERSIONS: [u16; 2] = [14, 15];
const UNICODE_VERSIONS: std::ops::RangeInclusive<u16> = 23..=40;
const ANSI_EOF_AT: usize = 168;
const UNICODE_EOF_AT: usize = 184;
/// 文件头所在的那一页。
const HEADER_PAGE: u64 = 512;

pub struct Pst;

static INFO: FormatInfo = FormatInfo {
    extension: "pst",
    variants: &["ost"],
    description: "Outlook data file",
    max_size: 64 << 30,
    enabled_by_default: true,
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"!BDN",
}];

/// 初值为 0、最后不取反的 CRC-32。通用 CRC-32 是初值全 1、最后取反；CRC 的计算对（初值, 数据）是线性的，
/// 所以 R(0, d) = R(全 1, d) ⊕ R(全 1, 等长的 0) = crc32(d) ⊕ crc32(等长的 0)。
fn partial_crc(data: &[u8]) -> u32 {
    const ZEROS: [u8; CRC_LEN] = [0; CRC_LEN];
    let zeros = ZEROS.get(..data.len().min(CRC_LEN)).unwrap_or_default();
    crc32(&[data]) ^ crc32(&[zeros])
}

/// 文件头说的文件长度与扩展名，各项都合规时才给出。
fn file_length(block: &[u8]) -> Option<(u64, Option<&'static str>)> {
    let client = le16(block, 8)?;
    let version = le16(block, 10)?;
    let extension = match client {
        CLIENT_PST => None,
        CLIENT_OST => Some("ost"),
        _ => return None,
    };
    let eof = if ANSI_VERSIONS.contains(&version) {
        u64::from(le32(block, ANSI_EOF_AT)?)
    } else if UNICODE_VERSIONS.contains(&version) {
        le64(block, UNICODE_EOF_AT)?
    } else {
        return None;
    };
    let covered = block.get(CRC_FROM..CRC_FROM + CRC_LEN)?;
    // 创建与访问的平台固定为 0x01
    let plausible =
        le32(block, 4)? == partial_crc(covered) && block.get(14..16) == Some([1, 1].as_slice());
    (plausible && eof > HEADER_PAGE).then_some((eof, extension))
}

impl FileFormat for Pst {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let (length, extension) = file_length(block)?;
        Some(Box::new(Walker::new(
            INFO.max_size,
            KnownLength::new(length).with_extension(extension),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::{check_all_splits, run_keeping};

    #[test]
    fn length_comes_from_the_root_structure() {
        for (unicode, ost, extension) in [
            (true, false, None),
            (false, false, None),
            (true, true, Some("ost")),
        ] {
            let sample = samples::pst(unicode, ost, 40_000);
            let mut data = sample.clone();
            data.extend_from_slice(b"!BDN and something else");
            check_all_splits(
                || Pst.header_check(&data).unwrap(),
                &data,
                Some(sample.len() as u64),
            );
            let truncated = &sample[..sample.len() - 1];
            check_all_splits(|| Pst.header_check(truncated).unwrap(), truncated, None);
            let mut recovering = Pst.header_check(&data).unwrap();
            run_keeping(&mut recovering, &data, 4096);
            assert_eq!(recovering.extension(), extension);
        }
    }

    #[test]
    fn crc_and_header_fields_are_checked() {
        let sample = samples::pst(true, false, 40_000);
        let changed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            Pst.header_check(&data).is_none()
        };
        assert!(changed(UNICODE_EOF_AT, &[1]), "CRC 覆盖文件结尾的位置");
        assert!(changed(4, &[0]), "CRC 不对");
        // 改了之后重算 CRC：只看字段本身
        let resealed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            let crc = partial_crc(&data[CRC_FROM..CRC_FROM + CRC_LEN]);
            data[4..8].copy_from_slice(&crc.to_le_bytes());
            Pst.header_check(&data).is_none()
        };
        assert!(!resealed(
            UNICODE_EOF_AT,
            &sample[UNICODE_EOF_AT..UNICODE_EOF_AT + 1]
        ));
        assert!(resealed(10, &[16, 0]), "版本不对");
        assert!(resealed(8, b"AB"), "客户端标识不对");
        assert!(resealed(14, &[2]), "平台不对");
        assert!(resealed(UNICODE_EOF_AT, &[0; 8]), "结尾落在文件头里");
    }

    #[test]
    fn partial_crc_matches_the_bitwise_definition() {
        let data: Vec<u8> = (0..CRC_LEN).map(|index| (index * 7 + 3) as u8).collect();
        assert_eq!(partial_crc(&data), samples::raw_crc32(&data));
        assert_eq!(partial_crc(&[0; CRC_LEN]), 0);
    }
}
