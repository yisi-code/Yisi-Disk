//! 非常驻属性的内容：按运行表把虚拟簇换成分区里的位置来读，稀疏的部分读作 0。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use super::record::Extent;
use crate::FsError;
use crate::reader::Piece;
use crate::region::Region;

/// 缓存按 64 KiB 一块，最多 512 块（32 MiB），满了整个清空。
const CHUNK: u64 = 64 * 1024;
const MAX_CHUNKS: usize = 512;

pub(crate) struct Stream {
    region: Region,
    cluster_size: u64,
    extents: Vec<Extent>,
    /// 内容的长度（属性记的数据长度）。
    size: u64,
    chunks: Mutex<HashMap<u64, Arc<[u8]>>>,
}

impl Stream {
    pub fn new(region: Region, cluster_size: u64, extents: Vec<Extent>, size: u64) -> Self {
        Self {
            region,
            cluster_size,
            extents,
            size,
            chunks: Mutex::new(HashMap::new()),
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// 内容里第 `offset` 个字节在分区里的位置；稀疏或超出运行表时为 `None`。
    pub fn disk_offset(&self, offset: u64) -> Option<u64> {
        let vcn = offset / self.cluster_size;
        let extent = self.extent_at(vcn)?;
        let lcn = extent.lcn?.checked_add(vcn - extent.vcn)?;
        lcn.checked_mul(self.cluster_size)?
            .checked_add(offset % self.cluster_size)
    }

    fn extent_at(&self, vcn: u64) -> Option<&Extent> {
        let index = self
            .extents
            .partition_point(|extent| extent.vcn.saturating_add(extent.length) <= vcn);
        self.extents
            .get(index)
            .filter(|extent| extent.vcn <= vcn && vcn - extent.vcn < extent.length)
    }

    /// 读满 `buf`：超出内容长度、或运行表没有覆盖到时返回错误。
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|&end| end <= self.size)
            .ok_or(FsError::OutOfRange { offset })?;
        let mut position = offset;
        while position < end {
            let vcn = position / self.cluster_size;
            let extent = self
                .extent_at(vcn)
                .ok_or(FsError::ChainBroken { offset: position })?;
            let extent_end = (extent.vcn + extent.length)
                .checked_mul(self.cluster_size)
                .ok_or(FsError::OutOfRange { offset: position })?;
            let take = end.min(extent_end) - position;
            let filled = (position - offset) as usize;
            let Some(target) = buf.get_mut(filled..filled + take as usize) else {
                return Err(FsError::OutOfRange { offset: position });
            };
            match extent.lcn {
                Some(lcn) => {
                    let at = lcn
                        .checked_add(vcn - extent.vcn)
                        .and_then(|cluster| cluster.checked_mul(self.cluster_size))
                        .and_then(|bytes| bytes.checked_add(position % self.cluster_size))
                        .ok_or(FsError::OutOfRange { offset: position })?;
                    self.region.read_exact(at, target)?;
                }
                None => target.fill(0),
            }
            position += take;
        }
        Ok(())
    }

    /// 与 [`read_at`](Self::read_at) 相同，经分块缓存读：适合反复读附近的小块（MFT 记录、分配位图）。
    pub fn read_cached(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|&end| end <= self.size)
            .ok_or(FsError::OutOfRange { offset })?;
        let mut position = offset;
        while position < end {
            let index = position / CHUNK;
            let chunk = self.chunk(index)?;
            let inside = (position - index * CHUNK) as usize;
            let take = ((end - position) as usize).min(chunk.len().saturating_sub(inside));
            let filled = (position - offset) as usize;
            let (Some(source), Some(target)) = (
                chunk.get(inside..inside + take),
                buf.get_mut(filled..filled + take),
            ) else {
                return Err(FsError::OutOfRange { offset: position });
            };
            if take == 0 {
                return Err(FsError::OutOfRange { offset: position });
            }
            target.copy_from_slice(source);
            position += take as u64;
        }
        Ok(())
    }

    fn chunk(&self, index: u64) -> Result<Arc<[u8]>, FsError> {
        if let Some(chunk) = self
            .chunks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&index)
        {
            return Ok(Arc::clone(chunk));
        }
        let begin = index * CHUNK;
        let len = CHUNK.min(self.size.saturating_sub(begin));
        let mut data = vec![0u8; len as usize];
        self.read_at(begin, &mut data)?;
        let chunk: Arc<[u8]> = data.into();
        let mut chunks = self.chunks.lock().unwrap_or_else(PoisonError::into_inner);
        if chunks.len() >= MAX_CHUNKS {
            chunks.clear();
        }
        chunks.insert(index, Arc::clone(&chunk));
        Ok(chunk)
    }

    /// 把前 `size` 字节换成文件读取用的片段。运行表覆盖不到的部分不给，读到那里时报簇链中断。
    pub fn pieces(&self, size: u64) -> Vec<Piece> {
        let mut pieces = Vec::with_capacity(self.extents.len());
        let mut covered = 0u64;
        for extent in &self.extents {
            if covered >= size || extent.vcn.checked_mul(self.cluster_size) != Some(covered) {
                break;
            }
            let Some(bytes) = extent.length.checked_mul(self.cluster_size) else {
                break;
            };
            let length = bytes.min(size - covered);
            match extent
                .lcn
                .and_then(|lcn| lcn.checked_mul(self.cluster_size))
            {
                Some(offset) => pieces.push(Piece::Disk { offset, length }),
                None if extent.lcn.is_none() => pieces.push(Piece::Zeros(length)),
                None => break,
            }
            covered += length;
        }
        pieces
    }
}
