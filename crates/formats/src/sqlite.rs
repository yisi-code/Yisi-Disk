//! SQLite 数据库（SQLite 文件格式说明第 1.3 节）：100 字节的库头，以 `SQLite format 3\0` 开头。
//! 库头记着页大小（偏移 16，大端 2 字节，1 表示 65536）与库的页数（偏移 28）；页数只在
//! 「有效版本号」（偏移 92）等于修改计数（偏移 24）时可信（3.7.0 以后的版本都这样写），
//! 文件长度就是页大小乘页数。页数不可信的老库不认，免得猜错长度。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{KnownLength, Walker};
use yisi_part::bytes::{be16, be32};

const MAGIC: &[u8] = b"SQLite format 3\0";

pub struct Sqlite;

static INFO: FormatInfo = FormatInfo {
    extension: "sqlite",
    variants: &[],
    description: "SQLite database",
    max_size: 16 << 30,
    enabled_by_default: true,
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: MAGIC,
}];

/// 库头说的文件长度，各项都合规时才给出。
fn database_length(block: &[u8]) -> Option<u64> {
    let page_size = match be16(block, 16)? {
        1 => 65536,
        size => u32::from(size),
    };
    let write_version = *block.get(18)?;
    let read_version = *block.get(19)?;
    // 规范要求这三项固定为 64、32、32
    let fractions = block.get(21..24)? == [64, 32, 32];
    let plausible = page_size.is_power_of_two()
        && (512..=65536).contains(&page_size)
        && matches!(write_version, 1 | 2)
        && matches!(read_version, 1 | 2)
        && fractions;
    let pages = be32(block, 28)?;
    let trusted = be32(block, 92)? == be32(block, 24)?;
    (plausible && trusted && pages > 0).then(|| u64::from(page_size) * u64::from(pages))
}

impl FileFormat for Sqlite {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let length = database_length(block)?;
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
    fn length_is_page_size_times_page_count() {
        for (page_size, pages) in [(4096u32, 3u32), (1024, 10), (65536, 2)] {
            let sample = samples::sqlite(page_size, pages);
            assert_eq!(sample.len() as u64, u64::from(page_size) * u64::from(pages));
            let mut data = sample.clone();
            data.extend_from_slice(b"next file");
            check_all_splits(
                || Sqlite.header_check(&data).unwrap(),
                &data,
                Some(sample.len() as u64),
            );
        }
    }

    #[test]
    fn untrusted_or_odd_headers_are_rejected() {
        let sample = samples::sqlite(4096, 3);
        let changed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            Sqlite.header_check(&data).is_none()
        };
        assert!(changed(92, &[0, 0, 0, 99]), "页数不可信");
        assert!(changed(16, &[0x0C, 0x00]), "页大小不是 2 的幂");
        assert!(changed(21, &[65]), "固定字段不对");
        assert!(changed(28, &[0, 0, 0, 0]), "页数为 0");
    }
}
