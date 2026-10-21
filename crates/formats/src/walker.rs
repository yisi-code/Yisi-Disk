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
}

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
}

pub(crate) struct Walker<S> {
    structure: S,
    mode: Mode,
    /// 下一步允许的最小位置。
    cursor: u64,
    /// 最近一次交付或匹配处起的字节，供下一步回看。
    recent_at: u64,
    recent: Vec<u8>,
}

impl<S: Structure> Walker<S> {
    pub(crate) fn new(structure: S) -> Self {
        let first = structure.first();
        let mut walker = Self {
            structure,
            mode: Mode::Invalid,
            cursor: 0,
            recent_at: 0,
            recent: Vec::new(),
        };
        walker.mode = walker.enter(first);
        walker
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
        match step {
            Step::Need { at, len } if at >= self.cursor && (1..=MAX_NEED).contains(&len) => {
                let mut buffer = Vec::with_capacity(len);
                buffer.extend_from_slice(self.recalled(at, len));
                Mode::Need { at, len, buffer }
            }
            Step::Scan { from, scanner } if from >= self.cursor => {
                let carry = self.recalled(from, usize::MAX).to_vec();
                Mode::Scan {
                    from: from + carry.len() as u64,
                    scanner,
                    carry,
                }
            }
            Step::Done(length) if length >= self.cursor => Mode::Done(length),
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
                Mode::Invalid => return DataCheck::Invalid,
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
                            self.cursor = at;
                            self.recent_at = at;
                            self.recent = window.split_off(index.min(window.len()));
                            self.structure.step(Event::Found { at })
                        }
                        None => {
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

impl<S: Structure> Recovering for Walker<S> {
    fn data_check(&mut self, offset: u64, block: &[u8]) -> DataCheck {
        self.feed(offset, block)
    }

    fn open_ended(&self) -> bool {
        self.structure.open_ended()
    }

    fn finish(&mut self, received: u64) -> Option<u64> {
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
            Mode::Scan { .. } | Mode::Invalid => None,
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
        let mut offset = 0u64;
        for block in data.chunks(block_size) {
            match recovering.data_check(offset, block) {
                DataCheck::Continue => {}
                DataCheck::Complete(length) => return Some(length),
                DataCheck::Invalid => return None,
            }
            offset += block.len() as u64;
        }
        recovering.finish(data.len() as u64)
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
                    let Some(length) = crate::bytes::be32(bytes, 0) else {
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
            || Box::new(Walker::new(LengthThenMarker { phase: 0 })),
            &data,
            Some(expected),
        );
    }

    #[test]
    fn missing_marker_is_incomplete() {
        let data = [0, 0, 0, 1, b'x', b'E', b'N'];
        check_all_splits(
            || Box::new(Walker::new(LengthThenMarker { phase: 0 })),
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
            || Box::new(Walker::new(PeekThenRead { peeked: false })),
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
        check_all_splits(|| Box::new(Walker::new(GoesBackwards)), &[0u8; 16], None);

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
        let mut walker = Walker::new(TooBig);
        assert_eq!(walker.data_check(0, &[0; 8]), DataCheck::Invalid);
    }
}
