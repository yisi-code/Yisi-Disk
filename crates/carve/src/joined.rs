//! 把源上的几段首尾相接，当作一块设备来读：由两段拼成的文件另存时在它上面重新核对、预览时从它读。

use yisi_io::{BlockDevice, IoError};

use crate::Fragment;

pub struct FragmentView<'d> {
    device: &'d dyn BlockDevice,
    fragments: Vec<Fragment>,
    size: u64,
}

impl<'d> FragmentView<'d> {
    /// `fragments` 按文件里的先后；超出设备末尾的部分截掉（之后的段也就接不上了，一并不要）。
    pub fn new(device: &'d dyn BlockDevice, fragments: &[Fragment]) -> Self {
        let mut kept = Vec::with_capacity(fragments.len());
        let mut size = 0u64;
        for fragment in fragments {
            let available = device.size().saturating_sub(fragment.offset);
            let length = fragment.length.min(available);
            kept.push(Fragment {
                offset: fragment.offset,
                length,
            });
            size = size.saturating_add(length);
            if length < fragment.length {
                break;
            }
        }
        Self {
            device,
            fragments: kept,
            size,
        }
    }
}

impl BlockDevice for FragmentView<'_> {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.device.sector_size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        let mut start = 0u64;
        for fragment in &self.fragments {
            let end = start + fragment.length;
            if offset < end {
                let within = offset - start;
                let take =
                    usize::try_from(end - offset).map_or(buf.len(), |left| left.min(buf.len()));
                let Some(target) = buf.get_mut(..take) else {
                    return Ok(0);
                };
                return self.device.read_at(fragment.offset + within, target);
            }
            start = end;
        }
        Ok(0)
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
            let stop = (start + buf.len()).min(self.0.len());
            buf[..stop - start].copy_from_slice(&self.0[start..stop]);
            Ok(stop - start)
        }
    }

    #[test]
    fn reads_across_fragments_and_stops_at_the_device_end() {
        let device = Bytes((0..100u8).collect());
        let view = FragmentView::new(
            &device,
            &[
                Fragment {
                    offset: 10,
                    length: 5,
                },
                Fragment {
                    offset: 90,
                    length: 20,
                },
            ],
        );
        assert_eq!(view.size(), 15);
        let mut all = vec![0; 15];
        view.read_exact_at(0, &mut all).unwrap();
        assert_eq!(
            all,
            [10, 11, 12, 13, 14, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99]
        );
        let mut middle = [0; 4];
        view.read_exact_at(3, &mut middle).unwrap();
        assert_eq!(middle, [13, 14, 90, 91]);
        assert_eq!(view.read_at(15, &mut middle).unwrap(), 0);
    }
}
