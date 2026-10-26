//! 从字节切片里按偏移取小端整数。越界时返回 `None`，不 panic。分区表与文件系统的结构都是小端。

pub fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    bytes
        .get(at..at.checked_add(2)?)?
        .try_into()
        .ok()
        .map(u16::from_le_bytes)
}

pub fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at.checked_add(4)?)?
        .try_into()
        .ok()
        .map(u32::from_le_bytes)
}

pub fn le64(bytes: &[u8], at: usize) -> Option<u64> {
    bytes
        .get(at..at.checked_add(8)?)?
        .try_into()
        .ok()
        .map(u64::from_le_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_little_endian_and_rejects_out_of_range() {
        let bytes = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        assert_eq!(le16(&bytes, 0), Some(0x0201));
        assert_eq!(le32(&bytes, 4), Some(0x0807_0605));
        assert_eq!(le64(&bytes, 0), Some(0x0807_0605_0403_0201));
        assert_eq!(le32(&bytes, 5), None);
        assert_eq!(le16(&bytes, usize::MAX), None);
    }
}
