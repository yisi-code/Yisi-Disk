//! CHM（编译的 HTML 帮助，ITSF 格式）：文件头以 `ITSF` 开头（版本 2 头长 0x58、版本 3 头长 0x60），
//! 第 0x38 字节起是头段 0 的位置与长度（各 8 字节，长度 0x18）、目录段的位置与长度。
//! 头段 0 以 0x01FE 开头，第 8 字节起是整个文件的长度；目录段以 `ITSP` 开头。
//! 文件长度取头段 0 记的值，核对它装得下目录段。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::{le32, le64};

const SECTION0_LEN: u64 = 0x18;
const SECTION0_MAGIC: u32 = 0x01FE;
const DIRECTORY_MAGIC: &[u8] = b"ITSP";

pub struct Chm;

static INFO: FormatInfo = FormatInfo {
    extension: "chm",
    variants: &[],
    description: "Compiled HTML help",
    max_size: 2 << 30,
    enabled_by_default: true,
    // 不压缩的内容段里可以直接存着图片等文件
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"ITSF",
}];

impl FileFormat for Chm {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let header_len = match le32(block, 4)? {
            2 => 0x58,
            3 => 0x60,
            _ => return None,
        };
        let section0_at = le64(block, 0x38)?;
        let directory_at = le64(block, 0x48)?;
        let directory_len = le64(block, 0x50)?;
        let plausible = le32(block, 8)? == header_len
            && le32(block, 0x0C)? == 1
            && le64(block, 0x40)? == SECTION0_LEN
            && section0_at >= u64::from(header_len)
            && section0_at
                .checked_add(SECTION0_LEN)
                .is_some_and(|end| directory_at >= end)
            && directory_len >= DIRECTORY_MAGIC.len() as u64;
        plausible.then(|| {
            Box::new(Walker::new(
                INFO.max_size,
                ChmStructure {
                    section0_at,
                    directory_at,
                    directory_end: directory_at.saturating_add(directory_len),
                    length: None,
                },
            )) as Box<dyn Recovering>
        })
    }
}

struct ChmStructure {
    section0_at: u64,
    directory_at: u64,
    directory_end: u64,
    /// 头段 0 记的文件长度。
    length: Option<u64>,
}

impl Structure for ChmStructure {
    fn first(&self) -> Step {
        Step::Need {
            at: self.section0_at,
            len: SECTION0_LEN as usize,
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { bytes, .. } = event else {
            return Step::Invalid;
        };
        match self.length {
            None => match (le32(bytes, 0), le64(bytes, 8)) {
                (Some(SECTION0_MAGIC), Some(length)) if length >= self.directory_end => {
                    self.length = Some(length);
                    Step::Need {
                        at: self.directory_at,
                        len: DIRECTORY_MAGIC.len(),
                    }
                }
                _ => Step::Invalid,
            },
            Some(length) if bytes == DIRECTORY_MAGIC => Step::Done(length),
            Some(_) => Step::Invalid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Chm.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn length_comes_from_header_section_0() {
        for (version, content) in [(3, 5000), (2, 100)] {
            let sample = samples::chm(version, content);
            let mut data = sample.clone();
            data.extend_from_slice(b"ITSF and more");
            check(&data, Some(sample.len() as u64));
            check(&sample[..sample.len() - 1], None);
        }
    }

    #[test]
    fn implausible_headers_and_sections_are_rejected() {
        let sample = samples::chm(3, 1000);
        let changed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            data
        };
        assert!(Chm.header_check(&changed(4, &[4])).is_none(), "版本");
        assert!(
            Chm.header_check(&changed(8, &[0x58])).is_none(),
            "头长度与版本不符"
        );
        assert!(
            Chm.header_check(&changed(0x40, &[0x20])).is_none(),
            "头段 0 的长度"
        );
        let section0_at = le64(&sample, 0x38).unwrap() as usize;
        check(&changed(section0_at, &[0xFF]), None);
        // 文件长度装不下目录段
        check(&changed(section0_at + 8, &[1, 0, 0, 0, 0, 0, 0, 0]), None);
        let directory_at = le64(&sample, 0x48).unwrap() as usize;
        check(&changed(directory_at, b"ITSX"), None);
        // 头段 0 的位置接近上限也不溢出
        assert!(Chm.header_check(&changed(0x38, &[0xFF; 8])).is_none());
    }
}
