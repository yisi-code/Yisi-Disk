//! gzip（RFC 1952）：一个或几个首尾相接的成员。每个成员是 10 字节的固定头（`1F 8B`、压缩方法 8 即 deflate、
//! 标志、修改时间、额外标志、操作系统），按标志依次跟着可选字段：FEXTRA（2 字节长度加内容）、FNAME 与 FCOMMENT
//! （以 0 结尾的 ISO 8859-1 文字）、FHCRC（之前整个头的 CRC-32 的低 16 位）；之后是 deflate 流（RFC 1951），
//! 最后 8 字节是解压出的内容的 CRC-32 与长度（ISIZE，按 2^32 取模），都是小端。
//!
//! 头里没有长度，结尾只有把 deflate 流解到结束才知道：用共用的流式解压（[`crate::inflate`]），解出的内容只计数、
//! 算 CRC-32，不保留。一个成员的尾部核对通过，紧接着又是一个合规的成员头，就接着走（几个 .gz 直接拼起来、
//! 并行压缩工具分段写出的都是这样）；不是的话文件在这个成员的结尾结束。
//!
//! 不经结构遍历器：遍历器把一段数据收齐才交给插件，按段要数据就会在文件结束之后多读；深度恢复要在文件结束后的
//! 第一个块就得知结尾，才能从那里接着找下一个文件。所以每块到了就解。
//!
//! 部分恢复：流在中途断了或对不上时，存到解到的位置为止（解压程序解得出断点之前的内容）；尾部核对不上时存下整个成员。
//! 至少要已有一个核对通过的成员，或这个成员已经解过 [`INTACT_INPUT`] 字节的压缩数据。
//! 断点可能偏后：流断在动态 Huffman 块中间、后面接着别的数据时，那些数据照样解码得通（码表是完整的前缀码，
//! 输出满 32 KiB 后任何回引都合法），要等碰上块结束码、下一个块头对不上才出错，断点会多带上一段后来的数据
//! （实测几百字节到十几 KiB）。解压程序先解出断点之前的原内容，到后面才出错或解出乱码。
//!
//! 按内容命名：第一个成员解出的开头是 TAR 的头块，或 FNAME 里的原名以 `.tar` 结尾时，存成 tgz。

use crate::inflate::{INTACT_INPUT, Inflated, Inflater};
use crate::plugin::{DataCheck, FileFormat, FormatInfo, Recovering, Signature};
use yisi_part::bytes::{le16, le32};
use yisi_part::crc32::crc32;

/// `1F 8B` 与压缩方法 8（deflate），规范只定义了这一种方法。
const MAGIC: [u8; 3] = [0x1F, 0x8B, 0x08];
const FIXED_HEADER: usize = 10;
const FLAG_HEADER_CRC: u8 = 0x02;
const FLAG_EXTRA: u8 = 0x04;
const FLAG_NAME: u8 = 0x08;
const FLAG_COMMENT: u8 = 0x10;
/// 保留位，规范要求为 0。
const FLAG_RESERVED: u8 = 0xE0;
const TRAILER: usize = 8;
/// FNAME、FCOMMENT 最长多少字节（不含结尾的 0）。规范没有上限，实际的原名与注释远短于此。
const MAX_TEXT: usize = 64 * 1024;
/// 成员头最长多少字节：固定头、FEXTRA（长度字段最大 0xFFFF）、两段文字与 FHCRC。
const MAX_HEADER: usize = FIXED_HEADER + 2 + 0xFFFF + 2 * (MAX_TEXT + 1) + 2;
/// 按内容判断是不是 TAR：看解压出的第一个头块。
const TAR_HEADER: usize = 512;

pub struct Gzip;

static INFO: FormatInfo = FormatInfo {
    extension: "gz",
    variants: &["tgz"],
    description: "gzip compressed file",
    max_size: 64 << 30,
    enabled_by_default: true,
    // 压缩不动的内容（JPEG、ZIP 等）deflate 原样存放（stored 块），里面的文件可能正好落在块边界上
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: &MAGIC,
}];

/// 固定头：签名、压缩方法、保留位。
fn fixed_header_ok(bytes: &[u8]) -> bool {
    bytes.len() >= FIXED_HEADER
        && bytes.starts_with(&MAGIC)
        && bytes.get(3).is_some_and(|flags| flags & FLAG_RESERVED == 0)
}

enum Header {
    /// 还不够，要更多字节。
    Short,
    Bad,
    /// 成员头共 `len` 字节；`tar_name`：原名以 `.tar` 结尾。
    Parsed {
        len: usize,
        tar_name: bool,
    },
}

/// 从 `at` 起以 0 结尾的文字，返回 0 之后的位置。原名里不该有控制字符，注释里可以换行与制表。
fn text(bytes: &[u8], at: usize, comment: bool) -> Result<usize, Header> {
    let Some(rest) = bytes.get(at..) else {
        return Err(Header::Short);
    };
    for (index, &byte) in rest.iter().enumerate().take(MAX_TEXT + 1) {
        match byte {
            0 => return Ok(at + index + 1),
            b'\t' | b'\n' | b'\r' if comment => {}
            0x01..=0x1F => return Err(Header::Bad),
            _ => {}
        }
    }
    Err(if rest.len() > MAX_TEXT {
        Header::Bad
    } else {
        Header::Short
    })
}

/// 解析一个成员头（`bytes` 从成员开头起）。
fn parse_header(bytes: &[u8]) -> Header {
    if bytes.len() < FIXED_HEADER {
        return Header::Short;
    }
    if !fixed_header_ok(bytes) {
        return Header::Bad;
    }
    let flags = bytes.get(3).copied().unwrap_or_default();
    let mut at = FIXED_HEADER;
    if flags & FLAG_EXTRA != 0 {
        let Some(len) = le16(bytes, at) else {
            return Header::Short;
        };
        at += 2 + usize::from(len);
    }
    let mut tar_name = false;
    if flags & FLAG_NAME != 0 {
        let end = match text(bytes, at, false) {
            Ok(end) => end,
            Err(header) => return header,
        };
        let name = bytes.get(at..end - 1).unwrap_or_default();
        tar_name = name.len() >= 4
            && name
                .get(name.len() - 4..)
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case(b".tar"));
        at = end;
    }
    if flags & FLAG_COMMENT != 0 {
        at = match text(bytes, at, true) {
            Ok(end) => end,
            Err(header) => return header,
        };
    }
    if flags & FLAG_HEADER_CRC != 0 {
        let (Some(stored), Some(covered)) = (le16(bytes, at), bytes.get(..at)) else {
            return Header::Short;
        };
        if crc32(&[covered]) as u16 != stored {
            return Header::Bad;
        }
        at += 2;
    }
    if bytes.len() < at {
        return Header::Short;
    }
    Header::Parsed { len: at, tar_name }
}

impl FileFormat for Gzip {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        if !fixed_header_ok(block) || matches!(parse_header(block), Header::Bad) {
            return None;
        }
        Some(Box::new(GzipRecovering {
            phase: Phase::Header {
                start: 0,
                bytes: Vec::new(),
            },
            received: 0,
            members: 0,
            good: 0,
            member_input: 0,
            tar_name: false,
            head: Vec::new(),
        }))
    }
}

enum Phase {
    /// 成员头：从 `start` 起已经收到、还没解析完的字节。
    Header {
        start: u64,
        bytes: Vec<u8>,
    },
    Deflate(Inflater),
    /// 尾部：解出的内容的 CRC-32 与长度，已经收到的尾部字节。
    Trailer {
        crc: u32,
        size: u64,
        bytes: Vec<u8>,
    },
    /// 成员在 `end` 结束、核对通过，看后面的字节是不是又一个成员头。
    Next {
        end: u64,
        bytes: Vec<u8>,
    },
    Done(u64),
    Broken,
}

/// 处理完一段之后：从某个位置接着处理这一块，或者给出这一块的结论。
enum Flow {
    At(u64),
    Return(DataCheck),
}

struct GzipRecovering {
    phase: Phase,
    /// 已经收到的字节数，下一块从这里开始。
    received: u64,
    /// 尾部核对通过的成员数。
    members: u32,
    /// 部分恢复的断点：解到的位置、流的结尾或核对过的成员的结尾。
    good: u64,
    /// 当前成员已经解过的压缩数据字节数。
    member_input: u64,
    tar_name: bool,
    /// 第一个成员解出的开头。
    head: Vec<u8>,
}

impl GzipRecovering {
    /// 从文件里的 `pos` 处起处理这一块剩下的 `rest`。
    fn step(&mut self, phase: Phase, pos: u64, rest: &[u8]) -> (Phase, Flow) {
        match phase {
            Phase::Done(end) => (Phase::Done(end), Flow::Return(DataCheck::Complete(end))),
            Phase::Broken => (Phase::Broken, Flow::Return(DataCheck::Invalid)),
            Phase::Header { start, mut bytes } => {
                let room = MAX_HEADER.saturating_sub(bytes.len());
                bytes.extend_from_slice(rest.get(..room.min(rest.len())).unwrap_or_default());
                match parse_header(&bytes) {
                    Header::Short if bytes.len() < MAX_HEADER => (
                        Phase::Header { start, bytes },
                        Flow::Return(DataCheck::Continue),
                    ),
                    Header::Short | Header::Bad => {
                        (Phase::Broken, Flow::Return(DataCheck::Invalid))
                    }
                    Header::Parsed { len, tar_name } => {
                        let first = self.members == 0;
                        if first {
                            self.tar_name = tar_name;
                        }
                        self.member_input = 0;
                        let head = if first { TAR_HEADER } else { 0 };
                        let inflater = Inflater::deflate().with_crc32().keep_head(head);
                        (Phase::Deflate(inflater), Flow::At(start + len as u64))
                    }
                }
            }
            Phase::Deflate(mut inflater) => {
                let result = inflater.feed(rest);
                self.member_input = inflater.input_used();
                match result {
                    Inflated::More => {
                        self.good = pos + rest.len() as u64;
                        (Phase::Deflate(inflater), Flow::Return(DataCheck::Continue))
                    }
                    Inflated::End { used } => {
                        let end = pos + used as u64;
                        self.good = end;
                        if self.members == 0 {
                            self.head = inflater.head().to_vec();
                        }
                        let trailer = Phase::Trailer {
                            crc: inflater.crc32(),
                            size: inflater.output_len(),
                            bytes: Vec::with_capacity(TRAILER),
                        };
                        (trailer, Flow::At(end))
                    }
                    Inflated::Broken { used } => {
                        self.good = pos + used as u64;
                        (Phase::Broken, Flow::Return(DataCheck::Invalid))
                    }
                }
            }
            Phase::Trailer {
                crc,
                size,
                mut bytes,
            } => {
                let take = (TRAILER - bytes.len()).min(rest.len());
                bytes.extend_from_slice(rest.get(..take).unwrap_or_default());
                let next = pos + take as u64;
                if bytes.len() < TRAILER {
                    return (
                        Phase::Trailer { crc, size, bytes },
                        Flow::Return(DataCheck::Continue),
                    );
                }
                // deflate 流走完了：内容核对不上也存下整个成员
                self.good = next;
                if le32(&bytes, 0) == Some(crc) && le32(&bytes, 4) == Some(size as u32) {
                    self.members += 1;
                    let next_member = Phase::Next {
                        end: next,
                        bytes: Vec::with_capacity(FIXED_HEADER),
                    };
                    (next_member, Flow::At(next))
                } else {
                    (Phase::Broken, Flow::Return(DataCheck::Invalid))
                }
            }
            Phase::Next { end, mut bytes } => {
                let take = (FIXED_HEADER - bytes.len()).min(rest.len());
                bytes.extend_from_slice(rest.get(..take).unwrap_or_default());
                let next = pos + take as u64;
                let done = (Phase::Done(end), Flow::Return(DataCheck::Complete(end)));
                if !bytes.iter().zip(MAGIC).all(|(&byte, magic)| byte == magic) {
                    return done;
                }
                if bytes.len() < FIXED_HEADER {
                    return (
                        Phase::Next { end, bytes },
                        Flow::Return(DataCheck::Continue),
                    );
                }
                if fixed_header_ok(&bytes) {
                    (Phase::Header { start: end, bytes }, Flow::At(next))
                } else {
                    done
                }
            }
        }
    }
}

impl Recovering for GzipRecovering {
    fn data_check(&mut self, offset: u64, block: &[u8]) -> DataCheck {
        if let Phase::Done(end) = self.phase {
            return DataCheck::Complete(end);
        }
        let end = offset
            .checked_add(block.len() as u64)
            .filter(|_| offset == self.received);
        let Some(end) = end else {
            self.phase = Phase::Broken;
            return DataCheck::Invalid;
        };
        self.received = end;
        let mut pos = offset;
        loop {
            let rest = pos
                .checked_sub(offset)
                .and_then(|skip| usize::try_from(skip).ok())
                .and_then(|skip| block.get(skip..));
            let Some(rest) = rest else {
                self.phase = Phase::Broken;
                return DataCheck::Invalid;
            };
            let phase = std::mem::replace(&mut self.phase, Phase::Broken);
            let (phase, flow) = self.step(phase, pos, rest);
            self.phase = phase;
            match flow {
                Flow::At(next) => pos = next,
                Flow::Return(check) => return check,
            }
        }
    }

    fn finish(&mut self, received: u64) -> Option<u64> {
        match &self.phase {
            // 最后一个成员之后不够一个成员头：文件到这个成员为止
            Phase::Done(end) | Phase::Next { end, .. } => Some(*end),
            _ => None,
        }
        .filter(|&end| end <= received)
    }

    fn extension(&self) -> Option<&'static str> {
        let head = match &self.phase {
            Phase::Deflate(inflater) if self.members == 0 => inflater.head(),
            _ => &self.head,
        };
        (self.tar_name || crate::tar::first_header_ok(head)).then_some("tgz")
    }

    fn intact(&mut self, received: u64) -> Option<u64> {
        if self.members == 0 && self.member_input < INTACT_INPUT {
            return None;
        }
        Some(self.good.min(received)).filter(|&length| length > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples::{self, GzipLayout};
    use crate::walker::tests::{
        check_all_splits, check_intact_splits, run, run_intact, run_keeping,
    };

    fn make(data: &[u8]) -> impl Fn() -> Box<dyn Recovering> + '_ {
        move || Gzip.header_check(data).unwrap()
    }

    fn sample(len: usize, seed: u64) -> Vec<u8> {
        samples::gzip(&samples::compressible(len, seed), &GzipLayout::default())
    }

    #[test]
    fn a_member_ends_where_its_trailer_checks_out() {
        let content = samples::compressible(12_000, 2);
        let layouts = [
            GzipLayout::default(),
            GzipLayout {
                name: Some("notes.txt"),
                ..GzipLayout::default()
            },
            GzipLayout {
                extra: Some(b"AB\x02\0xy"),
                comment: Some("line one\r\n\tline two"),
                header_crc: true,
                ..GzipLayout::default()
            },
            GzipLayout {
                level: 0,
                ..GzipLayout::default()
            },
            GzipLayout {
                level: 9,
                name: Some("x"),
                header_crc: true,
                ..GzipLayout::default()
            },
        ];
        let tails: [&[u8]; 5] = [
            b"",
            &[0; 600],
            &samples::png(&[10]),
            b"\x1f\x8b",
            b"\x1f\x8b\x08\xe0 not a member header",
        ];
        for layout in &layouts {
            let member = samples::gzip(&content, layout);
            for tail in tails {
                let mut data = member.clone();
                data.extend_from_slice(tail);
                check_all_splits(make(&data), &data, Some(member.len() as u64));
            }
        }
        let empty = samples::gzip(b"", &GzipLayout::default());
        check_all_splits(make(&empty), &empty, Some(empty.len() as u64));
    }

    #[test]
    fn members_one_after_another_make_one_file() {
        let a = samples::gzip(
            &samples::compressible(9000, 3),
            &GzipLayout {
                name: Some("a.log"),
                ..GzipLayout::default()
            },
        );
        let b = samples::gzip(b"", &GzipLayout::default());
        let c = samples::gzip(
            &samples::compressible(5000, 4),
            &GzipLayout {
                header_crc: true,
                ..GzipLayout::default()
            },
        );
        let whole = [&a[..], &b, &c].concat();
        let mut data = whole.clone();
        data.extend(samples::jpg(100));
        check_all_splits(make(&data), &data, Some(whole.len() as u64));

        // 后面是坏了的成员头：文件到前一个成员为止
        let mut data = [&a[..], &b].concat();
        data.extend_from_slice(b"\x1f\x8b\x08\x80 and more bytes");
        check_all_splits(make(&data), &data, Some((a.len() + b.len()) as u64));
    }

    #[test]
    fn a_cut_stream_keeps_what_was_decoded() {
        let member = sample(40_000, 5);
        let n = member.len();
        // 截在 deflate 流里：存到截断处
        for cut in [n / 2, n - TRAILER - 1] {
            check_all_splits(make(&member), &member[..cut], None);
            check_intact_splits(make(&member), &member[..cut], Some(cut as u64));
        }
        // 截在尾部里：存到流的结尾
        check_all_splits(make(&member), &member[..n - 3], None);
        check_intact_splits(make(&member), &member[..n - 3], Some((n - TRAILER) as u64));
        // 只解出一点点：说不出可靠的一段
        let small = sample(3000, 6);
        check_intact_splits(make(&small), &small[..small.len() / 2], None);
        // 截在头里
        let named = samples::gzip(
            b"abc",
            &GzipLayout {
                name: Some("a-rather-long-original-name.txt"),
                ..GzipLayout::default()
            },
        );
        check_intact_splits(make(&named), &named[..20], None);
    }

    #[test]
    fn a_wrong_trailer_keeps_the_whole_member_as_incomplete() {
        let member = sample(20_000, 7);
        let n = member.len();
        for at in [n - TRAILER, n - 1] {
            let mut bad = member.clone();
            bad[at] ^= 1;
            check_all_splits(make(&bad), &bad, None);
            check_intact_splits(make(&bad), &bad, Some(n as u64));
        }
        // 流中间坏了：存到出错处附近，不超过数据
        let mut bad = member.clone();
        bad[n / 2] ^= 0xFF;
        for block_size in [1, 512, 4096] {
            assert_eq!(run(make(&bad)(), &bad, block_size), None);
            let kept = run_intact(make(&bad)(), &bad, block_size).unwrap();
            assert!(kept <= n as u64, "{kept}");
        }
    }

    #[test]
    fn a_later_member_cut_short_keeps_the_members_before_it() {
        let a = sample(2000, 8);
        let b = samples::gzip(
            &samples::compressible(20_000, 9),
            &GzipLayout {
                name: Some("second.txt"),
                ..GzipLayout::default()
            },
        );
        // 截在第二个成员的流里：第一个成员核对过了，存到截断处
        let data = [&a[..], &b[..b.len() / 2]].concat();
        check_all_splits(make(&data), &data, None);
        check_intact_splits(make(&data), &data, Some(data.len() as u64));
        // 截在第二个成员的原名里：存到第一个成员的结尾
        let data = [&a[..], &b[..14]].concat();
        check_all_splits(make(&data), &data, None);
        check_intact_splits(make(&data), &data, Some(a.len() as u64));
        // 第二个成员的固定头都没收全：第一个成员就是整个文件
        let data = [&a[..], &b[..5]].concat();
        check_all_splits(make(&data), &data, Some(a.len() as u64));
    }

    #[test]
    fn a_tar_inside_is_named_tgz() {
        let tar = samples::tar(&[("a.txt", 100), ("b.bin", 3000)], false, false);
        let text = samples::compressible(2000, 10);
        let cases: [(&[u8], Option<&str>, Option<&str>); 4] = [
            (&tar, None, Some("tgz")),
            (&text, Some("backup.TAR"), Some("tgz")),
            (&text, Some("a.txt"), None),
            (&text, None, None),
        ];
        for (content, name, expected) in cases {
            let data = samples::gzip(
                content,
                &GzipLayout {
                    name,
                    ..GzipLayout::default()
                },
            );
            let mut recovering = Gzip.header_check(&data).unwrap();
            assert_eq!(
                run_keeping(&mut recovering, &data, 512),
                Some(data.len() as u64)
            );
            assert_eq!(recovering.extension(), expected, "{name:?}");
        }
    }

    #[test]
    fn malformed_headers_are_rejected() {
        let good = samples::gzip(
            b"abc",
            &GzipLayout {
                name: Some("a"),
                header_crc: true,
                ..GzipLayout::default()
            },
        );
        assert!(Gzip.header_check(&good).is_some());
        let mut reserved = good.clone();
        reserved[3] |= 0x20;
        assert!(Gzip.header_check(&reserved).is_none());
        // FHCRC 在原名 "a\0" 之后
        let mut header_crc = good.clone();
        header_crc[12] ^= 1;
        assert!(Gzip.header_check(&header_crc).is_none());
        let mut control = good.clone();
        control[10] = 0x01;
        assert!(Gzip.header_check(&control).is_none());
        assert!(Gzip.header_check(&good[..9]).is_none());
    }
}
