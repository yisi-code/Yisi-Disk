//! GIF（GIF87a / GIF89a 规范）：6 字节文件头、7 字节逻辑屏幕描述、可选的全局颜色表，
//! 之后是一串块：扩展块（0x21、标签、子块）与图像（0x2C、9 字节图像描述、可选的局部颜色表、
//! LZW 最小码长、子块），以 0x3B 结束。子块是 1 字节长度加数据，长度 0 表示这一串子块结束。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::le16;

const EXTENSION: u8 = 0x21;
const IMAGE: u8 = 0x2C;
const TRAILER: u8 = 0x3B;
/// 规范里的扩展标签：纯文本、图形控制、注释、应用。
const EXTENSION_LABELS: [u8; 4] = [0x01, 0xF9, 0xFE, 0xFF];
/// 颜色表标志位与大小（2 的 n+1 次方项，每项 3 字节）。
const COLOR_TABLE_FLAG: u8 = 0x80;

pub struct Gif;

static INFO: FormatInfo = FormatInfo {
    extension: "gif",
    variants: &[],
    description: "GIF image",
    max_size: 128 << 20,
    enabled_by_default: true,
    nests_files: false,
};

static SIGNATURES: [Signature; 2] = [
    Signature {
        offset: 0,
        bytes: b"GIF87a",
    },
    Signature {
        offset: 0,
        bytes: b"GIF89a",
    },
];

/// 颜色表的字节数；标志位没置时为 0。
fn color_table_len(packed: u8) -> u64 {
    if packed & COLOR_TABLE_FLAG == 0 {
        0
    } else {
        3 << ((packed & 0x07) + 1)
    }
}

impl FileFormat for Gif {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let width = le16(block, 6)?;
        let height = le16(block, 8)?;
        (width > 0 && height > 0).then(|| {
            Box::new(Walker::new(
                INFO.max_size,
                GifStructure {
                    phase: Phase::Screen,
                    images: 0,
                },
            )) as Box<dyn Recovering>
        })
    }
}

enum Phase {
    /// 等逻辑屏幕描述（文件头之后 7 字节）。
    Screen,
    /// 等块的第一个字节。
    Block,
    /// 等扩展块的标签。
    Label,
    /// 等图像描述的 9 字节。
    ImageDescriptor,
    /// 等 LZW 最小码长。
    CodeSize,
    /// 等子块的长度字节。
    SubBlock,
}

struct GifStructure {
    phase: Phase,
    images: u32,
}

impl GifStructure {
    fn sub_blocks(&mut self, at: u64) -> Step {
        self.phase = Phase::SubBlock;
        Step::Need { at, len: 1 }
    }

    fn block(&mut self, at: u64) -> Step {
        self.phase = Phase::Block;
        Step::Need { at, len: 1 }
    }
}

impl Structure for GifStructure {
    fn first(&self) -> Step {
        Step::Need { at: 6, len: 7 }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        let Some(&first) = bytes.first() else {
            return Step::Invalid;
        };
        match self.phase {
            Phase::Screen => {
                let Some(&packed) = bytes.get(4) else {
                    return Step::Invalid;
                };
                self.block(at + 7 + color_table_len(packed))
            }
            Phase::Block => match first {
                EXTENSION => {
                    self.phase = Phase::Label;
                    Step::Need { at: at + 1, len: 1 }
                }
                IMAGE => {
                    self.phase = Phase::ImageDescriptor;
                    Step::Need { at: at + 1, len: 9 }
                }
                TRAILER if self.images > 0 => Step::Done(at + 1),
                _ => Step::Invalid,
            },
            Phase::Label => {
                if !EXTENSION_LABELS.contains(&first) {
                    return Step::Invalid;
                }
                self.sub_blocks(at + 1)
            }
            Phase::ImageDescriptor => {
                let (Some(width), Some(height), Some(&packed)) =
                    (le16(bytes, 4), le16(bytes, 6), bytes.get(8))
                else {
                    return Step::Invalid;
                };
                if width == 0 || height == 0 {
                    return Step::Invalid;
                }
                self.images += 1;
                self.phase = Phase::CodeSize;
                Step::Need {
                    at: at + 9 + color_table_len(packed),
                    len: 1,
                }
            }
            Phase::CodeSize => {
                // 规范里是 2 到 8；有的编码器写到 12（LZW 码长的上限）
                if !(2..=12).contains(&first) {
                    return Step::Invalid;
                }
                self.sub_blocks(at + 1)
            }
            Phase::SubBlock => {
                if first == 0 {
                    self.block(at + 1)
                } else {
                    self.sub_blocks(at + 1 + u64::from(first))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn recover(data: &[u8]) -> Box<dyn Recovering> {
        Gif.header_check(data).unwrap()
    }

    #[test]
    fn walks_extensions_and_frames_to_the_trailer() {
        for (frames, global) in [(1, true), (3, false), (5, true)] {
            let sample = samples::gif(frames, global);
            let mut data = sample.clone();
            data.extend_from_slice(b"\x3B\x21\xF9 trailing bytes after the trailer");
            check_all_splits(|| recover(&data), &data, Some(sample.len() as u64));
        }
    }

    #[test]
    fn truncated_or_broken_files_are_rejected() {
        let sample = samples::gif(2, true);
        let truncated = &sample[..sample.len() - 1];
        check_all_splits(|| recover(truncated), truncated, None);

        // 不认识的块类型
        let mut broken = sample.clone();
        let last_block = sample.len() - 1;
        broken[last_block] = 0x99;
        check_all_splits(|| recover(&broken), &broken, None);

        // 没有图像就结束
        let mut empty = sample[..13 + 3 * 4].to_vec();
        empty.push(TRAILER);
        check_all_splits(|| recover(&empty), &empty, None);
    }
}
