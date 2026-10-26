use std::fmt;

/// GPT 里的 GUID，按磁盘上的字节保存。前三段在磁盘上是小端，显示时按惯例转成大端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    /// 由惯用的文本形式（`C12A7328-F81F-11D2-BA4B-00A0C93EC93B`）在编译期构造，供已知类型的常量使用。
    #[expect(
        clippy::indexing_slicing,
        reason = "只用于常量：下标由循环条件限定在各数组长度之内"
    )]
    pub const fn from_text(text: &str) -> Self {
        let text = text.as_bytes();
        // 文本里各字节的十六进制位置，以及它们在磁盘上的顺序
        const ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
        let mut digits = [0u8; 32];
        let mut count = 0;
        let mut index = 0;
        while index < text.len() && count < 32 {
            let ch = text[index];
            let value = match ch {
                b'0'..=b'9' => Some(ch - b'0'),
                b'a'..=b'f' => Some(ch - b'a' + 10),
                b'A'..=b'F' => Some(ch - b'A' + 10),
                _ => None,
            };
            if let Some(value) = value {
                digits[count] = value;
                count += 1;
            }
            index += 1;
        }
        let mut bytes = [0u8; 16];
        let mut position = 0;
        while position < 16 {
            bytes[ORDER[position]] = digits[position * 2] << 4 | digits[position * 2 + 1];
            position += 1;
        }
        Self(bytes)
    }

    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|&byte| byte == 0)
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 前三段在磁盘上是小端，按惯例倒过来显示
        const ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
        for (position, index) in ORDER.into_iter().enumerate() {
            if matches!(position, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{:02X}", self.0.get(index).copied().unwrap_or(0))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips_through_disk_order() {
        let text = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
        let guid = Guid::from_text(text);
        assert_eq!(guid.0[..4], [0x28, 0x73, 0x2A, 0xC1]);
        assert_eq!(guid.to_string(), text);
        assert!(!guid.is_zero());
        assert!(Guid([0; 16]).is_zero());
    }
}
