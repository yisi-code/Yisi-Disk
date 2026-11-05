//! 7z（7-Zip 的格式说明 7zFormat.txt）：32 字节签名头——6 字节签名、2 字节版本、
//! 起始头的 CRC32（校验后面 20 字节），下一头（文件列表）的偏移、长度与 CRC32。
//! 下一头在压缩数据之后、是文件的最后一部分，所以文件长度是 32 + 偏移 + 长度。
//! 下一头不太大时读出来核对 CRC，确认没有截短、没有认错。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, MAX_NEED, Step, Structure, Walker};
use yisi_part::bytes::{le32, le64};
use yisi_part::crc32::crc32;

const MAGIC: &[u8] = b"7z\xBC\xAF\x27\x1C";
const SIGNATURE_HEADER_LEN: u64 = 32;

pub struct SevenZip;

static INFO: FormatInfo = FormatInfo {
    extension: "7z",
    variants: &[],
    description: "7-Zip archive",
    max_size: 64 << 30,
    enabled_by_default: true,
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: MAGIC,
}];

impl FileFormat for SevenZip {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let major = *block.get(6)?;
        let start_header = block.get(12..32)?;
        if major != 0 || le32(block, 8)? != crc32(&[start_header]) {
            return None;
        }
        let offset = le64(block, 12)?;
        let size = le64(block, 20)?;
        let next_crc = le32(block, 28)?;
        let header_at = SIGNATURE_HEADER_LEN.checked_add(offset)?;
        let end = header_at.checked_add(size)?;
        Some(Box::new(Walker::new(
            INFO.max_size,
            SevenZipStructure {
                header_at,
                size,
                next_crc,
                end,
            },
        )))
    }
}

struct SevenZipStructure {
    header_at: u64,
    size: u64,
    next_crc: u32,
    end: u64,
}

impl Structure for SevenZipStructure {
    fn first(&self) -> Step {
        match usize::try_from(self.size) {
            Ok(len @ 1..=MAX_NEED) => Step::Need {
                at: self.header_at,
                len,
            },
            // 空压缩包（没有下一头），或下一头太大不读：按长度
            _ => Step::Done(self.end),
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        match event {
            Event::Bytes { bytes, .. } if crc32(&[bytes]) == self.next_crc => Step::Done(self.end),
            _ => Step::Invalid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    #[test]
    fn length_is_the_signature_header_plus_the_next_header_end() {
        for (packed, header) in [(1000, 60), (0, 0), (20_000, 300)] {
            let sample = samples::sevenzip(packed, header);
            let mut data = sample.clone();
            data.extend_from_slice(b"7z\xBC\xAF\x27\x1C and then something else");
            check_all_splits(
                || SevenZip.header_check(&data).unwrap(),
                &data,
                Some(sample.len() as u64),
            );
        }
    }

    #[test]
    fn crc_mismatches_and_truncation_are_rejected() {
        let sample = samples::sevenzip(500, 40);
        let mut start = sample.clone();
        start[15] ^= 1;
        assert!(SevenZip.header_check(&start).is_none(), "起始头 CRC 不对");

        let mut next = sample.clone();
        let last = next.len() - 1;
        next[last] ^= 1;
        check_all_splits(|| SevenZip.header_check(&next).unwrap(), &next, None);

        let truncated = &sample[..sample.len() - 5];
        check_all_splits(
            || SevenZip.header_check(truncated).unwrap(),
            truncated,
            None,
        );
    }
}
