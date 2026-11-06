//! Matroska / WebM（EBML，RFC 8794；Matroska 是 RFC 9559）：开头是 EBML 头元素（ID `1A 45 DF A3`），
//! 里面的 DocType 说是 `matroska` 还是 `webm`；之后是 Segment 元素（ID `18 53 80 67`），它的长度就是文件余下的部分。
//! 元素是 ID（1–4 字节，首字节前导 0 的个数定长度）、长度（1–8 字节的变长整数）、内容。
//! Segment 的长度写成「未知」（全 1，边录边写的直播流）时算不出结尾，不认。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};

const EBML_ID: u32 = 0x1A45_DFA3;
const DOC_TYPE_ID: u32 = 0x4282;
const SEGMENT_ID: u32 = 0x1853_8067;
/// 读 Segment 的 ID 与长度要的字节数。
const SEGMENT_HEAD: usize = 4 + 8;

pub struct Mkv;

static INFO: FormatInfo = FormatInfo {
    extension: "mkv",
    variants: &["webm"],
    description: "Matroska / WebM video",
    max_size: 64 << 30,
    enabled_by_default: true,
    nests_files: false,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"\x1A\x45\xDF\xA3",
}];

/// 元素 ID：保留长度标记位。返回 ID 与字节数。
fn element_id(bytes: &[u8]) -> Option<(u32, usize)> {
    let first = *bytes.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 {
        return None;
    }
    let id = bytes
        .get(..len)?
        .iter()
        .fold(0u32, |id, &byte| (id << 8) | u32::from(byte));
    Some((id, len))
}

/// 元素长度：去掉长度标记位；全 1 表示未知，返回 `None` 作为值。返回（值，字节数）。
fn element_size(bytes: &[u8]) -> Option<(Option<u64>, usize)> {
    let first = *bytes.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let mask = 0xFFu8.checked_shr(len as u32).unwrap_or(0);
    let mut value = u64::from(first & mask);
    let mut all_ones = first & mask == mask;
    for &byte in bytes.get(1..len)? {
        value = (value << 8) | u64::from(byte);
        all_ones &= byte == 0xFF;
    }
    Some(((!all_ones).then_some(value), len))
}

/// EBML 头：给出 DocType 与 EBML 头之后的位置。
fn ebml_header(block: &[u8]) -> Option<(&[u8], u64)> {
    let (id, id_len) = element_id(block)?;
    let (header_size, size_len) = element_size(block.get(id_len..)?)?;
    let header_size = usize::try_from(header_size?).ok()?;
    if id != EBML_ID || header_size > 1024 {
        return None;
    }
    let body_start = id_len + size_len;
    let body = block.get(body_start..body_start + header_size)?;
    let mut at = 0;
    while at < body.len() {
        let (id, id_len) = element_id(body.get(at..)?)?;
        let (size, size_len) = element_size(body.get(at + id_len..)?)?;
        let size = usize::try_from(size?).ok()?;
        let value_at = at + id_len + size_len;
        if id == DOC_TYPE_ID {
            let doc_type = body.get(value_at..value_at + size)?;
            let doc_type = doc_type.split(|&byte| byte == 0).next().unwrap_or_default();
            return Some((doc_type, (body_start + header_size) as u64));
        }
        at = value_at + size;
    }
    None
}

impl FileFormat for Mkv {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let (doc_type, segment_at) = ebml_header(block)?;
        let extension = match doc_type {
            b"matroska" => None,
            b"webm" => Some("webm"),
            _ => return None,
        };
        Some(Box::new(Walker::new(
            INFO.max_size,
            MkvStructure {
                segment_at,
                extension,
                body_at: None,
            },
        )))
    }
}

struct MkvStructure {
    segment_at: u64,
    extension: Option<&'static str>,
    /// Segment 的内容从哪里开始（读到 Segment 的头之后才知道）。
    body_at: Option<u64>,
}

impl Structure for MkvStructure {
    fn first(&self) -> Step {
        Step::Need {
            at: self.segment_at,
            len: SEGMENT_HEAD,
        }
    }

    /// 部分恢复：Segment 的内容开始之后才保存，到断点为止（内容核对不了，与完整时一样只看 Segment 的长度）。
    fn intact(&self, cut: u64) -> Option<u64> {
        self.body_at.filter(|&body_at| cut > body_at).map(|_| cut)
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        let Some((SEGMENT_ID, id_len)) = element_id(bytes) else {
            return Step::Invalid;
        };
        match element_size(bytes.get(id_len..).unwrap_or_default()) {
            Some((Some(size), size_len)) => {
                let body_at = at + (id_len + size_len) as u64;
                self.body_at = Some(body_at);
                match body_at.checked_add(size) {
                    Some(end) => Step::Done(end),
                    None => Step::Invalid,
                }
            }
            _ => Step::Invalid,
        }
    }

    fn extension(&self) -> Option<&'static str> {
        self.extension
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::{check_all_splits, run_keeping};

    #[test]
    fn length_is_the_end_of_the_segment() {
        for (doc_type, extension) in [("matroska", None), ("webm", Some("webm"))] {
            let sample = samples::mkv(doc_type, 5000, false);
            let mut data = sample.clone();
            data.extend_from_slice(b"\x1A\x45\xDF\xA3 then something else");
            check_all_splits(
                || Mkv.header_check(&data).unwrap(),
                &data,
                Some(sample.len() as u64),
            );
            let mut recovering = Mkv.header_check(&data).unwrap();
            run_keeping(&mut recovering, &data, 64);
            assert_eq!(recovering.extension(), extension);
        }
    }

    #[test]
    fn unknown_segment_size_or_other_doc_types_are_rejected() {
        let live = samples::mkv("webm", 300, true);
        check_all_splits(|| Mkv.header_check(&live).unwrap(), &live, None);
        assert!(
            Mkv.header_check(&samples::mkv("other", 300, false))
                .is_none()
        );
    }
}
