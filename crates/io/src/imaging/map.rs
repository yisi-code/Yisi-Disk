//! 坏区图：把设备从头到尾分成首尾相接的区段，每段一个读取状态。纯文本，与镜像放在一起，中断后据此续做。

use std::fmt::Write as _;

/// 坏区图文本的格式版本。
const VERSION: u32 = 1;

/// 区段的读取状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SegmentState {
    /// 还没读过。
    Untried,
    /// 整块读的时候出过错，待修边：从两端按扇区向内读。
    Trim,
    /// 修边后剩下的中间部分，待细读：逐扇区读一遍。
    Scrape,
    /// 按扇区读过、读不出。镜像里这一段是 0。
    Bad,
    /// 已读出并写进镜像。
    Done,
}

impl SegmentState {
    const ALL: [Self; 5] = [
        Self::Untried,
        Self::Trim,
        Self::Scrape,
        Self::Bad,
        Self::Done,
    ];

    fn keyword(self) -> &'static str {
        match self {
            Self::Untried => "untried",
            Self::Trim => "trim",
            Self::Scrape => "scrape",
            Self::Bad => "bad",
            Self::Done => "done",
        }
    }

    fn from_keyword(keyword: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|state| state.keyword() == keyword)
    }

    fn index(self) -> usize {
        match self {
            Self::Untried => 0,
            Self::Trim => 1,
            Self::Scrape => 2,
            Self::Bad => 3,
            Self::Done => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub offset: u64,
    pub length: u64,
    pub state: SegmentState,
}

impl Segment {
    fn end(&self) -> u64 {
        self.offset + self.length
    }
}

/// 坏区图记录的来源：续做时三项都要与当前的源一致，防止把另一块盘的数据续进同一个镜像。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapHeader {
    pub size: u64,
    pub sector_size: u32,
    /// 来源的标识（磁盘的厂商、型号、序列号，或镜像文件的路径），只占一行。
    pub source: String,
}

#[derive(Debug, thiserror::Error)]
#[error("坏区图第 {line} 行：{reason}")]
pub struct MapError {
    pub line: usize,
    pub reason: String,
}

/// 坏区图。区段按偏移排列、首尾相接、覆盖整个设备，相邻的同状态区段总是合并。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadMap {
    header: MapHeader,
    segments: Vec<Segment>,
    /// 各状态的总字节数，按 [`SegmentState::index`] 排列。
    totals: [u64; 5],
}

impl BadMap {
    /// 新的坏区图：整个设备都还没读过。
    pub fn new(mut header: MapHeader) -> Self {
        header.source = single_line(&header.source);
        let mut totals = [0; 5];
        let mut segments = Vec::new();
        if header.size > 0 {
            segments.push(Segment {
                offset: 0,
                length: header.size,
                state: SegmentState::Untried,
            });
            totals[SegmentState::Untried.index()] = header.size;
        }
        Self {
            header,
            segments,
            totals,
        }
    }

    pub fn header(&self) -> &MapHeader {
        &self.header
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// 处于 `state` 的总字节数。
    pub fn bytes(&self, state: SegmentState) -> u64 {
        self.totals[state.index()]
    }

    /// 处于 `state` 的各个区段，依次给出起点与长度。
    pub fn ranges(&self, state: SegmentState) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.segments
            .iter()
            .filter(move |segment| segment.state == state)
            .map(|segment| (segment.offset, segment.length))
    }

    /// 把 `[offset, offset + length)` 设为 `state`，超出设备的部分忽略。
    pub fn set(&mut self, offset: u64, length: u64, state: SegmentState) {
        let end = offset.saturating_add(length).min(self.header.size);
        if offset >= end {
            return;
        }
        // 与这一段重叠的区段是 [first, last)
        let first = self
            .segments
            .partition_point(|segment| segment.end() <= offset);
        let last = first
            + self.segments[first..]
                .iter()
                .take_while(|segment| segment.offset < end)
                .count();

        let mut replacement = Vec::with_capacity(3);
        if let Some(head) = self.segments.get(first)
            && head.offset < offset
        {
            replacement.push(Segment {
                offset: head.offset,
                length: offset - head.offset,
                state: head.state,
            });
        }
        replacement.push(Segment {
            offset,
            length: end - offset,
            state,
        });
        if last > first
            && let Some(tail) = self.segments.get(last - 1)
            && tail.end() > end
        {
            replacement.push(Segment {
                offset: end,
                length: tail.end() - end,
                state: tail.state,
            });
        }

        for segment in &self.segments[first..last] {
            let overlap = segment.end().min(end) - segment.offset.max(offset);
            self.totals[segment.state.index()] -= overlap;
        }
        self.totals[state.index()] += end - offset;

        let inserted = replacement.len();
        self.segments.splice(first..last, replacement);
        self.merge(first.saturating_sub(1), first + inserted + 1);
    }

    /// 从 `from` 起往后第一段处于 `state` 的区域：区段跨过 `from` 时从 `from` 算起。
    pub fn next_in(&self, state: SegmentState, from: u64) -> Option<(u64, u64)> {
        let start = self
            .segments
            .partition_point(|segment| segment.end() <= from);
        self.segments[start..]
            .iter()
            .find(|segment| segment.state == state)
            .map(|segment| (segment.offset.max(from), segment.end()))
    }

    /// `before` 之前最后一段处于 `state` 的区域：区段跨过 `before` 时截到 `before`。
    pub fn prev_in(&self, state: SegmentState, before: u64) -> Option<(u64, u64)> {
        let end = self
            .segments
            .partition_point(|segment| segment.offset < before);
        self.segments[..end]
            .iter()
            .rev()
            .find(|segment| segment.state == state)
            .map(|segment| (segment.offset, segment.end().min(before)))
    }

    /// 合并下标 `[from, to)` 范围内相邻的同状态区段。
    fn merge(&mut self, from: usize, to: usize) {
        let mut index = from;
        let mut to = to.min(self.segments.len());
        while index + 1 < to {
            let next = self.segments[index + 1];
            let current = &mut self.segments[index];
            if current.state == next.state && current.end() == next.offset {
                current.length += next.length;
                self.segments.remove(index + 1);
                to -= 1;
            } else {
                index += 1;
            }
        }
    }

    /// 写成文本。
    pub fn to_text(&self) -> String {
        let mut text = String::new();
        text.push_str("# Yisi-Disk 坏区图：每行一个区段「起始偏移 长度 状态」，单位是字节\n");
        text.push_str("# 状态：untried 未读、done 已读、trim 待修边、scrape 待细读、bad 读不出（镜像里是 0）\n");
        let header = &self.header;
        let _ = writeln!(text, "version {VERSION}");
        let _ = writeln!(text, "size {}", header.size);
        let _ = writeln!(text, "sector {}", header.sector_size);
        let _ = writeln!(text, "source {}", header.source);
        for segment in &self.segments {
            let _ = writeln!(
                text,
                "{} {} {}",
                segment.offset,
                segment.length,
                segment.state.keyword()
            );
        }
        text
    }

    /// 读回 [`to_text`](Self::to_text) 写出的文本。区段必须首尾相接、覆盖整个设备，否则拒绝：
    /// 坏区图决定哪些位置不再读，宁可不续做，也不能把没读过的位置当作读完了。
    pub fn parse(text: &str) -> Result<Self, MapError> {
        let mut version = None;
        let mut size = None;
        let mut sector_size = None;
        let mut source = None;
        let mut segments: Vec<Segment> = Vec::new();
        let mut totals = [0u64; 5];

        for (index, raw) in text.lines().enumerate() {
            let line_number = index + 1;
            let invalid = |reason: &str| MapError {
                line: line_number,
                reason: reason.to_owned(),
            };
            let line = raw.trim_end_matches('\r');
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "version" => {
                    version = Some(rest.parse::<u32>().map_err(|_| invalid("版本号不是数字"))?);
                }
                "size" => size = Some(rest.parse::<u64>().map_err(|_| invalid("容量不是数字"))?),
                "sector" => {
                    sector_size = Some(
                        rest.parse::<u32>()
                            .map_err(|_| invalid("扇区大小不是数字"))?,
                    );
                }
                "source" => source = Some(rest.to_owned()),
                _ => {
                    let mut fields = line.split(' ');
                    let (Some(offset), Some(length), Some(state), None) =
                        (fields.next(), fields.next(), fields.next(), fields.next())
                    else {
                        return Err(invalid("区段应为「起始偏移 长度 状态」三项"));
                    };
                    let offset = offset
                        .parse::<u64>()
                        .map_err(|_| invalid("起始偏移不是数字"))?;
                    let length = length.parse::<u64>().map_err(|_| invalid("长度不是数字"))?;
                    let state =
                        SegmentState::from_keyword(state).ok_or_else(|| invalid("不认识的状态"))?;
                    let expected = segments.last().map_or(0, Segment::end);
                    if offset != expected {
                        return Err(invalid("区段与上一段没有首尾相接"));
                    }
                    if length == 0 {
                        return Err(invalid("区段长度为 0"));
                    }
                    let end = offset
                        .checked_add(length)
                        .ok_or_else(|| invalid("区段超出了数值范围"))?;
                    match segments.last_mut() {
                        Some(last) if last.state == state => last.length += length,
                        _ => segments.push(Segment {
                            offset,
                            length,
                            state,
                        }),
                    }
                    totals[state.index()] += end - offset;
                }
            }
        }

        let line_count = text.lines().count();
        let missing = |what: &str| MapError {
            line: line_count,
            reason: format!("缺少{what}"),
        };
        let version = version.ok_or_else(|| missing("版本号"))?;
        if version != VERSION {
            return Err(MapError {
                line: line_count,
                reason: format!("版本 {version} 不受支持"),
            });
        }
        let header = MapHeader {
            size: size.ok_or_else(|| missing("容量"))?,
            sector_size: sector_size.ok_or_else(|| missing("扇区大小"))?,
            source: source.ok_or_else(|| missing("来源"))?,
        };
        if segments.last().map_or(0, Segment::end) != header.size {
            return Err(MapError {
                line: line_count,
                reason: format!("区段没有覆盖整个设备（{} 字节）", header.size),
            });
        }
        Ok(Self {
            header,
            segments,
            totals,
        })
    }
}

/// 来源标识只占一行：换行与回车换成空格。
fn single_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(size: u64) -> MapHeader {
        MapHeader {
            size,
            sector_size: 512,
            source: "disk WDC WD10 12345".to_owned(),
        }
    }

    fn states(map: &BadMap) -> Vec<(u64, u64, SegmentState)> {
        map.segments()
            .iter()
            .map(|segment| (segment.offset, segment.length, segment.state))
            .collect()
    }

    #[test]
    fn set_splits_and_merges_and_keeps_totals() {
        use SegmentState::*;
        let mut map = BadMap::new(header(10_000));
        map.set(1000, 500, Done);
        map.set(3000, 1000, Trim);
        assert_eq!(
            states(&map),
            vec![
                (0, 1000, Untried),
                (1000, 500, Done),
                (1500, 1500, Untried),
                (3000, 1000, Trim),
                (4000, 6000, Untried)
            ]
        );
        // 跨过多段、与两边同状态的相接
        map.set(1500, 1500, Done);
        map.set(0, 1000, Done);
        assert_eq!(
            states(&map),
            vec![(0, 3000, Done), (3000, 1000, Trim), (4000, 6000, Untried)]
        );
        map.set(3500, 100, Bad);
        map.set(9000, 5000, Done);
        assert_eq!(map.bytes(Done), 4000);
        assert_eq!(map.bytes(Trim), 900);
        assert_eq!(map.bytes(Bad), 100);
        assert_eq!(map.bytes(Untried), 5000);
        let total: u64 = SegmentState::ALL
            .into_iter()
            .map(|state| map.bytes(state))
            .sum();
        assert_eq!(total, 10_000);
        assert_eq!(map.segments().last().unwrap().length, 1000);
    }

    #[test]
    fn next_and_prev_clip_to_the_cursor() {
        use SegmentState::*;
        let mut map = BadMap::new(header(10_000));
        map.set(2000, 2000, Done);
        map.set(6000, 1000, Done);
        assert_eq!(map.next_in(Untried, 0), Some((0, 2000)));
        assert_eq!(map.next_in(Untried, 1500), Some((1500, 2000)));
        assert_eq!(map.next_in(Untried, 2000), Some((4000, 6000)));
        assert_eq!(map.next_in(Untried, 9999), Some((9999, 10_000)));
        assert_eq!(map.next_in(Untried, 10_000), None);
        assert_eq!(map.prev_in(Untried, 10_000), Some((7000, 10_000)));
        assert_eq!(map.prev_in(Untried, 6500), Some((4000, 6000)));
        assert_eq!(map.prev_in(Untried, 5000), Some((4000, 5000)));
        assert_eq!(map.prev_in(Untried, 0), None);
        assert_eq!(map.next_in(Bad, 0), None);
    }

    #[test]
    fn text_round_trip() {
        use SegmentState::*;
        let mut map = BadMap::new(MapHeader {
            source: "image D:\\a b.img\r\nx".to_owned(),
            ..header(1_000_000)
        });
        map.set(0, 4096, Done);
        map.set(4096, 512, Bad);
        map.set(4608, 1024, Scrape);
        map.set(8192, 65536, Trim);
        let text = map.to_text();
        assert!(text.contains("source image D:\\a b.img  x\n"), "{text}");
        assert_eq!(BadMap::parse(&text).unwrap(), map);
        // Windows 换行也能读
        assert_eq!(BadMap::parse(&text.replace('\n', "\r\n")).unwrap(), map);
    }

    #[test]
    fn empty_device_has_no_segments() {
        let map = BadMap::new(header(0));
        assert!(map.segments().is_empty());
        assert_eq!(BadMap::parse(&map.to_text()).unwrap(), map);
    }

    #[test]
    fn broken_maps_are_rejected() {
        let good = "version 1\nsize 1000\nsector 512\nsource s\n0 512 done\n512 488 bad\n";
        assert!(BadMap::parse(good).is_ok());
        let cases = [
            (
                "version 2\nsize 1000\nsector 512\nsource s\n0 1000 done\n",
                "版本",
            ),
            ("size 1000\nsector 512\nsource s\n0 1000 done\n", "版本号"),
            ("version 1\nsector 512\nsource s\n0 1000 done\n", "容量"),
            ("version 1\nsize 1000\nsector 512\n0 1000 done\n", "来源"),
            (
                "version 1\nsize 1000\nsector 512\nsource s\n0 512 done\n",
                "覆盖",
            ),
            (
                "version 1\nsize 1000\nsector 512\nsource s\n0 512 done\n600 400 bad\n",
                "首尾",
            ),
            (
                "version 1\nsize 1000\nsector 512\nsource s\n0 512 done\n512 0 bad\n512 488 bad\n",
                "为 0",
            ),
            (
                "version 1\nsize 1000\nsector 512\nsource s\n0 1000 fine\n",
                "状态",
            ),
            (
                "version 1\nsize 1000\nsector 512\nsource s\n0 1000\n",
                "三项",
            ),
            (
                "version 1\nsize 1000\nsector 512\nsource s\n0 -1 done\n",
                "长度",
            ),
            (
                "version 1\nsize 1000\nsector 512\nsource s\n0 1000 done\n1000 18446744073709551615 done\n",
                "数值范围",
            ),
            (
                "version 1\nsize 1000\nsector 512\nsource s\n0 2000 done\n",
                "覆盖",
            ),
        ];
        for (text, reason) in cases {
            let error = BadMap::parse(text).unwrap_err();
            assert!(error.reason.contains(reason), "{text:?} → {error}");
        }
    }
}
