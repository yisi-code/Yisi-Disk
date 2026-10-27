//! MP4 / MOV（ISO 基础媒体文件格式）：一串顶层 box（4 字节大端长度 + 4 字节类型），以 ftyp 开头。
//! 文件没有结束标志：遇到不认识的顶层 box、或数据结束时，在最后一个完整 box 之后结束。
//!
//! 结束处还要能证明文件没被截短，否则判为不完整：
//! - 分片文件的 sidx 登记了索引内容的总长度，文件至少要到那里；
//! - 最后一个 box 不能是 moof：它的样本数据在紧随其后的 mdat 里，mdat 没跟上说明后面还有。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::{be16, be32, be64};

/// 视为合法的顶层 box 类型；遇到其他类型时文件结束。
const TOP_LEVEL_BOXES: [&[u8; 4]; 17] = [
    b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"uuid", b"meta", b"pdin", b"moof",
    b"mfra", b"styp", b"sidx", b"ssix", b"prft", b"emsg", b"pnot",
];

/// sidx 每条引用的长度：reference_type 与 referenced_size、subsegment_duration、SAP 信息各 4 字节。
const SIDX_REFERENCE_LEN: usize = 12;

pub struct Mp4;

static INFO: FormatInfo = FormatInfo {
    extension: "mp4",
    description: "MPEG-4 / QuickTime video",
    max_size: 64 << 30,
    enabled_by_default: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 4,
    bytes: b"ftyp",
}];

impl FileFormat for Mp4 {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let size = be32(block, 0)?;
        let brand = block.get(8..12)?;
        let plausible = (16..=4096).contains(&size)
            && block.get(4..8) == Some(b"ftyp".as_slice())
            && brand
                .iter()
                .all(|byte| byte.is_ascii_graphic() || *byte == b' ');
        plausible.then(|| {
            Box::new(Walker::new(Mp4Structure {
                phase: Phase::Header,
                seen_moov: false,
                seen_mdat: false,
                last_is_moof: false,
                indexed_end: 0,
            })) as Box<dyn Recovering>
        })
    }
}

enum Phase {
    /// 等 box 头：长度 + 类型。
    Header,
    /// 长度字段为 1，等后面的 8 字节 64 位长度。
    LargeSize { start: u64, kind: &'static [u8; 4] },
    /// 等 sidx 的 box 体；`end` 是这个 box 的结尾，也是 first_offset 的起算点。
    Sidx { end: u64 },
}

struct Mp4Structure {
    phase: Phase,
    seen_moov: bool,
    seen_mdat: bool,
    /// 最近读到的 box（结束时就是最后一个完整的 box）是 moof。
    last_is_moof: bool,
    /// 各个 sidx 索引内容结尾的最大值：文件至少要到这里。
    indexed_end: u64,
}

impl Mp4Structure {
    fn complete(&self, at: u64) -> bool {
        self.seen_moov && self.seen_mdat && !self.last_is_moof && at >= self.indexed_end
    }

    fn end_at(&self, at: u64) -> Step {
        if self.complete(at) {
            Step::Done(at)
        } else {
            Step::Invalid
        }
    }

    /// box 头读完、长度已知：sidx 接着读 box 体，其他 box 整个跳过。
    fn after_header(&mut self, kind: &'static [u8; 4], start: u64, header: u64, size: u64) -> Step {
        let Some(end) = start.checked_add(size) else {
            return Step::Invalid;
        };
        if kind != b"sidx" {
            self.phase = Phase::Header;
            return Step::Need { at: end, len: 8 };
        }
        // box 体超过遍历器单步上限时由遍历器判为无效；合法的 sidx 最多约 768 KiB，不会超
        let Some(body) = size
            .checked_sub(header)
            .and_then(|body| usize::try_from(body).ok())
        else {
            return Step::Invalid;
        };
        self.phase = Phase::Sidx { end };
        // header 不超过 size，start + size 已确认不溢出
        Step::Need {
            at: start + header,
            len: body,
        }
    }
}

/// 按 sidx 的 box 体算出索引内容的结尾（ISO/IEC 14496-12 8.16.3）：
/// 起点是 sidx 之后的第一个字节，加上 first_offset，再加上各条引用的 referenced_size。
/// 结构不符或数值溢出时返回 `None`。
fn sidx_indexed_end(body: &[u8], anchor: u64) -> Option<u64> {
    // version、flags、reference_ID、timescale 之后：版本 0 的时间与偏移各 32 位，版本 1 各 64 位
    let (first_offset, count_at) = match body.first()? {
        0 => (u64::from(be32(body, 16)?), 22),
        1 => (be64(body, 20)?, 30),
        _ => return None,
    };
    let count = usize::from(be16(body, count_at)?);
    let references_at = count_at + 2;
    let references = body.get(references_at..references_at + count * SIDX_REFERENCE_LEN)?;
    references
        .as_chunks::<SIDX_REFERENCE_LEN>()
        .0
        .iter()
        .try_fold(anchor.checked_add(first_offset)?, |end, reference| {
            // 最高位是 reference_type（引用媒体还是下一级 sidx），长度在低 31 位
            end.checked_add(u64::from(be32(reference, 0)? & 0x7FFF_FFFF))
        })
}

impl Structure for Mp4Structure {
    fn first(&self) -> Step {
        Step::Need { at: 0, len: 8 }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        match self.phase {
            Phase::Header => {
                let (Some(size), Some(kind)) = (be32(bytes, 0), bytes.get(4..8)) else {
                    return Step::Invalid;
                };
                let Some(&known) = TOP_LEVEL_BOXES
                    .iter()
                    .find(|known| known.as_slice() == kind)
                else {
                    // 不认识的类型：文件在这里结束（后面多半是别的数据）
                    return self.end_at(at);
                };
                match known {
                    b"moov" => self.seen_moov = true,
                    b"mdat" => self.seen_mdat = true,
                    _ => {}
                }
                self.last_is_moof = known == b"moof";
                match size {
                    1 => {
                        self.phase = Phase::LargeSize {
                            start: at,
                            kind: known,
                        };
                        Step::Need { at: at + 8, len: 8 }
                    }
                    // 0 表示 box 一直延伸到文件末尾，无法知道文件在哪里结束
                    0..8 => Step::Invalid,
                    _ => self.after_header(known, at, 8, u64::from(size)),
                }
            }
            Phase::LargeSize { start, kind } => {
                let Some(size) = be64(bytes, 0).filter(|&size| size >= 16) else {
                    return Step::Invalid;
                };
                self.after_header(kind, start, 16, size)
            }
            Phase::Sidx { end } => {
                let Some(indexed_end) = sidx_indexed_end(bytes, end) else {
                    return Step::Invalid;
                };
                self.indexed_end = self.indexed_end.max(indexed_end);
                self.phase = Phase::Header;
                Step::Need { at: end, len: 8 }
            }
        }
    }

    /// 数据恰好在一个 box 结束处用完（例如在设备末尾）：满足完整条件就在这里结束。
    fn finish(&self, received: u64, waiting_at: Option<u64>) -> Option<u64> {
        (matches!(self.phase, Phase::Header)
            && waiting_at == Some(received)
            && self.complete(received))
        .then_some(received)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    const GARBAGE: &[u8] = b"\xFF\xD8\xFF\xE0garbage that is not a box";

    fn recover(data: &[u8]) -> Box<dyn Recovering> {
        Mp4.header_check(data).unwrap()
    }

    /// 后面接别的数据、正好在数据末尾结束，两种情况的结果都是 `expected`。
    fn check_followed_and_alone(data: &[u8], expected: Option<u64>) {
        let mut followed = data.to_vec();
        followed.extend_from_slice(GARBAGE);
        check_all_splits(|| recover(&followed), &followed, expected);
        check_all_splits(|| recover(data), data, expected);
    }

    /// 顶层 box 的（类型，开头）。
    fn boxes(data: &[u8]) -> Vec<(&[u8], usize)> {
        let mut found = Vec::new();
        let mut at = 0;
        while at + 8 <= data.len() {
            let size = be32(data, at).unwrap() as usize;
            found.push((&data[at + 4..at + 8], at));
            at += size;
        }
        found
    }

    fn box_start(data: &[u8], kind: &[u8], nth: usize) -> usize {
        boxes(data)
            .into_iter()
            .filter(|(found, _)| *found == kind)
            .nth(nth)
            .unwrap()
            .1
    }

    #[test]
    fn ends_before_an_unknown_box_or_at_end_of_data() {
        let sample = samples::mp4(5000, false);
        check_followed_and_alone(&sample, Some(sample.len() as u64));
    }

    #[test]
    fn large_size_boxes_are_followed() {
        let sample = samples::mp4(3000, true);
        let mut followed = sample.clone();
        followed.extend_from_slice(&[0u8; 32]);
        check_all_splits(|| recover(&followed), &followed, Some(sample.len() as u64));
    }

    #[test]
    fn missing_moov_or_mdat_is_rejected() {
        let mut only_ftyp = samples::mp4(100, false);
        only_ftyp.truncate(24);
        only_ftyp.extend_from_slice(&[0u8; 16]);
        check_all_splits(|| recover(&only_ftyp), &only_ftyp, None);
    }

    #[test]
    fn fragmented_files_reaching_their_sidx_end_are_complete() {
        for version in [0, 1] {
            for gap in [0, 24] {
                for mfra in [false, true] {
                    let sample = samples::mp4_fragmented(&[300, 50, 700], Some(version), gap, mfra);
                    check_followed_and_alone(&sample, Some(sample.len() as u64));
                }
            }
        }
        // 没有 sidx 的分片文件看不出总长度，以完整的 moof + mdat 结尾就算完整
        let plain = samples::mp4_fragmented(&[300, 50], None, 0, false);
        check_followed_and_alone(&plain, Some(plain.len() as u64));
    }

    #[test]
    fn fragmented_files_cut_short_of_their_sidx_end_are_rejected() {
        let sample = samples::mp4_fragmented(&[300, 50, 700], Some(1), 0, false);
        // 在分片之间截断：结构完整，但不到 sidx 登记的长度
        let cut = &sample[..box_start(&sample, b"moof", 1)];
        check_followed_and_alone(cut, None);
        // 截断后留下一个悬空的 moof
        let dangling = &sample[..box_start(&sample, b"mdat", 1)];
        check_followed_and_alone(dangling, None);
    }

    #[test]
    fn trailing_moof_without_its_mdat_is_rejected() {
        let plain = samples::mp4_fragmented(&[300, 50], None, 0, false);
        let dangling = &plain[..box_start(&plain, b"mdat", 1)];
        check_followed_and_alone(dangling, None);
    }

    #[test]
    fn broken_sidx_is_rejected() {
        for version in [0u8, 1] {
            let sample = samples::mp4_fragmented(&[300, 50], Some(version), 0, false);
            let body = box_start(&sample, b"sidx", 0) + 8;
            let count_at = body + if version == 0 { 22 } else { 30 };

            // 引用长度超出实际数据
            let mut oversized = sample.clone();
            oversized[count_at + 2..count_at + 6].copy_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
            check_followed_and_alone(&oversized, None);

            // 引用条数多于 box 装得下的
            let mut overcounted = sample.clone();
            overcounted[count_at + 1] += 1;
            check_followed_and_alone(&overcounted, None);

            // 不认识的版本
            let mut unknown = sample.clone();
            unknown[body] = 2;
            check_followed_and_alone(&unknown, None);
        }

        // first_offset 加上引用长度溢出 64 位
        let sample = samples::mp4_fragmented(&[300, 50], Some(1), 0, false);
        let first_offset = box_start(&sample, b"sidx", 0) + 8 + 20;
        let mut overflow = sample.clone();
        overflow[first_offset..first_offset + 8].copy_from_slice(&u64::MAX.to_be_bytes());
        check_followed_and_alone(&overflow, None);
    }
}
