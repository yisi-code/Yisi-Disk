//! TAR（POSIX.1-2001 的 ustar 交换格式，也认 GNU tar 的写法）：一个个 512 字节的块。每个成员是一个头块
//! （名字、八进制的长度与校验和……第 257 字节起是 `ustar`），之后是数据，按 512 字节补齐；
//! 校验和是头块全部字节之和（校验和字段按 8 个空格算）。归档以两个全 0 的块结束；写的程序通常再补 0 到
//! 记录长度（默认 20 块、10240 字节）的整数倍，补的这段也算进文件。
//!
//! 只认第一个头块带 `ustar` 的归档（老式 V7 归档的头块没有标识，太容易认错）；之后每个头块都核对校验和。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};

const BLOCK: u64 = 512;
/// 默认的记录长度：20 块。
const RECORD: u64 = 20 * BLOCK;
const CHECKSUM_FIELD: std::ops::Range<usize> = 148..156;
const SIZE_FIELD: std::ops::Range<usize> = 124..136;
const TYPE_FLAG: usize = 156;

pub struct Tar;

static INFO: FormatInfo = FormatInfo {
    extension: "tar",
    variants: &[],
    description: "TAR archive",
    max_size: 64 << 30,
    enabled_by_default: true,
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 257,
    bytes: b"ustar",
}];

/// 头块里的数字：八进制文字（前面可以有空格，以 NUL 或空格结束），或 GNU 的 base-256（首字节 0x80，其余是大端整数）。
fn number(field: &[u8]) -> Option<u64> {
    if field.first() == Some(&0x80) {
        return field.get(1..)?.iter().try_fold(0u64, |value, &byte| {
            value.checked_mul(256)?.checked_add(u64::from(byte))
        });
    }
    let mut value = 0u64;
    let mut ended = false;
    for &byte in field.iter().skip_while(|&&byte| byte == b' ') {
        match byte {
            b'0'..=b'7' if !ended => {
                value = value.checked_mul(8)?.checked_add(u64::from(byte - b'0'))?;
            }
            0 | b' ' => ended = true,
            _ => return None,
        }
    }
    Some(value)
}

/// 头块的校验和对不对。早期的实现按有符号字节求和，两种都认。
fn checksum_ok(header: &[u8]) -> bool {
    let Some(stored) = header.get(CHECKSUM_FIELD).and_then(number) else {
        return false;
    };
    let field = header.get(CHECKSUM_FIELD).unwrap_or_default();
    let blanks = 8 * u64::from(b' ');
    let unsigned: u64 = header.iter().map(|&byte| u64::from(byte)).sum::<u64>()
        - field.iter().map(|&byte| u64::from(byte)).sum::<u64>()
        + blanks;
    let signed: i64 = header
        .iter()
        .map(|&byte| i64::from(byte as i8))
        .sum::<i64>()
        - field.iter().map(|&byte| i64::from(byte as i8)).sum::<i64>()
        + blanks as i64;
    stored == unsigned || i64::try_from(stored).is_ok_and(|stored| stored == signed)
}

/// 头块之后的数据占多少字节（按块补齐）。链接、设备、目录、管道没有数据。
fn data_len(header: &[u8]) -> Option<u64> {
    let size = number(header.get(SIZE_FIELD)?)?;
    let has_data = !matches!(header.get(TYPE_FLAG), Some(&(b'1'..=b'6')));
    let size = if has_data { size } else { 0 };
    size.checked_next_multiple_of(BLOCK)
}

fn ustar_magic(header: &[u8]) -> bool {
    let magic = header.get(257..265);
    magic == Some(b"ustar\x0000".as_slice()) || magic == Some(b"ustar  \0".as_slice())
}

/// 一个 TAR 归档的第一个头块（开头 512 字节）：带 `ustar` 标识、校验和对得上、长度读得出。
/// gzip 按解压出的开头判断里面是不是 TAR 时也用它。
pub(crate) fn first_header_ok(block: &[u8]) -> bool {
    block.get(..BLOCK as usize).is_some_and(|header| {
        ustar_magic(header) && checksum_ok(header) && data_len(header).is_some()
    })
}

impl FileFormat for Tar {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        first_header_ok(block).then(|| {
            Box::new(Walker::new(
                INFO.max_size,
                TarStructure {
                    phase: Phase::Header,
                    members: 0,
                },
            )) as Box<dyn Recovering>
        })
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Header,
    /// 读到第一个全 0 块，等第二个。
    SecondZero,
    /// 两个全 0 块在 `end` 结束，看后面补到记录长度的是不是全 0。
    Padding {
        end: u64,
    },
}

struct TarStructure {
    phase: Phase,
    /// 核对过校验和的头块数。
    members: u32,
}

impl Structure for TarStructure {
    fn first(&self) -> Step {
        Step::Need {
            at: 0,
            len: BLOCK as usize,
        }
    }

    /// 部分恢复：至少一个头块核对过校验和才保存，到断点为止；解包软件读到断处报错，之前的成员照样取得出来。
    fn intact(&self, cut: u64) -> Option<u64> {
        (self.members > 0).then_some(cut)
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        let zeros = bytes.iter().all(|&byte| byte == 0);
        match self.phase {
            Phase::Header if zeros && at > 0 => {
                self.phase = Phase::SecondZero;
                Step::Need {
                    at: at + BLOCK,
                    len: BLOCK as usize,
                }
            }
            Phase::Header => match data_len(bytes).and_then(|data| data.checked_add(at + BLOCK)) {
                Some(next) if checksum_ok(bytes) => {
                    self.members += 1;
                    Step::Need {
                        at: next,
                        len: BLOCK as usize,
                    }
                }
                _ => Step::Invalid,
            },
            Phase::SecondZero if zeros => {
                let end = at + BLOCK;
                let padding = (RECORD - end % RECORD) % RECORD;
                if padding == 0 {
                    return Step::Done(end);
                }
                self.phase = Phase::Padding { end };
                Step::Need {
                    at: end,
                    len: padding as usize,
                }
            }
            Phase::SecondZero => Step::Invalid,
            Phase::Padding { .. } if zeros => Step::Done(at + bytes.len() as u64),
            Phase::Padding { end } => Step::Done(end),
        }
    }

    /// 补到记录长度的那段还没读完数据就用完了（设备末尾）：文件到两个全 0 块为止。
    fn finish(&self, received: u64, _waiting_at: Option<u64>) -> Option<u64> {
        match self.phase {
            Phase::Padding { end } => (end <= received).then_some(end),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Tar.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn members_are_followed_to_the_two_zero_blocks() {
        let entries: &[(&str, usize)] = &[("a.txt", 10), ("dir/", 0), ("b.bin", 1500)];
        for gnu in [false, true] {
            // 补到记录长度
            let padded = samples::tar(entries, true, gnu);
            assert_eq!(padded.len() % 10240, 0);
            let mut data = padded.clone();
            data.extend_from_slice(&[0x55; 600]);
            check(&data, Some(padded.len() as u64));
            // 只有两个全 0 块，后面不是 0
            let bare = samples::tar(entries, false, gnu);
            let mut data = bare.clone();
            data.extend_from_slice(&[0x55; 600]);
            check(&data, Some(bare.len() as u64));
            // 两个全 0 块之后设备就到头了
            check(&bare, Some(bare.len() as u64));
        }
    }

    #[test]
    fn checksums_and_end_blocks_are_required() {
        let sample = samples::tar(&[("a.txt", 10), ("b.txt", 700)], false, false);
        let mut wrong_sum = sample.clone();
        wrong_sum[1024 + 3] ^= 1; // 第二个成员的名字
        check(&wrong_sum, None);
        let mut first_sum = sample.clone();
        first_sum[0] ^= 1;
        assert!(Tar.header_check(&first_sum).is_none());
        // 少了第二个全 0 块
        check(&sample[..sample.len() - 512], None);
        // 第二个成员的长度用 base-256 写成接近上限的值（校验和照实重算）：算下一个头的位置不能溢出
        let mut huge = sample.clone();
        let header = &mut huge[1024..1536];
        header[124..136]
            .copy_from_slice(&[0x80, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFC, 0]);
        header[148..156].copy_from_slice(b"        ");
        let sum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        check(&huge, None);
    }

    #[test]
    fn numbers_are_octal_or_base_256() {
        assert_eq!(number(b"00000001750\0"), Some(1000));
        assert_eq!(number(b"   1750 \0\0\0\0"), Some(1000));
        assert_eq!(number(b"\0\0\0\0"), Some(0));
        assert_eq!(
            number(&[0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0]),
            Some(4096)
        );
        assert_eq!(number(b"0001 2"), None);
        assert_eq!(number(b"0009"), None);
    }
}
