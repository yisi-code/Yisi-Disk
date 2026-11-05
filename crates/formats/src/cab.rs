//! CAB（微软 Cabinet 文件格式说明）：36 字节的 CFHEADER，以 `MSCF` 开头，记着整个文件的长度（cbCabinet）、
//! 第一个文件项（CFFILE）的位置、版本 1.3、文件夹数与文件数、标志。长度写在头里；其余各项只用来排除误识别：
//! 三处保留字为 0、版本对、文件夹与文件都有、文件项在长度之内；第一个文件夹项（CFFOLDER）的压缩方式是规定的几种、
//! 数据在长度之内；第一个文件项所属的文件夹存在、属性位都有定义。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{CheckedLength, Walker};
use yisi_part::bytes::{le16, le32};

const HEADER_LEN: u32 = 36;
const FOLDER_LEN: u32 = 8;
/// 文件项的定长部分，之后是以 0 结尾的文件名。
const FILE_ENTRY_LEN: usize = 16;
/// 标志：有上一个 / 下一个分卷（头后面多了几个字符串）、头里带预留区。
const FLAG_PREV: u16 = 0x0001;
const FLAG_NEXT: u16 = 0x0002;
const FLAG_RESERVE: u16 = 0x0004;
/// 压缩方式的低 4 位：不压缩、MSZIP、Quantum、LZX。
const MAX_COMPRESSION: u16 = 3;
/// 文件项属性：只读、隐藏、系统、存档、执行、名字是 UTF-8。
const KNOWN_ATTRIBUTES: u16 = 0x00E7;
/// 文件项的文件夹序号可以是这几个特殊值（跨分卷的文件）。
const FOLDER_CONTINUED: u16 = 0xFFFD;

pub struct Cab;

static INFO: FormatInfo = FormatInfo {
    extension: "cab",
    variants: &[],
    description: "Windows cabinet archive",
    max_size: 2 << 30,
    enabled_by_default: true,
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"MSCF\0\0\0\0",
}];

impl FileFormat for Cab {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let length = le32(block, 8)?;
        let files_at = le32(block, 16)?;
        let folders = le16(block, 26)?;
        let files = le16(block, 28)?;
        let flags = le16(block, 30)?;
        let plausible = le32(block, 12)? == 0
            && le32(block, 20)? == 0
            && block.get(24..26) == Some([3, 1].as_slice())
            && folders >= 1
            && files >= 1
            && flags & !(FLAG_PREV | FLAG_NEXT | FLAG_RESERVE) == 0
            && u64::from(files_at) >= u64::from(HEADER_LEN + FOLDER_LEN * u32::from(folders))
            && u64::from(files_at) + FILE_ENTRY_LEN as u64 * u64::from(files) <= u64::from(length);
        if !plausible {
            return None;
        }
        // 没有预留区与分卷名时，第一个文件夹项紧跟在头后面
        if flags == 0 {
            let data_at = le32(block, 36)?;
            let compression = le16(block, 42)?;
            if data_at < files_at || data_at >= length || compression & 0x0F > MAX_COMPRESSION {
                return None;
            }
        }
        Some(Box::new(Walker::new(
            INFO.max_size,
            CheckedLength {
                at: u64::from(files_at),
                len: FILE_ENTRY_LEN,
                check: move |entry: &[u8]| file_entry_ok(entry, folders),
                length: u64::from(length),
            },
        )))
    }
}

/// 第一个文件项：所属的文件夹存在（或是跨分卷的特殊值），属性位都有定义。
fn file_entry_ok(entry: &[u8], folders: u16) -> bool {
    let (Some(folder), Some(attributes)) = (le16(entry, 8), le16(entry, 14)) else {
        return false;
    };
    (folder < folders || folder >= FOLDER_CONTINUED) && attributes & !KNOWN_ATTRIBUTES == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Cab.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn length_comes_from_the_cabinet_header() {
        for (files, data_len) in [(1, 10), (5, 4000)] {
            let sample = samples::cab(files, data_len);
            let mut data = sample.clone();
            data.extend_from_slice(b"MSCF\0\0\0\0 and something else");
            check(&data, Some(sample.len() as u64));
            check(&sample[..sample.len() - 1], None);
        }
    }

    #[test]
    fn implausible_headers_and_file_entries_are_rejected() {
        let sample = samples::cab(2, 100);
        let changed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            data
        };
        assert!(
            Cab.header_check(&changed(12, &[1])).is_none(),
            "保留字不是 0"
        );
        assert!(
            Cab.header_check(&changed(24, &[2, 1])).is_none(),
            "版本不对"
        );
        assert!(
            Cab.header_check(&changed(28, &[0, 0])).is_none(),
            "没有文件"
        );
        assert!(
            Cab.header_check(&changed(42, &[7, 0])).is_none(),
            "压缩方式不对"
        );
        assert!(
            Cab.header_check(&changed(8, &[40, 0, 0, 0])).is_none(),
            "长度装不下文件项"
        );
        // 第一个文件项的文件夹不存在
        let files_at = le32(&sample, 16).unwrap() as usize;
        check(&changed(files_at + 8, &[5, 0]), None);
    }
}
