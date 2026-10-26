//! CRC-32（IEEE 802.3，多项式 0xEDB88320）：GPT 的头与分区表项、PNG 的块都用它校验。

const TABLE: [u32; 256] = make_table();

#[expect(
    clippy::indexing_slicing,
    reason = "编译期求值，下标 n 由循环条件限定在 0..256"
)]
const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut value = n as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                0xEDB8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[n] = value;
        n += 1;
    }
    table
}

/// 依次计算多段数据的 CRC-32，相当于把它们拼起来再算。
pub fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for part in parts {
        for &byte in *part {
            let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
            crc = TABLE.get(index).copied().unwrap_or(0) ^ (crc >> 8);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_values() {
        assert_eq!(crc32(&[b""]), 0);
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
        // PNG 的 IEND 块：类型 + 空数据
        assert_eq!(crc32(&[b"IEND"]), 0xAE42_6082);
        assert_eq!(crc32(&[b"1234", b"56789"]), 0xCBF4_3926);
    }
}
