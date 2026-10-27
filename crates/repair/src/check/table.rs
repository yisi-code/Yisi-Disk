//! FAT 表（FAT12/16/32 与 exFAT 共用）的按块读取，以及「哪个簇归哪个文件」的认领表。
//! 表项的位置与含义按 [`yisi_fs::format`]，与浏览共用。

use std::collections::HashMap;

pub(crate) use yisi_fs::format::FatWidth as Width;
use yisi_fs::format::{FatEntry, decode_fat_entry, fat_entry_raw, fat_entry_span, is_cluster};

use crate::check::Ctx;
use crate::check::disk::Span;
use crate::check::issue::Area;

/// 每块 64 KiB，按块起点对齐读取；表项不会跨块（FAT12 除外，按字节拼）。
const CHUNK: u64 = 64 * 1024;
/// 最多缓存这么多块（32 MiB），满了整个清空：查询基本是顺序的。
const MAX_CHUNKS: usize = 512;
/// 认领表最多跟踪这么多个簇（每簇 4 字节，256 MiB）；更大的卷不做交叉链接、丢失簇与位图的检查。
pub(crate) const MAX_TRACKED_CLUSTERS: u64 = 64 << 20;

/// 一个 FAT 表项的含义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Link {
    Free,
    Next(u32),
    End,
    Bad,
    /// 不是合法的簇号，也不是标记值。
    Invalid(u32),
    /// 表项所在的扇区读不出，或者 FAT 太短、没有这一项。
    Unknown,
}

impl Link {
    /// 在 FAT 里算作「已占用」：不是空闲，也不是坏簇。
    pub fn is_used(self) -> bool {
        matches!(self, Self::Next(_) | Self::End | Self::Invalid(_))
    }
}

struct Chunk {
    data: Vec<u8>,
    bad: Vec<Span>,
}

/// 一份 FAT。
pub(crate) struct FatTable {
    /// 分区内的起点与字节数。
    start: u64,
    length: u64,
    pub width: Width,
    /// 簇数；合法的簇号是 2 到 `count + 1`。
    pub count: u32,
    chunks: HashMap<u64, Chunk>,
}

impl FatTable {
    pub fn new(start: u64, length: u64, width: Width, count: u32) -> Self {
        Self {
            start,
            length,
            width,
            count,
            chunks: HashMap::new(),
        }
    }

    pub fn is_valid(&self, cluster: u32) -> bool {
        is_cluster(cluster, self.count)
    }

    /// 表项 `cluster` 的原始值；读不出或超出 FAT 时为 `None`。
    pub fn raw(&mut self, ctx: &mut Ctx, cluster: u32) -> Option<u32> {
        let (at, len) = fat_entry_span(self.width, cluster);
        let mut bytes = [0u8; 4];
        for (index, slot) in bytes.iter_mut().take(len).enumerate() {
            *slot = self.byte(ctx, at.checked_add(index as u64)?)?;
        }
        fat_entry_raw(self.width, cluster, &bytes)
    }

    pub fn link(&mut self, ctx: &mut Ctx, cluster: u32) -> Link {
        let Some(raw) = self.raw(ctx, cluster) else {
            return Link::Unknown;
        };
        match decode_fat_entry(self.width, raw, self.count) {
            FatEntry::Free => Link::Free,
            FatEntry::Next(next) => Link::Next(next),
            FatEntry::End => Link::End,
            FatEntry::Bad => Link::Bad,
            FatEntry::Invalid(value) => Link::Invalid(value),
        }
    }

    fn byte(&mut self, ctx: &mut Ctx, at: u64) -> Option<u8> {
        if at >= self.length {
            return None;
        }
        let index = at / CHUNK;
        let inside = at - index * CHUNK;
        if !self.chunks.contains_key(&index) {
            if self.chunks.len() >= MAX_CHUNKS {
                self.chunks.clear();
            }
            let begin = index * CHUNK;
            let len = CHUNK.min(self.length - begin);
            let mut data = vec![0u8; len as usize];
            let bad = ctx
                .read(self.start.checked_add(begin)?, &mut data, Area::Fat, None)
                .unwrap_or_else(|| {
                    vec![Span {
                        offset: 0,
                        length: len,
                    }]
                });
            self.chunks.insert(index, Chunk { data, bad });
        }
        let chunk = self.chunks.get(&index)?;
        if chunk.bad.iter().any(|span| span.overlaps(inside, 1)) {
            return None;
        }
        chunk.data.get(inside as usize).copied()
    }
}

/// 认领表里的一个文件或目录。
struct Node {
    /// 上级目录；系统结构（exFAT 的分配位图、大写表）没有上级。
    parent: Option<u32>,
    name: Box<str>,
}

/// 根目录在认领表里的编号。
pub(crate) const ROOT: u32 = 0;

/// 每个簇归哪个文件或目录。卷太大时不跟踪（`map` 为空），只记文件与目录。
pub(crate) struct Owners {
    /// 下标是簇号，值是认领者编号加 1，0 表示没人认领。
    map: Vec<u32>,
    nodes: Vec<Node>,
}

impl Owners {
    /// `clusters` 是簇号的上界（簇数加 2）。
    pub fn new(clusters: u64) -> Self {
        let map = if clusters <= MAX_TRACKED_CLUSTERS {
            vec![0u32; clusters as usize]
        } else {
            Vec::new()
        };
        Self {
            map,
            nodes: vec![Node {
                parent: None,
                name: "".into(),
            }],
        }
    }

    pub fn tracking(&self) -> bool {
        !self.map.is_empty()
    }

    /// 新建一个认领者：`parent` 目录下名为 `name` 的项；`parent` 为 `None` 时是系统结构，路径就是名字。
    pub fn add(&mut self, parent: Option<u32>, name: &str) -> u32 {
        let id = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
        self.nodes.push(Node {
            parent,
            name: name.into(),
        });
        id
    }

    pub fn owner(&self, cluster: u32) -> Option<u32> {
        match self.map.get(cluster as usize) {
            Some(&0) | None => None,
            Some(&value) => Some(value - 1),
        }
    }

    pub fn set(&mut self, cluster: u32, id: u32) {
        if let Some(slot) = self.map.get_mut(cluster as usize) {
            *slot = id.saturating_add(1);
        }
    }

    /// 认领者的路径：根目录是 `/`，其下的项以 `/` 开头；系统结构就是它的名字。
    pub fn path(&self, id: u32) -> String {
        let mut parts = Vec::new();
        let mut current = Some(id);
        let mut system = false;
        // 链再长也不会超过节点数；防止编号错乱时死循环
        for _ in 0..self.nodes.len() {
            let Some(node) = current.and_then(|at| self.nodes.get(at as usize)) else {
                break;
            };
            if current == Some(ROOT) {
                break;
            }
            parts.push(node.name.as_ref());
            if node.parent.is_none() {
                system = true;
                break;
            }
            current = node.parent;
        }
        parts.reverse();
        if system {
            return parts.join("/");
        }
        format!("/{}", parts.join("/"))
    }
}
