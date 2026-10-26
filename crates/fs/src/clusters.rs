//! FAT 与 exFAT 共用的簇区与 FAT 表：簇号换算成位置、沿 FAT 链找出文件占用的簇。

use crate::FsError;
use crate::region::{ChunkCache, Region};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FatWidth {
    Fat12,
    Fat16,
    Fat32,
    Exfat,
}

/// FAT 表项的含义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Link {
    Next(u32),
    End,
    Free,
    /// 坏簇标记，或指向簇区之外的值。
    Broken,
}

/// 连续的若干簇。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Run {
    pub first: u32,
    pub count: u32,
}

/// 沿 FAT 链找到的簇。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chain {
    pub runs: Vec<Run>,
    /// 找满了要求的簇数，或者链正常结束；链在中途断了（空闲、坏簇、越界、成环）时为 `false`。
    pub complete: bool,
}

impl Chain {
    pub fn clusters(&self) -> impl Iterator<Item = u32> + '_ {
        self.runs
            .iter()
            .flat_map(|run| (0..run.count).filter_map(move |step| run.first.checked_add(step)))
    }

    pub fn len(&self) -> u64 {
        self.runs.iter().map(|run| u64::from(run.count)).sum()
    }
}

pub(crate) struct Clusters {
    pub region: Region,
    /// 2 号簇在分区里的字节偏移。
    pub heap_offset: u64,
    pub cluster_size: u64,
    /// 簇数；合法的簇号是 2 到 `count + 1`。
    pub count: u32,
    pub width: FatWidth,
    pub fat: ChunkCache,
}

impl Clusters {
    pub fn is_valid(&self, cluster: u32) -> bool {
        cluster >= 2 && u64::from(cluster) < u64::from(self.count) + 2
    }

    /// 簇在分区里的字节偏移。
    pub fn offset(&self, cluster: u32) -> Option<u64> {
        if !self.is_valid(cluster) {
            return None;
        }
        u64::from(cluster - 2)
            .checked_mul(self.cluster_size)?
            .checked_add(self.heap_offset)
    }

    /// 读 FAT 里 `cluster` 的表项。
    pub fn link(&self, cluster: u32) -> Result<Link, FsError> {
        let raw = match self.width {
            FatWidth::Fat12 => {
                let at = u64::from(cluster) + u64::from(cluster / 2);
                let Some(bytes) = self.fat.bytes::<2>(at)? else {
                    return Ok(Link::Broken);
                };
                let pair = u16::from_le_bytes(bytes);
                u32::from(if cluster % 2 == 1 {
                    pair >> 4
                } else {
                    pair & 0x0FFF
                })
            }
            FatWidth::Fat16 => match self.fat.bytes::<2>(u64::from(cluster) * 2)? {
                Some(bytes) => u32::from(u16::from_le_bytes(bytes)),
                None => return Ok(Link::Broken),
            },
            FatWidth::Fat32 | FatWidth::Exfat => {
                match self.fat.bytes::<4>(u64::from(cluster) * 4)? {
                    Some(bytes) => u32::from_le_bytes(bytes),
                    None => return Ok(Link::Broken),
                }
            }
        };
        let (value, end) = match self.width {
            FatWidth::Fat12 => (raw, 0xFF8),
            FatWidth::Fat16 => (raw, 0xFFF8),
            FatWidth::Fat32 => (raw & 0x0FFF_FFFF, 0x0FFF_FFF8),
            FatWidth::Exfat => (raw, 0xFFFF_FFF8),
        };
        Ok(if value >= end {
            Link::End
        } else if value == 0 {
            Link::Free
        } else if self.is_valid(value) {
            Link::Next(value)
        } else {
            Link::Broken
        })
    }

    /// 从 `first` 起沿 FAT 链最多找 `limit` 个簇。
    pub fn chain(&self, first: u32, limit: u64) -> Result<Chain, FsError> {
        let mut runs: Vec<Run> = Vec::new();
        let mut found = 0u64;
        let mut cluster = first;
        if !self.is_valid(first) {
            return Ok(Chain {
                runs,
                complete: limit == 0,
            });
        }
        loop {
            match runs.last_mut() {
                Some(run) if run.first.checked_add(run.count) == Some(cluster) => run.count += 1,
                _ => runs.push(Run {
                    first: cluster,
                    count: 1,
                }),
            }
            found += 1;
            // 链长超过簇总数就一定成环了
            if found >= limit || found > u64::from(self.count) {
                return Ok(Chain {
                    runs,
                    complete: found >= limit,
                });
            }
            match self.link(cluster)? {
                Link::Next(next) => cluster = next,
                Link::End => {
                    return Ok(Chain {
                        runs,
                        complete: true,
                    });
                }
                Link::Free | Link::Broken => {
                    return Ok(Chain {
                        runs,
                        complete: false,
                    });
                }
            }
        }
    }

    /// 存放 `size` 字节要多少簇。
    pub fn clusters_for(&self, size: u64) -> u64 {
        size.div_ceil(self.cluster_size)
    }

    /// 从 `first` 起连续 `count` 个簇；超出簇区时截短，第二个值为 `false`。
    pub fn contiguous(&self, first: u32, count: u64) -> (Vec<Run>, bool) {
        if count == 0 {
            return (Vec::new(), true);
        }
        if !self.is_valid(first) {
            return (Vec::new(), false);
        }
        let available = u64::from(self.count) + 2 - u64::from(first);
        let take = count.min(available).min(u64::from(u32::MAX));
        let run = Run {
            first,
            count: take as u32,
        };
        (vec![run], take == count)
    }

    /// 读一串簇的内容，最多 `max_bytes` 字节。
    pub fn read_runs(&self, runs: &[Run], max_bytes: u64) -> Result<Vec<u8>, FsError> {
        let mut data = Vec::new();
        for run in runs {
            let Some(offset) = self.offset(run.first) else {
                break;
            };
            let room = max_bytes.saturating_sub(data.len() as u64);
            let len = (u64::from(run.count) * self.cluster_size).min(room);
            if len == 0 {
                break;
            }
            let start = data.len();
            data.resize(start + len as usize, 0);
            if let Some(target) = data.get_mut(start..) {
                self.region.read_exact(offset, target)?;
            }
        }
        Ok(data)
    }
}
