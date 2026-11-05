//! 非常驻属性的内容：按运行表把虚拟簇换成分区里的位置来读，稀疏的部分读作 0。

use super::record::Extent;
use crate::FsError;
use crate::reader::Piece;
use crate::region::{Chunks, Region};

pub(crate) struct Stream {
    region: Region,
    cluster_size: u64,
    extents: Vec<Extent>,
    /// 内容的长度（属性记的数据长度）。
    size: u64,
    /// [`read_cached`](Self::read_cached) 的分块缓存。
    cache: Chunks,
}

impl Stream {
    pub fn new(region: Region, cluster_size: u64, extents: Vec<Extent>, size: u64) -> Self {
        Self {
            region,
            cluster_size,
            extents,
            size,
            cache: Chunks::new(size),
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
        if self
            .cache
            .read(offset, buf, |begin, data| self.read_at(begin, data))?
        {
            Ok(())
        } else {
            Err(FsError::OutOfRange { offset })
        }
    }

    /// 压缩的属性：每 `unit` 个簇是一个压缩单元。单元里的簇全分配了是原样存放；全是稀疏的读作 0；
    /// 分配了一部分、后面是稀疏的，分配的那几个簇里是 LZNT1 压缩的内容，读时解开。
    /// 运行表覆盖不到的单元不给，读到那里时报簇链中断。
    pub fn compressed_pieces(&self, size: u64, unit: u64) -> Vec<Piece> {
        let mut pieces = Vec::new();
        let Some(unit_bytes) = unit.checked_mul(self.cluster_size) else {
            return pieces;
        };
        let mut covered = 0u64;
        while covered < size {
            let length = unit_bytes.min(size - covered);
            let first = covered / self.cluster_size;
            let end = first + unit;
            // 这个单元里分配了的簇（按分区里的位置合并相邻的）、有没有稀疏的
            let mut sources: Vec<(u64, u64)> = Vec::new();
            let mut allocated = 0u64;
            let mut sparse = false;
            let mut vcn = first;
            while vcn < end {
                let Some(extent) = self.extent_at(vcn) else {
                    break;
                };
                let take = (extent.vcn + extent.length - vcn).min(end - vcn);
                match extent.lcn {
                    Some(lcn) => {
                        let Some(offset) = lcn
                            .checked_add(vcn - extent.vcn)
                            .and_then(|cluster| cluster.checked_mul(self.cluster_size))
                        else {
                            return pieces;
                        };
                        let bytes = take * self.cluster_size;
                        match sources.last_mut() {
                            Some((at, len)) if *at + *len == offset => *len += bytes,
                            _ => sources.push((offset, bytes)),
                        }
                        allocated += take;
                    }
                    None => sparse = true,
                }
                vcn += take;
            }
            if sparse && allocated == 0 {
                pieces.push(Piece::Zeros(length));
            } else if sparse {
                pieces.push(Piece::Lznt1 { sources, length });
            } else if allocated * self.cluster_size >= length {
                // 原样存放：按分区里的位置逐段给出，截到这个单元的长度
                let mut left = length;
                for (offset, bytes) in sources {
                    let take = bytes.min(left);
                    pieces.push(Piece::Disk {
                        offset,
                        length: take,
                    });
                    left -= take;
                    if left == 0 {
                        break;
                    }
                }
            } else {
                break;
            }
            covered += length;
        }
        pieces
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
