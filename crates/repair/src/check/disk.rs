//! 检查用的分区只读视图：整段读不出时逐扇区重读，读不出的扇区补 0 并记下，检查照常往下走。

use std::collections::BTreeMap;

use yisi_io::BlockDevice;

/// 一段字节：起点与长度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Span {
    pub offset: u64,
    pub length: u64,
}

impl Span {
    /// 与 `[start, start + length)` 是否有重叠。
    pub fn overlaps(&self, start: u64, length: u64) -> bool {
        self.offset < start.saturating_add(length)
            && start < self.offset.saturating_add(self.length)
    }
}

pub(crate) struct Disk<'a> {
    device: &'a dyn BlockDevice,
    offset: u64,
    length: u64,
    /// 重读的粒度：设备的逻辑扇区大小。
    sector: u64,
    /// 读不出的区段（分区内偏移 → 长度），相邻的合并。
    unreadable: BTreeMap<u64, u64>,
}

impl<'a> Disk<'a> {
    pub fn new(device: &'a dyn BlockDevice, offset: u64, length: u64) -> Self {
        Self {
            device,
            offset,
            length,
            sector: u64::from(device.sector_size()).max(512),
            unreadable: BTreeMap::new(),
        }
    }

    pub fn length(&self) -> u64 {
        self.length
    }

    /// 读分区内 `at` 起的 `buf`。超出分区时返回 `None`、不读；否则返回读不出的区段（相对 `buf` 起点），
    /// 各带一个标记：这段以前没有记过。读不出的部分补 0。
    pub fn read(&mut self, at: u64, buf: &mut [u8]) -> Option<Vec<(Span, bool)>> {
        let end = at.checked_add(buf.len() as u64)?;
        if end > self.length {
            return None;
        }
        let absolute = self.offset.checked_add(at)?;
        if self.device.read_exact_at(absolute, buf).is_ok() {
            return Some(Vec::new());
        }
        let mut bad: Vec<Span> = Vec::new();
        let mut position = 0u64;
        let total = buf.len() as u64;
        while position < total {
            let here = absolute.saturating_add(position);
            let take = (self.sector - here % self.sector).min(total - position);
            let range = position as usize..(position + take) as usize;
            if let Some(piece) = buf.get_mut(range)
                && self.device.read_exact_at(here, piece).is_err()
            {
                piece.fill(0);
                match bad.last_mut() {
                    Some(last) if last.offset + last.length == position => last.length += take,
                    _ => bad.push(Span {
                        offset: position,
                        length: take,
                    }),
                }
            }
            position += take;
        }
        Some(
            bad.into_iter()
                .map(|span| {
                    let fresh = self.record(at + span.offset, span.length);
                    (span, fresh)
                })
                .collect(),
        )
    }

    /// 读不出的总字节数（同一位置只算一次）。
    pub fn unreadable_bytes(&self) -> u64 {
        self.unreadable
            .values()
            .fold(0u64, |sum, &length| sum.saturating_add(length))
    }

    /// 记下一段读不出的区段；整段以前都记过时返回 `false`。
    fn record(&mut self, offset: u64, length: u64) -> bool {
        let end = offset.saturating_add(length);
        let known = self
            .unreadable
            .range(..=offset)
            .next_back()
            .is_some_and(|(&start, &len)| start.saturating_add(len) >= end);
        if known {
            return false;
        }
        let mut start = offset;
        let mut stop = end;
        let touching: Vec<u64> = self
            .unreadable
            .range(..=end)
            .filter(|&(&begin, &len)| begin.saturating_add(len) >= offset)
            .map(|(&begin, _)| begin)
            .collect();
        for begin in touching {
            if let Some(len) = self.unreadable.remove(&begin) {
                start = start.min(begin);
                stop = stop.max(begin.saturating_add(len));
            }
        }
        self.unreadable.insert(start, stop - start);
        true
    }
}
