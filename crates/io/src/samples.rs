//! 测试样本：按 EWF（E01）的结构把一段数据写成分段的 E01 文件（文件头、header、volume / data、sectors、table、
//! table2、hash、next / done 各节，带各处的 Adler-32），压缩块与未压缩块可以混着放。

use std::path::{Path, PathBuf};

use crate::adler32;
use crate::ewf::{EVF_SIGNATURE, segment_path};

/// 怎样切块、分段、存放。
#[derive(Debug, Clone, Copy)]
pub struct EwfLayout {
    pub sectors_per_chunk: u32,
    pub bytes_per_sector: u32,
    /// 每段最多放这么多块。
    pub chunks_per_segment: usize,
    /// 每张块表最多这么多项：一段里超过时分成几组 sectors / table / table2。
    pub chunks_per_table: usize,
    /// 第几块存成未压缩的（其余用 zlib 压缩）。
    pub stored: fn(u64) -> bool,
}

impl Default for EwfLayout {
    fn default() -> Self {
        Self {
            sectors_per_chunk: 64,
            bytes_per_sector: 512,
            chunks_per_segment: 1 << 20,
            chunks_per_table: 16375,
            stored: |index| index % 3 == 1,
        }
    }
}

/// 一块存在哪里（测试据此改坏某一块）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EwfChunk {
    /// 第几段（从 0 起）。
    pub segment: usize,
    /// 在段文件里的偏移。
    pub offset: u64,
    /// 存了多少字节（未压缩的含末尾 4 字节校验和）。
    pub length: u64,
    pub compressed: bool,
}

/// 拼好的各段文件内容。
#[derive(Debug, Clone)]
pub struct EwfSample {
    pub segments: Vec<Vec<u8>>,
    pub chunks: Vec<EwfChunk>,
}

impl EwfSample {
    /// 把各段写成 `first`（如 `x.E01`）与它同目录的后续段，返回各段的路径。
    pub fn write(&self, first: &Path) -> Vec<PathBuf> {
        self.segments
            .iter()
            .enumerate()
            .map(|(index, bytes)| {
                let path = if index == 0 {
                    first.to_path_buf()
                } else {
                    segment_path(first, index as u32 + 1)
                        .expect("第一段的扩展名应是 E01 这样的样式")
                };
                std::fs::write(&path, bytes).expect("写段文件");
                path
            })
            .collect()
    }
}

/// 把 `data`（长度是扇区的整数倍）拼成 E01 各段。
pub fn ewf_sample(data: &[u8], layout: &EwfLayout) -> EwfSample {
    let sector = layout.bytes_per_sector as usize;
    assert!(
        data.len().is_multiple_of(sector),
        "数据长度应是扇区的整数倍"
    );
    let chunk_size = layout.sectors_per_chunk as usize * sector;
    let chunks: Vec<&[u8]> = data.chunks(chunk_size).collect();
    let volume = volume_data(layout, chunks.len() as u32, (data.len() / sector) as u64);
    let groups: Vec<&[&[u8]]> = if chunks.is_empty() {
        vec![&[]]
    } else {
        chunks.chunks(layout.chunks_per_segment).collect()
    };
    let mut segments = Vec::new();
    let mut placed = Vec::new();
    let mut index = 0u64;
    for (segment, group) in groups.iter().enumerate() {
        let mut out = Vec::new();
        out.extend_from_slice(&EVF_SIGNATURE);
        out.push(1);
        out.extend_from_slice(&(segment as u16 + 1).to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        if segment == 0 {
            let header = b"1\r\nmain\r\nc\tn\ta\te\tt\tm\tu\tp\r\n\t\tsample\t\t\t\t\t\t\r\n\r\n";
            push_section(
                &mut out,
                "header",
                &miniz_oxide::deflate::compress_to_vec_zlib(header, 6),
            );
            push_section(&mut out, "volume", &volume);
        } else {
            push_section(&mut out, "data", &volume);
        }
        for table_chunks in group.chunks(layout.chunks_per_table.max(1)) {
            let sectors_start = out.len() as u64;
            let mut body = Vec::new();
            let mut offsets = Vec::new();
            for chunk in table_chunks {
                let offset = sectors_start + 76 + body.len() as u64;
                let compressed = !(layout.stored)(index);
                let stored = if compressed {
                    miniz_oxide::deflate::compress_to_vec_zlib(chunk, 6)
                } else {
                    let mut stored = chunk.to_vec();
                    stored.extend_from_slice(&adler32(chunk).to_le_bytes());
                    stored
                };
                placed.push(EwfChunk {
                    segment,
                    offset,
                    length: stored.len() as u64,
                    compressed,
                });
                offsets.push((offset, compressed));
                body.extend_from_slice(&stored);
                index += 1;
            }
            push_section(&mut out, "sectors", &body);
            // 块的偏移相对于 sectors 节的开头（EnCase 6 以后的写法）
            let mut table = vec![0u8; 24];
            table[..4].copy_from_slice(&(offsets.len() as u32).to_le_bytes());
            table[8..16].copy_from_slice(&sectors_start.to_le_bytes());
            let header_sum = adler32(&table[..20]);
            table[20..24].copy_from_slice(&header_sum.to_le_bytes());
            let mut entries = Vec::new();
            for (offset, compressed) in offsets {
                let relative = (offset - sectors_start) as u32;
                let flag = if compressed { 0x8000_0000 } else { 0 };
                entries.extend_from_slice(&(relative | flag).to_le_bytes());
            }
            table.extend_from_slice(&entries);
            table.extend_from_slice(&adler32(&entries).to_le_bytes());
            push_section(&mut out, "table", &table);
            push_section(&mut out, "table2", &table);
        }
        if segment + 1 == groups.len() {
            let mut hash = vec![0u8; 32];
            hash.extend_from_slice(&adler32(&[0u8; 32]).to_le_bytes());
            push_section(&mut out, "hash", &hash);
            push_terminal(&mut out, "done");
        } else {
            push_terminal(&mut out, "next");
        }
        segments.push(out);
    }
    EwfSample {
        segments,
        chunks: placed,
    }
}

/// EnCase 写法的 1052 字节 volume 节数据。
fn volume_data(layout: &EwfLayout, chunk_count: u32, sector_count: u64) -> Vec<u8> {
    let mut volume = vec![0u8; 1052];
    volume[0] = 1;
    volume[4..8].copy_from_slice(&chunk_count.to_le_bytes());
    volume[8..12].copy_from_slice(&layout.sectors_per_chunk.to_le_bytes());
    volume[12..16].copy_from_slice(&layout.bytes_per_sector.to_le_bytes());
    volume[16..24].copy_from_slice(&sector_count.to_le_bytes());
    volume[52] = 1;
    let sum = adler32(&volume[..1048]);
    volume[1048..].copy_from_slice(&sum.to_le_bytes());
    volume
}

fn descriptor(kind: &str, next: u64, size: u64) -> [u8; 76] {
    let mut descriptor = [0u8; 76];
    descriptor[..kind.len()].copy_from_slice(kind.as_bytes());
    descriptor[16..24].copy_from_slice(&next.to_le_bytes());
    descriptor[24..32].copy_from_slice(&size.to_le_bytes());
    let sum = adler32(&descriptor[..72]);
    descriptor[72..].copy_from_slice(&sum.to_le_bytes());
    descriptor
}

fn push_section(out: &mut Vec<u8>, kind: &str, data: &[u8]) {
    let start = out.len() as u64;
    let size = 76 + data.len() as u64;
    out.extend_from_slice(&descriptor(kind, start + size, size));
    out.extend_from_slice(data);
}

/// next / done：下一节的偏移指向自己，长度记 0（EnCase 的写法）。
fn push_terminal(out: &mut Vec<u8>, kind: &str) {
    let start = out.len() as u64;
    out.extend_from_slice(&descriptor(kind, start, 0));
}
