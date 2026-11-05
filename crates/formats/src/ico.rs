//! ICO / CUR（Windows 图标与光标资源文件）：6 字节的目录头（保留 0、类型 1 图标 / 2 光标、图像个数），
//! 之后每个图像一条 16 字节的目录项（宽、高、颜色数、保留 0、位面数与每像素位数（光标是热点坐标）、
//! 图像数据的长度与位置）。文件没有总长度，取各图像结尾的最大值。
//!
//! 签名只有 4 个字节、开头还是两个 0，所以核对得更严：目录项的保留字节为 0、图标的位面数与位数合规，
//! 第一个图像紧跟在目录后面、图像互不重叠，每个图像的开头都是 PNG 签名或 40 字节的位图信息头。

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
    nests_files: false,
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
        Some(Box::new(Walker::new(
            INFO.max_size,
            IcoStructure {
                cursor: kind == TYPE_CURSOR,
                images,
                queue: ReadQueue::default(),
                end: 0,
            },
        )))
    }
}

struct IcoStructure {
    cursor: bool,
    images: u16,
    /// 还没核对开头的图像。
    queue: ReadQueue<()>,
    end: u64,
}

impl IcoStructure {
    fn directory_len(&self) -> usize {
        DIRECTORY_HEAD + ENTRY_LEN * usize::from(self.images)
    }

    /// 读到整个目录：核对各项，排好要核对开头的图像。
    fn directory(&mut self, bytes: &[u8]) -> Option<()> {
        let mut ranges = Vec::with_capacity(usize::from(self.images));
        for entry in bytes.get(DIRECTORY_HEAD..)?.as_chunks::<ENTRY_LEN>().0 {
            let reserved = *entry.get(3)?;
            let planes = le16(entry, 4)?;
            let bits = le16(entry, 6)?;
            let size = le32(entry, 8)?;
            let at = le32(entry, 12)?;
            let icon_ok = self.cursor || (planes <= 1 && BITS_PER_PIXEL.contains(&bits));
            if reserved != 0 || !icon_ok || size < MIN_IMAGE {
                return None;
            }
            ranges.push((u64::from(at), u64::from(at) + u64::from(size)));
        }
        ranges.sort_unstable();
        let mut previous_end = self.directory_len() as u64;
        if ranges.first().map(|&(at, _)| at) != Some(previous_end) {
            return None;
        }
        for (at, end) in ranges {
            if at < previous_end {
                return None;
            }
            previous_end = end;
            self.end = self.end.max(end);
            self.queue.push(at, ());
        }
        Some(())
    }

    fn next_image(&mut self) -> Step {
        match self.queue.pop() {
            Some((at, ())) => Step::Need {
                at,
                len: PNG_SIGNATURE.len(),
            },
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
        if bytes == PNG_SIGNATURE || le32(bytes, 0) == Some(BITMAP_INFO_LEN) {
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
        check(&changed(6 + 3, &[1]), None); // 保留字节
        check(&changed(6 + 6, &[3, 0]), None); // 位数
        check(&changed(6 + 12, &[0, 1, 0, 0]), None); // 第一个图像不紧跟目录
        check(&changed(6 + 16 + 12, &[40, 0, 0, 0]), None); // 两个图像重叠
        let first_image = 6 + 2 * 16;
        check(&changed(first_image, b"GIF89a"), None); // 图像开头不对
        assert!(Ico.header_check(&changed(4, &[0, 0])).is_none(), "没有图像");
    }
}
