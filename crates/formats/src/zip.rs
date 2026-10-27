//! ZIP：依次是各文件的本地文件头与数据、中央目录，最后是中央目录结束记录（带注释）。
//! 本地文件头里没有写大小（用数据描述符）时，往后找下一条记录的签名。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Scanner, Step, Structure, Walker};
use yisi_part::bytes::{le16, le32, le64};

const LOCAL_FILE: u32 = 0x0403_4B50;
const CENTRAL_DIRECTORY: u32 = 0x0201_4B50;
const END_OF_CENTRAL_DIRECTORY: u32 = 0x0605_4B50;
const ZIP64_END: u32 = 0x0606_4B50;
const ZIP64_LOCATOR: u32 = 0x0706_4B50;
const DATA_DESCRIPTOR: u32 = 0x0807_4B50;

/// 本地文件头标志位 3：大小写在数据后面的数据描述符里。
const FLAG_DATA_DESCRIPTOR: u16 = 1 << 3;
/// ZIP64 扩展字段的标识。
const ZIP64_EXTRA: u16 = 0x0001;

/// 大小未知时，数据之后的第一条记录：数据描述符、下一个本地文件头或中央目录。
const NEXT_RECORD: Scanner = Scanner {
    find: |window| {
        window.windows(4).position(|candidate| {
            matches!(
                candidate,
                [b'P', b'K', 7, 8] | [b'P', b'K', 3, 4] | [b'P', b'K', 1, 2]
            )
        })
    },
    overlap: 3,
};

pub struct Zip;

static INFO: FormatInfo = FormatInfo {
    extension: "zip",
    description: "ZIP archive",
    max_size: 16 << 30,
    enabled_by_default: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"PK\x03\x04",
}];

impl FileFormat for Zip {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let version_needed = le16(block, 4)?;
        let name_length = le16(block, 26)?;
        let plausible =
            le32(block, 0) == Some(LOCAL_FILE) && version_needed <= 63 && name_length > 0;
        plausible.then(|| {
            Box::new(Walker::new(ZipStructure {
                phase: Phase::Record,
            })) as Box<dyn Recovering>
        })
    }
}

#[derive(Clone, Copy)]
enum Phase {
    /// 等 4 字节记录签名。
    Record,
    LocalFile,
    /// 等本地文件头的扩展字段，从中读 ZIP64 大小。
    Zip64Extra {
        data_start: u64,
        flags: u16,
    },
    CentralDirectory,
    EndOfCentralDirectory,
    Zip64End,
}

struct ZipStructure {
    phase: Phase,
}

impl ZipStructure {
    fn record_at(&mut self, at: u64) -> Step {
        self.phase = Phase::Record;
        Step::Need { at, len: 4 }
    }

    /// 文件数据之后：有数据描述符就跳过它，接着读下一条记录。
    fn after_data(&mut self, data_end: u64) -> Step {
        self.record_at(data_end)
    }
}

impl Structure for ZipStructure {
    fn first(&self) -> Step {
        Step::Need { at: 0, len: 4 }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let (at, bytes) = match event {
            Event::Found { at } => return self.record_at(at),
            Event::Bytes { at, bytes } => (at, bytes),
        };
        match self.phase {
            Phase::Record => {
                let Some(signature) = le32(bytes, 0) else {
                    return Step::Invalid;
                };
                let (phase, len) = match signature {
                    LOCAL_FILE => (Phase::LocalFile, 30),
                    CENTRAL_DIRECTORY => (Phase::CentralDirectory, 46),
                    END_OF_CENTRAL_DIRECTORY => (Phase::EndOfCentralDirectory, 22),
                    ZIP64_END => (Phase::Zip64End, 12),
                    // 定长 20 字节，直接跳过
                    ZIP64_LOCATOR => return self.record_at(at + 20),
                    // 带签名的数据描述符：签名 + CRC + 两个 4 字节大小
                    DATA_DESCRIPTOR => return self.record_at(at + 16),
                    _ => return Step::Invalid,
                };
                self.phase = phase;
                Step::Need { at, len }
            }
            Phase::LocalFile => {
                let (Some(flags), Some(compressed), Some(name), Some(extra)) = (
                    le16(bytes, 6),
                    le32(bytes, 18),
                    le16(bytes, 26),
                    le16(bytes, 28),
                ) else {
                    return Step::Invalid;
                };
                let extra_start = at + 30 + u64::from(name);
                let data_start = extra_start + u64::from(extra);
                if compressed == u32::MAX && extra > 0 {
                    self.phase = Phase::Zip64Extra { data_start, flags };
                    return Step::Need {
                        at: extra_start,
                        len: usize::from(extra),
                    };
                }
                if flags & FLAG_DATA_DESCRIPTOR != 0 && compressed == 0 {
                    return Step::Scan {
                        from: data_start,
                        scanner: NEXT_RECORD,
                    };
                }
                self.after_data(data_start + u64::from(compressed))
            }
            Phase::Zip64Extra { data_start, flags } => {
                let Some(compressed) = zip64_compressed_size(bytes) else {
                    return Step::Invalid;
                };
                if flags & FLAG_DATA_DESCRIPTOR != 0 && compressed == 0 {
                    return Step::Scan {
                        from: data_start,
                        scanner: NEXT_RECORD,
                    };
                }
                match data_start.checked_add(compressed) {
                    Some(data_end) => self.after_data(data_end),
                    None => Step::Invalid,
                }
            }
            Phase::CentralDirectory => {
                let (Some(name), Some(extra), Some(comment)) =
                    (le16(bytes, 28), le16(bytes, 30), le16(bytes, 32))
                else {
                    return Step::Invalid;
                };
                self.record_at(at + 46 + u64::from(name) + u64::from(extra) + u64::from(comment))
            }
            Phase::EndOfCentralDirectory => match le16(bytes, 20) {
                Some(comment) => Step::Done(at + 22 + u64::from(comment)),
                None => Step::Invalid,
            },
            Phase::Zip64End => {
                // 记录长度不含签名与长度字段本身
                match le64(bytes, 4).and_then(|size| size.checked_add(at + 12)) {
                    Some(next) => self.record_at(next),
                    None => Step::Invalid,
                }
            }
        }
    }
}

/// 在扩展字段里找 ZIP64 字段（标识 0x0001），取其中的压缩后大小（原始大小之后的 8 字节）。
fn zip64_compressed_size(extra: &[u8]) -> Option<u64> {
    let mut at = 0usize;
    while let (Some(id), Some(size)) = (le16(extra, at), le16(extra, at + 2)) {
        if id == ZIP64_EXTRA {
            return le64(extra, at + 4 + 8);
        }
        at = at.checked_add(4 + usize::from(size))?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn recover(data: &[u8]) -> Box<dyn Recovering> {
        Zip.header_check(data).unwrap()
    }

    #[test]
    fn follows_local_headers_central_directory_and_comment() {
        let sample = samples::zip(&[("a.txt", 100, false), ("b.bin", 3000, false)], b"comment");
        let mut data = sample.clone();
        data.extend_from_slice(b"PK\x03\x04 next archive would start here");
        check_all_splits(|| recover(&data), &data, Some(sample.len() as u64));
    }

    #[test]
    fn data_descriptors_are_found_by_scanning() {
        let sample = samples::zip(&[("stream.dat", 2000, true), ("x", 10, false)], b"");
        check_all_splits(|| recover(&sample), &sample, Some(sample.len() as u64));
    }

    #[test]
    fn missing_end_record_is_incomplete() {
        let sample = samples::zip(&[("a.txt", 50, false)], b"");
        let truncated = &sample[..sample.len() - 10];
        check_all_splits(|| recover(truncated), truncated, None);
    }
}
