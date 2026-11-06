//! ISO 9660（ECMA-119）光盘镜像：开头 32 KiB 是系统区（通常全是 0，可启动的镜像里是引导代码与分区表），
//! 从第 16 个 2048 字节的逻辑扇区（32 KiB 处）起是一串卷描述符：类型、`CD001`、版本，以类型 255 的
//! 卷描述符集结束符收尾。主卷描述符（类型 1，按惯例就在 32 KiB 处）记着卷空间的大小（逻辑块数）与逻辑块大小，
//! 两者相乘就是整个镜像的长度。
//!
//! 系统区认不出来，所以在主卷描述符那一块认（[`FileFormat::lead`]：文件从它前面 32 KiB 算起）。
//! 主卷描述符里很多数值都按「先小端、再大端」各写一遍，两份要一致，再加上根目录记录的形状，随机数据很难凑上。
//! 镜像里装着各种文件（`nests_files`）。部分恢复：卷描述符集走完之后的断点。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::{be16, be32, le16, le32};

/// 卷描述符所在的逻辑扇区的大小，与逻辑块大小无关。
const SECTOR: u64 = 2048;
/// 系统区：卷描述符从这里开始。
const SYSTEM_AREA: u64 = 16 * SECTOR;
const IDENTIFIER: &[u8] = b"CD001";
const PRIMARY: u8 = 1;
const TERMINATOR: u8 = 255;
/// 引导记录、补充 / 增强卷描述符、分区描述符。
const OTHER_DESCRIPTORS: [u8; 3] = [0, 2, 3];
/// 卷描述符集最多走这么多个（实际一般不超过 5 个）。
const MAX_DESCRIPTORS: u64 = 64;
/// 主卷描述符的文件结构版本在第 881 字节。
const FILE_STRUCTURE_VERSION_AT: usize = 881;
/// 根目录记录在主卷描述符的第 156 字节，34 字节长。
const ROOT_RECORD_AT: usize = 156;
const ROOT_RECORD_LEN: u8 = 34;
/// 目录记录的标志位：是目录。
const DIRECTORY_FLAG: u8 = 0x02;

pub struct Iso;

static INFO: FormatInfo = FormatInfo {
    extension: "iso",
    variants: &[],
    description: "ISO 9660 disc image",
    max_size: 128 << 30,
    enabled_by_default: true,
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"\x01CD001\x01",
}];

impl FileFormat for Iso {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn lead(&self) -> u64 {
        SYSTEM_AREA
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        primary_length(block)?;
        Some(Box::new(Walker::new(
            INFO.max_size,
            IsoStructure {
                length: 0,
                descriptors: 0,
                descriptors_end: None,
            },
        )))
    }
}

/// 两字节、四字节「小端一份、大端一份」的数值：两份一致才给出。
fn both16(bytes: &[u8], at: usize) -> Option<u16> {
    let value = le16(bytes, at)?;
    (be16(bytes, at + 2)? == value).then_some(value)
}

fn both32(bytes: &[u8], at: usize) -> Option<u32> {
    let value = le32(bytes, at)?;
    (be32(bytes, at + 4)? == value).then_some(value)
}

/// 主卷描述符的开头（至少前 190 字节，到根目录记录为止）说得通时，给出整个镜像的长度（卷空间大小 × 逻辑块大小）。
fn primary_length(descriptor: &[u8]) -> Option<u64> {
    let head_ok = descriptor.first() == Some(&PRIMARY)
        && descriptor.get(1..6) == Some(IDENTIFIER)
        && descriptor.get(6) == Some(&1)
        && descriptor.get(7) == Some(&0)
        && descriptor.get(72..80)?.iter().all(|&byte| byte == 0);
    if !head_ok {
        return None;
    }
    let blocks = both32(descriptor, 80)?;
    let set_size = both16(descriptor, 120)?;
    let sequence = both16(descriptor, 124)?;
    let block_size = both16(descriptor, 128)?;
    both32(descriptor, 132)?;
    // 逻辑块大小是 2 的幂，不小于 512、不大于逻辑扇区
    let block_size_ok = block_size.is_power_of_two() && (512..=2048).contains(&block_size);
    if !block_size_ok || set_size == 0 || !(1..=set_size).contains(&sequence) {
        return None;
    }
    let length = u64::from(blocks) * u64::from(block_size);
    // 至少装得下系统区、主卷描述符与结束符
    if length < SYSTEM_AREA + 2 * SECTOR {
        return None;
    }
    // 根目录记录：34 字节、是目录、名字是一个 0 字节，位置与长度两份一致、在镜像里面
    let root = descriptor.get(ROOT_RECORD_AT..ROOT_RECORD_AT + usize::from(ROOT_RECORD_LEN))?;
    let extent = both32(root, 2)?;
    let size = both32(root, 10)?;
    let root_ok = root.first() == Some(&ROOT_RECORD_LEN)
        && root
            .get(25)
            .is_some_and(|flags| flags & DIRECTORY_FLAG != 0)
        && both16(root, 28).is_some()
        && root.get(32..34) == Some([1, 0].as_slice())
        && size > 0
        && u64::from(extent) * u64::from(block_size) + u64::from(size) <= length;
    root_ok.then_some(length)
}

struct IsoStructure {
    /// 主卷描述符说的镜像长度。
    length: u64,
    /// 已经走过的卷描述符个数。
    descriptors: u64,
    /// 卷描述符集结束符的结尾（走到了才有）。
    descriptors_end: Option<u64>,
}

impl Structure for IsoStructure {
    fn first(&self) -> Step {
        Step::Need {
            at: SYSTEM_AREA,
            len: SECTOR as usize,
        }
    }

    /// 部分恢复：卷描述符集走完之后才保存，到断点为止（路径表与目录多在镜像开头，前面这些文件还取得出来）。
    fn intact(&self, cut: u64) -> Option<u64> {
        self.descriptors_end.filter(|&end| cut > end).map(|_| cut)
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        if at == SYSTEM_AREA {
            let file_structure_ok = bytes.get(FILE_STRUCTURE_VERSION_AT) == Some(&1);
            return match primary_length(bytes) {
                Some(length) if file_structure_ok => {
                    self.length = length;
                    self.descriptors = 1;
                    Step::Need {
                        at: at + SECTOR,
                        len: 7,
                    }
                }
                _ => Step::Invalid,
            };
        }
        // 之后每个卷描述符只看头：类型、`CD001`、版本（增强卷描述符的版本是 2）
        let (Some(&kind), Some(&version)) = (bytes.first(), bytes.get(6)) else {
            return Step::Invalid;
        };
        if bytes.get(1..6) != Some(IDENTIFIER) || !(1..=2).contains(&version) {
            return Step::Invalid;
        }
        self.descriptors += 1;
        let next = at + SECTOR;
        if kind == TERMINATOR {
            if next > self.length {
                return Step::Invalid;
            }
            self.descriptors_end = Some(next);
            return Step::Done(self.length);
        }
        if (kind == PRIMARY || OTHER_DESCRIPTORS.contains(&kind))
            && self.descriptors < MAX_DESCRIPTORS
        {
            Step::Need { at: next, len: 7 }
        } else {
            Step::Invalid
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples::{self, IsoLayout, Noise};
    use crate::walker::tests::{check_all_splits, check_intact_splits, run};

    /// ISO 在卷描述符那一块认，从文件开头送数据。
    fn recover(data: &[u8]) -> Box<dyn Recovering> {
        Iso.header_check(&data[SYSTEM_AREA as usize..]).unwrap()
    }

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| recover(data), data, expected);
    }

    #[test]
    fn length_is_volume_space_size_times_logical_block_size() {
        for layout in [
            IsoLayout::default(),
            IsoLayout {
                block_size: 512,
                joliet: true,
                boot_record: true,
            },
        ] {
            let sample = samples::iso(&layout);
            let mut data = sample.clone();
            data.extend_from_slice(b"\x01CD001\x01 and more data after the image");
            check(&data, Some(sample.len() as u64));
            check(&sample, Some(sample.len() as u64));
        }
    }

    #[test]
    fn missing_terminator_or_mismatched_copies_are_rejected() {
        let sample = samples::iso(&IsoLayout::default());
        let changed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            data
        };
        let pvd = SYSTEM_AREA as usize;
        // 卷空间大小的大端那一份对不上
        assert!(
            Iso.header_check(&changed(pvd + 84, &[0, 0, 0, 1])[pvd..])
                .is_none()
        );
        // 逻辑块大小不合规
        assert!(
            Iso.header_check(&changed(pvd + 128, &[0, 3, 3, 0])[pvd..])
                .is_none()
        );
        // 根目录记录不是目录
        assert!(
            Iso.header_check(&changed(pvd + 156 + 25, &[0])[pvd..])
                .is_none()
        );
        // 文件结构版本不对
        check(&changed(pvd + 881, &[2]), None);
        // 结束符的位置上是别的东西
        check(&changed(pvd + 2048, b"\xFFCD002"), None);
        // 镜像只剩卷描述符之前
        check(&sample[..pvd + 4096], None);
    }

    #[test]
    fn cut_images_keep_what_follows_the_descriptor_set() {
        let sample = samples::iso(&IsoLayout::default());
        let cut = &sample[..sample.len() - 3000];
        check(cut, None);
        check_intact_splits(|| recover(cut), cut, Some(cut.len() as u64));
        // 卷描述符集还没走完就断了：说不出可靠的一段
        let early = &sample[..SYSTEM_AREA as usize + 1000];
        check_intact_splits(|| recover(early), early, None);
    }

    /// 卷空间大小是 u32 的最大值：乘出来的长度超出格式上限，判为认错了，算术不溢出。
    #[test]
    fn largest_volume_space_size_is_beyond_the_limit_without_overflow() {
        let mut sample = samples::iso(&IsoLayout::default());
        let at = SYSTEM_AREA as usize + 80;
        sample[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        sample[at + 4..at + 8].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            primary_length(&sample[SYSTEM_AREA as usize..]),
            Some(u64::from(u32::MAX) * 2048)
        );
        check(&sample, None);
        check_intact_splits(|| recover(&sample), &sample, None);
    }

    /// 32 KiB 随机数据之后放一个只有签名的主卷描述符：两份数值对不上，不认。
    #[test]
    fn signature_in_random_bytes_is_not_an_image() {
        let mut noise = Noise::new(9);
        for _ in 0..200 {
            let mut block = noise.bytes(2048);
            block[..7].copy_from_slice(b"\x01CD001\x01");
            if let Some(recovering) = Iso.header_check(&block) {
                let mut data = noise.bytes(SYSTEM_AREA as usize);
                data.extend_from_slice(&block);
                data.extend(noise.bytes(8192));
                assert_eq!(run(recovering, &data, 512), None);
            }
        }
    }

    /// 随机改坏、截短：只能给出结果或「不认识」，不能 panic。
    #[test]
    fn mutated_images_never_panic() {
        let seed = samples::iso(&IsoLayout {
            joliet: true,
            ..IsoLayout::default()
        });
        let mut noise = Noise::new(11);
        for _ in 0..500 {
            let mut data = seed.clone();
            for _ in 0..(1 + noise.next_u64() % 6) {
                let at = SYSTEM_AREA as usize + noise.next_u64() as usize % 4096;
                data[at] = noise.next_u64() as u8;
            }
            data.truncate(SYSTEM_AREA as usize + 512 + noise.next_u64() as usize % 40_000);
            if let Some(mut recovering) = Iso.header_check(&data[SYSTEM_AREA as usize..]) {
                let length = crate::walker::tests::run_keeping(&mut recovering, &data, 700);
                assert!(length.is_none_or(|length| length <= data.len() as u64));
            }
        }
    }
}
