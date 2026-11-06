//! 按结构逐段解析文件。插件只描述「下一步要看哪几个字节」或「从哪里开始找什么」，
//! 跨块拼接、跳过大段数据、跨块查找都在这里处理，插件不必关心数据是怎么分块送来的。

use crate::plugin::{DataCheck, Recovering};

/// 一次 `Need` 最多要这么多字节，防止损坏的长度字段让缓冲无限增长。
pub(crate) const MAX_NEED: usize = 1 << 20;

/// 遍历器交给插件的事件。
pub(crate) enum Event<'a> {
    /// 从 `at` 起的字节，长度正好是上一步 `Need` 要的长度。
    Bytes { at: u64, bytes: &'a [u8] },
    /// `Scan` 在 `at` 处找到了匹配。
    Found { at: u64 },
}

/// 插件要求的下一步。位置都是相对文件开头的绝对偏移。
///
/// 下一步不能早于上一个事件的位置（`Bytes` 的 `at` 或 `Found` 的 `at`）；
/// 可以落在刚交付或刚匹配的字节中间（例如先读签名、再从同一位置读整个记录），遍历器会用保留下来的字节补上。
pub(crate) enum Step {
    /// 需要从 `at` 起的 `len` 个字节。
    Need {
        at: u64,
        len: usize,
    },
    /// 从 `from` 起往后查找。
    Scan {
        from: u64,
        scanner: Scanner,
    },
    /// 文件在这个长度处结束。
    Done(u64),
    Invalid,
}

#[derive(Clone, Copy)]
pub(crate) struct Scanner {
    /// 在窗口里找匹配，返回匹配开头在窗口中的下标。
    pub find: fn(&[u8]) -> Option<usize>,
    /// 一次匹配要看的字节数减一：窗口在块与块之间保留这么多字节，跨块的匹配不会漏掉。
    pub overlap: usize,
}

pub(crate) trait Structure: Send {
    fn first(&self) -> Step;

    fn step(&mut self, event: Event<'_>) -> Step;

    /// 见 [`Recovering::open_ended`]。
    fn open_ended(&self) -> bool {
        false
    }

    /// 数据到此为止时文件的长度。`waiting_at` 是遍历器正在等待、还一个字节都没收到的位置（如果是这样）。
    fn finish(&self, _received: u64, _waiting_at: Option<u64>) -> Option<u64> {
        None
    }

    /// 见 [`Recovering::extension`]。
    fn extension(&self) -> Option<&'static str> {
        None
    }

    /// 部分恢复（见 [`Recovering::intact`]）：文件在 `cut` 处断了，之前的数据都与结构对得上，给出要保存的长度；
    /// 默认说不出（`None`）。`cut` 由遍历器给出：结构对不上时是那一步的位置，正在等的一步只收到一部分时是那一步的开头，
    /// 其余（跳过大段数据、查找、已知结尾）是数据断开的位置。插件也可以给出自己记着的、更靠前的位置
    /// （例如最后一个核对过的帧的结尾）。
    fn intact(&self, _cut: u64) -> Option<u64> {
        None
    }

    /// `Scan` 查找时经过的字节：从 `at` 起的 `bytes`，按顺序、不重复，不含匹配处及其之后的字节
    /// （匹配的开头跨块时，它前面几个字节可能已经在上一次交过来）。要逐字节看内容的格式（JPEG 的熵编码数据）用它；
    /// 默认什么也不做。
    fn scanned(&mut self, _at: u64, _bytes: &[u8]) {}

    /// 数据到此为止，正在等的一步只收到了一部分：从 `at` 起的 `bytes`（不足要的长度）。
    /// 按固定大小一段段要数据、自己在里面找边界的格式（FLAC 的帧）用它把最后这一段也看完，之后才问
    /// [`finish`](Self::finish) 与 [`intact`](Self::intact)。默认什么也不做。
    fn short(&mut self, _at: u64, _bytes: &[u8]) {}
}

#[derive(Clone)]
enum Mode {
    Need {
        at: u64,
        len: usize,
        buffer: Vec<u8>,
    },
    Scan {
        /// 下一个还没看过的字节。
        from: u64,
        scanner: Scanner,
        /// 紧挨在 `from` 之前、还要参与匹配的字节。
        carry: Vec<u8>,
    },
    Done(u64),
    Invalid,
    /// 数据在等 `at` 起的一步时用完了，收到的那一部分已经交给 [`Structure::short`]。
    Short {
        at: u64,
    },
}

#[derive(Clone)]
pub(crate) struct Walker<S> {
    structure: S,
    mode: Mode,
    /// 下一步允许的最小位置。
    cursor: u64,
    /// 格式的最大尺寸：要的位置或给出的长度超过它时判为认错了（长度字段离谱）。
    max_size: u64,
    /// 最近一次交付或匹配处起的字节，供下一步回看。
    recent_at: u64,
    recent: Vec<u8>,
}

impl<S: Structure> Walker<S> {
    /// `max_size` 是格式的 [`FormatInfo::max_size`](crate::FormatInfo)。
    pub(crate) fn new(max_size: u64, structure: S) -> Self {
        let first = structure.first();
        let mut walker = Self {
            structure,
            mode: Mode::Invalid,
            cursor: 0,
            max_size,
            recent_at: 0,
            recent: Vec::new(),
        };
        walker.mode = walker.enter(first);
        walker
    }

    pub(crate) fn structure(&self) -> &S {
        &self.structure
    }

    pub(crate) fn structure_mut(&mut self) -> &mut S {
        &mut self.structure
    }

    /// 已经看过、从 `at` 起最多 `max` 个字节。
    fn recalled(&self, at: u64, max: usize) -> &[u8] {
        let Some(skip) = at.checked_sub(self.recent_at) else {
            return &[];
        };
        let tail = usize::try_from(skip)
            .ok()
            .and_then(|skip| self.recent.get(skip..))
            .unwrap_or_default();
        tail.get(..max.min(tail.len())).unwrap_or_default()
    }

    fn enter(&mut self, step: Step) -> Mode {
        let within = |at: u64| at >= self.cursor && at <= self.max_size;
        match step {
            Step::Need { at, len } if within(at) && (1..=MAX_NEED).contains(&len) => {
                let mut buffer = Vec::with_capacity(len);
                buffer.extend_from_slice(self.recalled(at, len));
                Mode::Need { at, len, buffer }
            }
            Step::Scan { from, scanner } if within(from) => {
                let carry = self.recalled(from, usize::MAX).to_vec();
                Mode::Scan {
                    from: from + carry.len() as u64,
                    scanner,
                    carry,
                }
            }
            Step::Done(length) if within(length) => Mode::Done(length),
            _ => Mode::Invalid,
        }
    }

    fn feed(&mut self, offset: u64, block: &[u8]) -> DataCheck {
        let Some(end) = offset.checked_add(block.len() as u64) else {
            return DataCheck::Invalid;
        };
        loop {
            let step = match &mut self.mode {
                Mode::Done(length) => return DataCheck::Complete(*length),
                Mode::Invalid | Mode::Short { .. } => return DataCheck::Invalid,
                Mode::Need { at, len, buffer } => {
                    if buffer.len() < *len {
                        let next = *at + buffer.len() as u64;
                        if next >= end {
                            return DataCheck::Continue;
                        }
                        // 要的字节已经在之前的块里过去、又没保留下来：插件往回要数据，是插件的缺陷
                        let Some(start) = next.checked_sub(offset) else {
                            self.mode = Mode::Invalid;
                            return DataCheck::Invalid;
                        };
                        let start = start as usize;
                        let take = (*len - buffer.len()).min(block.len() - start);
                        let Some(piece) = block.get(start..start + take) else {
                            return DataCheck::Invalid;
                        };
                        buffer.extend_from_slice(piece);
                        if buffer.len() < *len {
                            return DataCheck::Continue;
                        }
                    }
                    let at = *at;
                    let bytes = std::mem::take(buffer);
                    self.cursor = at;
                    let step = self.structure.step(Event::Bytes { at, bytes: &bytes });
                    self.recent_at = at;
                    self.recent = bytes;
                    step
                }
                Mode::Scan {
                    from,
                    scanner,
                    carry,
                } => {
                    if *from >= end {
                        return DataCheck::Continue;
                    }
                    let Some(start) = from.checked_sub(offset) else {
                        self.mode = Mode::Invalid;
                        return DataCheck::Invalid;
                    };
                    let Some(region) = block.get(start as usize..) else {
                        return DataCheck::Invalid;
                    };
                    let window_start = *from - carry.len() as u64;
                    let mut window = std::mem::take(carry);
                    window.extend_from_slice(region);
                    match (scanner.find)(&window) {
                        Some(index) => {
                            let at = window_start + index as u64;
                            if let Some(seen) = at
                                .checked_sub(*from)
                                .and_then(|seen| region.get(..seen as usize))
                                .filter(|seen| !seen.is_empty())
                            {
                                self.structure.scanned(*from, seen);
                            }
                            self.cursor = at;
                            self.recent_at = at;
                            self.recent = window.split_off(index.min(window.len()));
                            self.structure.step(Event::Found { at })
                        }
                        None => {
                            self.structure.scanned(*from, region);
                            let keep = scanner.overlap.min(window.len());
                            *carry = window.split_off(window.len() - keep);
                            *from = end;
                            return DataCheck::Continue;
                        }
                    }
                }
            };
            self.mode = self.enter(step);
        }
    }
}

impl<S: Structure> Walker<S> {
    /// 数据用完时，正在等的一步收到的那一部分交给插件看（[`Structure::short`]），只交一次。
    fn flush_short(&mut self) {
        let Mode::Need { at, buffer, .. } = &mut self.mode else {
            return;
        };
        if buffer.is_empty() {
            return;
        }
        let (at, bytes) = (*at, std::mem::take(buffer));
        self.mode = Mode::Short { at };
        self.structure.short(at, &bytes);
    }

    /// 部分恢复的断点（见 [`Structure::intact`]），不超过 `received`。
    fn cut(&self, received: u64) -> u64 {
        let cut = match &self.mode {
            // 最后一步的位置：那一步读到的东西对不上，或它要的下一步不合理
            Mode::Invalid => self.cursor,
            Mode::Need { at, buffer, .. } if !buffer.is_empty() => *at,
            Mode::Short { at } => *at,
            Mode::Need { .. } | Mode::Scan { .. } | Mode::Done(_) => received,
        };
        cut.min(received)
    }
}

impl<S: Structure> Recovering for Walker<S> {
    fn data_check(&mut self, offset: u64, block: &[u8]) -> DataCheck {
        self.feed(offset, block)
    }

    fn open_ended(&self) -> bool {
        self.structure.open_ended()
    }

    fn finish(&mut self, received: u64) -> Option<u64> {
        self.flush_short();
        let waiting_at = match &self.mode {
            Mode::Done(length) => return (*length <= received).then_some(*length),
            Mode::Need { at, buffer, .. } if buffer.is_empty() => Some(*at),
            _ => None,
        };
        self.structure
            .finish(received, waiting_at)
            .filter(|&length| length <= received)
    }

    fn waiting_at(&self) -> Option<u64> {
        match &self.mode {
            Mode::Need { at, buffer, .. } => at.checked_add(buffer.len() as u64),
            Mode::Done(length) => Some(*length),
            Mode::Scan { .. } | Mode::Invalid | Mode::Short { .. } => None,
        }
    }

    fn extension(&self) -> Option<&'static str> {
        self.structure.extension()
    }

    fn intact(&mut self, received: u64) -> Option<u64> {
        self.flush_short();
        let cut = self.cut(received);
        self.structure
            .intact(cut)
            .filter(|&length| length > 0 && length <= received)
    }
}

/// 要读的东西分散在文件各处、位置是读到别处才知道的格式（TIFF 的 IFD 与数组、复合文档的 FAT 扇区）：
/// 按位置从小到大排着，一样一样读。遍历器只能往后读，排到时位置已经过去的取不回来，由使用方决定怎么处理。
pub(crate) struct ReadQueue<T> {
    items: std::collections::BTreeMap<u64, Vec<T>>,
}

impl<T> Default for ReadQueue<T> {
    fn default() -> Self {
        Self {
            items: std::collections::BTreeMap::new(),
        }
    }
}

impl<T> ReadQueue<T> {
    pub(crate) fn push(&mut self, at: u64, item: T) {
        self.items.entry(at).or_default().push(item);
    }

    /// 位置最小的一样。
    pub(crate) fn pop(&mut self) -> Option<(u64, T)> {
        let mut first = self.items.first_entry()?;
        let at = *first.key();
        let item = first.get_mut().pop();
        if first.get().is_empty() {
            first.remove();
        }
        item.map(|item| (at, item))
    }

    pub(crate) fn first_at(&self) -> Option<u64> {
        self.items.keys().next().copied()
    }
}

/// 文件头里就写明了总长度的格式（BMP、SQLite 等）：不用再看后面的内容，按这个长度取数据。
/// 文件头同时说明了该存成哪个扩展名的（PST 与 OST、WOFF 与 WOFF2），一并带上。
pub(crate) struct KnownLength {
    length: u64,
    extension: Option<&'static str>,
    /// 部分恢复：断点在这之后（文件头之后真正的内容开始了）才保存前面那一段；`None` 不做部分恢复。
    intact_after: Option<u64>,
}

impl KnownLength {
    pub(crate) fn new(length: u64) -> Self {
        Self {
            length,
            extension: None,
            intact_after: None,
        }
    }

    /// 见 [`Recovering::extension`]。
    pub(crate) fn with_extension(self, extension: Option<&'static str>) -> Self {
        Self { extension, ..self }
    }

    /// 截短时保存断点之前的一段（见 [`Recovering::intact`]），断点要在 `after` 之后：
    /// 文件头核对过、断点之前又有了真正的内容（例如位图的像素、音频的采样）。内容本身核对不了，与完整时一样。
    pub(crate) fn intact_after(self, after: u64) -> Self {
        Self {
            intact_after: Some(after),
            ..self
        }
    }
}

impl Structure for KnownLength {
    fn first(&self) -> Step {
        Step::Done(self.length)
    }

    fn step(&mut self, _event: Event<'_>) -> Step {
        Step::Invalid
    }

    fn extension(&self) -> Option<&'static str> {
        self.extension
    }

    fn intact(&self, cut: u64) -> Option<u64> {
        self.intact_after.filter(|&after| cut > after).map(|_| cut)
    }
}

/// 总长度在文件头里就能算出来、另有一处可以核对的格式（CAB 的第一个文件项、RAF 的 JPEG 预览、
/// EVTX 的最后一块、注册表的第一个 hive bin、图元文件的结束记录）：先读 `at` 起的 `len` 字节交给 `check`，
/// 通过了文件就到 `length`，否则不认。
pub(crate) struct CheckedLength<F> {
    pub at: u64,
    pub len: usize,
    pub check: F,
    pub length: u64,
}

impl<F: Fn(&[u8]) -> bool + Send> Structure for CheckedLength<F> {
    fn first(&self) -> Step {
        Step::Need {
            at: self.at,
            len: self.len,
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        match event {
            Event::Bytes { bytes, .. } if (self.check)(bytes) => Step::Done(self.length),
            _ => Step::Invalid,
        }
    }
}

/// 在窗口里找一个固定字节串。
pub(crate) fn find_bytes(window: &[u8], pattern: &[u8]) -> Option<usize> {
    if pattern.is_empty() {
        return None;
    }
    window
        .windows(pattern.len())
        .position(|candidate| candidate == pattern)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 把同一份数据按各种块大小送进去，结果必须一致：块边界不能影响判断。
    pub(crate) fn check_all_splits(
        make: impl Fn() -> Box<dyn Recovering>,
        data: &[u8],
        expected: Option<u64>,
    ) {
        let block_sizes = (1..=40).chain([64, 100, 511, 512, 4096, data.len().max(1)]);
        for block_size in block_sizes {
            assert_eq!(
                run(make(), data, block_size),
                expected,
                "块大小 {block_size}"
            );
        }
    }

    /// 按块送入数据，返回判断出的文件长度；`None` 表示无效或不完整。
    pub(crate) fn run(
        mut recovering: Box<dyn Recovering>,
        data: &[u8],
        block_size: usize,
    ) -> Option<u64> {
        run_keeping(&mut recovering, data, block_size)
    }

    /// 同 [`run`]，读完后 `recovering` 留给调用方（再问它按内容定的扩展名）。
    /// 给出的长度超过数据末尾时同深度恢复在设备末尾的处理：不完整。
    pub(crate) fn run_keeping(
        recovering: &mut Box<dyn Recovering>,
        data: &[u8],
        block_size: usize,
    ) -> Option<u64> {
        let mut offset = 0u64;
        for block in data.chunks(block_size) {
            match recovering.data_check(offset, block) {
                DataCheck::Continue => {}
                DataCheck::Complete(length) => {
                    return (length <= data.len() as u64).then_some(length);
                }
                DataCheck::Invalid => return None,
            }
            offset += block.len() as u64;
        }
        recovering.finish(data.len() as u64)
    }

    /// 按块送入数据，文件不完整时再问部分恢复（同深度恢复：结构对不上时这一块已经交给插件看过；
    /// 给出的结尾超出数据末尾时取到末尾）。完整或说不出可靠的一段时返回 `None`。
    pub(crate) fn run_intact(
        mut recovering: Box<dyn Recovering>,
        data: &[u8],
        block_size: usize,
    ) -> Option<u64> {
        let mut offset = 0u64;
        for block in data.chunks(block_size) {
            match recovering.data_check(offset, block) {
                DataCheck::Continue => {}
                DataCheck::Complete(length) if length <= data.len() as u64 => return None,
                DataCheck::Complete(_) => return recovering.intact(data.len() as u64),
                DataCheck::Invalid => return recovering.intact(offset + block.len() as u64),
            }
            offset += block.len() as u64;
        }
        if recovering.finish(data.len() as u64).is_some() {
            return None;
        }
        recovering.intact(data.len() as u64)
    }

    /// 同 [`check_all_splits`]，看的是部分恢复给出的长度。
    pub(crate) fn check_intact_splits(
        make: impl Fn() -> Box<dyn Recovering>,
        data: &[u8],
        expected: Option<u64>,
    ) {
        let block_sizes = (1..=40).chain([64, 100, 511, 512, 4096, data.len().max(1)]);
        for block_size in block_sizes {
            assert_eq!(
                run_intact(make(), data, block_size),
                expected,
                "块大小 {block_size}"
            );
        }
    }

    /// 测试用结构：头 4 字节是大端长度 n，随后 n 字节，再找一个 "END"，文件到 "END" 为止。
    struct LengthThenMarker {
        phase: u8,
    }

    impl Structure for LengthThenMarker {
        fn first(&self) -> Step {
            Step::Need { at: 0, len: 4 }
        }

        fn step(&mut self, event: Event<'_>) -> Step {
            match (self.phase, event) {
                (0, Event::Bytes { at, bytes }) => {
                    self.phase = 1;
                    let Some(length) = yisi_part::bytes::be32(bytes, 0) else {
                        return Step::Invalid;
                    };
                    Step::Scan {
                        from: at + 4 + u64::from(length),
                        scanner: Scanner {
                            find: |window| find_bytes(window, b"END"),
                            overlap: 2,
                        },
                    }
                }
                (1, Event::Found { at }) => Step::Done(at + 3),
                _ => Step::Invalid,
            }
        }
    }

    #[test]
    fn need_skip_and_scan_work_across_any_block_boundary() {
        let mut data = vec![0, 0, 0, 5];
        data.extend_from_slice(b"ENDxx"); // 被跳过的 5 字节里有假的 END
        data.extend_from_slice(b"..E.EN.END");
        data.extend_from_slice(b"tail");
        let expected = data.len() as u64 - 4;
        check_all_splits(
            || Box::new(Walker::new(u64::MAX, LengthThenMarker { phase: 0 })),
            &data,
            Some(expected),
        );
    }

    #[test]
    fn missing_marker_is_incomplete() {
        let data = [0, 0, 0, 1, b'x', b'E', b'N'];
        check_all_splits(
            || Box::new(Walker::new(u64::MAX, LengthThenMarker { phase: 0 })),
            &data,
            None,
        );
    }

    /// 先读 2 字节，再从同一位置读 6 字节：要靠保留下来的字节补上。
    struct PeekThenRead {
        peeked: bool,
    }

    impl Structure for PeekThenRead {
        fn first(&self) -> Step {
            Step::Need { at: 1, len: 2 }
        }

        fn step(&mut self, event: Event<'_>) -> Step {
            match event {
                Event::Bytes { at, bytes } if !self.peeked => {
                    self.peeked = true;
                    if bytes == b"bc" {
                        Step::Need { at, len: 6 }
                    } else {
                        Step::Invalid
                    }
                }
                Event::Bytes { at, bytes } if bytes == b"bcdefg" => Step::Done(at + 6),
                _ => Step::Invalid,
            }
        }
    }

    #[test]
    fn re_reading_just_delivered_bytes_works_across_blocks() {
        check_all_splits(
            || Box::new(Walker::new(u64::MAX, PeekThenRead { peeked: false })),
            b"abcdefgh",
            Some(7),
        );
    }

    struct GoesBackwards;

    impl Structure for GoesBackwards {
        fn first(&self) -> Step {
            Step::Need { at: 4, len: 4 }
        }

        fn step(&mut self, _event: Event<'_>) -> Step {
            Step::Need { at: 0, len: 4 }
        }
    }

    #[test]
    fn stepping_backwards_or_oversized_needs_are_invalid() {
        check_all_splits(
            || Box::new(Walker::new(u64::MAX, GoesBackwards)),
            &[0u8; 16],
            None,
        );

        struct TooBig;
        impl Structure for TooBig {
            fn first(&self) -> Step {
                Step::Need {
                    at: 0,
                    len: MAX_NEED + 1,
                }
            }
            fn step(&mut self, _event: Event<'_>) -> Step {
                Step::Invalid
            }
        }
        let mut walker = Walker::new(u64::MAX, TooBig);
        assert_eq!(walker.data_check(0, &[0; 8]), DataCheck::Invalid);
    }

    #[test]
    fn lengths_and_positions_beyond_the_format_maximum_are_invalid() {
        let mut fits = Walker::new(100, KnownLength::new(100));
        assert_eq!(fits.data_check(0, &[0; 8]), DataCheck::Complete(100));
        let mut too_long = Walker::new(100, KnownLength::new(101));
        assert_eq!(too_long.data_check(0, &[0; 8]), DataCheck::Invalid);
        // 第一步就要读上限之外的位置
        let far = Walker::new(3, LengthThenMarker { phase: 0 });
        assert_eq!(far.waiting_at(), Some(0));
        check_all_splits(
            || Box::new(Walker::new(5, LengthThenMarker { phase: 0 })),
            &[0, 0, 0, 9, 1, 2, 3],
            None,
        );
    }
}
