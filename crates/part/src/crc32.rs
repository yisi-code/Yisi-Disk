//! 按位反射、查表计算的 CRC：
//! - CRC-32（IEEE 802.3，多项式 0xEDB88320）：GPT 的头与分区表项、PNG 的块都用它校验；
//! - CRC-32C（Castagnoli，多项式 0x82F63B78）与 CRC-16（多项式 0xA001）：ext4 的超级块、块组描述符用它们校验。
//!
//! 三种只是多项式与寄存器宽度不同，查表与逐字节更新共用一份。

const TABLE: [u32; 256] = make_table(0xEDB8_8320);
const CASTAGNOLI: [u32; 256] = make_table(0x82F6_3B78);
const CRC16: [u32; 256] = make_table(0xA001);

#[expect(
    clippy::indexing_slicing,
    reason = "编译期求值，下标 n 由循环条件限定在 0..256"
)]
const fn make_table(polynomial: u32) -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut value = n as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                polynomial ^ (value >> 1)
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

/// 从寄存器值 `crc` 起依次喂入多段数据，不做首尾取反。
fn update(table: &[u32; 256], mut crc: u32, parts: &[&[u8]]) -> u32 {
    for part in parts {
        for &byte in *part {
            let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
            crc = table.get(index).copied().unwrap_or(0) ^ (crc >> 8);
        }
    }
    crc
}

/// 依次计算多段数据的 CRC-32，相当于把它们拼起来再算。
pub fn crc32(parts: &[&[u8]]) -> u32 {
    !update(&TABLE, 0xFFFF_FFFF, parts)
}

/// CRC-32C 从 `seed` 接着算，首尾都不取反（ext4 的用法：超级块从 `!0` 算起，块组描述符从按 UUID 算出的种子算起）。
/// 通常所说的 CRC-32C 是 `!crc32c(!0, …)`。
pub fn crc32c(seed: u32, parts: &[&[u8]]) -> u32 {
    update(&CASTAGNOLI, seed, parts)
}

/// CRC-16（多项式 0xA001）从 `seed` 接着算，首尾都不取反（ext4 旧式块组描述符校验和从 `0xFFFF` 算起）。
pub fn crc16(seed: u16, parts: &[&[u8]]) -> u16 {
    update(&CRC16, u32::from(seed), parts) as u16
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

    #[test]
    fn castagnoli_and_crc16_match_known_values() {
        // CRC-32C 的标准校验值（首尾取反）
        assert_eq!(!crc32c(!0, &[b"123456789"]), 0xE306_9283);
        assert_eq!(!crc32c(!0, &[b"1234", b"56789"]), 0xE306_9283);
        // CRC-16/ARC（初值 0）与 CRC-16/MODBUS（初值 0xFFFF）
        assert_eq!(crc16(0, &[b"123456789"]), 0xBB3D);
        assert_eq!(crc16(0xFFFF, &[b"123456789"]), 0x4B37);
    }
}
