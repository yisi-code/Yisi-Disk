//! 流式解 deflate（RFC 1951）与 zlib（RFC 1950）流，只为找到流在哪里结束、核对解出的内容：
//! 解出的字节只计数、按需算 CRC-32、按需留下开头一小段，不保留整份内容。解压用 miniz_oxide 的流式接口，
//! 输出写进 32 KiB 的环形窗口（deflate 往回引用最远 32 KiB），一个流占用的内存有界，与文件多大无关。
//!
//! 数据每到一块就解一块：流在哪里结束只有解到那里才知道，深度恢复要在文件结束后的第一个块就得知结尾。
//! gzip 与压缩的 SWF（`CWS`）共用。

use miniz_oxide::inflate::TINFLStatus;
use miniz_oxide::inflate::core::inflate_flags::{
    TINFL_FLAG_HAS_MORE_INPUT, TINFL_FLAG_PARSE_ZLIB_HEADER,
};
use miniz_oxide::inflate::core::{DecompressorOxide, decompress};
use yisi_part::crc32::crc32_continue;

/// 环形窗口的大小：deflate 往回引用的最远距离（miniz_oxide 要求是 2 的幂）。
const WINDOW: usize = 32 * 1024;

/// 部分恢复的门槛：断了的流至少已经解过这么多字节的压缩数据，才算核对过文件头之后真正的内容。
/// 按压缩数据算、不按解出的字节算：随机数据当作固定 Huffman 编码解，碰上一串长回引就能解出好几 KiB，
/// 但每个符号都有约 2% 的机会碰上无效的长度码或距离码，走不远。实测一百万份 16 KiB 的随机数据，
/// 最多用掉 596 字节、最多解出 4822 字节（有 10 份超过 4 KiB）。只存不压的块不受此限，
/// 但它的长度要与长度的反码对上（2^-16）。
pub(crate) const INTACT_INPUT: u64 = 4096;

/// 送入一段数据后的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Inflated {
    /// 这段数据都用掉了，流还没结束。
    More,
    /// 流在这段数据的第 `used` 字节处结束（zlib 流的 Adler-32 已核对）。
    End { used: usize },
    /// 数据对不上：deflate 编码无效、回引越界，或 zlib 的头、Adler-32 不对。`used` 是出错时用掉的字节。
    Broken { used: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopped {
    No,
    End,
    Broken,
}

pub(crate) struct Inflater {
    state: Box<DecompressorOxide>,
    window: Vec<u8>,
    /// 下一个解出的字节写在窗口的哪里。
    at: usize,
    flags: u32,
    /// 已经用掉的输入（压缩数据）字节数。
    input_used: u64,
    output_len: u64,
    /// 解出内容的 CRC-32；不需要时为 `None`。
    crc: Option<u32>,
    /// 解出内容的开头，最多留 `head_len` 字节。
    head: Vec<u8>,
    head_len: usize,
    stopped: Stopped,
}

impl Inflater {
    fn new(flags: u32) -> Self {
        Self {
            state: Box::default(),
            window: vec![0; WINDOW],
            at: 0,
            flags,
            input_used: 0,
            output_len: 0,
            crc: None,
            head: Vec::new(),
            head_len: 0,
            stopped: Stopped::No,
        }
    }

    /// 不带外壳的 deflate 流（gzip 的成员里就是这种）。
    pub(crate) fn deflate() -> Self {
        Self::new(0)
    }

    /// zlib 流：2 字节的头、deflate 流、4 字节大端的 Adler-32（由 miniz_oxide 核对）。
    pub(crate) fn zlib() -> Self {
        Self::new(TINFL_FLAG_PARSE_ZLIB_HEADER)
    }

    /// 边解边算解出内容的 CRC-32（[`crc32`](Self::crc32)）。
    pub(crate) fn with_crc32(self) -> Self {
        Self {
            crc: Some(0),
            ..self
        }
    }

    /// 留下解出内容的前 `len` 字节（[`head`](Self::head)），按内容定扩展名时用。
    pub(crate) fn keep_head(self, len: usize) -> Self {
        Self {
            head: Vec::with_capacity(len),
            head_len: len,
            ..self
        }
    }

    /// 已经用掉的输入字节数（含 zlib 的头）。
    pub(crate) fn input_used(&self) -> u64 {
        self.input_used
    }

    /// 已经解出的字节数。
    pub(crate) fn output_len(&self) -> u64 {
        self.output_len
    }

    /// 已经解出的内容的 CRC-32；没要求算（[`with_crc32`](Self::with_crc32)）时为 0。
    pub(crate) fn crc32(&self) -> u32 {
        self.crc.unwrap_or(0)
    }

    pub(crate) fn head(&self) -> &[u8] {
        &self.head
    }

    /// 接着送入一段数据。流已经结束或对不上之后再送，结果不变，一个字节也不用。
    pub(crate) fn feed(&mut self, input: &[u8]) -> Inflated {
        match self.stopped {
            Stopped::End => return Inflated::End { used: 0 },
            Stopped::Broken => return Inflated::Broken { used: 0 },
            Stopped::No => {}
        }
        let mut used = 0usize;
        loop {
            let rest = input.get(used..).unwrap_or_default();
            let (status, consumed, written) = decompress(
                &mut self.state,
                rest,
                &mut self.window,
                self.at,
                self.flags | TINFL_FLAG_HAS_MORE_INPUT,
            );
            let end = self.at.saturating_add(written);
            let out = self.window.get(self.at..end).unwrap_or_default();
            self.output_len = self.output_len.saturating_add(out.len() as u64);
            if let Some(crc) = &mut self.crc {
                *crc = crc32_continue(*crc, &[out]);
            }
            let room = self.head_len.saturating_sub(self.head.len());
            self.head
                .extend_from_slice(out.get(..room.min(out.len())).unwrap_or_default());
            // 写到窗口末尾时 miniz_oxide 停下（HasMoreOutput），下一次从窗口开头接着写
            self.at = end & (WINDOW - 1);
            used = used.saturating_add(consumed);
            self.input_used = self.input_used.saturating_add(consumed as u64);
            match status {
                TINFLStatus::Done => {
                    self.stopped = Stopped::End;
                    return Inflated::End { used };
                }
                TINFLStatus::NeedsMoreInput => return Inflated::More,
                // 窗口写满了，用剩下的输入接着解；什么也没做却说还有输出，是解压器的异常，当作对不上
                TINFLStatus::HasMoreOutput if consumed > 0 || written > 0 => {}
                _ => {
                    self.stopped = Stopped::Broken;
                    return Inflated::Broken { used };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples::{self, Noise};
    use miniz_oxide::deflate::{compress_to_vec, compress_to_vec_zlib};
    use yisi_part::crc32::crc32;

    /// 按 `block` 字节一段送入，返回流在第几个字节结束。
    fn end_of(mut inflater: Inflater, data: &[u8], block: usize) -> (Inflater, Option<usize>) {
        let mut offset = 0;
        for piece in data.chunks(block) {
            match inflater.feed(piece) {
                Inflated::More => offset += piece.len(),
                Inflated::End { used } => return (inflater, Some(offset + used)),
                Inflated::Broken { .. } => return (inflater, None),
            }
        }
        (inflater, None)
    }

    #[test]
    fn stream_end_output_and_crc_are_found_in_any_block_size() {
        // 比窗口大得多：输出要绕环形窗口好几圈
        let content = samples::compressible(200_000, 3);
        for level in [0, 1, 6, 9] {
            let packed = compress_to_vec(&content, level);
            let mut data = packed.clone();
            data.extend_from_slice(b"trailing bytes");
            for block in [1, 7, 512, 4096, data.len()] {
                let (inflater, end) =
                    end_of(Inflater::deflate().with_crc32().keep_head(10), &data, block);
                assert_eq!(end, Some(packed.len()), "压缩级别 {level}、块 {block}");
                assert_eq!(inflater.output_len(), content.len() as u64);
                assert_eq!(inflater.crc32(), crc32(&[&content]));
                assert_eq!(inflater.head(), &content[..10]);
            }
        }
    }

    #[test]
    fn zlib_checks_its_adler32() {
        let content = samples::compressible(50_000, 4);
        let packed = compress_to_vec_zlib(&content, 6);
        let (inflater, end) = end_of(Inflater::zlib(), &packed, 333);
        assert_eq!(end, Some(packed.len()));
        assert_eq!(inflater.output_len(), content.len() as u64);

        let mut bad = packed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(end_of(Inflater::zlib(), &bad, 333).1, None);
    }

    #[test]
    fn cut_or_random_streams_never_end() {
        let packed = compress_to_vec(&samples::compressible(20_000, 5), 6);
        let cut = &packed[..packed.len() / 2];
        let (inflater, end) = end_of(Inflater::deflate(), cut, 100);
        assert_eq!(end, None);
        assert!(inflater.output_len() > 0);
        assert_eq!(inflater.stopped, Stopped::No);

        // 随机数据当作流解：走不到部分恢复的门槛
        let mut noise = Noise::new(11);
        for _ in 0..2000 {
            let data = noise.bytes(16 * 1024);
            let (inflater, _) = end_of(Inflater::deflate(), &data, 512);
            assert!(inflater.input_used() < INTACT_INPUT);
        }
    }
}
