//! 从字节切片里按偏移取整数。越界时返回 `None`，不 panic。分区表与文件系统的结构是小端，
//! 文件格式（JPEG、PNG、MP4 等）多为大端；各 crate 共用这一份。

pub fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    bytes
        .get(at..at.checked_add(2)?)?
        .try_into()
        .ok()
        .map(u16::from_be_bytes)
}

pub fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at.checked_add(4)?)?
        .try_into()
        .ok()
        .map(u32::from_be_bytes)
}

pub fn be64(bytes: &[u8], at: usize) -> Option<u64> {
    bytes
        .get(at..at.checked_add(8)?)?
        .try_into()
        .ok()
        .map(u64::from_be_bytes)
}

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
    fn reads_both_byte_orders_and_rejects_out_of_range() {
        let bytes = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        assert_eq!(be16(&bytes, 0), Some(0x0102));
        assert_eq!(le16(&bytes, 0), Some(0x0201));
        assert_eq!(be32(&bytes, 4), Some(0x0506_0708));
        assert_eq!(le32(&bytes, 4), Some(0x0807_0605));
        assert_eq!(be64(&bytes, 0), Some(0x0102_0304_0506_0708));
        assert_eq!(le64(&bytes, 0), Some(0x0807_0605_0403_0201));
        assert_eq!(be32(&bytes, 5), None);
        assert_eq!(le16(&bytes, usize::MAX), None);
    }
}
