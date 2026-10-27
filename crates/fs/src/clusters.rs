//! FAT 与 exFAT 共用的簇区与 FAT 表：簇号换算成位置、沿 FAT 链找出文件占用的簇。

use crate::FsError;
use crate::format::{
    FatEntry, cluster_offset, decode_fat_entry, fat_entry_raw, fat_entry_span, is_cluster, push_run,
};
pub(crate) use crate::format::{FatWidth, Run};
use crate::region::{ChunkCache, Region};

/// FAT 表项的含义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Link {
    Next(u32),
    End,
    Free,
    /// 坏簇标记，或指向簇区之外的值。
    Broken,
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
        is_cluster(cluster, self.count)
    }

    /// 簇在分区里的字节偏移。
    pub fn offset(&self, cluster: u32) -> Option<u64> {
        cluster_offset(cluster, self.count, self.cluster_size, self.heap_offset)
    }

    /// 读 FAT 里 `cluster` 的表项。
    pub fn link(&self, cluster: u32) -> Result<Link, FsError> {
        let (at, len) = fat_entry_span(self.width, cluster);
        let bytes = match len {
            2 => self.fat.bytes::<2>(at)?.map(|bytes| bytes.to_vec()),
            _ => self.fat.bytes::<4>(at)?.map(|bytes| bytes.to_vec()),
        };
        let Some(raw) = bytes.and_then(|bytes| fat_entry_raw(self.width, cluster, &bytes)) else {
            return Ok(Link::Broken);
        };
        Ok(match decode_fat_entry(self.width, raw, self.count) {
            FatEntry::Next(next) => Link::Next(next),
            FatEntry::End => Link::End,
            FatEntry::Free => Link::Free,
            FatEntry::Bad | FatEntry::Invalid(_) => Link::Broken,
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
            push_run(&mut runs, cluster);
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
