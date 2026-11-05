//! LZNT1：NTFS 压缩文件用的算法（依据微软公开的 MS-XCA 规范）。内容按 4 KiB 分块，每块前有 2 字节的块头：
//! 低 12 位是块的字节数减 1，第 12–14 位固定为 3，第 15 位为 1 表示压缩、为 0 表示原样存放。
//! 压缩的块里每 8 项前有一个标记字节：位为 0 的项是 1 个原样字节，为 1 的项是 2 字节的回溯（往前多远、抄多长）。

/// 每块解出来最多这么多字节。
pub(crate) const BLOCK: usize = 4096;

/// 把 `input` 解到 `output`（调用方按压缩单元里这一段的长度给出、已填 0）。块头为 0 或输入用完时结束，
/// 后面保持 0；一块解出来不足 4 KiB 时，下一块仍从下一个 4 KiB 处开始。格式不对时返回 `None`。
pub(crate) fn decompress(input: &[u8], output: &mut [u8]) -> Option<()> {
    let mut at = 0usize;
    let mut block_start = 0usize;
    while block_start < output.len() {
        let Some(&[low, high]) = input.get(at..at + 2) else {
            break;
        };
        let header = u16::from_le_bytes([low, high]);
        if header == 0 {
            break;
        }
        if (header >> 12) & 0x7 != 3 {
            return None;
        }
        let length = usize::from(header & 0x0FFF) + 1;
        let data = input.get(at + 2..at + 2 + length)?;
        at += 2 + length;
        let block_end = output.len().min(block_start + BLOCK);
        let block = output.get_mut(block_start..block_end)?;
        if header & 0x8000 == 0 {
            let take = data.len().min(block.len());
            block.get_mut(..take)?.copy_from_slice(data.get(..take)?);
        } else {
            decompress_block(data, block)?;
        }
        block_start += BLOCK;
    }
    Some(())
}

/// 解一个压缩的块。解出的内容超出 `block` 时截断（只会在最后一段不足 4 KiB 时发生）。
fn decompress_block(data: &[u8], block: &mut [u8]) -> Option<()> {
    let mut at = 0usize;
    let mut out = 0usize;
    while let Some(&flags) = data.get(at) {
        at += 1;
        for bit in 0..8 {
            if at >= data.len() {
                return Some(());
            }
            if flags & (1 << bit) == 0 {
                if let Some(slot) = block.get_mut(out) {
                    *slot = *data.get(at)?;
                }
                out += 1;
                at += 1;
                continue;
            }
            let token = u16::from_le_bytes([*data.get(at)?, *data.get(at + 1)?]);
            at += 2;
            // 往前多远用几位，随这一块已解出的字节数增长：16 个以内 4 位，之后每翻一倍多 1 位，最多 12 位
            let mut offset_bits = 4u32;
            while offset_bits < 12 && out > (1usize << offset_bits) {
                offset_bits += 1;
            }
            let length_bits = 16 - offset_bits;
            let back = usize::from(token >> length_bits) + 1;
            let count = usize::from(token & ((1u16 << length_bits) - 1)) + 3;
            if back > out {
                return None;
            }
            for _ in 0..count {
                if out >= block.len() {
                    return Some(());
                }
                *block.get_mut(out)? = *block.get(out - back)?;
                out += 1;
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::samples::{LZNT1_PACKED as PACKED, LZNT1_PLAIN as PLAIN};

    #[test]
    fn system_compressed_unit_is_restored() {
        let mut output = vec![0u8; PLAIN.len()];
        decompress(PACKED, &mut output).unwrap();
        assert!(output == PLAIN);
    }

    #[test]
    fn the_end_of_the_input_leaves_zeros_and_a_short_output_is_cut() {
        // 只给前 3 块：后面保持 0
        let mut at = 0;
        for _ in 0..3 {
            let header = u16::from_le_bytes([PACKED[at], PACKED[at + 1]]);
            at += 2 + usize::from(header & 0x0FFF) + 1;
        }
        let mut output = vec![0u8; PLAIN.len()];
        decompress(&PACKED[..at], &mut output).unwrap();
        assert!(output[..3 * BLOCK] == PLAIN[..3 * BLOCK]);
        assert!(output[3 * BLOCK..].iter().all(|&byte| byte == 0));
        // 文件末尾那一段不足一个压缩单元：只解到要的长度
        let mut short = vec![0u8; 5000];
        decompress(PACKED, &mut short).unwrap();
        assert!(short == PLAIN[..5000]);
    }

    #[test]
    fn malformed_blocks_are_rejected() {
        let mut output = vec![0u8; BLOCK];
        // 签名位不是 3
        assert_eq!(decompress(&[0x05, 0x80, 0, 0], &mut output), None);
        // 块长超出输入
        assert_eq!(decompress(&[0xFF, 0xBF, 0x00], &mut output), None);
        // 第一项就回溯：前面还什么都没有
        assert_eq!(
            decompress(&[0x02, 0xB0, 0x01, 0x00, 0x00], &mut output),
            None
        );
        // 原样存放的块
        let mut raw = vec![0x02, 0x30];
        raw.extend_from_slice(b"abc");
        decompress(&raw, &mut output).unwrap();
        assert_eq!(&output[..4], b"abc\0");
    }
}
