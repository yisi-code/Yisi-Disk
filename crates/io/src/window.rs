use crate::{BlockDevice, IoError};

/// 设备上的一段，当作一块独立的设备来读：偏移从这一段的开头算起，读不到这一段之外。
/// 用来只扫描、只预览源上的某个文件所在的位置。
pub struct Window<'d> {
    device: &'d dyn BlockDevice,
    start: u64,
    length: u64,
}

impl<'d> Window<'d> {
    /// `device` 上从 `start` 起 `length` 字节；超出设备末尾的部分截掉。
    pub fn new(device: &'d dyn BlockDevice, start: u64, length: u64) -> Self {
        let available = device.size().saturating_sub(start);
        Self {
            device,
            start,
            length: length.min(available),
        }
    }
}

impl BlockDevice for Window<'_> {
    fn size(&self) -> u64 {
        self.length
    }

    fn sector_size(&self) -> u32 {
        self.device.sector_size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        let Some(remaining) = self.length.checked_sub(offset).filter(|&left| left > 0) else {
            return Ok(0);
        };
        let len = usize::try_from(remaining).map_or(buf.len(), |left| left.min(buf.len()));
        let Some(position) = self.start.checked_add(offset) else {
            return Ok(0);
        };
        match buf.get_mut(..len) {
            Some(part) => self.device.read_at(position, part),
            None => Ok(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Bytes(Vec<u8>);

    impl BlockDevice for Bytes {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }

        fn sector_size(&self) -> u32 {
            512
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
            let start = (offset as usize).min(self.0.len());
            let end = (start + buf.len()).min(self.0.len());
            buf[..end - start].copy_from_slice(&self.0[start..end]);
            Ok(end - start)
        }
    }

    #[test]
    fn reads_stay_inside_the_window() {
        let device = Bytes((0..=255u8).collect());
        let window = Window::new(&device, 100, 20);
        assert_eq!(window.size(), 20);
        let mut buf = [0u8; 8];
        assert_eq!(window.read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(buf, [100, 101, 102, 103, 104, 105, 106, 107]);
        assert_eq!(window.read_at(16, &mut buf).unwrap(), 4);
        assert_eq!(buf[..4], [116, 117, 118, 119]);
        assert_eq!(window.read_at(20, &mut buf).unwrap(), 0);
        // 超出设备末尾的部分截掉
        assert_eq!(Window::new(&device, 250, 100).size(), 6);
        assert_eq!(Window::new(&device, 300, 100).size(), 0);
    }
}
