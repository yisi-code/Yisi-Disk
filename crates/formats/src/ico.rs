//! ICO / CUR（Windows 图标与光标资源文件）：6 字节的目录头（保留 0、类型 1 图标 / 2 光标、图像个数），
//! 之后每个图像一条 16 字节的目录项（宽、高、颜色数、保留 0、位面数与每像素位数（光标是热点坐标）、
//! 图像数据的长度与位置）。文件没有总长度，取各图像结尾的最大值。
//!
//! 签名只有 4 个字节、开头还是两个 0，所以核对得更严：目录项的保留字节为 0，
//! 第一个图像紧跟在目录后面、图像互不重叠，每个图像的开头都是 PNG 签名或 40 字节的位图信息头；
//! 位图图像的目录项里位面数与位数合规（PNG 图像的这两项常被写成别的值，例如 `0`、`3328`，不核对）。
//! 文件头那一块里看得到的目录项先核对一遍，图像数据里碰巧像图标文件头的一块（位图信息头错开 10 个字节就是
//! `00 00 01 00`）认不成新的图标。图标里装着 PNG，算作可能装着别的文件（`nests_files`）。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, ReadQueue, Step, Structure, Walker};
use yisi_part::bytes::{le16, le32};

const DIRECTORY_HEAD: usize = 6;
const ENTRY_LEN: usize = 16;
/// 目录头里的类型：1 是图标（签名已经限定只有 1、2 两种）。
const TYPE_CURSOR: u16 = 2;
const MAX_IMAGES: u16 = 256;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const BITMAP_INFO_LEN: u32 = 40;
/// 最小的图像：一个位图信息头。
const MIN_IMAGE: u32 = BITMAP_INFO_LEN;
const BITS_PER_PIXEL: [u16; 7] = [0, 1, 4, 8, 16, 24, 32];

pub struct Ico;

static INFO: FormatInfo = FormatInfo {
    extension: "ico",
    variants: &["cur"],
    description: "Windows icon / cursor",
    max_size: 64 << 20,
    enabled_by_default: true,
    nests_files: true,
};

static SIGNATURES: [Signature; 2] = [
    Signature {
        offset: 0,
        bytes: b"\0\0\x01\0",
    },
    Signature {
        offset: 0,
        bytes: b"\0\0\x02\0",
    },
];

impl FileFormat for Ico {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let kind = le16(block, 2)?;
        let images = le16(block, 4)?;
        if !(1..=MAX_IMAGES).contains(&images) {
            return None;
        }
        // 这一块里看得到的目录项先核对一遍
        let directory_len = (DIRECTORY_HEAD + ENTRY_LEN * usize::from(images)) as u64;
        let visible: Vec<&[u8; ENTRY_LEN]> = block
            .get(DIRECTORY_HEAD..)?
            .as_chunks::<ENTRY_LEN>()
            .0
            .iter()
            .take(usize::from(images))
            .collect();
        if !visible.iter().all(|entry| entry_ok(*entry, directory_len)) {
            return None;
        }
        // 目录整个看得到时，最靠前的图像要正好紧跟在目录后面；那张图像的开头也在这一块里时，要是 PNG 签名或
        // 位图信息头。只有 4 字节的签名在声音、图像数据里很常见（静音段的 `00 00 01 00 01 00`），不这样核对，
        // 不装别的文件的格式（WAV 等）会把它当成新文件开头、判自己已被覆盖。
        if visible.len() == usize::from(images) {
            let first = visible.iter().filter_map(|entry| le32(*entry, 12)).min()?;
            if u64::from(first) != directory_len {
                return None;
            }
            let start = usize::try_from(first).ok()?;
            if let Some(head) = block.get(start..start.checked_add(PNG_SIGNATURE.len())?)
                && head != PNG_SIGNATURE
                && le32(head, 0) != Some(BITMAP_INFO_LEN)
            {
                return None;
            }
        }
        Some(Box::new(Walker::new(
            INFO.max_size,
            IcoStructure {
                cursor: kind == TYPE_CURSOR,
                images,
                queue: ReadQueue::default(),
                bitmap_ok: false,
                end: 0,
            },
        )))
    }
}

/// 一条目录项本身说得通：保留字节为 0，图像不比位图信息头还小，在目录之后。
fn entry_ok(entry: &[u8], directory_len: u64) -> bool {
    let (Some(&reserved), Some(size), Some(at)) = (entry.get(3), le32(entry, 8), le32(entry, 12))
    else {
        return false;
    };
    reserved == 0 && size >= MIN_IMAGE && u64::from(at) >= directory_len
}

struct IcoStructure {
    cursor: bool,
    images: u16,
    /// 还没核对开头的图像，以及它是位图时目录项里的位面数与位数合不合规。
    queue: ReadQueue<bool>,
    /// 正在核对开头的图像是位图时，目录项合不合规。
    bitmap_ok: bool,
    end: u64,
}

impl IcoStructure {
    fn directory_len(&self) -> usize {
        DIRECTORY_HEAD + ENTRY_LEN * usize::from(self.images)
    }

    /// 读到整个目录：核对各项，排好要核对开头的图像。
    fn directory(&mut self, bytes: &[u8]) -> Option<()> {
        let directory_len = self.directory_len() as u64;
        let mut ranges = Vec::with_capacity(usize::from(self.images));
        for entry in bytes.get(DIRECTORY_HEAD..)?.as_chunks::<ENTRY_LEN>().0 {
            if !entry_ok(entry, directory_len) {
                return None;
            }
            let planes = le16(entry, 4)?;
            let bits = le16(entry, 6)?;
            let size = le32(entry, 8)?;
            let at = le32(entry, 12)?;
            let bitmap_ok = self.cursor || (planes <= 1 && BITS_PER_PIXEL.contains(&bits));
            // 两个 u32 相加放在 u64 里，不会溢出
            ranges.push((u64::from(at), u64::from(at) + u64::from(size), bitmap_ok));
        }
        ranges.sort_unstable();
        let mut previous_end = directory_len;
        if ranges.first().map(|&(at, ..)| at) != Some(previous_end) {
            return None;
        }
        for (at, end, bitmap_ok) in ranges {
            if at < previous_end {
                return None;
            }
            previous_end = end;
            self.end = self.end.max(end);
            self.queue.push(at, bitmap_ok);
        }
        Some(())
    }

    fn next_image(&mut self) -> Step {
        match self.queue.pop() {
            Some((at, bitmap_ok)) => {
                self.bitmap_ok = bitmap_ok;
                Step::Need {
                    at,
                    len: PNG_SIGNATURE.len(),
                }
            }
            None => Step::Done(self.end),
        }
    }
}

impl Structure for IcoStructure {
    fn first(&self) -> Step {
        Step::Need {
            at: 0,
            len: self.directory_len(),
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        if at == 0 {
            return match self.directory(bytes) {
                Some(()) => self.next_image(),
                None => Step::Invalid,
            };
        }
        let bitmap = self.bitmap_ok && le32(bytes, 0) == Some(BITMAP_INFO_LEN);
        if bytes == PNG_SIGNATURE || bitmap {
            self.next_image()
        } else {
            Step::Invalid
        }
    }

    fn extension(&self) -> Option<&'static str> {
        self.cursor.then_some("cur")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::{check_all_splits, run_keeping};

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Ico.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn length_is_the_end_of_the_last_image() {
        let images: &[(bool, usize)] = &[(false, 300), (true, 1200), (false, 40)];
        for (cursor, extension) in [(false, None), (true, Some("cur"))] {
            let sample = samples::ico(images, cursor);
            let mut data = sample.clone();
            data.extend_from_slice(b"\0\0\x01\0\x01\0 and more");
            check(&data, Some(sample.len() as u64));
            check(&sample[..sample.len() - 1], None);
            let mut recovering = Ico.header_check(&data).unwrap();
            run_keeping(&mut recovering, &data, 100);
            assert_eq!(recovering.extension(), extension);
        }
    }

    #[test]
    fn implausible_directories_or_images_are_rejected() {
        let sample = samples::ico(&[(true, 200), (false, 100)], false);
        let changed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            data
        };
        // 文件头那一块就拒掉，或者往后读时拒掉
        let rejected = |data: &[u8]| {
            if Ico.header_check(data).is_some() {
                check(data, None);
            }
        };
        assert!(Ico.header_check(&changed(6 + 3, &[1])).is_none()); // 保留字节
        rejected(&changed(6 + 16 + 6, &[3, 0])); // 位图图像的位数
        assert!(Ico.header_check(&changed(6 + 12, &[0, 1, 0, 0])).is_none()); // 第一个图像不紧跟目录
        rejected(&changed(6 + 16 + 12, &[40, 0, 0, 0])); // 两个图像重叠
        let first_image = 6 + 2 * 16;
        assert!(Ico.header_check(&changed(first_image, b"GIF89a")).is_none()); // 图像开头不对
        assert!(Ico.header_check(&changed(4, &[0, 0])).is_none(), "没有图像");
    }

    /// 真实图标的写法：PNG 图像的目录项里位面数为 0、位数是 `3328` 这样的值（位图图像的照样核对）。
    #[test]
    fn png_images_do_not_need_plausible_planes_and_bits() {
        let sample = samples::ico(&[(true, 600), (false, 200), (false, 100)], false);
        let mut data = sample.clone();
        for index in 0..3 {
            data[6 + 16 * index + 4..6 + 16 * index + 6].copy_from_slice(&[0, 0]);
        }
        data[6 + 6..6 + 8].copy_from_slice(&3328u16.to_le_bytes());
        check(&data, Some(sample.len() as u64));
        // 位图图像的位数不合规照样不认
        data[6 + 16 + 6..6 + 16 + 8].copy_from_slice(&3328u16.to_le_bytes());
        check(&data, None);
    }

    /// 位图信息头错开 10 个字节（宽的高 16 位、高的低 16 位为 0，位面数 1、位数 32、不压缩）像图标文件头：
    /// 目录项对不上，不认。
    #[test]
    fn silent_audio_that_starts_like_an_icon_is_not_an_icon_header() {
        // 静音的 16 位 PCM：`00 00 01 00 01 00` 之后是零与极小的样本值
        let mut block = vec![0u8; 512];
        block[..6].copy_from_slice(&[0, 0, 1, 0, 1, 0]);
        block[6..22].copy_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0, 0x40, 0, 0, 0, 0x30, 0, 0, 0]);
        assert!(Ico.header_check(&block).is_none());
    }

    #[test]
    fn a_shifted_bitmap_header_is_not_an_icon_header() {
        let mut block = vec![0u8; 512];
        block[..12].copy_from_slice(&[0, 0, 1, 0, 32, 0, 0, 0, 0, 0, 0x13, 0x0B]);
        block[12..16].copy_from_slice(&0x0B13u32.to_le_bytes());
        assert!(Ico.header_check(&block).is_none());
    }
}
