//! JPEG 熵编码数据的逐位核对：照 SOF、DHT、DRI、SOS 段建好哈夫曼表与每次扫描的块布局，逐个 MCU 做哈夫曼解码
//! （不反量化、不做 IDCT），核对码字合法、系数位置不越界、DC 值在范围内、RST 标记在该出现的地方按顺序出现、
//! 每次扫描正好解出 SOF 说的那么多 MCU、扫描结束后紧接着就是标记、到 EOI 时每个分量都扫过。
//!
//! 只核对哈夫曼编码的顺序式（SOF0、SOF1，8 位或 12 位）；渐进式、无损、分层、算术编码与靠 DNL 定高度的不核对
//! （[`Entropy::is_inert`]），文件照原来的标记段走法判断。
//!
//! 数据按文件里的顺序送入；核对状态可以复制，复制出来的接着送别处的数据（两段重组）。

use std::sync::Arc;

use yisi_part::bytes::be16;

/// 查表时一次看这么多位：码长不超过它的码字一次查到，更长的逐个码长比较。
const LOOKAHEAD: u32 = 9;

/// 一个 MCU 里最多这么多块（标准的上限）。
const MAX_BLOCKS_PER_MCU: usize = 10;

#[derive(Debug)]
struct Huffman {
    /// 按开头 [`LOOKAHEAD`] 位查：高 8 位是码长、低 8 位是符号；0 表示码长更长或不是码字。
    fast: [u16; 1 << LOOKAHEAD],
    /// 各码长（1–16，下标即码长）最大的码字，没有这个码长时为 -1。
    max_code: [i32; 17],
    /// 各码长第一个码字的符号下标减去这个码字。
    value_offset: [i32; 17],
    values: Vec<u8>,
    /// 当 DC 表用时，一个 DC 最少占几位（码长加数值位数）。
    min_dc_bits: u32,
    /// 当 AC 表用时，一块的交流系数最少占几位：EOB 的码长；没有 EOB 时按最短的码字算。
    min_ac_bits: u32,
}

impl Huffman {
    /// 按 DHT 里的 16 个码长计数与符号建表。码字溢出、或用到全 1 的码字（不是合法的表）时为 `None`。
    fn new(counts: &[u8], values: &[u8]) -> Option<Self> {
        let mut fast = [0u16; 1 << LOOKAHEAD];
        let mut max_code = [-1i32; 17];
        let mut value_offset = [0i32; 17];
        let mut code: u32 = 0;
        let mut index: usize = 0;
        let (mut min_dc_bits, mut min_length, mut eob) = (u32::MAX, u32::MAX, None);
        for length in 1..=16u32 {
            let count = usize::from(*counts.get(length as usize - 1)?);
            if count > 0 {
                *value_offset.get_mut(length as usize)? = index as i32 - code as i32;
                for _ in 0..count {
                    let value = *values.get(index)?;
                    min_dc_bits = min_dc_bits.min(length + u32::from(value));
                    min_length = min_length.min(length);
                    if value == 0 && eob.is_none() {
                        eob = Some(length);
                    }
                    if length <= LOOKAHEAD {
                        let shift = LOOKAHEAD - length;
                        let first = (code << shift) as usize;
                        let entry = ((length as u16) << 8) | u16::from(value);
                        for slot in fast.get_mut(first..first + (1 << shift))? {
                            *slot = entry;
                        }
                    }
                    code += 1;
                    index += 1;
                }
                *max_code.get_mut(length as usize)? = code as i32 - 1;
            }
            if code >= 1 << length {
                return None;
            }
            code <<= 1;
        }
        (index == values.len()).then_some(Self {
            fast,
            max_code,
            value_offset,
            values: values.to_vec(),
            min_dc_bits,
            min_ac_bits: eob.unwrap_or(min_length),
        })
    }

    /// `peek` 是缓冲最前面的 16 位（不够 16 位时低位补 0）。返回码长与符号；不是码字时为 `None`。
    fn decode(&self, peek: u16) -> Option<(u32, u8)> {
        let entry = *self.fast.get(usize::from(peek >> (16 - LOOKAHEAD)))?;
        let peek = u32::from(peek);
        if entry != 0 {
            return Some((u32::from(entry >> 8), entry as u8));
        }
        for length in LOOKAHEAD + 1..=16 {
            let code = (peek >> (16 - length)) as i32;
            if code <= *self.max_code.get(length as usize)? {
                let index = code + self.value_offset.get(length as usize)?;
                return Some((length, *self.values.get(usize::try_from(index).ok()?)?));
            }
        }
        None
    }
}

#[derive(Debug)]
struct Frame {
    /// 8 或 12 位。
    precision: u8,
    width: u32,
    height: u32,
    /// 各分量的标识与采样因子。
    components: Vec<(u8, u32, u32)>,
    max_h: u32,
    max_v: u32,
}

/// 一次扫描的布局（扫描开始时定下，之后不变）。
#[derive(Debug)]
struct Layout {
    /// MCU 里的各块依次属于扫描里的第几个分量。
    blocks: Vec<u8>,
    dc: Vec<Arc<Huffman>>,
    ac: Vec<Arc<Huffman>>,
    /// 扫描里的分量对应帧里的哪几个（位掩码）。
    covers: u8,
    mcus: u64,
    restart: u64,
    max_dc_size: u32,
    max_ac_size: u32,
    dc_limit: i32,
    /// 一个 MCU 最少占几位（两段重组时估第二段至少还有多长）。
    mcu_min_bits: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 正在解 MCU。
    Data,
    /// 一个重启间隔解完了，接下来只能是填充位与下一个 RST 标记。
    AwaitRestart,
    /// 这次扫描的 MCU 都解完了，接下来只能是填充位与结束熵编码数据的标记。
    Ended,
}

/// 解码中途不对。
struct Mismatch;

#[derive(Debug, Clone)]
struct Scan {
    layout: Arc<Layout>,
    cursor: Cursor,
}

/// 扫描里解到哪了。与布局分开放，解码时布局只读、它可改。
#[derive(Debug, Clone)]
struct Cursor {
    /// 已经解完的 MCU 数。
    mcu: u64,
    /// 当前 MCU 里第几块、块里下一个系数的位置（0 是 DC）。
    block: usize,
    k: u32,
    dc: [i32; 4],
    since_restart: u64,
    next_restart: u8,
    phase: Phase,
    /// 还没用掉的位，从最高位起。
    bits: u64,
    count: u32,
    /// 上一个字节是 0xFF，要看下一个字节才知道是填充的数据字节、RST 标记还是填充字节。
    pending_ff: bool,
}

impl Scan {
    fn new(layout: Arc<Layout>) -> Self {
        Self {
            layout,
            cursor: Cursor {
                mcu: 0,
                block: 0,
                k: 0,
                dc: [0; 4],
                since_restart: 0,
                next_restart: 0,
                phase: Phase::Data,
                bits: 0,
                count: 0,
                pending_ff: false,
            },
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<(), Mismatch> {
        let mut rest = bytes;
        while let Some((&byte, tail)) = rest.split_first() {
            if self.cursor.pending_ff {
                self.cursor.pending_ff = false;
                match byte {
                    0x00 => self.load(&[0xFF])?,
                    // 标记前的填充字节
                    0xFF => self.cursor.pending_ff = true,
                    0xD0..=0xD7 => self.restart(byte - 0xD0)?,
                    // 结束熵编码数据的标记由遍历器在交过来之前截住，这里出现就是对不上
                    _ => return Err(Mismatch),
                }
                rest = tail;
            } else if byte == 0xFF {
                self.cursor.pending_ff = true;
                rest = tail;
            } else {
                // 到下一个 0xFF 之前都是普通的数据字节，成批装进缓冲
                let (plain, after) = rest.split_at(find_ff(rest).unwrap_or(rest.len()));
                self.load(plain)?;
                rest = after;
            }
        }
        Ok(())
    }

    /// 装入数据字节（已去掉填充的 0x00），缓冲攒满就解码。
    fn load(&mut self, mut bytes: &[u8]) -> Result<(), Mismatch> {
        while !bytes.is_empty() {
            // 间隔或扫描结束之后只能是填充位与标记
            if self.cursor.phase != Phase::Data {
                return Err(Mismatch);
            }
            let cursor = &mut self.cursor;
            let room = ((64 - cursor.count) / 8) as usize;
            let (now, later) = bytes.split_at(room.min(bytes.len()));
            for &byte in now {
                cursor.bits |= u64::from(byte) << (56 - cursor.count);
                cursor.count += 8;
            }
            bytes = later;
            if cursor.count > 56 {
                self.decode(false)?;
            }
        }
        Ok(())
    }

    /// 解出缓冲里够解的符号。`last`：后面没有数据了（遇到了标记），剩下的位必须正好解完当前的间隔或扫描。
    fn decode(&mut self, last: bool) -> Result<(), Mismatch> {
        let (layout, cursor) = (&*self.layout, &mut self.cursor);
        // 位不够解下一个符号：后面还有数据就等，没有了就是对不上
        let short = if last { Err(Mismatch) } else { Ok(()) };
        while cursor.phase == Phase::Data {
            let slot = usize::from(*layout.blocks.get(cursor.block).ok_or(Mismatch)?);
            if cursor.k == 0 && !cursor.dc(layout, slot)? {
                return short;
            }
            if !cursor.ac(layout.ac.get(slot).ok_or(Mismatch)?, layout.max_ac_size)? {
                return short;
            }
            cursor.next_block(layout);
        }
        // 间隔或扫描结束时剩下的只能是凑满一个字节的填充位，按标准是 1
        let padding = cursor.count;
        if padding >= 8 || (padding > 0 && cursor.bits >> (64 - padding) != (1 << padding) - 1) {
            return Err(Mismatch);
        }
        Ok(())
    }

    /// 遇到 RST 标记：当前间隔必须正好解完，标记的序号对得上。
    fn restart(&mut self, number: u8) -> Result<(), Mismatch> {
        self.decode(true)?;
        let cursor = &mut self.cursor;
        if cursor.phase != Phase::AwaitRestart || number != cursor.next_restart {
            return Err(Mismatch);
        }
        cursor.next_restart = (number + 1) & 7;
        cursor.since_restart = 0;
        cursor.dc = [0; 4];
        cursor.bits = 0;
        cursor.count = 0;
        cursor.phase = Phase::Data;
        Ok(())
    }

    /// 熵编码数据结束（遇到了别的标记）：这次扫描必须正好解完。
    fn end(&mut self) -> Result<(), Mismatch> {
        // 标记开头的 0xFF 已经交过来了，不算数据
        self.cursor.pending_ff = false;
        self.decode(true)?;
        if self.cursor.phase == Phase::Ended {
            Ok(())
        } else {
            Err(Mismatch)
        }
    }
}

impl Cursor {
    /// 查表解出缓冲开头的码字；解不出时，位够 16 位就是对不上，不够可能只是位还不够（`Ok(None)`）。
    fn lookup(&self, table: &Huffman) -> Result<Option<(u32, u8)>, Mismatch> {
        match table.decode((self.bits >> 48) as u16) {
            Some(found) => Ok(Some(found)),
            None if self.count >= 16 => Err(Mismatch),
            None => Ok(None),
        }
    }

    /// 解这一块的 DC。位不够时返回 `false`，什么也不动。
    fn dc(&mut self, layout: &Layout, slot: usize) -> Result<bool, Mismatch> {
        let table = layout.dc.get(slot).ok_or(Mismatch)?;
        let Some((length, size)) = self.lookup(table)? else {
            return Ok(false);
        };
        let size = u32::from(size);
        if size > layout.max_dc_size {
            return Err(Mismatch);
        }
        let total = length + size;
        if total > self.count {
            return Ok(false);
        }
        let value = if size == 0 {
            0
        } else {
            ((self.bits << length) >> (64 - size)) as i32
        };
        self.bits <<= total;
        self.count -= total;
        let predictor = self.dc.get_mut(slot).ok_or(Mismatch)?;
        *predictor = predictor
            .checked_add(extend(value, size))
            .filter(|value| value.abs() <= layout.dc_limit)
            .ok_or(Mismatch)?;
        self.k = 1;
        Ok(true)
    }

    /// 解这一块的交流系数直到块结束（只核对位置，不取数值）。位不够时返回 `false`，已经解出的保留。
    fn ac(&mut self, table: &Huffman, max_size: u32) -> Result<bool, Mismatch> {
        // 这里是热点：在局部变量上解，最后写回
        let (mut bits, mut count, mut k) = (self.bits, self.count, self.k);
        let outcome = loop {
            if k >= 64 {
                break Ok(true);
            }
            let Some((length, symbol)) = table.decode((bits >> 48) as u16) else {
                break if count >= 16 {
                    Err(Mismatch)
                } else {
                    Ok(false)
                };
            };
            let (run, size) = (u32::from(symbol >> 4), u32::from(symbol & 0x0F));
            if size > max_size {
                break Err(Mismatch);
            }
            let total = length + size;
            if total > count {
                break Ok(false);
            }
            bits <<= total;
            count -= total;
            if size == 0 {
                match run {
                    // EOB
                    0 => k = 64,
                    // ZRL：16 个 0，之后还要有非零系数
                    15 if k + 16 <= 63 => k += 16,
                    _ => break Err(Mismatch),
                }
            } else {
                k += run;
                if k > 63 {
                    break Err(Mismatch);
                }
                k += 1;
            }
        };
        (self.bits, self.count, self.k) = (bits, count, k);
        outcome
    }

    /// 一块解完，到下一块；MCU 解完时数一个，看间隔或扫描是否到头。
    fn next_block(&mut self, layout: &Layout) {
        self.k = 0;
        self.block += 1;
        if self.block == layout.blocks.len() {
            self.block = 0;
            self.mcu += 1;
            self.since_restart += 1;
            if self.mcu == layout.mcus {
                self.phase = Phase::Ended;
            } else if layout.restart > 0 && self.since_restart == layout.restart {
                self.phase = Phase::AwaitRestart;
            }
        }
    }
}

/// 0xFF 后面跟着它时，熵编码数据到此结束（不是填充的 0x00、RST 标记或填充字节）。
pub(super) fn ends_entropy(next: u8) -> bool {
    next != 0x00 && next != 0xFF && !(0xD0..=0xD7).contains(&next)
}

/// 熵编码数据在 `window` 里的哪里结束：第一个后面跟着 [`ends_entropy`] 的 0xFF。
pub(super) fn find_entropy_end(window: &[u8]) -> Option<usize> {
    let mut from = 0;
    loop {
        let at = from + find_ff(window.get(from..)?)?;
        if ends_entropy(*window.get(at + 1)?) {
            return Some(at);
        }
        from = at + 1;
    }
}

/// 第一个 0xFF 的位置。熵编码数据里 0xFF 很少，一次看 8 个字节。
fn find_ff(bytes: &[u8]) -> Option<usize> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    let (words, rest) = bytes.as_chunks::<8>();
    let mut base = 0;
    for word in words {
        let inverted = !u64::from_le_bytes(*word);
        // 取反后为 0 的字节就是原来的 0xFF
        if inverted.wrapping_sub(ONES) & !inverted & HIGHS != 0 {
            return word
                .iter()
                .position(|&byte| byte == 0xFF)
                .map(|at| base + at);
        }
        base += 8;
    }
    rest.iter()
        .position(|&byte| byte == 0xFF)
        .map(|at| base + at)
}

/// 按位数把数值还原成有符号数（JPEG 的 EXTEND）。
fn extend(value: i32, size: u32) -> i32 {
    if size == 0 {
        0
    } else if value < 1 << (size - 1) {
        value - (1 << size) + 1
    } else {
        value
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// 正在核对（还没见到 SOF 也算）。
    Live,
    /// 不是能核对的编码方式，或表与段不成样子：不核对。
    Inert,
    /// 内容对不上了。
    Broken,
    /// 到 EOI 为止一路对得上。
    Complete,
}

#[derive(Debug, Clone)]
pub(super) struct Entropy {
    state: State,
    frame: Option<Arc<Frame>>,
    dc_tables: [Option<Arc<Huffman>>; 4],
    ac_tables: [Option<Arc<Huffman>>; 4],
    restart: u64,
    scan: Option<Scan>,
    /// 已经扫完的分量（位掩码）。
    covered: u8,
}

impl Entropy {
    pub(super) fn new() -> Self {
        Self {
            state: State::Live,
            frame: None,
            dc_tables: Default::default(),
            ac_tables: Default::default(),
            restart: 0,
            scan: None,
            covered: 0,
        }
    }

    /// 一概不核对（没有打开两段重组）：不要段内容、不解码，开销与没有核对时一样。
    pub(super) fn off() -> Self {
        Self {
            state: State::Inert,
            ..Self::new()
        }
    }

    /// 不核对（编码方式不支持，或段不成样子）。
    pub(super) fn is_inert(&self) -> bool {
        self.state == State::Inert
    }

    pub(super) fn is_broken(&self) -> bool {
        self.state == State::Broken
    }

    pub(super) fn is_complete(&self) -> bool {
        self.state == State::Complete
    }

    /// 已经开始解图像数据、到现在都对得上：这时的状态可以留作两段重组的起点。
    pub(super) fn in_progress(&self) -> bool {
        self.state == State::Live && (self.scan.is_some() || self.covered != 0)
    }

    /// 两段重组时粗看 `data`（从块边界起、按 `block` 分块）里哪几块的开头可能是第二段的开头。只看标记、不解码：
    /// 这次扫描还剩的 MCU 至少要占那么多字节，在那之前就出现结束熵编码数据的标记的，一定接不上；
    /// 有重启间隔时，接下来的第一个 RST 标记序号要对、离开头要够远，扫描结束之前不能没有它；
    /// 没有重启间隔（或这次扫描在当前间隔里就结束）时不能出现 RST。别的照片的熵编码数据靠这一条排除得最多。
    /// 不在熵编码数据中间时说不准，每块都算。
    pub(super) fn possible_starts(&self, data: &[u8], block: usize) -> Vec<usize> {
        let blocks = data.len().div_ceil(block.max(1));
        let Some(scan) = self.scan.as_ref().filter(|_| self.state == State::Live) else {
            return (0..blocks).collect();
        };
        let (layout, cursor) = (&scan.layout, &scan.cursor);
        // 还要解这么多个 MCU 至少占的字节；缓冲里还没用掉的位也算数
        let bytes_for = |mcus: u64| {
            let bits = mcus
                .saturating_mul(layout.mcu_min_bits)
                .saturating_sub(u64::from(cursor.count));
            usize::try_from(bits / 8).unwrap_or(usize::MAX)
        };
        let left_in_scan = layout.mcus.saturating_sub(cursor.mcu);
        // 当前这个 MCU 可能已经解了大半，不算它
        let need = bytes_for(left_in_scan.saturating_sub(1));
        // 接下来的 RST：序号与离开头至少多远；`None` 是扫描结束之前不该再有 RST
        let restart = match cursor.phase {
            Phase::AwaitRestart => Some((cursor.next_restart, 0)),
            Phase::Data if layout.restart > 0 => {
                let left_in_interval = layout.restart.saturating_sub(cursor.since_restart);
                (left_in_scan > left_in_interval).then(|| {
                    (
                        cursor.next_restart,
                        bytes_for(left_in_interval.saturating_sub(1)),
                    )
                })
            }
            _ => None,
        };
        // 这一段里全部的标记（0xFF 后面不是 0x00、也不是 0xFF）：位置与第二个字节，只找一遍
        let mut markers = Vec::new();
        let mut from = 0;
        while let Some(at) = data.get(from..).and_then(find_ff).map(|at| from + at) {
            match data.get(at + 1) {
                Some(0x00 | 0xFF) => {}
                Some(&kind) => markers.push((at, kind)),
                None => break,
            }
            from = at + 1;
        }
        let mut next = 0;
        let mut starts = Vec::new();
        for index in 0..blocks {
            let start = index * block;
            let Some(&head) = data.get(start) else {
                break;
            };
            while markers.get(next).is_some_and(|&(at, _)| at < start) {
                next += 1;
            }
            // 第一段最后一个字节是 0xFF 时，它和第二段的第一个字节凑成一对
            let first = if cursor.pending_ff && head != 0x00 && head != 0xFF {
                Some((0, head))
            } else {
                markers.get(next).map(|&(at, kind)| (at - start, kind))
            };
            let fits = match first {
                None => true,
                Some((distance, kind @ 0xD0..=0xD7)) => {
                    restart.is_some_and(|(number, at_least)| {
                        kind == 0xD0 + number && distance >= at_least
                    })
                }
                Some((distance, _)) => restart.is_none() && distance >= need,
            };
            if fits {
                starts.push(index);
            }
        }
        starts
    }

    /// 这个标记段的内容要交给 [`segment`](Self::segment)。
    pub(super) fn wants(&self, marker: u8) -> bool {
        self.state == State::Live && matches!(marker, 0xC0..=0xCF | 0xDA | 0xDC..=0xDF)
    }

    /// 标记段的内容（长度字段之后的部分）。
    pub(super) fn segment(&mut self, marker: u8, body: &[u8]) {
        if self.state != State::Live {
            return;
        }
        let ok = match marker {
            0xC0 | 0xC1 if self.frame.is_none() => self.frame(marker, body),
            0xC4 => self.tables(body),
            0xDD => be16(body, 0).is_some_and(|interval| {
                self.restart = u64::from(interval);
                body.len() == 2
            }),
            0xDA => self.start_scan(body),
            // 渐进式、无损、分层、算术编码、DNL 等：不核对
            _ => false,
        };
        if !ok {
            self.inert();
        }
    }

    /// 熵编码数据，按顺序。
    pub(super) fn entropy(&mut self, bytes: &[u8]) {
        if self.state != State::Live {
            return;
        }
        let Some(scan) = self.scan.as_mut() else {
            return self.broken();
        };
        if scan.feed(bytes).is_err() {
            self.broken();
        }
    }

    /// 熵编码数据遇到标记结束了。
    pub(super) fn entropy_end(&mut self) {
        if self.state != State::Live {
            return;
        }
        match self.scan.take().map(|mut scan| scan.end().map(|()| scan)) {
            Some(Ok(scan)) => self.covered |= scan.layout.covers,
            _ => self.broken(),
        }
    }

    /// 到了 EOI。
    pub(super) fn eoi(&mut self) {
        if self.state != State::Live {
            return;
        }
        let all = self
            .frame
            .as_ref()
            .map_or(0, |frame| (1u8 << frame.components.len()) - 1);
        if self.scan.is_none() && all != 0 && self.covered == all {
            self.state = State::Complete;
        } else {
            self.broken();
        }
    }

    fn broken(&mut self) {
        self.state = State::Broken;
        self.scan = None;
    }

    fn inert(&mut self) {
        self.state = State::Inert;
        self.scan = None;
        self.frame = None;
        self.dc_tables = Default::default();
        self.ac_tables = Default::default();
    }

    fn frame(&mut self, marker: u8, body: &[u8]) -> bool {
        let (Some(&precision), Some(height), Some(width), Some(&count)) =
            (body.first(), be16(body, 1), be16(body, 3), body.get(5))
        else {
            return false;
        };
        let precision_ok = precision == 8 || (marker == 0xC1 && precision == 12);
        let count = usize::from(count);
        // 高度为 0 要靠 DNL 定，不核对
        if !precision_ok
            || width == 0
            || height == 0
            || !(1..=4).contains(&count)
            || body.len() != 6 + 3 * count
        {
            return false;
        }
        let mut components: Vec<(u8, u32, u32)> = Vec::with_capacity(count);
        for index in 0..count {
            let (Some(&id), Some(&sampling)) = (body.get(6 + 3 * index), body.get(7 + 3 * index))
            else {
                return false;
            };
            let (h, v) = (u32::from(sampling >> 4), u32::from(sampling & 0x0F));
            if !(1..=4).contains(&h)
                || !(1..=4).contains(&v)
                || components.iter().any(|&(other, ..)| other == id)
            {
                return false;
            }
            components.push((id, h, v));
        }
        let max_h = components.iter().map(|&(_, h, _)| h).max().unwrap_or(1);
        let max_v = components.iter().map(|&(.., v)| v).max().unwrap_or(1);
        self.frame = Some(Arc::new(Frame {
            precision,
            width: u32::from(width),
            height: u32::from(height),
            components,
            max_h,
            max_v,
        }));
        true
    }

    fn tables(&mut self, mut body: &[u8]) -> bool {
        while !body.is_empty() {
            let (Some(&kind), Some(counts)) = (body.first(), body.get(1..17)) else {
                return false;
            };
            let total: usize = counts.iter().map(|&count| usize::from(count)).sum();
            let (class, id) = (kind >> 4, usize::from(kind & 0x0F));
            let Some(values) = body.get(17..17 + total) else {
                return false;
            };
            let Some(table) = Huffman::new(counts, values) else {
                return false;
            };
            let slot = match class {
                0 => self.dc_tables.get_mut(id),
                1 => self.ac_tables.get_mut(id),
                _ => None,
            };
            let Some(slot) = slot else {
                return false;
            };
            *slot = Some(Arc::new(table));
            body = body.get(17 + total..).unwrap_or_default();
        }
        true
    }

    fn start_scan(&mut self, body: &[u8]) -> bool {
        let Some(frame) = self.frame.clone() else {
            return false;
        };
        let Some(&count) = body.first() else {
            return false;
        };
        let count = usize::from(count);
        if !(1..=4).contains(&count) || body.len() != 4 + 2 * count {
            return false;
        }
        // 顺序式：每次扫描都是整个 0–63，不分位
        if body.get(1 + 2 * count..) != Some(&[0, 63, 0][..]) {
            return false;
        }
        let mut blocks = Vec::new();
        let (mut dc, mut ac) = (Vec::new(), Vec::new());
        let mut covers = 0u8;
        let mut single = (1u64, 1u64);
        for slot in 0..count {
            let (Some(&id), Some(&selectors)) = (body.get(1 + 2 * slot), body.get(2 + 2 * slot))
            else {
                return false;
            };
            let Some(index) = frame.components.iter().position(|&(other, ..)| other == id) else {
                return false;
            };
            if covers & (1 << index) != 0 {
                return false;
            }
            covers |= 1 << index;
            let (Some(Some(dc_table)), Some(Some(ac_table))) = (
                self.dc_tables.get(usize::from(selectors >> 4)),
                self.ac_tables.get(usize::from(selectors & 0x0F)),
            ) else {
                return false;
            };
            dc.push(Arc::clone(dc_table));
            ac.push(Arc::clone(ac_table));
            let Some(&(_, h, v)) = frame.components.get(index) else {
                return false;
            };
            if count == 1 {
                // 只有一个分量时按它自己的块数，一块一个 MCU
                let width = (frame.width * h).div_ceil(frame.max_h);
                let height = (frame.height * v).div_ceil(frame.max_v);
                single = (u64::from(width.div_ceil(8)), u64::from(height.div_ceil(8)));
                blocks.push(slot as u8);
            } else {
                blocks.extend(std::iter::repeat_n(slot as u8, (h * v) as usize));
            }
        }
        if blocks.len() > MAX_BLOCKS_PER_MCU {
            return false;
        }
        let mcus = if count == 1 {
            single.0 * single.1
        } else {
            u64::from(frame.width.div_ceil(8 * frame.max_h))
                * u64::from(frame.height.div_ceil(8 * frame.max_v))
        };
        let wide = frame.precision == 12;
        let mcu_min_bits = blocks
            .iter()
            .filter_map(|&slot| {
                let slot = usize::from(slot);
                Some(u64::from(dc.get(slot)?.min_dc_bits) + u64::from(ac.get(slot)?.min_ac_bits))
            })
            .sum();
        self.scan = Some(Scan::new(Arc::new(Layout {
            blocks,
            dc,
            ac,
            covers,
            mcus,
            restart: self.restart,
            max_dc_size: if wide { 15 } else { 11 },
            max_ac_size: if wide { 14 } else { 10 },
            dc_limit: if wide { 32_767 } else { 2047 },
            mcu_min_bits,
        })));
        true
    }
}
