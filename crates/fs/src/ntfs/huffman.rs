//! 规范 Huffman 码的查表：XPRESS（[MS-XCA]）与 LZX（[MS-PATCH]）都按码长定码——码长短的在前，
//! 同长的按符号序。查表按最长码长 `bits` 位，一个长 L 位的码占 2^(bits−L) 项。

/// 查表里没分配到的码：真读到时才算错（只有一两个符号的码可能填不满码空间）。
pub(crate) const UNUSED: u16 = u16::MAX;

/// 按 `lengths`（各符号的码长，0 表示没用到）建查表。码长超出 `bits`、码多到超出码空间、
/// 或一个符号都没有时为 `None`。
pub(crate) fn decoding_table(lengths: &[u8], bits: u32) -> Option<Vec<u16>> {
    let mut table = vec![UNUSED; 1 << bits];
    let mut next = 0usize;
    for length in 1..=bits {
        for (symbol, &code_length) in lengths.iter().enumerate() {
            if u32::from(code_length) != length {
                continue;
            }
            let span = 1usize << (bits - length);
            table
                .get_mut(next..next + span)?
                .fill(u16::try_from(symbol).ok()?);
            next += span;
        }
    }
    let too_long = lengths.iter().any(|&length| u32::from(length) > bits);
    (next > 0 && !too_long).then_some(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_assigned_shortest_first_then_by_symbol() {
        // 符号 0、1 各 2 位，符号 2 是 1 位：2 → 0，0 → 10，1 → 11
        let table = decoding_table(&[2, 2, 1], 2).unwrap();
        assert_eq!(table, [2, 2, 0, 1]);
        // 填不满：没分配到的码是 UNUSED
        assert_eq!(decoding_table(&[1], 2).unwrap(), [0, 0, UNUSED, UNUSED]);
        // 超出码空间、码长超出、没有符号
        assert!(decoding_table(&[1, 1, 1], 2).is_none());
        assert!(decoding_table(&[3], 2).is_none());
        assert!(decoding_table(&[0, 0], 2).is_none());
    }
}
