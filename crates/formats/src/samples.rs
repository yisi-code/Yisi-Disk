//! 测试样本：按格式结构现场生成的最小合法文件，内容由固定种子的伪随机数填充，可以重放。
//! 只在测试里使用（本 crate 的测试，或其他 crate 打开 `test-samples` 特性后的测试）。

use yisi_part::crc32::crc32;

/// 固定种子的 xorshift64 伪随机数。
pub struct Noise(u64);

impl Noise {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }
}

/// PNG：1×1 灰度图，`idat_sizes` 给出每个 IDAT 块的数据长度。
pub fn png(idat_sizes: &[usize]) -> Vec<u8> {
    let mut noise = Noise::new(0x0050_4E47);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    png_chunk(&mut out, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
    for &size in idat_sizes {
        png_chunk(&mut out, b"IDAT", &noise.bytes(size));
    }
    png_chunk(&mut out, b"IEND", &[]);
    out
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

/// 基线 JPEG：APP0、带缩略图的 APP1、DQT、SOF0、DHT、SOS，`entropy_len` 字节熵编码数据（含填充与 RST 标记），EOI。
pub fn jpg(entropy_len: usize) -> Vec<u8> {
    let mut noise = Noise::new(0x004A_5047);
    let mut out = b"\xFF\xD8".to_vec();
    jpg_segment(&mut out, 0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    // 缩略图本身是一个完整 JPEG，含 SOI 与 EOI，随 APP1 段整段跳过
    let mut exif = b"Exif\0\0".to_vec();
    exif.extend_from_slice(b"\xFF\xD8\xFF\xDB\x00\x04ab\xFF\xDA\x00\x04cd\x12\x34\xFF\xD9");
    jpg_segment(&mut out, 0xE1, &exif);
    jpg_segment(&mut out, 0xDB, &noise.bytes(65));
    jpg_segment(&mut out, 0xC0, &[8, 0, 1, 0, 1, 1, 1, 0x11, 0]);
    jpg_segment(&mut out, 0xC4, &noise.bytes(40));
    jpg_segment(&mut out, 0xDA, &[1, 1, 0, 0, 63, 0]);
    jpg_entropy(&mut out, &mut noise, entropy_len);
    out.extend_from_slice(b"\xFF\xD9");
    out
}

/// 渐进式 JPEG：两次扫描之间插入 DHT 段。
pub fn jpg_progressive(entropy_len: usize) -> Vec<u8> {
    let mut noise = Noise::new(0x0050_524F);
    let mut out = b"\xFF\xD8".to_vec();
    jpg_segment(&mut out, 0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    jpg_segment(&mut out, 0xC2, &[8, 0, 1, 0, 1, 1, 1, 0x11, 0]);
    for _ in 0..2 {
        jpg_segment(&mut out, 0xC4, &noise.bytes(30));
        jpg_segment(&mut out, 0xDA, &[1, 1, 0, 0, 63, 0]);
        jpg_entropy(&mut out, &mut noise, entropy_len);
    }
    out.extend_from_slice(b"\xFF\xD9");
    out
}

fn jpg_segment(out: &mut Vec<u8>, marker: u8, data: &[u8]) {
    out.extend_from_slice(&[0xFF, marker]);
    out.extend_from_slice(&((data.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(data);
}

/// 熵编码数据：每个 0xFF 后补 0x00，每隔一段插一个 RST 标记，偶尔在标记前加填充 0xFF。
fn jpg_entropy(out: &mut Vec<u8>, noise: &mut Noise, len: usize) {
    for (index, byte) in noise.bytes(len).into_iter().enumerate() {
        out.push(byte);
        if byte == 0xFF {
            out.push(0x00);
        }
        if index % 200 == 199 {
            out.extend_from_slice(&[0xFF, 0xD0 + (index / 200 % 8) as u8]);
        }
    }
    out.push(0xFF); // 填充字节，紧接其后的是下一个标记
}

/// PDF：`objects` 个对象，`incremental` 时再追加一次增量更新（第二个 `%%EOF`）。结尾带换行。
pub fn pdf(objects: usize, incremental: bool) -> Vec<u8> {
    let mut noise = Noise::new(0x0050_4446);
    let mut out = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    for number in 1..=objects {
        let content = noise.bytes(64);
        out.extend_from_slice(format!("{number} 0 obj\n<< /Length 64 >>\nstream\n").as_bytes());
        out.extend_from_slice(&content);
        out.extend_from_slice(b"\nendstream\nendobj\n");
    }
    out.extend_from_slice(
        b"xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 1 >>\nstartxref\n9\n%%EOF\n",
    );
    if incremental {
        out.extend_from_slice(b"9 0 obj\n<< /Type /Annot >>\nendobj\nxref\n0 1\n0000000000 65535 f \ntrailer\n<< /Prev 9 >>\nstartxref\n42\n%%EOF\r\n");
    }
    out
}

/// ZIP：每项是（文件名、数据长度、是否用数据描述符），存储方式（不压缩），带中央目录与注释。
/// 数据里不出现字母 P，免得被当成记录签名。
pub fn zip(entries: &[(&str, usize, bool)], comment: &[u8]) -> Vec<u8> {
    let mut noise = Noise::new(0x005A_4950);
    let mut out = Vec::new();
    let mut central = Vec::new();
    for &(name, size, descriptor) in entries {
        let offset = out.len() as u32;
        let data: Vec<u8> = noise
            .bytes(size)
            .into_iter()
            .map(|byte| if byte == b'P' { b'Q' } else { byte })
            .collect();
        let crc = crc32(&[&data]);
        let flags: u16 = if descriptor { 0x08 } else { 0 };
        let (header_crc, header_size) = if descriptor {
            (0, 0)
        } else {
            (crc, data.len() as u32)
        };
        put32(&mut out, 0x0403_4B50);
        put16(&mut out, 20);
        put16(&mut out, flags);
        put16(&mut out, 0);
        put32(&mut out, 0);
        put32(&mut out, header_crc);
        put32(&mut out, header_size);
        put32(&mut out, header_size);
        put16(&mut out, name.len() as u16);
        put16(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&data);
        if descriptor {
            put32(&mut out, 0x0807_4B50);
            put32(&mut out, crc);
            put32(&mut out, data.len() as u32);
            put32(&mut out, data.len() as u32);
        }

        put32(&mut central, 0x0201_4B50);
        put16(&mut central, 20);
        put16(&mut central, 20);
        put16(&mut central, flags);
        put16(&mut central, 0);
        put32(&mut central, 0);
        put32(&mut central, crc);
        put32(&mut central, data.len() as u32);
        put32(&mut central, data.len() as u32);
        put16(&mut central, name.len() as u16);
        put16(&mut central, 0);
        put16(&mut central, 0);
        put16(&mut central, 0);
        put16(&mut central, 0);
        put32(&mut central, 0);
        put32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    put32(&mut out, 0x0605_4B50);
    put16(&mut out, 0);
    put16(&mut out, 0);
    put16(&mut out, entries.len() as u16);
    put16(&mut out, entries.len() as u16);
    put32(&mut out, central.len() as u32);
    put32(&mut out, central_offset);
    put16(&mut out, comment.len() as u16);
    out.extend_from_slice(comment);
    out
}

fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// MP4：ftyp、free、moov、mdat（`mdat_size` 字节数据）；`large_mdat` 时 mdat 用 64 位长度。
pub fn mp4(mdat_size: usize, large_mdat: bool) -> Vec<u8> {
    let mut noise = Noise::new(0x004D_5034);
    let mut out = Vec::new();
    mp4_box(&mut out, b"ftyp", b"isom\x00\x00\x02\x00isommp41");
    mp4_box(&mut out, b"free", &[0; 8]);
    mp4_box(&mut out, b"moov", &noise.bytes(100));
    let payload = noise.bytes(mdat_size);
    if large_mdat {
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(b"mdat");
        out.extend_from_slice(&(16 + payload.len() as u64).to_be_bytes());
        out.extend_from_slice(&payload);
    } else {
        mp4_box(&mut out, b"mdat", &payload);
    }
    out
}

/// 分片 MP4：ftyp、moov，`sidx_version` 给出时加一个索引全部分片的 sidx（版本 0 或 1），
/// `gap` 大于 0 时在 sidx 与第一个分片之间插一个这么长的 free（sidx 的 first_offset 跳过它，至少 8），
/// 然后每个 `mdat_sizes` 一组 moof + mdat，`mfra` 时结尾再加一个不在索引范围内的 mfra。
pub fn mp4_fragmented(
    mdat_sizes: &[usize],
    sidx_version: Option<u8>,
    gap: usize,
    mfra: bool,
) -> Vec<u8> {
    let mut noise = Noise::new(0x0046_4D50);
    let mut out = Vec::new();
    mp4_box(&mut out, b"ftyp", b"iso6\x00\x00\x00\x00iso6dash");
    mp4_box(&mut out, b"moov", &noise.bytes(100));
    let mut fragments = Vec::new();
    for &size in mdat_sizes {
        let mut fragment = Vec::new();
        mp4_box(&mut fragment, b"moof", &noise.bytes(40));
        mp4_box(&mut fragment, b"mdat", &noise.bytes(size));
        fragments.push(fragment);
    }
    if let Some(version) = sidx_version {
        let mut sidx = vec![version, 0, 0, 0];
        sidx.extend_from_slice(&1u32.to_be_bytes()); // reference_ID
        sidx.extend_from_slice(&1000u32.to_be_bytes()); // timescale
        if version == 0 {
            sidx.extend_from_slice(&0u32.to_be_bytes());
            sidx.extend_from_slice(&(gap as u32).to_be_bytes());
        } else {
            sidx.extend_from_slice(&0u64.to_be_bytes());
            sidx.extend_from_slice(&(gap as u64).to_be_bytes());
        }
        sidx.extend_from_slice(&0u16.to_be_bytes());
        sidx.extend_from_slice(&(fragments.len() as u16).to_be_bytes());
        for fragment in &fragments {
            sidx.extend_from_slice(&(fragment.len() as u32).to_be_bytes()); // reference_type 0：媒体
            sidx.extend_from_slice(&1000u32.to_be_bytes()); // subsegment_duration
            sidx.extend_from_slice(&0x9000_0000u32.to_be_bytes()); // 以 1 型 SAP 开头
        }
        mp4_box(&mut out, b"sidx", &sidx);
    }
    if gap > 0 {
        mp4_box(&mut out, b"free", &vec![0; gap.saturating_sub(8)]);
    }
    for fragment in &fragments {
        out.extend_from_slice(fragment);
    }
    if mfra {
        mp4_box(&mut out, b"mfra", &noise.bytes(16));
    }
    out
}

fn mp4_box(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&(8 + payload.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
}
