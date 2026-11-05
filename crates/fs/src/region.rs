//! 分区内的只读视图，以及 FAT、分配位图这类反复查询的元数据的分块缓存。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use yisi_io::BlockDevice;

use crate::FsError;

/// 分区在设备上的范围。偏移以分区开头为 0，读超出分区的部分视为错误，不会读到别的分区。
/// 打开卷、诊断与重建引导扇区读分区里的内容都经它。
#[derive(Clone)]
pub struct Region {
    device: Arc<dyn BlockDevice>,
    offset: u64,
    length: u64,
}

impl Region {
    pub fn new(device: Arc<dyn BlockDevice>, offset: u64, length: u64) -> Self {
        Self {
            device,
            offset,
            length,
        }
    }

    pub fn length(&self) -> u64 {
        self.length
    }

    pub fn read_exact(&self, at: u64, buf: &mut [u8]) -> Result<(), FsError> {
        if at
            .checked_add(buf.len() as u64)
            .is_none_or(|end| end > self.length)
        {
            return Err(FsError::OutOfRange { offset: at });
        }
        let absolute = self
            .offset
            .checked_add(at)
            .ok_or(FsError::OutOfRange { offset: at })?;
        self.device.read_exact_at(absolute, buf)?;
        Ok(())
    }
}

/// 每块 64 KiB，按块起点对齐读取。
const CHUNK: u64 = 64 * 1024;
/// 最多缓存这么多块（32 MiB），满了就整个清空：查询基本是顺序的，命中率不靠淘汰策略。
const MAX_CHUNKS: usize = 512;

/// 一段 `length` 字节内容的分块缓存；一块怎么读出由使用方给（分区里的一段元数据、NTFS 的数据流）。
pub(crate) struct Chunks {
    length: u64,
    chunks: Mutex<HashMap<u64, Arc<[u8]>>>,
}

impl Chunks {
    pub fn new(length: u64) -> Self {
        Self {
            length,
            chunks: Mutex::new(HashMap::new()),
        }
    }

    /// 从第 `at` 个字节起读满 `buf`，没缓存的块用 `load(块起点, 块)` 读进来；超出这段内容时为 `false`。
    pub fn read(
        &self,
        at: u64,
        buf: &mut [u8],
        load: impl Fn(u64, &mut [u8]) -> Result<(), FsError>,
    ) -> Result<bool, FsError> {
        let Some(end) = at
            .checked_add(buf.len() as u64)
            .filter(|&end| end <= self.length)
        else {
            return Ok(false);
        };
        let mut position = at;
        while position < end {
            let index = position / CHUNK;
            let chunk = self.chunk(index, &load)?;
            let inside = (position - index * CHUNK) as usize;
            let take = ((end - position) as usize).min(chunk.len().saturating_sub(inside));
            let filled = (position - at) as usize;
            let (Some(source), Some(target)) = (
                chunk.get(inside..inside + take),
                buf.get_mut(filled..filled + take),
            ) else {
                return Ok(false);
            };
            if take == 0 {
                return Ok(false);
            }
            target.copy_from_slice(source);
            position += take as u64;
        }
        Ok(true)
    }

    fn chunk(
        &self,
        index: u64,
        load: &impl Fn(u64, &mut [u8]) -> Result<(), FsError>,
    ) -> Result<Arc<[u8]>, FsError> {
        if let Some(chunk) = self
            .chunks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&index)
        {
            return Ok(Arc::clone(chunk));
        }
        let begin = index * CHUNK;
        let len = CHUNK.min(self.length - begin.min(self.length));
        let mut data = vec![0u8; len as usize];
        load(begin, &mut data)?;
        let chunk: Arc<[u8]> = data.into();
        let mut chunks = self.chunks.lock().unwrap_or_else(PoisonError::into_inner);
        if chunks.len() >= MAX_CHUNKS {
            chunks.clear();
        }
        chunks.insert(index, Arc::clone(&chunk));
        Ok(chunk)
    }
}

/// 分区内一段元数据（FAT、exFAT 分配位图）的分块缓存。
pub(crate) struct ChunkCache {
    region: Region,
    /// 这段元数据在分区内的起点。
    start: u64,
    cache: Chunks,
}

impl ChunkCache {
    pub fn new(region: Region, start: u64, length: u64) -> Self {
        Self {
            region,
            start,
            cache: Chunks::new(length),
        }
    }

    /// 元数据里第 `at` 个字节起的 `N` 个字节；超出这段元数据时返回 `None`。
    pub fn bytes<const N: usize>(&self, at: u64) -> Result<Option<[u8; N]>, FsError> {
        let mut out = [0u8; N];
        let inside = self.cache.read(at, &mut out, |begin, data| {
            self.region.read_exact(self.start + begin, data)
        })?;
        Ok(inside.then_some(out))
    }
}
