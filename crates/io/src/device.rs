use crate::IoError;

/// 只读块设备。
///
/// 引擎拿到的设备都实现这个 trait，它**没有写方法**：写盘只能生成写入计划，交给磁盘代理执行。
pub trait BlockDevice: Send + Sync {
    /// 设备总字节数。
    fn size(&self) -> u64;

    /// 逻辑扇区大小（字节）。
    fn sector_size(&self) -> u32;

    /// 从 `offset` 起读入 `buf`，偏移和长度都不要求按扇区对齐。
    ///
    /// 读到设备末尾时截断，返回实际读到的字节数；`offset` 不小于 [`size`](Self::size) 时返回 0。
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError>;

    /// 读满 `buf`；设备在此之前结束时返回 [`IoError::UnexpectedEof`]。
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let mut filled = 0;
        while filled < buf.len() {
            let eof = IoError::UnexpectedEof {
                offset,
                expected: buf.len(),
                read: filled,
            };
            let Some(position) = offset.checked_add(filled as u64) else {
                return Err(eof);
            };
            let read = self.read_at(position, &mut buf[filled..])?;
            if read == 0 {
                return Err(eof);
            }
            filled += read;
        }
        Ok(())
    }
}

/// 扇区大小只接受 512 到 64 KiB 之间的 2 的幂。
pub(crate) fn validate_sector_size(sector_size: u32) -> Result<u32, IoError> {
    if sector_size.is_power_of_two() && (512..=65536).contains(&sector_size) {
        Ok(sector_size)
    } else {
        Err(IoError::InvalidSectorSize(sector_size))
    }
}
