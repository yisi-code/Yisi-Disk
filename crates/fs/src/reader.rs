//! 按簇读出一个文件。

use std::io::{self, Read};

use crate::FsError;
use crate::clusters::{Clusters, Run};
use crate::region::Region;

/// 一个文件的内容。长度之内、有效数据长度之后的部分读作 0（exFAT 规范）；
/// 簇链在长度之前就断了时，读到断点处返回错误。
pub struct FileReader {
    region: Region,
    /// 各段在分区里的字节偏移与长度，依次拼起来就是文件内容（至多到 `size`）。
    extents: Vec<(u64, u64)>,
    size: u64,
    valid_size: u64,
    position: u64,
    /// 当前位置所在的段，以及这一段在文件里的起点。
    cursor: usize,
    cursor_start: u64,
}

impl FileReader {
    pub(crate) fn new(clusters: &Clusters, runs: &[Run], size: u64, valid_size: u64) -> Self {
        let mut extents = Vec::with_capacity(runs.len());
        let mut covered = 0u64;
        for run in runs {
            if covered >= size {
                break;
            }
            let Some(offset) = clusters.offset(run.first) else {
                break;
            };
            let length = (u64::from(run.count) * clusters.cluster_size).min(size - covered);
            extents.push((offset, length));
            covered += length;
        }
        Self {
            region: clusters.region.clone(),
            extents,
            size,
            valid_size: valid_size.min(size),
            position: 0,
            cursor: 0,
            cursor_start: 0,
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// 文件第一个字节在分区里的偏移；没有数据时为 `None`。
    pub fn first_offset(&self) -> Option<u64> {
        self.extents.first().map(|&(offset, _)| offset)
    }

    /// 簇链能覆盖的长度；小于 [`size`](Self::size) 说明链断了，之后的部分读不出来。
    pub fn readable_size(&self) -> u64 {
        self.extents.iter().map(|&(_, length)| length).sum()
    }

    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, FsError> {
        if self.position >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(self.size - self.position);
        if self.position >= self.valid_size {
            let zeros = want as usize;
            if let Some(target) = buf.get_mut(..zeros) {
                target.fill(0);
            }
            self.position += want;
            return Ok(zeros);
        }
        let want = want.min(self.valid_size - self.position);
        loop {
            let Some(&(offset, length)) = self.extents.get(self.cursor) else {
                return Err(FsError::ChainBroken {
                    offset: self.position,
                });
            };
            let inside = self.position - self.cursor_start;
            if inside >= length {
                self.cursor += 1;
                self.cursor_start += length;
                continue;
            }
            let take = want.min(length - inside) as usize;
            let Some(target) = buf.get_mut(..take) else {
                return Ok(0);
            };
            self.region.read_exact(offset + inside, target)?;
            self.position += take as u64;
            return Ok(take);
        }
    }
}

impl Read for FileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_some(buf).map_err(io::Error::other)
    }
}
