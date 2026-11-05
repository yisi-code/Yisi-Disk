//! Windows 注册表配置单元（NTUSER.DAT、SYSTEM 等）：4096 字节的基本块（`regf`、两个序号、写入时间、
//! 主版本 1、次版本、文件类型 0（主文件）、格式 1、根键位置、hive bin 数据的总长度、簇因子 1……
//! 第 508 字节是前 508 字节按 4 字节异或的校验和），之后是一个个以 `hbin` 开头的 hive bin。
//! 文件长度是 4096 + hive bin 数据的总长度；读第一个 hive bin 的开头核对它的位置。
//! 最后一个 hive bin 之后可能还留着没用的空间，它不属于任何 hive bin，不算进文件。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{CheckedLength, Walker};
use yisi_part::bytes::le32;

const BASE_BLOCK: u64 = 4096;
const CHECKSUM_AT: usize = 508;
/// hive bin 按 4096 字节对齐。
const BIN_ALIGNMENT: u32 = 4096;

pub struct Regf;

static INFO: FormatInfo = FormatInfo {
    extension: "dat",
    variants: &[],
    description: "Windows registry hive",
    max_size: 4 << 30,
    enabled_by_default: true,
    nests_files: false,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"regf",
}];

/// 基本块的校验和：前 127 个 4 字节小端值的异或；结果是全 1 时记成 0xFFFFFFFE，是 0 时记成 1。
fn checksum(block: &[u8]) -> Option<u32> {
    let value = block
        .get(..CHECKSUM_AT)?
        .as_chunks::<4>()
        .0
        .iter()
        .fold(0u32, |value, word| value ^ u32::from_le_bytes(*word));
    Some(match value {
        u32::MAX => u32::MAX - 1,
        0 => 1,
        value => value,
    })
}

impl FileFormat for Regf {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let bins = le32(block, 40)?;
        let plausible = le32(block, 20)? == 1
            && (2..=6).contains(&le32(block, 24)?)
            && le32(block, 28)? == 0
            && le32(block, 32)? == 1
            && le32(block, 44)? == 1
            && bins > 0
            && bins % BIN_ALIGNMENT == 0
            && le32(block, CHECKSUM_AT)? == checksum(block)?;
        plausible.then(|| {
            Box::new(Walker::new(
                INFO.max_size,
                CheckedLength {
                    at: BASE_BLOCK,
                    len: 8,
                    // 第一个 hive bin 记的自身位置（相对 hive bin 数据的开头）是 0
                    check: |bytes: &[u8]| {
                        bytes.get(..4) == Some(b"hbin".as_slice()) && le32(bytes, 4) == Some(0)
                    },
                    length: BASE_BLOCK + u64::from(bins),
                },
            )) as Box<dyn Recovering>
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Regf.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn length_is_the_base_block_plus_the_hive_bins() {
        for bins in [1, 4] {
            let sample = samples::regf(bins);
            let mut data = sample.clone();
            data.extend_from_slice(&[0x5A; 3000]);
            check(&data, Some(sample.len() as u64));
            check(&sample[..sample.len() - 1], None);
        }
    }

    #[test]
    fn checksum_and_first_bin_are_checked() {
        let sample = samples::regf(2);
        let mut sum = sample.clone();
        sum[48] ^= 1; // 文件名里的一个字节
        assert!(Regf.header_check(&sum).is_none());
        let mut bin = sample.clone();
        bin[BASE_BLOCK as usize] = b'x';
        check(&bin, None);
        assert_eq!(checksum(&[0; 512]), Some(1));
    }
}
