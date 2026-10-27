//! 一次运行只读的范围：一组按扇区对齐、排好序、互不相接的区间。坏区图仍然覆盖整个设备，范围以外的区段原样保留。

use std::io;

use super::map::{BadMap, SegmentState};

/// 这一次要读的范围，每项是 `[起点, 终点)`。整个设备是一个从 0 到容量的区间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    ranges: Vec<(u64, u64)>,
}

impl Scope {
    /// 整个设备。
    pub fn whole(size: u64) -> Self {
        Self {
            ranges: if size > 0 {
                vec![(0, size)]
            } else {
                Vec::new()
            },
        }
    }

    /// 由（起点，长度）列表得出范围：起点向下、终点向上对齐到扇区（不超过容量），重叠或相接的合并。
    /// 长度为 0 的项忽略；有项的起点不在设备内，或者一项也不剩时返回错误。
    pub fn new(ranges: &[(u64, u64)], size: u64, sector_size: u32) -> io::Result<Self> {
        let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
        let sector = u64::from(sector_size.max(1));
        let mut aligned = Vec::with_capacity(ranges.len());
        for &(offset, length) in ranges {
            if length == 0 {
                continue;
            }
            if offset >= size {
                return Err(invalid(format!("范围起点 {offset} 超出设备容量 {size}")));
            }
            let start = offset - offset % sector;
            let end = offset
                .saturating_add(length)
                .min(size)
                .div_ceil(sector)
                .saturating_mul(sector)
                .min(size);
            aligned.push((start, end));
        }
        aligned.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(aligned.len());
        for (start, end) in aligned {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        if merged.is_empty() {
            return Err(invalid("没有选要读的范围".to_owned()));
        }
        Ok(Self { ranges: merged })
    }

    /// 各区间，每项是 `[起点, 终点)`。
    pub fn ranges(&self) -> &[(u64, u64)] {
        &self.ranges
    }

    /// 范围的总字节数。
    pub fn total(&self) -> u64 {
        self.ranges.iter().map(|&(start, end)| end - start).sum()
    }

    /// 是不是容量为 `size` 的整个设备。
    pub fn is_whole(&self, size: u64) -> bool {
        *self == Self::whole(size)
    }

    /// 范围之内处于 `state` 的字节数。
    pub fn bytes(&self, map: &BadMap, state: SegmentState) -> u64 {
        self.ranges
            .iter()
            .map(|&(start, end)| map.bytes_within(state, start, end))
            .sum()
    }

    /// 范围之内还没有结论的字节数（未读、待修边、待细读）。
    pub fn unfinished(&self, map: &BadMap) -> u64 {
        [
            SegmentState::Untried,
            SegmentState::Trim,
            SegmentState::Scrape,
        ]
        .into_iter()
        .map(|state| self.bytes(map, state))
        .sum()
    }

    /// 同 [`BadMap::next_in`]，只看范围之内。
    pub(super) fn next_in(
        &self,
        map: &BadMap,
        state: SegmentState,
        mut from: u64,
    ) -> Option<(u64, u64)> {
        loop {
            let (start, end) = map.next_in(state, from)?;
            let &(range_start, range_end) = self
                .ranges
                .iter()
                .find(|&&(_, range_end)| range_end > start)?;
            if range_start < end {
                return Some((start.max(range_start), end.min(range_end)));
            }
            from = range_start;
        }
    }

    /// 同 [`BadMap::prev_in`]，只看范围之内。
    pub(super) fn prev_in(
        &self,
        map: &BadMap,
        state: SegmentState,
        mut before: u64,
    ) -> Option<(u64, u64)> {
        loop {
            let (start, end) = map.prev_in(state, before)?;
            let &(range_start, range_end) = self
                .ranges
                .iter()
                .rev()
                .find(|&&(range_start, _)| range_start < end)?;
            if range_end > start {
                return Some((start.max(range_start), end.min(range_end)));
            }
            before = range_end;
        }
    }
}
