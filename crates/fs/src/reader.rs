//! 读出一个文件：按簇（FAT、exFAT）或按 NTFS 的运行表，也可以是存放在 MFT 记录里的小文件。

use std::io::{self, Read};

use crate::FsError;
use crate::clusters::{Clusters, Run};
use crate::region::Region;

/// 文件内容的一段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    /// 分区里从 `offset` 起的 `length` 字节。
    Disk { offset: u64, length: u64 },
    /// 没有分配空间的部分（NTFS 稀疏文件的空洞），读作 0。
    Zeros(u64),
    /// 已经读进内存的内容（NTFS 存放在 MFT 记录里的小文件）。
    Bytes(Vec<u8>),
}

impl Piece {
    fn len(&self) -> u64 {
        match self {
            Self::Disk { length, .. } | Self::Zeros(length) => *length,
            Self::Bytes(bytes) => bytes.len() as u64,
        }
    }
}

/// 一个文件的内容。长度之内、有效数据长度之后的部分读作 0（exFAT、NTFS 的规则）；
/// 各段拼起来不够长度（簇链断了）时，读到断点处返回错误。
pub struct FileReader {
    region: Region,
    /// 依次拼起来就是文件内容（至多到 `size`）。
    pieces: Vec<Piece>,
    size: u64,
    valid_size: u64,
    position: u64,
    /// 当前位置所在的段，以及这一段在文件里的起点。
    cursor: usize,
    cursor_start: u64,
}

impl FileReader {
    pub(crate) fn new(clusters: &Clusters, runs: &[Run], size: u64, valid_size: u64) -> Self {
        let mut pieces = Vec::with_capacity(runs.len());
        let mut covered = 0u64;
        for run in runs {
            if covered >= size {
                break;
            }
            let Some(offset) = clusters.offset(run.first) else {
                break;
            };
            let length = (u64::from(run.count) * clusters.cluster_size).min(size - covered);
            pieces.push(Piece::Disk { offset, length });
            covered += length;
        }
        Self::from_pieces(clusters.region.clone(), pieces, size, valid_size)
    }

    /// 各段超出 `size` 的部分不会被读到。
    pub(crate) fn from_pieces(
        region: Region,
        pieces: Vec<Piece>,
        size: u64,
        valid_size: u64,
    ) -> Self {
        Self {
            region,
            pieces,
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

    /// 文件第一个字节在分区里的偏移；没有数据，或内容不在簇里时为 `None`。
    pub fn first_offset(&self) -> Option<u64> {
        match self.pieces.first() {
            Some(Piece::Disk { offset, .. }) => Some(*offset),
            _ => None,
        }
    }

    /// 各段能覆盖的长度（不超过 [`size`](Self::size)）；小于它说明链断了，之后的部分读不出来。
    pub fn readable_size(&self) -> u64 {
        self.pieces
            .iter()
            .map(Piece::len)
            .fold(0u64, u64::saturating_add)
            .min(self.size)
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
            let Some(piece) = self.pieces.get(self.cursor) else {
                return Err(FsError::ChainBroken {
                    offset: self.position,
                });
            };
            let length = piece.len();
            let inside = self.position - self.cursor_start;
            if inside >= length {
                self.cursor += 1;
                self.cursor_start = self.cursor_start.saturating_add(length);
                continue;
            }
            let take = want.min(length - inside) as usize;
            let Some(target) = buf.get_mut(..take) else {
                return Ok(0);
            };
            match piece {
                Piece::Disk { offset, .. } => {
                    let at = offset
                        .checked_add(inside)
                        .ok_or(FsError::OutOfRange { offset: *offset })?;
                    self.region.read_exact(at, target)?;
                }
                Piece::Zeros(_) => target.fill(0),
                Piece::Bytes(bytes) => {
                    let start = inside as usize;
                    let source = bytes.get(start..start + take).ok_or(FsError::ChainBroken {
                        offset: self.position,
                    })?;
                    target.copy_from_slice(source);
                }
            }
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
