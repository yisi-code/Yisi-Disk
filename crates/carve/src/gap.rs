//! 两段重组：文件（目前只有能逐位核对内容的 JPEG）存成了两段——开头一段、中间隔着别的数据、剩下的一段在后面别处——
//! 时，第一段之后核对就对不上了。格式插件给出第一段可能在哪几个块边界结束、从那里接着核对的状态
//! （[`yisi_formats::Recovering::resumes`]）；这里在后面一段范围里按块对齐找第二段的开头：每个块开头都试着接上去核对，
//! 一路核对到文件结尾、各项都对得上的才算找到。
//!
//! 候选同时往前推进（读一遍范围就核对全部候选）。块开头先由插件粗看一遍（[`Resume::possible_starts`]），
//! 绝大多数在这里就排除；留下的才开候选，错的候选通常几百字节之内就对不上。候选数、白白核对的字节数都有上限，
//! 与扫描一样每块查一次取消。读过的段留给接下来几次找（[`ReadCache`]）。

use std::collections::VecDeque;

use yisi_formats::{DataCheck, Recovering, Resume};
use yisi_io::BlockDevice;

use crate::{CHUNK_SIZE, CarveControl, Chunk, read_chunk};

/// 第二段要在第一段之后这么远以内开始、并且结束（从第一段最早可能的结尾算）。
pub(crate) const SEARCH_SPAN: u64 = 64 << 20;

/// 同时核对的候选最多这么多个，再多的块开头先不试。
const MAX_CANDIDATES: usize = 16;

/// 一次重组里对不上的候选白白核对的字节加起来最多这么多，超过就放弃（另外每个文件最多读 [`SEARCH_SPAN`]）。
/// 拼成的那个候选核对的字节不算：第二段多长都要核对完。
const MAX_WASTED: u64 = 64 << 20;

/// 有候选已经一路核对过这么多字节时，先不开新的候选：别的数据里接不了这么远，它多半就是第二段，
/// 后面的块是它的数据，从中间开始的候选只会白白核对到文件结尾。它对不上了再接着开。
const SETTLED: u64 = 4 << 10;

/// 留着的段数：正好盖住一次要看的范围。
const CACHED_CHUNKS: usize = (SEARCH_SPAN / CHUNK_SIZE as u64) as usize;

/// 拼成了：第一段是文件开头起 `first_len` 字节，第二段是源上从 `second_start` 起 `second_len` 字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reassembled {
    pub first_len: u64,
    pub second_start: u64,
    pub second_len: u64,
}

pub(crate) enum Search {
    Found(Reassembled),
    NotFound,
    Cancelled,
}

/// 找第二段时读过的段（按 1 MiB 对齐），留给接下来几次找：断了的文件常常一个接一个，往后要看的范围大半重叠，
/// 物理磁盘上不必一遍遍重读。最多留 [`SEARCH_SPAN`] 那么多，先读的先丢。
pub(crate) struct ReadCache {
    chunks: VecDeque<Chunk>,
}

impl ReadCache {
    pub(crate) fn new() -> Self {
        Self {
            chunks: VecDeque::new(),
        }
    }

    /// 从 `start`（1 MiB 对齐）起的一段。
    fn chunk(&mut self, device: &dyn BlockDevice, block_size: usize, start: u64) -> Option<&Chunk> {
        if let Some(index) = self.chunks.iter().position(|chunk| chunk.start == start) {
            return self.chunks.get(index);
        }
        let buffer = if self.chunks.len() >= CACHED_CHUNKS {
            self.chunks
                .pop_front()
                .map(|chunk| chunk.data)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        self.chunks
            .push_back(read_chunk(device, block_size, 0, start, buffer));
        self.chunks.back()
    }
}

struct Candidate {
    first_len: u64,
    second_start: u64,
    /// 已经接着核对过的第二段字节数。
    checked: u64,
    recovering: Box<dyn Recovering>,
}

/// 在 `device` 上找从 `file_start` 开始的文件的第二段。`resumes` 是格式插件给出的起点（最可能的在前），
/// 拼成的文件不超过 `max_size`。
pub(crate) fn search(
    device: &dyn BlockDevice,
    cache: &mut ReadCache,
    file_start: u64,
    resumes: &[Resume],
    block_size: usize,
    max_size: u64,
    control: &dyn CarveControl,
) -> Search {
    let block = block_size as u64;
    let Some(earliest) = resumes.iter().map(|resume| resume.first_len).min() else {
        return Search::NotFound;
    };
    let from = file_start.saturating_add(earliest).saturating_add(block);
    let end = from.saturating_add(SEARCH_SPAN).min(device.size());
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut wasted = 0u64;
    let mut chunk_start = from - from % CHUNK_SIZE as u64;
    while chunk_start < end {
        let Some(chunk) = cache.chunk(device, block_size, chunk_start) else {
            break;
        };
        if chunk.data.is_empty() {
            break;
        }
        // 这一段里从 `from` 起的部分（`from` 按块对齐，段按 1 MiB 对齐，块大小整除 1 MiB）
        let skip = usize::try_from(from.saturating_sub(chunk_start)).unwrap_or(usize::MAX);
        let first_block = skip / block_size;
        let data = chunk
            .data
            .get(first_block * block_size..)
            .unwrap_or_default();
        // 各起点粗看这一段里哪几块可能是第二段的开头（一次给一整段，插件可以往后看第二段至少要有多长）
        let possible: Vec<Vec<usize>> = resumes
            .iter()
            .map(|resume| resume.possible_starts(data, block_size))
            .collect();
        let mut next_possible = vec![0usize; resumes.len()];
        for (index, piece) in data.chunks(block_size).enumerate() {
            let at = chunk_start + ((first_block + index) * block_size) as u64;
            if at >= end {
                break;
            }
            // 与扫描一样每块查一次（暂停时在这里等着）
            if control.is_cancelled() {
                return Search::Cancelled;
            }
            if chunk
                .unreadable
                .get(first_block + index)
                .copied()
                .unwrap_or(true)
            {
                // 第二段不会跨过读不出的块
                candidates.clear();
                continue;
            }
            if !candidates
                .iter()
                .any(|candidate| candidate.checked >= SETTLED)
            {
                for ((resume, possible), next) in
                    resumes.iter().zip(&possible).zip(&mut next_possible)
                {
                    while possible.get(*next).is_some_and(|&start| start < index) {
                        *next += 1;
                    }
                    if candidates.len() >= MAX_CANDIDATES {
                        break;
                    }
                    // 第一段与第二段之间至少隔一块（紧接着的就是原来那样连续读下去，已经对不上了）
                    if at >= file_start + resume.first_len + block
                        && possible.get(*next) == Some(&index)
                    {
                        candidates.push(Candidate {
                            first_len: resume.first_len,
                            second_start: at,
                            checked: 0,
                            recovering: resume.start(),
                        });
                    }
                }
            }
            let mut found = None;
            candidates.retain_mut(|candidate| {
                if found.is_some() {
                    return true;
                }
                let before = candidate.first_len + candidate.checked;
                let len = piece.len() as u64;
                let keep = match candidate.recovering.data_check(before, piece) {
                    DataCheck::Continue => {
                        candidate.checked += len;
                        before + len <= max_size
                    }
                    DataCheck::Complete(length)
                        if length > before && length <= before + len && length <= max_size =>
                    {
                        found = Some(Reassembled {
                            first_len: candidate.first_len,
                            second_start: candidate.second_start,
                            second_len: length - candidate.first_len,
                        });
                        return false;
                    }
                    DataCheck::Complete(_) | DataCheck::Invalid => false,
                };
                if !keep {
                    wasted += candidate.checked + len;
                }
                keep
            });
            if let Some(found) = found {
                return Search::Found(found);
            }
            if wasted > MAX_WASTED {
                return Search::NotFound;
            }
        }
        chunk_start += CHUNK_SIZE as u64;
    }
    Search::NotFound
}
