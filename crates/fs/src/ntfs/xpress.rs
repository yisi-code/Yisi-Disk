//! XPRESS 的 LZ77+Huffman 解压（微软 [MS-XCA] 2.2.4）：WOF 系统压缩的 XPRESS4K / 8K / 16K 用它。
//!
//! 压缩数据按输出每 65536 字节一段：段首 256 字节是 512 个符号的码长（各 4 位，偶数符号在低 4 位），
//! 之后是位流（每次取 16 位小端字，从高位起用）与插在中间的字节（长匹配的长度）。符号小于 256 是字面字节，
//! 否则低 4 位是匹配长度、高 4 位是偏移的位数。只解到要的长度为止。

use super::huffman::{UNUSED, decoding_table};

/// 每段输出的长度。
const BLOCK: usize = 65536;
/// 码长表：512 个符号各 4 位。
const TABLE_LEN: usize = 256;
const SYMBOLS: usize = 512;
/// 码长最长 15 位，查表按 15 位。
const MAX_BITS: u32 = 15;

/// 解开 `input`，正好写满 `output`；数据不对（码长表不成前缀码、引用超出已解出的部分、输入不够）时为 `None`。
pub(crate) fn decompress(input: &[u8], output: &mut [u8]) -> Option<()> {
    let mut reader = Reader {
        input,
        position: 0,
        bits: 0,
        count: 0,
    };
    let mut written = 0;
    while written < output.len() {
        let lengths = read_lengths(input.get(reader.position..reader.position + TABLE_LEN)?);
        reader.position += TABLE_LEN;
        let table = decoding_table(&lengths, MAX_BITS)?;
        reader.start()?;
        let block_end = (written + BLOCK).min(output.len());
        while written < block_end {
            let code = *table.get(reader.peek(MAX_BITS) as usize)?;
            if code == UNUSED {
                return None;
            }
            let symbol = usize::from(code);
            reader.consume(u32::from(*lengths.get(symbol)?))?;
            if symbol < 256 {
                *output.get_mut(written)? = symbol as u8;
                written += 1;
                continue;
            }
            let symbol = symbol - 256;
            let mut length = symbol & 0x0F;
            let offset_bits = (symbol >> 4) as u32;
            if length == 15 {
                length = usize::from(reader.byte()?);
                if length == 255 {
                    length = usize::from(reader.word()?);
                    if length == 0 {
                        length = reader.dword()? as usize;
                    }
                    length = length.checked_sub(15)?;
                }
                length += 15;
            }
            length += 3;
            let offset = (reader.peek(offset_bits) as usize) + (1 << offset_bits);
            reader.consume(offset_bits)?;
            let from = written.checked_sub(offset)?;
            // 匹配可以与自己重叠（offset 小于长度时重复前面的内容），只能逐字节复制
            for index in 0..length.min(output.len() - written) {
                *output.get_mut(written + index)? = *output.get(from + index)?;
            }
            written += length.min(output.len() - written);
        }
    }
    Some(())
}

fn read_lengths(table: &[u8]) -> [u8; SYMBOLS] {
    let mut lengths = [0u8; SYMBOLS];
    for ([low, high], byte) in lengths.as_chunks_mut::<2>().0.iter_mut().zip(table) {
        *low = byte & 0x0F;
        *high = byte >> 4;
    }
    lengths
}

/// 位流：`bits` 的高位是还没用的位，`count` 是其中有效的位数；字节型的长度从 `position` 直接读。
struct Reader<'a> {
    input: &'a [u8],
    position: usize,
    bits: u32,
    count: i32,
}

impl Reader<'_> {
    /// 段开始：先装两个 16 位字。
    fn start(&mut self) -> Option<()> {
        let first = u32::from(self.read16()?);
        let second = u32::from(self.read16()?);
        self.bits = (first << 16) | second;
        self.count = 32;
        Some(())
    }

    fn read16(&mut self) -> Option<u16> {
        let bytes = self.input.get(self.position..self.position + 2)?;
        self.position += 2;
        Some(u16::from_le_bytes(bytes.try_into().ok()?))
    }

    /// 最高 `n` 位（不消耗）。
    fn peek(&self, n: u32) -> u32 {
        if n == 0 { 0 } else { self.bits >> (32 - n) }
    }

    /// 用掉 `n` 位，不够 16 位时从输入补一个字（输入到头时补 0：最后一个符号后面可能就没有字了）。
    fn consume(&mut self, n: u32) -> Option<()> {
        self.bits = self.bits.checked_shl(n).unwrap_or(0);
        self.count -= n as i32;
        if self.count < 16 {
            let word = if self.position + 2 <= self.input.len() {
                self.read16()?
            } else {
                0
            };
            self.bits |= u32::from(word) << (16 - self.count);
            self.count += 16;
        }
        Some(())
    }

    fn byte(&mut self) -> Option<u8> {
        let byte = *self.input.get(self.position)?;
        self.position += 1;
        Some(byte)
    }

    fn word(&mut self) -> Option<u16> {
        self.read16()
    }

    fn dword(&mut self) -> Option<u32> {
        let bytes = self.input.get(self.position..self.position + 4)?;
        self.position += 4;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_broken_tables_and_short_input() {
        // 码长全是 1：两个以上的符号都是 1 位，超出码空间
        let input = vec![0x11u8; 300];
        assert!(decompress(&input, &mut [0u8; 10]).is_none());
        // 一个符号都没有
        assert!(decompress(&[0u8; 300], &mut [0u8; 10]).is_none());
        // 输入短于码长表
        assert!(decompress(&[0u8; 10], &mut [0u8; 10]).is_none());
    }
}
