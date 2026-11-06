//! FLAC（RFC 9639）：`fLaC`，之后是一串元数据块（第一块是 STREAMINFO，最后一块的头上有标记），再之后是一帧帧的音频。
//!
//! 帧里没有写长度，结尾要靠结构证明：
//! - 帧头有同步码、帧号（定长块）或首个采样的序号（变长块）、本帧的采样数，以 CRC-8 结束；
//! - 帧体是每个声道一个子帧（常数、原样、固定预测、线性预测，后两种的残差按 Rice 编码），按位走完、补齐到字节，
//!   后面是整帧的 CRC-16。走到哪里结束由子帧的结构算出，再用 CRC-16 核对，不靠找下一个同步码；
//! - 帧号接得上前一帧、声道数与位深和 STREAMINFO 一致、除最后一帧外采样数不少于 STREAMINFO 记的最小块；
//! - STREAMINFO 记着总采样数：各帧的采样数加起来正好等于它时，文件在那一帧结束（后面的 ID3v1 之类的附加数据不算）。
//!   总采样数记为 0（未知，边录边写的流）时算不出结尾，不认。
//!
//! 部分恢复：到最后一个核对过 CRC-16 的帧为止。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::{be16, be32};

const MAGIC: &[u8] = b"fLaC";
const STREAMINFO: u8 = 0;
const STREAMINFO_LEN: usize = 34;
/// 元数据块的类型：STREAMINFO 之外，PADDING、APPLICATION、SEEKTABLE、VORBIS_COMMENT、CUESHEET、PICTURE。
const METADATA_TYPES: std::ops::RangeInclusive<u8> = 1..=6;
const SEEKTABLE: u8 = 3;
const SEEKPOINT_LEN: u32 = 18;
/// 帧区按这么大一段段要数据。
const CHUNK: usize = 64 * 1024;
/// 一帧最大约 8 声道 × 65536 个采样 × 33 位再加上帧头帧尾，还没走完一帧的数据超过这么多就不是 FLAC。
const MAX_FRAME_BYTES: usize = 3 << 20;
/// 帧头里的采样率代码 1–11 对应的值（0 是用 STREAMINFO 的，12–14 写在帧头后面，15 不允许）。
const SAMPLE_RATES: [u32; 12] = [
    0, 88_200, 176_400, 192_000, 8_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 96_000,
];
/// 帧头里的位深代码对应的值；3 保留，0 是用 STREAMINFO 的。
const BIT_DEPTHS: [u8; 8] = [0, 8, 12, 0, 16, 20, 24, 32];

pub struct Flac;

static INFO: FormatInfo = FormatInfo {
    extension: "flac",
    variants: &[],
    description: "FLAC audio",
    max_size: 4 << 30,
    enabled_by_default: true,
    // 元数据里可能有封面图片（PICTURE 块）
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: MAGIC,
}];

impl FileFormat for Flac {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        // 第一个元数据块必须是 34 字节的 STREAMINFO，而且说得通
        let head = block.get(4..8)?;
        if !block.starts_with(MAGIC)
            || head.first()? & 0x7F != STREAMINFO
            || be24(head, 1)? != STREAMINFO_LEN as u32
        {
            return None;
        }
        stream_info(block.get(8..8 + STREAMINFO_LEN)?)?;
        Some(Box::new(Walker::new(
            INFO.max_size,
            FlacStructure {
                phase: Phase::BlockHeader,
                info: None,
                buffer: Vec::new(),
                buffer_at: 0,
                next_read: 0,
                variable: None,
                frames: 0,
                samples: 0,
                good_end: 0,
                complete: None,
            },
        )))
    }
}

fn be24(bytes: &[u8], at: usize) -> Option<u32> {
    let field = bytes.get(at..at + 3)?;
    Some(
        field
            .iter()
            .fold(0, |value, &byte| (value << 8) | u32::from(byte)),
    )
}

/// STREAMINFO 里用得上的几项。
#[derive(Clone, Copy)]
struct StreamInfo {
    min_block: u32,
    max_block: u32,
    /// 最大帧长，0 表示没记。
    max_frame: u32,
    sample_rate: u32,
    channels: u8,
    bits: u8,
    total: u64,
}

/// 核对 STREAMINFO 的内容（RFC 9639 8.2）。
fn stream_info(body: &[u8]) -> Option<StreamInfo> {
    let min_block = u32::from(be16(body, 0)?);
    let max_block = u32::from(be16(body, 2)?);
    let min_frame = be24(body, 4)?;
    let max_frame = be24(body, 7)?;
    let packed = (u64::from(be32(body, 10)?) << 32) | u64::from(be32(body, 14)?);
    let sample_rate = (packed >> 44) as u32;
    let channels = ((packed >> 41) & 0x7) as u8 + 1;
    let bits = ((packed >> 36) & 0x1F) as u8 + 1;
    let total = packed & 0xF_FFFF_FFFF;
    let plausible = min_block >= 16
        && max_block >= min_block
        && sample_rate > 0
        && bits >= 4
        && total > 0
        && (min_frame == 0 || max_frame == 0 || min_frame <= max_frame);
    plausible.then_some(StreamInfo {
        min_block,
        max_block,
        max_frame,
        sample_rate,
        channels,
        bits,
        total,
    })
}

enum Phase {
    /// 等元数据块的 4 字节头。
    BlockHeader,
    /// 等 STREAMINFO 的内容；`last` 是它的头上有没有「最后一块」的标记。
    StreamInfo { last: bool },
    /// 帧区，一段段要数据。
    Frames,
}

struct FlacStructure {
    phase: Phase,
    info: Option<StreamInfo>,
    /// 帧区里还没走完的字节，从 `buffer_at`（当前帧的开头）起。
    buffer: Vec<u8>,
    buffer_at: u64,
    /// 下一段数据从哪里开始。
    next_read: u64,
    /// 第一帧定下的是变长块（帧头记采样序号）还是定长块（记帧号）。
    variable: Option<bool>,
    frames: u64,
    samples: u64,
    /// 最后一个核对过的帧的结尾。
    good_end: u64,
    /// 采样数够了：文件在这里结束。
    complete: Option<u64>,
}

/// 帧区走到哪了。
enum Walk {
    /// 走完了：文件在这里结束。
    Complete(u64),
    /// 还要数据。
    More,
    Invalid,
}

impl FlacStructure {
    fn enter_frames(&mut self, at: u64) -> Step {
        self.phase = Phase::Frames;
        self.buffer_at = at;
        self.next_read = at;
        self.good_end = at;
        Step::Need { at, len: CHUNK }
    }

    /// 收到从 `at` 起的一段帧区数据：接在缓冲后面，能走完的帧都走完。
    fn take(&mut self, at: u64, bytes: &[u8]) -> Walk {
        let Some(info) = self.info else {
            return Walk::Invalid;
        };
        if at != self.next_read {
            return Walk::Invalid;
        }
        self.buffer.extend_from_slice(bytes);
        self.next_read = at + bytes.len() as u64;
        let mut used = 0usize;
        let walk = loop {
            let rest = self.buffer.get(used..).unwrap_or_default();
            match frame(rest, &info) {
                Err(Stop::More) => break Walk::More,
                Err(Stop::Bad) => break Walk::Invalid,
                Ok(found) => {
                    if !self.accept(&info, &found) {
                        break Walk::Invalid;
                    }
                    used += found.len;
                    self.good_end = self.buffer_at + used as u64;
                    if self.samples == info.total {
                        self.complete = Some(self.good_end);
                        break Walk::Complete(self.good_end);
                    }
                }
            }
        };
        self.buffer.drain(..used.min(self.buffer.len()));
        self.buffer_at = self.good_end;
        // 还要数据说明当前这一帧比缓冲里的还长：缓冲已经到了一帧的上限（记了最大帧长时按它）就不对了
        let limit = match info.max_frame {
            0 => MAX_FRAME_BYTES,
            max => MAX_FRAME_BYTES.min(max as usize),
        };
        let too_long = self.buffer.len() >= limit;
        match walk {
            Walk::More if too_long => Walk::Invalid,
            other => other,
        }
    }

    /// 帧接不接得上前面的：定长 / 变长不变、帧号连续、采样数不超过总数、除最后一帧外不少于最小块。
    fn accept(&mut self, info: &StreamInfo, found: &Frame) -> bool {
        if *self.variable.get_or_insert(found.variable) != found.variable {
            return false;
        }
        let expected = if found.variable {
            self.samples
        } else {
            self.frames
        };
        let Some(samples) = self.samples.checked_add(u64::from(found.block)) else {
            return false;
        };
        let size_ok = found.block <= info.max_block
            && samples <= info.total
            && (samples == info.total || found.block >= info.min_block);
        if found.number != expected || !size_ok {
            return false;
        }
        if info.max_frame != 0 && found.len > info.max_frame as usize {
            return false;
        }
        self.samples = samples;
        self.frames += 1;
        true
    }
}

impl Structure for FlacStructure {
    fn first(&self) -> Step {
        Step::Need {
            at: MAGIC.len() as u64,
            len: 4,
        }
    }

    /// 部分恢复：到最后一个核对过 CRC-16 的帧为止，至少要有一帧。
    fn intact(&self, _cut: u64) -> Option<u64> {
        (self.frames > 0).then_some(self.good_end)
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        match self.phase {
            Phase::BlockHeader => {
                let (Some(&head), Some(len)) = (bytes.first(), be24(bytes, 1)) else {
                    return Step::Invalid;
                };
                let (last, kind) = (head & 0x80 != 0, head & 0x7F);
                if self.info.is_none() {
                    if kind != STREAMINFO || len != STREAMINFO_LEN as u32 {
                        return Step::Invalid;
                    }
                    self.phase = Phase::StreamInfo { last };
                    return Step::Need {
                        at: at + 4,
                        len: STREAMINFO_LEN,
                    };
                }
                if !METADATA_TYPES.contains(&kind)
                    || (kind == SEEKTABLE && len % SEEKPOINT_LEN != 0)
                {
                    return Step::Invalid;
                }
                let next = at + 4 + u64::from(len);
                if last {
                    self.enter_frames(next)
                } else {
                    Step::Need { at: next, len: 4 }
                }
            }
            Phase::StreamInfo { last } => {
                let Some(info) = stream_info(bytes) else {
                    return Step::Invalid;
                };
                self.info = Some(info);
                let next = at + STREAMINFO_LEN as u64;
                if last {
                    self.enter_frames(next)
                } else {
                    self.phase = Phase::BlockHeader;
                    Step::Need { at: next, len: 4 }
                }
            }
            Phase::Frames => match self.take(at, bytes) {
                Walk::Complete(end) => Step::Done(end),
                Walk::More => Step::Need {
                    at: self.next_read,
                    len: CHUNK,
                },
                Walk::Invalid => Step::Invalid,
            },
        }
    }

    /// 数据在一段帧区数据中途用完（文件正好在设备末尾）：把收到的那一部分也走完。
    fn short(&mut self, at: u64, bytes: &[u8]) {
        if matches!(self.phase, Phase::Frames) {
            self.take(at, bytes);
        }
    }

    fn finish(&self, received: u64, _waiting_at: Option<u64>) -> Option<u64> {
        self.complete.filter(|&end| end <= received)
    }
}

/// 一个走完、核对过的帧。
struct Frame {
    len: usize,
    /// 帧头里的号：定长块是帧号，变长块是首个采样的序号。
    number: u64,
    variable: bool,
    block: u32,
}

/// 走帧时停下的原因：数据不够，或结构不对。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    More,
    Bad,
}

/// 从 `data` 开头走一帧（RFC 9639 第 9 章）。
fn frame(data: &[u8], info: &StreamInfo) -> Result<Frame, Stop> {
    let mut header = Bytes { data, at: 0 };
    let sync = header.u8()?;
    let second = header.u8()?;
    if sync != 0xFF || second & 0xFE != 0xF8 {
        return Err(Stop::Bad);
    }
    let variable = second & 1 != 0;
    let codes = header.u8()?;
    let (block_code, rate_code) = (codes >> 4, codes & 0x0F);
    let layout = header.u8()?;
    let (channel_code, depth_code) = (layout >> 4, (layout >> 1) & 0x7);
    if block_code == 0 || rate_code == 15 || layout & 1 != 0 || depth_code == 3 {
        return Err(Stop::Bad);
    }
    let number = header.coded_number(if variable { 6 } else { 5 })?;
    let block = match block_code {
        1 => 192,
        2..=5 => 576 << (block_code - 2),
        6 => u32::from(header.u8()?) + 1,
        7 => u32::from(header.u16()?) + 1,
        _ => 256 << (block_code - 8),
    };
    let rate = match rate_code {
        0 => info.sample_rate,
        12 => u32::from(header.u8()?) * 1000,
        13 => u32::from(header.u16()?),
        14 => u32::from(header.u16()?) * 10,
        code => SAMPLE_RATES
            .get(usize::from(code))
            .copied()
            .ok_or(Stop::Bad)?,
    };
    let bits = match depth_code {
        0 => info.bits,
        code => BIT_DEPTHS
            .get(usize::from(code))
            .copied()
            .ok_or(Stop::Bad)?,
    };
    // 各声道的位深：独立声道一样；左右与差、右与差、中与差这三种，「差」那个声道多 1 位
    let side = match channel_code {
        0..=7 if channel_code + 1 == info.channels => None,
        8 | 10 if info.channels == 2 => Some(1),
        9 if info.channels == 2 => Some(0),
        _ => return Err(Stop::Bad),
    };
    if rate != info.sample_rate || bits != info.bits {
        return Err(Stop::Bad);
    }
    let header_len = header.at;
    let crc = header.u8()?;
    if crc != crc8(data.get(..header_len).unwrap_or_default()) {
        return Err(Stop::Bad);
    }

    let mut bits_reader = Bits {
        data,
        position: (header_len + 1) * 8,
    };
    for channel in 0..info.channels {
        let depth = u32::from(bits) + u32::from(side == Some(channel));
        subframe(&mut bits_reader, block, depth)?;
    }
    // 补齐到字节的位都是 0，之后是整帧的 CRC-16
    let padding = (8 - bits_reader.position % 8) % 8;
    if bits_reader.read(padding as u32)? != 0 {
        return Err(Stop::Bad);
    }
    let end = bits_reader.position / 8;
    let stored = be16(data, end).ok_or(Stop::More)?;
    if stored != crc16(data.get(..end).unwrap_or_default()) {
        return Err(Stop::Bad);
    }
    Ok(Frame {
        len: end + 2,
        number,
        variable,
        block,
    })
}

/// 一个声道的子帧（RFC 9639 9.2）：只走结构、不还原采样。`depth` 是这个声道的位深。
fn subframe(bits: &mut Bits<'_>, block: u32, depth: u32) -> Result<(), Stop> {
    if bits.read(1)? != 0 {
        return Err(Stop::Bad);
    }
    let kind = bits.read(6)? as u32;
    let wasted = if bits.read(1)? == 1 {
        bits.unary()? + 1
    } else {
        0
    };
    if wasted >= u64::from(depth) {
        return Err(Stop::Bad);
    }
    // 上一行保证 wasted < depth（不超过 33）
    let depth = u64::from(depth) - wasted;
    let block_len = u64::from(block);
    match kind {
        // 常数：一个采样
        0 => bits.skip(depth),
        // 原样：每个采样都在
        1 => bits.skip(depth * block_len),
        // 固定预测（0–4 阶）：阶数个原样的预热采样，之后是残差
        8..=12 => {
            let order = kind - 8;
            if order > block {
                return Err(Stop::Bad);
            }
            bits.skip(depth * u64::from(order))?;
            residual(bits, block, order)
        }
        // 线性预测（1–32 阶）：预热采样、系数精度、移位、系数，之后是残差
        32..=63 => {
            let order = kind - 31;
            if order > block {
                return Err(Stop::Bad);
            }
            bits.skip(depth * u64::from(order))?;
            let precision = bits.read(4)?;
            if precision == 0xF {
                return Err(Stop::Bad);
            }
            bits.skip(5)?;
            bits.skip((precision + 1) * u64::from(order))?;
            residual(bits, block, order)
        }
        _ => Err(Stop::Bad),
    }
}

/// 残差（RFC 9639 9.2.7）：按分区，每区一个 Rice 参数（或转义成原样的位数），每个残差是一元的商加参数位的余数。
fn residual(bits: &mut Bits<'_>, block: u32, order: u32) -> Result<(), Stop> {
    let parameter_bits = match bits.read(2)? {
        0 => 4,
        1 => 5,
        _ => return Err(Stop::Bad),
    };
    let escape = (1u64 << parameter_bits) - 1;
    let partition_order = bits.read(4)? as u32;
    let per_partition = block >> partition_order;
    if per_partition << partition_order != block || per_partition < order {
        return Err(Stop::Bad);
    }
    for partition in 0..(1u32 << partition_order) {
        let count = u64::from(if partition == 0 {
            per_partition - order
        } else {
            per_partition
        });
        let parameter = bits.read(parameter_bits)?;
        if parameter == escape {
            let width = bits.read(5)?;
            bits.skip(width * count)?;
        } else {
            for _ in 0..count {
                bits.unary()?;
                bits.skip(parameter)?;
            }
        }
    }
    Ok(())
}

/// 按字节读帧头。
struct Bytes<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bytes<'_> {
    fn u8(&mut self) -> Result<u8, Stop> {
        let byte = *self.data.get(self.at).ok_or(Stop::More)?;
        self.at += 1;
        Ok(byte)
    }

    fn u16(&mut self) -> Result<u16, Stop> {
        Ok((u16::from(self.u8()?) << 8) | u16::from(self.u8()?))
    }

    /// 帧号或采样序号：与 UTF-8 一样的变长写法，最多 `max_extra` 个后续字节。
    fn coded_number(&mut self, max_extra: u32) -> Result<u64, Stop> {
        let first = self.u8()?;
        // 开头的 1 的个数：0 个是单字节，n（2 到 7）个是后面还有 n - 1 个字节；10xxxxxx 不能开头，0xFF 没有定义
        let ones = first.leading_ones();
        if ones == 1 || ones > max_extra + 1 || first == 0xFF {
            return Err(Stop::Bad);
        }
        let extra = ones.saturating_sub(1);
        // 开头的 1 与随后的 0 之外的低位是数值
        let mask = (0xFFu32 >> (ones + 1)) as u8;
        let mut value = u64::from(first & mask);
        for _ in 0..extra {
            let next = self.u8()?;
            if next & 0xC0 != 0x80 {
                return Err(Stop::Bad);
            }
            value = (value << 6) | u64::from(next & 0x3F);
        }
        Ok(value)
    }
}

/// 按位读帧体（高位在前）。读过了数据末尾时答复 [`Stop::More`]。
struct Bits<'a> {
    data: &'a [u8],
    /// 下一个位的位置。
    position: usize,
}

impl Bits<'_> {
    fn available(&self) -> usize {
        self.data.len().saturating_mul(8)
    }

    fn skip(&mut self, count: u64) -> Result<(), Stop> {
        let end = usize::try_from(count)
            .ok()
            .and_then(|count| self.position.checked_add(count))
            .ok_or(Stop::Bad)?;
        if end > self.available() {
            return Err(Stop::More);
        }
        self.position = end;
        Ok(())
    }

    /// 读 `count`（不超过 32）位。
    fn read(&mut self, count: u32) -> Result<u64, Stop> {
        let mut value = 0u64;
        for _ in 0..count {
            let byte = *self.data.get(self.position / 8).ok_or(Stop::More)?;
            let bit = (byte >> (7 - self.position % 8)) & 1;
            value = (value << 1) | u64::from(bit);
            self.position += 1;
        }
        Ok(value)
    }

    /// 一元编码：数到第一个 1 之前有几个 0。
    fn unary(&mut self) -> Result<u64, Stop> {
        let mut zeros = 0u64;
        loop {
            let byte = *self.data.get(self.position / 8).ok_or(Stop::More)?;
            let offset = self.position % 8;
            let rest = byte << offset;
            if rest != 0 {
                let leading = rest.leading_zeros() as usize;
                self.position += leading + 1;
                return Ok(zeros + leading as u64);
            }
            self.position += 8 - offset;
            zeros += (8 - offset) as u64;
        }
    }
}

#[expect(
    clippy::indexing_slicing,
    reason = "下标在 0..256 之内，const fn 里用不了 get_mut"
)]
const fn crc8_table() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u8;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 0x80 != 0 {
                (value << 1) ^ 0x07
            } else {
                value << 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

#[expect(
    clippy::indexing_slicing,
    reason = "下标在 0..256 之内，const fn 里用不了 get_mut"
)]
const fn crc16_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = (index as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 0x8000 != 0 {
                (value << 1) ^ 0x8005
            } else {
                value << 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

static CRC8_TABLE: [u8; 256] = crc8_table();
static CRC16_TABLE: [u16; 256] = crc16_table();

/// 帧头的 CRC-8：多项式 x^8 + x^2 + x + 1，初值 0。
pub(crate) fn crc8(data: &[u8]) -> u8 {
    data.iter().fold(0, |crc, &byte| {
        CRC8_TABLE
            .get(usize::from(crc ^ byte))
            .copied()
            .unwrap_or_default()
    })
}

/// 整帧的 CRC-16：多项式 x^16 + x^15 + x^2 + 1，初值 0。
pub(crate) fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0, |crc, &byte| {
        let index = usize::from((crc >> 8) as u8 ^ byte);
        (crc << 8) ^ CRC16_TABLE.get(index).copied().unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples::{self, FlacLayout};
    use crate::walker::tests::{check_all_splits, check_intact_splits};

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Flac.header_check(data).unwrap(), data, expected);
    }

    /// 第 `index` 帧在样本里的起点（帧从同步码 `FF F8` / `FF F9` 开始，样本里的帧都是按顺序生成的）。
    fn frame_starts(layout: &FlacLayout) -> Vec<usize> {
        samples::flac_frame_starts(layout)
    }

    #[test]
    fn crc_tables_match_the_bitwise_definition() {
        // RFC 9639 附录里的例子之外，再用逐位算法对一遍
        let data: Vec<u8> = (0u16..300).map(|value| (value * 7 + 3) as u8).collect();
        let mut crc8_bitwise = 0u8;
        let mut crc16_bitwise = 0u16;
        for &byte in &data {
            crc8_bitwise ^= byte;
            crc16_bitwise ^= u16::from(byte) << 8;
            for _ in 0..8 {
                crc8_bitwise = if crc8_bitwise & 0x80 != 0 {
                    (crc8_bitwise << 1) ^ 0x07
                } else {
                    crc8_bitwise << 1
                };
                crc16_bitwise = if crc16_bitwise & 0x8000 != 0 {
                    (crc16_bitwise << 1) ^ 0x8005
                } else {
                    crc16_bitwise << 1
                };
            }
        }
        assert_eq!(crc8(&data), crc8_bitwise);
        assert_eq!(crc16(&data), crc16_bitwise);
        // CRC-8 的标准校验值（"123456789"）
        assert_eq!(crc8(b"123456789"), 0xF4);
        assert_eq!(crc16(b"123456789"), 0xFEE8);
    }

    #[test]
    fn ends_after_the_frame_that_reaches_the_total_sample_count() {
        for layout in [
            FlacLayout::default(),
            FlacLayout {
                channels: 1,
                variable: true,
                picture: 3000,
                ..FlacLayout::default()
            },
            FlacLayout {
                channels: 6,
                frames: 3,
                max_frame_known: false,
                ..FlacLayout::default()
            },
        ] {
            let sample = samples::flac(&layout);
            let mut data = sample.clone();
            data.extend_from_slice(b"TAG and some bytes after the stream");
            check(&data, Some(sample.len() as u64));
            check(&sample, Some(sample.len() as u64));
        }
    }

    #[test]
    fn cut_or_corrupted_streams_are_not_complete_but_keep_their_good_frames() {
        let layout = FlacLayout::default();
        let sample = samples::flac(&layout);
        let starts = frame_starts(&layout);
        let make = |data: &[u8]| Flac.header_check(data).unwrap();

        // 在第 3 帧中间断了：不完整，保存到第 2 帧结尾
        let cut = &sample[..starts[3] + 10];
        check(cut, None);
        check_intact_splits(|| make(cut), cut, Some(starts[3] as u64));

        // 第 2 帧里改了一个字节：CRC-16 对不上，保存到第 1 帧结尾
        let mut corrupted = sample.clone();
        corrupted[starts[2] + 20] ^= 0x10;
        check(&corrupted, None);
        check_intact_splits(|| make(&corrupted), &corrupted, Some(starts[2] as u64));

        // 帧号跳了：帧头的 CRC-8 照算，接不上前面
        let mut skipped = sample.clone();
        skipped.drain(starts[1]..starts[2]);
        check(&skipped, None);
        check_intact_splits(|| make(&skipped), &skipped, Some(starts[1] as u64));

        // 一帧都没走完：说不出可靠的一段
        let early = &sample[..starts[0] + 5];
        check_intact_splits(|| make(early), early, None);
    }

    /// 帧号与采样序号的变长写法：最长的写法取到最大值不溢出，超出允许的字节数、续字节不对都不认。
    #[test]
    fn coded_numbers_at_their_limits() {
        let longest = [0xFE, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF];
        let read = |bytes: &[u8], max_extra| Bytes { data: bytes, at: 0 }.coded_number(max_extra);
        assert_eq!(read(&longest, 6), Ok((1 << 36) - 1));
        assert_eq!(read(&longest, 5), Err(Stop::Bad), "帧号最多 6 个字节");
        assert_eq!(read(&[0x7F], 5), Ok(0x7F));
        assert_eq!(read(&[0xC2, 0x80], 5), Ok(0x80));
        assert_eq!(read(&[0x80], 5), Err(Stop::Bad));
        assert_eq!(read(&[0xFF], 6), Err(Stop::Bad));
        assert_eq!(
            read(&[0xE0, 0x80, 0x40], 5),
            Err(Stop::Bad),
            "续字节不是 10xxxxxx"
        );
        assert_eq!(read(&[0xE0, 0x80], 5), Err(Stop::More));
    }

    /// 总采样数是 36 位的最大值、帧头的块大小取最大：累加用的是不会溢出的算法，采样数不够就是不完整。
    #[test]
    fn largest_sample_counts_do_not_overflow() {
        let mut sample = samples::flac(&FlacLayout::default());
        sample[8 + 13] |= 0x0F;
        sample[8 + 14..8 + 18].copy_from_slice(&[0xFF; 4]);
        let info = stream_info(&sample[8..8 + STREAMINFO_LEN]).unwrap();
        assert_eq!(info.total, (1 << 36) - 1);
        check(&sample, None);
        let mut structure = FlacStructure {
            phase: Phase::Frames,
            info: Some(info),
            buffer: Vec::new(),
            buffer_at: 0,
            next_read: 0,
            variable: Some(true),
            frames: 1,
            samples: u64::MAX - 10,
            good_end: 0,
            complete: None,
        };
        let frame = Frame {
            len: 100,
            number: u64::MAX - 10,
            variable: true,
            block: 576,
        };
        assert!(!structure.accept(&info, &frame));
    }

    #[test]
    fn unknown_total_or_implausible_stream_info_is_rejected() {
        let sample = samples::flac(&FlacLayout::default());
        let changed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            data
        };
        // 总采样数为 0：STREAMINFO 第 13 字节的低 4 位与之后 4 个字节
        let mut unknown = sample.clone();
        unknown[8 + 13] &= 0xF0;
        unknown[8 + 14..8 + 18].copy_from_slice(&[0; 4]);
        assert!(Flac.header_check(&unknown).is_none());
        assert!(
            Flac.header_check(&changed(4, &[1])).is_none(),
            "第一块不是 STREAMINFO"
        );
        assert!(
            Flac.header_check(&changed(8, &[0, 8])).is_none(),
            "最小块太小"
        );
        assert!(
            Flac.header_check(&changed(18, &[0, 0, 0])).is_none(),
            "采样率为 0"
        );
    }
}
