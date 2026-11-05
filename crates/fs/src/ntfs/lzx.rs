//! LZX 解压（位流格式见微软 [MS-PATCH] 的 LZX 一节），WOF 系统压缩的 LZX 算法用它。WOF 用的是 WIM 的变体：
//! - 每 32 KiB 一块单独压缩，窗口 32 KiB（30 个位置槽），重复偏移每块从 1、1、1 开始；
//! - 没有流头；每个压缩块的块头是 3 位类型，再 1 位：为 1 时块长 32768，否则跟 16 位块长；
//! - E8 转换（x86 调用指令的地址改写）总是开着，翻译大小 12000000，位置从这一块的开头算。
//!
//! 位流每次取 16 位小端字，从高位起用。三种块：逐字（verbatim）、对齐偏移（aligned，偏移低 3 位另有一棵树）、
//! 不压缩（对齐到 16 位后，3 个重复偏移各 4 字节，再是原样的字节，奇数长度后面补 1 字节）。
//! 主树（256 个字面字节 + 8 × 30 个匹配符号）与长度树（249 个符号）的码长用预树（20 个符号）按增量编码，
//! 同一块里前后两个压缩块的码长接着用。

use super::huffman::{UNUSED, decoding_table};

const MAIN_SYMBOLS: usize = 256 + 8 * POSITION_SLOTS;
const POSITION_SLOTS: usize = 30;
const LENGTH_SYMBOLS: usize = 249;
const PRETREE_SYMBOLS: usize = 20;
const ALIGNED_SYMBOLS: usize = 8;
/// 码长最长 16 位（增量编码对 17 取模），各棵树都按 16 位查表。
const MAX_BITS: u32 = 16;
const PRETREE_BITS: u32 = 4;
const ALIGNED_BITS: u32 = 3;
const DEFAULT_BLOCK: usize = 32768;
const E8_TRANSLATION_SIZE: i64 = 12_000_000;

const VERBATIM: u32 = 1;
const ALIGNED: u32 = 2;
const UNCOMPRESSED: u32 = 3;

/// 各位置槽的偏移基数与附加位数（32 KiB 窗口用前 30 个）。
const SLOT_BASE: [u32; POSITION_SLOTS] = [
    0, 1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536,
    2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576,
];
const SLOT_EXTRA: [u32; POSITION_SLOTS] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// 解开一块，正好写满 `output`；数据不对时为 `None`。
pub(crate) fn decompress(input: &[u8], output: &mut [u8]) -> Option<()> {
    let mut bits = Bits::new(input);
    let mut main_lengths = [0u8; MAIN_SYMBOLS];
    let mut length_lengths = [0u8; LENGTH_SYMBOLS];
    let mut recent = [1u32; 3];
    let mut written = 0usize;
    while written < output.len() {
        let kind = bits.read(3)?;
        let size = if bits.read(1)? == 1 {
            DEFAULT_BLOCK
        } else {
            bits.read(16)? as usize
        };
        if size == 0 {
            return None;
        }
        let end = (written + size).min(output.len());
        match kind {
            VERBATIM | ALIGNED => {
                let aligned = if kind == ALIGNED {
                    let mut lengths = [0u8; ALIGNED_SYMBOLS];
                    for length in &mut lengths {
                        *length = bits.read(ALIGNED_BITS)? as u8;
                    }
                    Some(Tree::new(&lengths)?)
                } else {
                    None
                };
                read_lengths(&mut bits, &mut main_lengths[..256])?;
                read_lengths(&mut bits, &mut main_lengths[256..])?;
                let main = Tree::new(&main_lengths)?;
                read_lengths(&mut bits, &mut length_lengths)?;
                // 长度树可以一个码都没有（这一块没有长匹配）
                let lengths = Tree::new(&length_lengths);
                while written < end {
                    let symbol = main.decode(&mut bits)?;
                    if symbol < 256 {
                        *output.get_mut(written)? = symbol as u8;
                        written += 1;
                        continue;
                    }
                    let symbol = symbol - 256;
                    let header = symbol & 7;
                    let slot = symbol >> 3;
                    let mut length = header + 2;
                    if header == 7 {
                        length += lengths.as_ref()?.decode(&mut bits)?;
                    }
                    let offset = match slot {
                        0 => recent[0],
                        1 => {
                            recent.swap(0, 1);
                            recent[0]
                        }
                        2 => {
                            recent.swap(0, 2);
                            recent[0]
                        }
                        _ => {
                            let extra = *SLOT_EXTRA.get(slot)?;
                            let base = *SLOT_BASE.get(slot)?;
                            let footer = match &aligned {
                                Some(tree) if extra >= 3 => {
                                    (bits.read(extra - 3)? << 3) + tree.decode(&mut bits)? as u32
                                }
                                _ => bits.read(extra)?,
                            };
                            let offset = (base + footer).checked_sub(2)?;
                            recent = [offset, recent[0], recent[1]];
                            offset
                        }
                    } as usize;
                    let from = written.checked_sub(offset)?;
                    let length = length.min(output.len() - written);
                    for index in 0..length {
                        *output.get_mut(written + index)? = *output.get(from + index)?;
                    }
                    written += length;
                }
            }
            UNCOMPRESSED => {
                let mut at = bits.byte_position_after_align();
                for slot in &mut recent {
                    *slot = u32::from_le_bytes(input.get(at..at + 4)?.try_into().ok()?);
                    at += 4;
                }
                let count = end - written;
                output
                    .get_mut(written..end)?
                    .copy_from_slice(input.get(at..at + count)?);
                written = end;
                at += count + (size & 1);
                bits = Bits::at(input, at);
            }
            _ => return None,
        }
    }
    undo_e8(output);
    Some(())
}

/// 用预树读一串码长（增量编码：新码长 = (旧码长 − 码) mod 17；17、18 是一串 0，19 是一串相同的值）。
fn read_lengths(bits: &mut Bits<'_>, lengths: &mut [u8]) -> Option<()> {
    let mut pre = [0u8; PRETREE_SYMBOLS];
    for length in &mut pre {
        *length = bits.read(PRETREE_BITS)? as u8;
    }
    let pretree = Tree::new(&pre)?;
    let mut index = 0;
    while index < lengths.len() {
        let code = pretree.decode(bits)?;
        let (count, value) = match code {
            0..=16 => (1, delta(*lengths.get(index)?, code)),
            17 => (bits.read(4)? as usize + 4, 0),
            18 => (bits.read(5)? as usize + 20, 0),
            19 => {
                let count = bits.read(1)? as usize + 4;
                let code = pretree.decode(bits)?;
                if code > 16 {
                    return None;
                }
                (count, delta(*lengths.get(index)?, code))
            }
            _ => return None,
        };
        lengths.get_mut(index..index + count)?.fill(value);
        index += count;
    }
    Some(())
}

fn delta(previous: u8, code: usize) -> u8 {
    ((usize::from(previous) + 17 - code) % 17) as u8
}

/// 还原 E8 转换：x86 的 `call` 指令（0xE8）后面的 4 字节在压缩前把相对地址改成了绝对地址，这里改回来。
/// 最后 10 个字节里的不处理。
fn undo_e8(data: &mut [u8]) {
    let end = data.len().saturating_sub(10);
    let mut index = 0;
    while index < end {
        if data.get(index) != Some(&0xE8) {
            index += 1;
            continue;
        }
        let Some(field) = data.get_mut(index + 1..index + 5) else {
            break;
        };
        let Ok(bytes) = <[u8; 4]>::try_from(&*field) else {
            break;
        };
        let position = index as i64;
        let absolute = i64::from(i32::from_le_bytes(bytes));
        if absolute >= -position && absolute < E8_TRANSLATION_SIZE {
            let relative = if absolute >= 0 {
                absolute - position
            } else {
                absolute + E8_TRANSLATION_SIZE
            };
            field.copy_from_slice(&(relative as i32).to_le_bytes());
        }
        index += 5;
    }
}

/// 一棵 Huffman 树的查表。
struct Tree {
    table: Vec<u16>,
    lengths: Vec<u8>,
}

impl Tree {
    fn new(lengths: &[u8]) -> Option<Self> {
        Some(Self {
            table: decoding_table(lengths, MAX_BITS)?,
            lengths: lengths.to_vec(),
        })
    }

    fn decode(&self, bits: &mut Bits<'_>) -> Option<usize> {
        let code = *self.table.get(bits.peek(MAX_BITS)? as usize)?;
        if code == UNUSED {
            return None;
        }
        let symbol = usize::from(code);
        bits.consume(u32::from(*self.lengths.get(symbol)?))?;
        Some(symbol)
    }
}

/// 位流：`buffer` 的高位是还没用的位，`count` 是其中有效的位数，`position` 是下一个要装的字。
struct Bits<'a> {
    input: &'a [u8],
    position: usize,
    buffer: u64,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self::at(input, 0)
    }

    fn at(input: &'a [u8], position: usize) -> Self {
        Self {
            input,
            position,
            buffer: 0,
            count: 0,
        }
    }

    /// 至少装到 `n` 位；输入到头时补 0（最后几个符号后面可能就没有字了）。
    fn fill(&mut self, n: u32) {
        while self.count < n {
            let word = match self.input.get(self.position..self.position + 2) {
                Some(&[low, high]) => u16::from_le_bytes([low, high]),
                _ => 0,
            };
            self.position += 2;
            self.buffer |= u64::from(word) << (48 - self.count);
            self.count += 16;
        }
    }

    fn peek(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        self.fill(n);
        Some((self.buffer >> (64 - n)) as u32)
    }

    fn consume(&mut self, n: u32) -> Option<()> {
        if n > self.count {
            return None;
        }
        self.buffer = self.buffer.checked_shl(n).unwrap_or(0);
        self.count -= n;
        Some(())
    }

    fn read(&mut self, n: u32) -> Option<u32> {
        let value = self.peek(n)?;
        self.consume(n)?;
        Some(value)
    }

    /// 不压缩块：丢掉到 16 位边界的位（已经对齐时丢一整个字），之后按字节读，返回字节位置。
    fn byte_position_after_align(&mut self) -> usize {
        let drop = if self.count.is_multiple_of(16) {
            16
        } else {
            self.count % 16
        };
        self.fill(drop);
        self.count -= drop;
        // 缓冲里剩下的整字还没被用，退回去按字节读
        self.position - (self.count / 8) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e8_at(data: &mut [u8], at: usize, value: i32) {
        data[at] = 0xE8;
        data[at + 1..at + 5].copy_from_slice(&value.to_le_bytes());
    }

    fn value_at(data: &[u8], at: usize) -> i32 {
        i32::from_le_bytes(data[at + 1..at + 5].try_into().unwrap())
    }

    #[test]
    fn e8_translation_is_undone_only_where_the_rules_allow() {
        let mut data = vec![0u8; 40];
        // 位置 5：绝对地址 105 改回相对地址 100
        e8_at(&mut data, 5, 105);
        // 位置 12：绝对地址 -3（不小于 -12）改成 -3 + 12000000
        e8_at(&mut data, 12, -3);
        // 位置 20：绝对地址 -30 小于 -20，不改；超出翻译大小的也不改
        e8_at(&mut data, 20, -30);
        e8_at(&mut data, 25, 20_000_000);
        // 最后 10 个字节里的不处理
        e8_at(&mut data, 31, 500);
        undo_e8(&mut data);
        assert_eq!(value_at(&data, 5), 100);
        assert_eq!(value_at(&data, 12), -3 + 12_000_000);
        assert_eq!(value_at(&data, 20), -30);
        assert_eq!(value_at(&data, 25), 20_000_000);
        assert_eq!(value_at(&data, 31), 500);
    }

    #[test]
    fn bad_block_types_and_short_input_are_rejected() {
        // 块类型 0：不存在
        assert!(decompress(&[0x00, 0x00, 0x00, 0x00], &mut [0u8; 16]).is_none());
        assert!(decompress(&[], &mut [0u8; 16]).is_none());
    }
}
