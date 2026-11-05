//! Windows 的系统压缩（WOF，`compact /exe`、CompactOS）：文件带标记为 0x80000017 的重分析点，
//! 无名数据流是长度为原文件大小的稀疏流，内容压缩后放在名为 `WofCompressedData` 的备用数据流里。
//!
//! 重分析点的内容：标记（4 字节）、长度与保留（各 2 字节），WOF 外部信息（版本 1，提供者 2 = 单个文件；
//! 提供者 1 是 WIMBoot，内容在 WIM 文件里，读不出），文件提供者信息（版本 1，算法：0 XPRESS4K、1 LZX、
//! 2 XPRESS8K、3 XPRESS16K）。
//!
//! 压缩流：原文件按块大小（XPRESS4K 4 KiB、XPRESS8K 8 KiB、XPRESS16K 16 KiB、LZX 32 KiB）分块，各块单独压缩；
//! 开头是块数减一个偏移（原文件不到 4 GiB 时各 4 字节，否则 8 字节），给出第 2 块起各块从偏移表之后多远开始。
//! 一块压缩后不比原来小时原样存放（压缩后长度等于原长度）。

use super::{lzx, xpress};
use crate::FsError;
use crate::reader::Pieces;
use yisi_part::bytes::{le32, le64};

/// 内容所在的备用数据流的名字。
pub(crate) const WOF_STREAM: &str = "WofCompressedData";
/// 重分析点内容里 WOF 各字段的位置。
const WOF_VERSION_AT: usize = 8;
const WOF_PROVIDER_AT: usize = 12;
const FILE_VERSION_AT: usize = 16;
const ALGORITHM_AT: usize = 20;
/// 提供者「单个文件」。
const PROVIDER_FILE: u32 = 2;
/// 偏移表最多读这么多字节（64 MiB 的表对应 4 KiB 一块、64 GiB 的文件）。
const MAX_TABLE: u64 = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WofAlgorithm {
    Xpress4K,
    Lzx,
    Xpress8K,
    Xpress16K,
}

impl WofAlgorithm {
    fn chunk_size(self) -> u64 {
        match self {
            Self::Xpress4K => 4 << 10,
            Self::Xpress8K => 8 << 10,
            Self::Xpress16K => 16 << 10,
            Self::Lzx => 32 << 10,
        }
    }

    fn decompress(self, input: &[u8], output: &mut [u8]) -> Option<()> {
        match self {
            Self::Lzx => lzx::decompress(input, output),
            _ => xpress::decompress(input, output),
        }
    }
}

/// 重分析点的内容是 WOF「单个文件」压缩时，给出算法；WIMBoot、不认识的版本或算法为 `None`（读不出）。
pub(crate) fn wof_algorithm(reparse: &[u8]) -> Option<WofAlgorithm> {
    let field = |at| le32(reparse, at);
    if field(0)? != super::TAG_WOF
        || field(WOF_VERSION_AT)? != 1
        || field(WOF_PROVIDER_AT)? != PROVIDER_FILE
        || field(FILE_VERSION_AT)? != 1
    {
        return None;
    }
    match field(ALGORITHM_AT)? {
        0 => Some(WofAlgorithm::Xpress4K),
        1 => Some(WofAlgorithm::Lzx),
        2 => Some(WofAlgorithm::Xpress8K),
        3 => Some(WofAlgorithm::Xpress16K),
        _ => None,
    }
}

/// 从压缩流里按块解开的读取器。最近解开的一块留着，连着读同一块时只解一次。
pub(crate) struct WofReader {
    compressed: Pieces,
    algorithm: WofAlgorithm,
    size: u64,
    /// 各块在压缩流里的起点，最后再加一个压缩流的结尾。
    starts: Vec<u64>,
    position: u64,
    chunk: Option<(usize, Vec<u8>)>,
}

impl WofReader {
    /// `size` 是原文件的大小（无名数据流的长度）。偏移表读不出或不自洽时返回错误。
    pub(crate) fn new(
        mut compressed: Pieces,
        algorithm: WofAlgorithm,
        size: u64,
    ) -> Result<Self, FsError> {
        let broken = || FsError::Invalid("系统压缩（WOF）的块偏移表不对");
        let chunk = algorithm.chunk_size();
        let count = size.div_ceil(chunk);
        let entry: u64 = if size > u64::from(u32::MAX) { 8 } else { 4 };
        let table_len = count.saturating_sub(1) * entry;
        if table_len > MAX_TABLE || table_len > compressed.size() {
            return Err(broken());
        }
        let mut table = vec![0u8; table_len as usize];
        if !table.is_empty() {
            compressed.read_exact_at(0, &mut table)?;
        }
        let mut starts = Vec::with_capacity(count as usize + 1);
        if count > 0 {
            starts.push(table_len);
        }
        for index in 0..count.saturating_sub(1) as usize {
            let offset = if entry == 8 {
                le64(&table, index * 8)
            } else {
                le32(&table, index * 4).map(u64::from)
            }
            .ok_or_else(broken)?;
            starts.push(table_len.checked_add(offset).ok_or_else(broken)?);
        }
        starts.push(compressed.size());
        if !starts.is_sorted() {
            return Err(broken());
        }
        Ok(Self {
            compressed,
            algorithm,
            size,
            starts,
            position: 0,
            chunk: None,
        })
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn first_offset(&self) -> Option<u64> {
        self.compressed.first_offset()
    }

    /// 压缩流读得全时是整个文件；压缩流的簇链断了时，到最后一个完整读得出的块为止。
    pub(crate) fn readable_size(&self) -> u64 {
        let readable = self.compressed.readable_size();
        if readable >= self.compressed.size() {
            return self.size;
        }
        // 第 n 块读得出，要它的结尾（下一块的起点）不超过读得出的长度
        let chunks = self
            .starts
            .iter()
            .skip(1)
            .take_while(|&&end| end <= readable)
            .count() as u64;
        (chunks * self.algorithm.chunk_size()).min(self.size)
    }

    /// 第 `index` 块解开后的内容。
    fn chunk(&mut self, index: usize) -> Result<&[u8], FsError> {
        if self.chunk.as_ref().map(|(cached, _)| *cached) != Some(index) {
            let chunk_size = self.algorithm.chunk_size();
            let length = chunk_size.min(self.size - index as u64 * chunk_size) as usize;
            let (Some(&start), Some(&end)) = (self.starts.get(index), self.starts.get(index + 1))
            else {
                return Err(FsError::Invalid("系统压缩（WOF）的块超出了偏移表"));
            };
            let packed_length = (end - start) as usize;
            if packed_length > length {
                return Err(FsError::Invalid("系统压缩（WOF）的一块比原来还长"));
            }
            let mut packed = vec![0u8; packed_length];
            self.compressed.read_exact_at(start, &mut packed)?;
            let unpacked = if packed_length == length {
                packed
            } else {
                let mut unpacked = vec![0u8; length];
                self.algorithm
                    .decompress(&packed, &mut unpacked)
                    .ok_or(FsError::Invalid("系统压缩（WOF）的内容解不开"))?;
                unpacked
            };
            self.chunk = Some((index, unpacked));
        }
        Ok(self
            .chunk
            .as_ref()
            .map(|(_, data)| data.as_slice())
            .unwrap_or_default())
    }

    pub(crate) fn read(&mut self, buf: &mut [u8]) -> Result<usize, FsError> {
        if self.position >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let chunk_size = self.algorithm.chunk_size();
        let index = (self.position / chunk_size) as usize;
        let inside = (self.position % chunk_size) as usize;
        let data = self.chunk(index)?;
        let available = data.get(inside..).unwrap_or_default();
        let take = available.len().min(buf.len());
        let (Some(target), Some(source)) = (buf.get_mut(..take), available.get(..take)) else {
            return Ok(0);
        };
        target.copy_from_slice(source);
        self.position += take as u64;
        Ok(take)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reparse(provider: u32, algorithm: u32) -> Vec<u8> {
        let mut data = vec![0u8; 24];
        data[0..4].copy_from_slice(&super::super::TAG_WOF.to_le_bytes());
        data[4..6].copy_from_slice(&16u16.to_le_bytes());
        data[8..12].copy_from_slice(&1u32.to_le_bytes());
        data[12..16].copy_from_slice(&provider.to_le_bytes());
        data[16..20].copy_from_slice(&1u32.to_le_bytes());
        data[20..24].copy_from_slice(&algorithm.to_le_bytes());
        data
    }

    #[test]
    fn the_algorithm_comes_from_the_reparse_point() {
        assert_eq!(wof_algorithm(&reparse(2, 0)), Some(WofAlgorithm::Xpress4K));
        assert_eq!(wof_algorithm(&reparse(2, 1)), Some(WofAlgorithm::Lzx));
        assert_eq!(wof_algorithm(&reparse(2, 2)), Some(WofAlgorithm::Xpress8K));
        assert_eq!(wof_algorithm(&reparse(2, 3)), Some(WofAlgorithm::Xpress16K));
        // WIMBoot、不认识的算法、截短的内容
        assert_eq!(wof_algorithm(&reparse(1, 0)), None);
        assert_eq!(wof_algorithm(&reparse(2, 9)), None);
        assert_eq!(wof_algorithm(&reparse(2, 0)[..20]), None);
    }

    fn reader(packed: Vec<u8>, algorithm: WofAlgorithm) -> Result<WofReader, FsError> {
        use std::sync::Arc;

        use crate::reader::{FileReader, Piece};
        use crate::region::Region;
        use crate::samples::{MemoryDisk, WOF_PLAIN_LEN};
        let region = Region::new(Arc::new(MemoryDisk::new(vec![0u8; 512], 512)), 0, 512);
        let length = packed.len() as u64;
        let pieces = FileReader::from_pieces(region, vec![Piece::Bytes(packed)], length, length)
            .into_pieces()
            .unwrap();
        WofReader::new(pieces, algorithm, WOF_PLAIN_LEN as u64)
    }

    fn read_to_end(reader: &mut WofReader) -> Result<Vec<u8>, FsError> {
        let mut data = Vec::new();
        let mut buffer = vec![0u8; 5000];
        loop {
            match reader.read(&mut buffer)? {
                0 => return Ok(data),
                read => data.extend_from_slice(&buffer[..read]),
            }
        }
    }

    /// 压缩流被改坏、截短时报错或读出别的内容，不能越界、死循环或崩溃。
    #[test]
    fn damaged_streams_fail_without_panicking() {
        use crate::samples::{LZNT1_PLAIN, WOF_PACKED, WOF_PLAIN_LEN};
        for (code, packed) in WOF_PACKED {
            let algorithm = wof_algorithm(&reparse(2, code)).unwrap();
            let intact = read_to_end(&mut reader(packed.to_vec(), algorithm).unwrap()).unwrap();
            assert!(intact == LZNT1_PLAIN[..WOF_PLAIN_LEN], "{algorithm:?}");
            let mut seed = 0x9E37_79B9_u32.wrapping_add(code);
            for round in 0..200 {
                let mut damaged = packed.to_vec();
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let at = seed as usize % damaged.len();
                if round % 4 == 0 {
                    damaged.truncate(at);
                } else {
                    for index in at..(at + 1 + round % 7).min(damaged.len()) {
                        damaged[index] ^= (seed >> 8) as u8 | 1;
                    }
                }
                if let Ok(mut reader) = reader(damaged, algorithm) {
                    let _ = read_to_end(&mut reader);
                }
            }
        }
    }
}
