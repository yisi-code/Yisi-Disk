//! Windows 事件日志 EVTX：4096 字节的文件头块，开头 128 字节是文件头（`ElfFile\0`、首末块号、下一个记录号、
//! 头长度 128、版本 3.1 / 3.2、头块长度 4096、块数……第 124 字节是前 120 字节的 CRC-32），
//! 之后是一个个 65536 字节、以 `ElfChnk\0` 开头的块。文件长度是 4096 + 块数 × 65536；
//! 读最后一块的开头核对它确实是块。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{CheckedLength, Walker};
use yisi_part::bytes::{le16, le32};
use yisi_part::crc32::crc32;

const MAGIC: &[u8] = b"ElfFile\0";
const CHUNK_MAGIC: &[u8] = b"ElfChnk\0";
const HEADER_BLOCK: u64 = 4096;
const CHUNK_LEN: u64 = 65536;
const HEADER_LEN: u32 = 128;
const CRC_COVERS: usize = 120;
const CRC_AT: usize = 124;

pub struct Evtx;

static INFO: FormatInfo = FormatInfo {
    extension: "evtx",
    variants: &[],
    description: "Windows event log",
    max_size: 4 << 30,
    enabled_by_default: true,
    nests_files: false,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: MAGIC,
}];

impl FileFormat for Evtx {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let chunks = le16(block, 42)?;
        let plausible = le32(block, 32)? == HEADER_LEN
            && matches!(le16(block, 36)?, 1 | 2)
            && le16(block, 38)? == 3
            && u64::from(le16(block, 40)?) == HEADER_BLOCK
            && chunks >= 1
            && le32(block, CRC_AT)? == crc32(&[block.get(..CRC_COVERS)?]);
        plausible.then(|| {
            let last_chunk = HEADER_BLOCK + (u64::from(chunks) - 1) * CHUNK_LEN;
            Box::new(Walker::new(
                INFO.max_size,
                CheckedLength {
                    at: last_chunk,
                    len: CHUNK_MAGIC.len(),
                    check: |bytes: &[u8]| bytes == CHUNK_MAGIC,
                    length: last_chunk + CHUNK_LEN,
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
        check_all_splits(|| Evtx.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn length_is_the_header_block_plus_the_chunks() {
        for chunks in [1, 3] {
            let sample = samples::evtx(chunks);
            assert_eq!(
                sample.len() as u64,
                HEADER_BLOCK + u64::from(chunks) * CHUNK_LEN
            );
            let mut data = sample.clone();
            data.extend_from_slice(b"ElfFile\0 and more");
            check(&data, Some(sample.len() as u64));
            check(&sample[..sample.len() - 1], None);
        }
    }

    #[test]
    fn crc_and_last_chunk_are_checked() {
        let sample = samples::evtx(2);
        let mut crc = sample.clone();
        crc[42] = 1; // 块数在 CRC 覆盖的范围里
        assert!(Evtx.header_check(&crc).is_none());
        let mut last = sample.clone();
        last[(HEADER_BLOCK + CHUNK_LEN) as usize] = b'X';
        check(&last, None);
    }
}
