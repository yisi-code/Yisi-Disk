//! 测试样本：按格式结构现场生成的最小合法文件，内容由固定种子的伪随机数填充，可以重放。
//! 只在测试里使用（本 crate 的测试，或其他 crate 打开 `test-samples` 特性后的测试），越界时直接 panic。
#![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]

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
    let entries: Vec<(&str, Vec<u8>, bool)> = entries
        .iter()
        .map(|&(name, size, descriptor)| {
            let data = noise
                .bytes(size)
                .into_iter()
                .map(|byte| if byte == b'P' { b'Q' } else { byte })
                .collect();
            (name, data, descriptor)
        })
        .collect();
    zip_with(&entries, comment)
}

/// 同 [`zip`]，每项的内容由调用方给出（例如 ODF 的 `mimetype`）。
pub fn zip_with(entries: &[(&str, Vec<u8>, bool)], comment: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, descriptor) in entries {
        let (name, descriptor) = (*name, *descriptor);
        let offset = out.len() as u32;
        let crc = crc32(&[data]);
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
        out.extend_from_slice(data);
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

/// BMP：Windows 3 版信息头（40 字节），不压缩；`height` 为负表示从上往下存。8 位及以下带调色板。
pub fn bmp(width: i32, height: i32, bits: u16) -> Vec<u8> {
    let mut noise = Noise::new(0x0042_4D50);
    let palette = if bits <= 8 { 4usize << bits } else { 0 };
    let row = (width as usize * usize::from(bits)).div_ceil(32) * 4;
    let pixels = row * height.unsigned_abs() as usize;
    let pixels_at = 14 + 40 + palette;
    let size = pixels_at + pixels;
    let mut out = b"BM".to_vec();
    put32(&mut out, size as u32);
    put32(&mut out, 0);
    put32(&mut out, pixels_at as u32);
    put32(&mut out, 40);
    put32(&mut out, width as u32);
    put32(&mut out, height as u32);
    put16(&mut out, 1);
    put16(&mut out, bits);
    put32(&mut out, 0);
    put32(&mut out, pixels as u32);
    put32(&mut out, 2835);
    put32(&mut out, 2835);
    put32(&mut out, 0);
    put32(&mut out, 0);
    out.extend(noise.bytes(palette + pixels));
    out
}

/// RIFF：`form` 是 AVI 、WAVE、WEBP 之一，`chunks` 是（类型、数据长度），奇数长度后面补一个字节。
pub fn riff(form: &[u8; 4], chunks: &[(&[u8; 4], usize)]) -> Vec<u8> {
    let mut noise = Noise::new(0x0052_4946);
    let mut body = form.to_vec();
    for &(kind, len) in chunks {
        body.extend_from_slice(kind);
        put32(&mut body, len as u32);
        body.extend(noise.bytes(len));
        if len % 2 == 1 {
            body.push(0);
        }
    }
    let mut out = b"RIFF".to_vec();
    put32(&mut out, body.len() as u32);
    out.extend(body);
    out
}

/// 7z：签名头之后是 `packed` 字节的「压缩数据」与 `header` 字节的下一头（内容随机，CRC 照实算）。
pub fn sevenzip(packed: usize, header: usize) -> Vec<u8> {
    let mut noise = Noise::new(0x0037_5A00);
    let packed_data = noise.bytes(packed);
    let next = noise.bytes(header);
    let mut start_header = Vec::new();
    start_header.extend_from_slice(&(packed as u64).to_le_bytes());
    start_header.extend_from_slice(&(header as u64).to_le_bytes());
    put32(&mut start_header, crc32(&[&next]));
    let mut out = b"7z\xBC\xAF\x27\x1C\x00\x04".to_vec();
    put32(&mut out, crc32(&[&start_header]));
    out.extend(start_header);
    out.extend(packed_data);
    out.extend(next);
    out
}

/// SQLite：`pages` 页、每页 `page_size` 字节，库头写明页数且可信，其余内容随机。
pub fn sqlite(page_size: u32, pages: u32) -> Vec<u8> {
    let mut noise = Noise::new(0x0053_514C);
    let mut out = noise.bytes(page_size as usize * pages as usize);
    out[..16].copy_from_slice(b"SQLite format 3\0");
    let encoded = if page_size == 65536 {
        1
    } else {
        page_size as u16
    };
    out[16..18].copy_from_slice(&encoded.to_be_bytes());
    out[18..24].copy_from_slice(&[1, 1, 0, 64, 32, 32]);
    out[24..28].copy_from_slice(&7u32.to_be_bytes());
    out[28..32].copy_from_slice(&pages.to_be_bytes());
    out[92..96].copy_from_slice(&7u32.to_be_bytes());
    out
}

/// 复合文档：版本 3（512 字节扇区）或 4（4096 字节扇区），一共 `sectors` 个扇区。第 0 个扇区是目录
/// （根项、名为 `stream` 的一项；`msi` 时根项带 MSI 的 CLSID），之后是数据，最后是 DIFAT 扇区与 FAT 扇区
/// （很多程序把 FAT 写在文件末尾）。FAT 扇区超过 109 个时用 DIFAT 扇区。
pub fn ole(version: u16, sectors: u32, stream: &str, msi: bool) -> Vec<u8> {
    const END_OF_CHAIN: u32 = 0xFFFF_FFFE;
    const FAT_SECTOR: u32 = 0xFFFF_FFFD;
    const DIFAT_SECTOR: u32 = 0xFFFF_FFFC;
    let mut noise = Noise::new(0x004F_4C45);
    let shift: u16 = if version == 4 { 12 } else { 9 };
    let size = 1usize << shift;
    let per_sector = (size / 4) as u32;
    // FAT 扇区数与 DIFAT 扇区数互相影响，算到稳定
    let (mut fat, mut difat) = (1u32, 0u32);
    loop {
        let needed_fat = sectors.div_ceil(per_sector);
        let needed_difat = needed_fat.saturating_sub(109).div_ceil(per_sector - 1);
        if (needed_fat, needed_difat) == (fat, difat) {
            break;
        }
        (fat, difat) = (needed_fat, needed_difat);
    }
    let first_fat = sectors - fat;
    let first_difat = first_fat - difat;
    let mut table = vec![0xFFFF_FFFFu32; (fat * per_sector) as usize];
    table[0] = END_OF_CHAIN;
    for sector in 1..first_difat {
        table[sector as usize] = if sector + 1 == first_difat {
            END_OF_CHAIN
        } else {
            sector + 1
        };
    }
    for sector in first_difat..first_fat {
        table[sector as usize] = DIFAT_SECTOR;
    }
    for sector in first_fat..sectors {
        table[sector as usize] = FAT_SECTOR;
    }

    let mut header = vec![0u8; size];
    header[..8].copy_from_slice(b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1");
    header[24..26].copy_from_slice(&0x3Eu16.to_le_bytes());
    header[26..28].copy_from_slice(&version.to_le_bytes());
    header[28..30].copy_from_slice(&0xFFFEu16.to_le_bytes());
    header[30..32].copy_from_slice(&shift.to_le_bytes());
    header[32..34].copy_from_slice(&6u16.to_le_bytes());
    header[40..44].copy_from_slice(&u32::from(version == 4).to_le_bytes());
    header[44..48].copy_from_slice(&fat.to_le_bytes());
    header[48..52].copy_from_slice(&0u32.to_le_bytes());
    header[56..60].copy_from_slice(&4096u32.to_le_bytes());
    header[60..64].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
    let difat_start = if difat > 0 { first_difat } else { END_OF_CHAIN };
    header[68..72].copy_from_slice(&difat_start.to_le_bytes());
    header[72..76].copy_from_slice(&difat.to_le_bytes());
    for index in 0..109u32 {
        let value = if index < fat {
            first_fat + index
        } else {
            0xFFFF_FFFF
        };
        let at = 76 + index as usize * 4;
        header[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    let mut directory = vec![0u8; size];
    let mut name_entry = |at: usize, name: &str| {
        let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        directory[at..at + units.len()].copy_from_slice(&units);
        directory[at + 64..at + 66].copy_from_slice(&((units.len() + 2) as u16).to_le_bytes());
    };
    name_entry(0, "Root Entry");
    name_entry(128, stream);
    if msi {
        directory[80..96].copy_from_slice(&[
            0x84, 0x10, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x46,
        ]);
    }

    let mut out = header;
    out.extend(directory);
    out.extend(noise.bytes(size * (first_difat - 1) as usize));
    for index in 0..difat {
        let mut sector = vec![0xFFu8; size];
        for slot in 0..per_sector - 1 {
            let fat_index = 109 + index * (per_sector - 1) + slot;
            if fat_index < fat {
                let at = slot as usize * 4;
                sector[at..at + 4].copy_from_slice(&(first_fat + fat_index).to_le_bytes());
            }
        }
        let next = if index + 1 < difat {
            first_difat + index + 1
        } else {
            END_OF_CHAIN
        };
        sector[size - 4..].copy_from_slice(&next.to_le_bytes());
        out.extend(sector);
    }
    for value in table {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

/// RAR 5 的变长整数。
fn rar_vint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

/// RAR 5 的一块：头 CRC32、头长度、头（类型、标志与其余字段），再接 `data`。
fn rar5_block(fields: &[u8], data: &[u8]) -> Vec<u8> {
    let mut covered = rar_vint(fields.len() as u64);
    covered.extend_from_slice(fields);
    let mut out = crc32(&[&covered]).to_le_bytes().to_vec();
    out.extend(covered);
    out.extend_from_slice(data);
    out
}

/// RAR 5：主头、每个 `sizes` 一个文件块（第一个文件名长 150 字节，头长度要两个字节），结束块。
pub fn rar5(sizes: &[usize]) -> Vec<u8> {
    let mut noise = Noise::new(0x0052_4135);
    let mut out = b"Rar!\x1A\x07\x01\x00".to_vec();
    out.extend(rar5_block(&[1, 0, 0], &[]));
    for (index, &size) in sizes.iter().enumerate() {
        // 类型 2、标志「有数据」、数据长度、文件标志、原始大小、属性、文件名
        let mut fields = vec![2, 0x02];
        fields.extend(rar_vint(size as u64));
        fields.extend_from_slice(&[0, 0, 0x20]);
        let name_len = if index == 0 { 150 } else { 5 };
        fields.extend(rar_vint(name_len));
        fields.extend(std::iter::repeat_n(b'a', name_len as usize));
        out.extend(rar5_block(&fields, &noise.bytes(size)));
    }
    out.extend(rar5_block(&[5, 0, 0], &[]));
    out
}

/// RAR 4 的一块：头 CRC（CRC32 的低 16 位，从类型算到头末尾）、类型、标志、头长度，其余字段，再接 `data`。
fn rar4_block(kind: u8, flags: u16, fields: &[u8], data: &[u8]) -> Vec<u8> {
    let mut covered = vec![kind];
    covered.extend_from_slice(&flags.to_le_bytes());
    covered.extend_from_slice(&((7 + fields.len()) as u16).to_le_bytes());
    covered.extend_from_slice(fields);
    let mut out = ((crc32(&[&covered]) & 0xFFFF) as u16)
        .to_le_bytes()
        .to_vec();
    out.extend(covered);
    out.extend_from_slice(data);
    out
}

/// RAR 1.5–4.x：标记块、归档头、每个 `sizes` 一个文件块，`end_block` 时有结束块。
pub fn rar4(sizes: &[usize], end_block: bool) -> Vec<u8> {
    let mut noise = Noise::new(0x0052_4134);
    let mut out = b"Rar!\x1A\x07\x00".to_vec();
    out.extend(rar4_block(0x73, 0, &[0; 6], &[]));
    for &size in sizes {
        let mut fields = Vec::new();
        put32(&mut fields, size as u32); // 压缩后大小
        put32(&mut fields, size as u32); // 原始大小
        fields.push(2); // 操作系统
        put32(&mut fields, 0); // 文件 CRC
        put32(&mut fields, 0); // 时间
        fields.extend_from_slice(&[29, 0x30]); // 解压版本、方式
        put16(&mut fields, 5); // 文件名长度
        put32(&mut fields, 0x20); // 属性
        fields.extend_from_slice(b"a.txt");
        out.extend(rar4_block(0x74, 0x8000, &fields, &noise.bytes(size)));
    }
    if end_block {
        out.extend(rar4_block(0x7B, 0x4000, &[], &[]));
    }
    out
}

/// Matroska / WebM：EBML 头（DocType 是 `doc_type`），Segment 装 `data_len` 字节；`unknown_size` 时
/// Segment 的长度写成「未知」。
pub fn mkv(doc_type: &str, data_len: usize, unknown_size: bool) -> Vec<u8> {
    let mut noise = Noise::new(0x004D_4B56);
    let mut body = vec![
        0x42, 0x86, 0x81, 1, 0x42, 0xF7, 0x81, 1, 0x42, 0xF2, 0x81, 4, 0x42, 0xF3, 0x81, 8,
    ];
    body.extend_from_slice(&[0x42, 0x82, 0x80 | doc_type.len() as u8]);
    body.extend_from_slice(doc_type.as_bytes());
    body.extend_from_slice(&[0x42, 0x87, 0x81, 4, 0x42, 0x85, 0x81, 2]);
    let mut out = vec![0x1A, 0x45, 0xDF, 0xA3, 0x80 | body.len() as u8];
    out.extend(body);
    out.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]);
    if unknown_size {
        out.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    } else {
        out.push(0x01);
        out.extend_from_slice(&(data_len as u64).to_be_bytes()[1..]);
    }
    out.extend(noise.bytes(data_len));
    out
}

/// MP3：ID3v2.3 标签（20 字节内容），标签之后 `padding` 个 0，`frames` 帧（`version` 3 是 MPEG-1、2 是 MPEG-2，
/// `layer` 1 是层 3、2 是层 2；128 或 64 kbps，第 3 帧带填充位），`id3v1` 时结尾有 128 字节的 ID3v1 标签。
pub fn mp3(version: u8, layer: u8, frames: usize, id3v1: bool, padding: usize) -> Vec<u8> {
    let mut noise = Noise::new(0x004D_5033);
    let mut out = b"ID3\x03\x00\x00\x00\x00\x00\x14".to_vec();
    out.extend(noise.bytes(20));
    out.extend(std::iter::repeat_n(0, padding));
    let b1 = 0xE0 | (version << 3) | (layer << 1) | 1;
    let bitrate_index: u8 = if version == 3 { 9 } else { 8 };
    for index in 0..frames {
        let padded = u8::from(index == 2);
        let header = [0xFF, b1, (bitrate_index << 4) | (padded << 1), 0x00];
        let (_, length) = crate::mp3::frame(&header).expect("样本的帧头合法");
        out.extend_from_slice(&header);
        out.extend(noise.bytes(length as usize - 4));
    }
    if id3v1 {
        out.extend_from_slice(b"TAG");
        out.extend(noise.bytes(125));
    }
    out
}

/// TIFF 样本的布局。
#[derive(Clone)]
pub struct TiffLayout {
    /// 大端（`MM`）；否则按 `magic`。
    pub big: bool,
    /// 小端时的文件头前 4 字节（`II*\0`，或 ORF 的 `IIRO`、RW2 的 `IIU\0`）。
    pub magic: [u8; 4],
    /// IFD 写在文件末尾（数据与数组都在它前面）；否则 IFD0 紧跟文件头。
    pub ifd_last: bool,
    /// IFD0 的厂商名。
    pub make: Option<&'static [u8]>,
    /// IFD0 带 DNGVersion。
    pub dng: bool,
    /// 有 IFD1，带一段 JPEG 预览（标签 513 / 514）。
    pub preview: bool,
    /// IFD0 带一个子 IFD，里面一个条带。
    pub sub_ifd: bool,
    /// IFD0 的条带数；0 表示没有图像数据。
    pub strips: usize,
}

impl Default for TiffLayout {
    fn default() -> Self {
        Self {
            big: false,
            magic: *b"II*\0",
            ifd_last: false,
            make: None,
            dng: false,
            preview: false,
            sub_ifd: false,
            strips: 3,
        }
    }
}

/// TIFF：按 `layout` 排的 IFD0（宽、高、厂商名、条带偏移与长度、子 IFD、DNGVersion）、子 IFD、IFD1 与各段数据。
/// 文件到最后一部分为止。
pub fn tiff(layout: &TiffLayout) -> Vec<u8> {
    let mut noise = Noise::new(0x0054_4946);
    let big = layout.big;
    let u16b = |value: u16| {
        if big {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    };
    let u32b = |value: u32| {
        if big {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    };
    let strips: Vec<Vec<u8>> = (0..layout.strips)
        .map(|index| noise.bytes(100 + 10 * index))
        .collect();
    let sub_strip = noise.bytes(300);
    let preview = noise.bytes(500);
    let make = layout.make.map(|make| [make, b"\0"].concat());
    let array_len = if layout.strips > 1 {
        4 * layout.strips
    } else {
        0
    };
    let make_len = make
        .as_ref()
        .map_or(0, |make| if make.len() > 4 { make.len() } else { 0 });
    let ifd0_entries = 2
        + usize::from(make.is_some())
        + if layout.strips > 0 { 2 } else { 0 }
        + usize::from(layout.sub_ifd)
        + usize::from(layout.dng);
    let ifd_len = |entries: usize| 2 + 12 * entries + 4;

    // 各部分的长度与顺序，先排出位置
    let mut parts: Vec<(&str, usize)> = Vec::new();
    let strip_parts: Vec<(&str, usize)> =
        strips.iter().map(|strip| ("strip", strip.len())).collect();
    let tail_parts = [
        ("sub_ifd", if layout.sub_ifd { ifd_len(4) } else { 0 }),
        (
            "sub_strip",
            if layout.sub_ifd { sub_strip.len() } else { 0 },
        ),
        ("ifd1", if layout.preview { ifd_len(2) } else { 0 }),
        ("preview", if layout.preview { preview.len() } else { 0 }),
    ];
    let extras = [
        ("offsets", array_len),
        ("counts", array_len),
        ("make", make_len),
    ];
    if layout.ifd_last {
        parts.extend(strip_parts);
        parts.extend(tail_parts);
        parts.extend(extras);
        parts.push(("ifd0", ifd_len(ifd0_entries)));
    } else {
        parts.push(("ifd0", ifd_len(ifd0_entries)));
        parts.extend(extras);
        parts.extend(strip_parts);
        parts.extend(tail_parts);
    }
    let mut at = 8usize;
    let mut positions: Vec<(&str, usize)> = Vec::new();
    for &(name, len) in &parts {
        positions.push((name, at));
        at += len;
    }
    let total = at;
    let place = |name: &str| {
        positions
            .iter()
            .find(|(part, _)| *part == name)
            .map_or(0, |&(_, at)| at as u32)
    };
    let strip_positions: Vec<u32> = positions
        .iter()
        .filter(|(name, _)| *name == "strip")
        .map(|&(_, at)| at as u32)
        .collect();

    let entry = |tag: u16, kind: u16, count: u32, value: [u8; 4]| {
        [u16b(tag).as_slice(), &u16b(kind), &u32b(count), &value].concat()
    };
    let short = |value: u16| {
        let mut bytes = [0u8; 4];
        bytes[..2].copy_from_slice(&u16b(value));
        bytes
    };
    let ifd = |entries: Vec<Vec<u8>>, next: u32| {
        let mut out = u16b(entries.len() as u16).to_vec();
        for entry in entries {
            out.extend(entry);
        }
        out.extend(u32b(next));
        out
    };

    let mut ifd0 = vec![entry(256, 3, 1, short(16)), entry(257, 3, 1, short(16))];
    if let Some(make) = &make {
        let value = if make.len() > 4 {
            u32b(place("make"))
        } else {
            let mut bytes = [0u8; 4];
            bytes[..make.len()].copy_from_slice(make);
            bytes
        };
        ifd0.push(entry(271, 2, make.len() as u32, value));
    }
    let counts: Vec<u32> = strips.iter().map(|strip| strip.len() as u32).collect();
    if layout.strips == 1 {
        ifd0.push(entry(273, 4, 1, u32b(strip_positions[0])));
        ifd0.push(entry(279, 4, 1, u32b(counts[0])));
    } else if layout.strips > 1 {
        ifd0.push(entry(273, 4, layout.strips as u32, u32b(place("offsets"))));
        ifd0.push(entry(279, 4, layout.strips as u32, u32b(place("counts"))));
    }
    if layout.sub_ifd {
        ifd0.push(entry(330, 4, 1, u32b(place("sub_ifd"))));
    }
    if layout.dng {
        ifd0.push(entry(50706, 1, 4, [1, 4, 0, 0]));
    }
    let next0 = if layout.preview { place("ifd1") } else { 0 };

    let mut out = vec![0u8; total];
    let magic = if big { *b"MM\0*" } else { layout.magic };
    out[..4].copy_from_slice(&magic);
    out[4..8].copy_from_slice(&u32b(place("ifd0")));
    let mut put = |at: u32, bytes: &[u8]| {
        out[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
    };
    put(place("ifd0"), &ifd(ifd0, next0));
    if array_len > 0 {
        let offsets: Vec<u8> = strip_positions.iter().flat_map(|&at| u32b(at)).collect();
        let lengths: Vec<u8> = counts.iter().flat_map(|&count| u32b(count)).collect();
        put(place("offsets"), &offsets);
        put(place("counts"), &lengths);
    }
    if let Some(make) = make.as_ref().filter(|make| make.len() > 4) {
        put(place("make"), make);
    }
    for (strip, &at) in strips.iter().zip(&strip_positions) {
        put(at, strip);
    }
    if layout.sub_ifd {
        let sub = vec![
            entry(256, 3, 1, short(8)),
            entry(257, 3, 1, short(8)),
            entry(273, 4, 1, u32b(place("sub_strip"))),
            entry(279, 4, 1, u32b(sub_strip.len() as u32)),
        ];
        put(place("sub_ifd"), &ifd(sub, 0));
        put(place("sub_strip"), &sub_strip);
    }
    if layout.preview {
        let ifd1 = vec![
            entry(513, 4, 1, u32b(place("preview"))),
            entry(514, 4, 1, u32b(preview.len() as u32)),
        ];
        put(place("ifd1"), &ifd(ifd1, 0));
        put(place("preview"), &preview);
    }
    out
}

/// GIF89a：16×16，`global_table` 时带 4 色全局颜色表；一个循环播放的应用扩展，之后 `frames` 帧
/// （每帧前一个图形控制扩展，奇数帧带局部颜色表），每帧的数据分成 255、255、37 字节三个子块。
pub fn gif(frames: usize, global_table: bool) -> Vec<u8> {
    let mut noise = Noise::new(0x0047_4946);
    let mut out = b"GIF89a".to_vec();
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(&[if global_table { 0x81 } else { 0 }, 0, 0]);
    if global_table {
        out.extend(noise.bytes(12));
    }
    out.extend_from_slice(&[0x21, 0xFF, 11]);
    out.extend_from_slice(b"NETSCAPE2.0");
    out.extend_from_slice(&[3, 1, 0, 0, 0]);
    for frame in 0..frames {
        out.extend_from_slice(&[0x21, 0xF9, 4, 0, 10, 0, 0, 0]);
        let local = frame % 2 == 1;
        out.push(0x2C);
        out.extend_from_slice(&[0, 0, 0, 0, 16, 0, 16, 0, if local { 0x81 } else { 0 }]);
        if local {
            out.extend(noise.bytes(12));
        }
        out.push(2);
        for size in [255u8, 255, 37] {
            out.push(size);
            out.extend(noise.bytes(usize::from(size)));
        }
        out.push(0);
    }
    out.push(0x3B);
    out
}

/// ISO 媒体文件：主品牌 `brand` 的 ftyp，之后依次是 `boxes` 列出的顶层 box，每个装 `body` 字节的随机数据。
/// HEIC 照片是 `[meta, mdat]`，CR3 是 `[moov, uuid, mdat]`。
pub fn iso_media(brand: &[u8; 4], boxes: &[(&[u8; 4], usize)]) -> Vec<u8> {
    let mut noise = Noise::new(0x0049_534F);
    let mut out = Vec::new();
    let mut ftyp = brand.to_vec();
    ftyp.extend_from_slice(&[0, 0, 0, 0]);
    ftyp.extend_from_slice(brand);
    mp4_box(&mut out, b"ftyp", &ftyp);
    for &(kind, body) in boxes {
        mp4_box(&mut out, kind, &noise.bytes(body));
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

fn put_be16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_be32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn be32_at(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
}

/// Ogg 页的 CRC（多项式 0x04C11DB7、不反射、初值 0，CRC 字段按 0 算），逐位算，与插件里查表的算法互相印证。
pub fn ogg_crc(page: &[u8]) -> u32 {
    let mut crc = 0u32;
    for (index, &byte) in page.iter().enumerate() {
        let byte = if (22..26).contains(&index) { 0 } else { byte };
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn ogg_page(out: &mut Vec<u8>, flags: u8, serial: u32, sequence: u32, body: &[u8]) {
    let mut lacing = Vec::new();
    let mut left = body.len();
    loop {
        if left >= 255 {
            lacing.push(255);
            left -= 255;
        } else {
            lacing.push(left as u8);
            break;
        }
    }
    let mut page = b"OggS\0".to_vec();
    page.push(flags);
    page.extend_from_slice(&(u64::from(sequence) * 1000).to_le_bytes()); // 粒度位置
    put32(&mut page, serial);
    put32(&mut page, sequence);
    put32(&mut page, 0);
    page.push(lacing.len() as u8);
    page.extend_from_slice(&lacing);
    page.extend_from_slice(body);
    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    out.extend(page);
}

/// Ogg：`links` 段首尾相接；每段里 `streams` 是各逻辑流第一个包的开头，先是全部 BOS 页，
/// 之后各流轮流 `pages` 个数据页，每个流的最后一页带 EOS 标志。
pub fn ogg(streams: &[&[u8]], pages: usize, links: usize) -> Vec<u8> {
    let mut noise = Noise::new(0x004F_4747);
    let mut out = Vec::new();
    for link in 0..links {
        let serial = |index: usize| 0x1000 * (link as u32 + 1) + index as u32;
        for (index, packet) in streams.iter().enumerate() {
            let mut body = packet.to_vec();
            body.extend(noise.bytes(20));
            ogg_page(&mut out, 0x02, serial(index), 0, &body);
        }
        for page in 1..=pages {
            for index in 0..streams.len() {
                let flags = if page == pages { 0x04 } else { 0 };
                let len = 50 + (noise.next_u64() % 600) as usize;
                let body = noise.bytes(len);
                ogg_page(&mut out, flags, serial(index), page as u32, &body);
            }
        }
    }
    out
}

/// IFF 的 `FORM`（AIFF、DjVu）：`prefix` 之后是 `FORM`、大端长度、`form`，`chunks` 是（类型、数据长度），奇数补一个字节。
pub fn iff(prefix: &[u8], form: &[u8; 4], chunks: &[(&[u8; 4], usize)]) -> Vec<u8> {
    let mut noise = Noise::new(0x0049_4646);
    let mut body = form.to_vec();
    for &(kind, len) in chunks {
        body.extend_from_slice(kind);
        put_be32(&mut body, len as u32);
        body.extend(noise.bytes(len));
        if len % 2 == 1 {
            body.push(0);
        }
    }
    let mut out = prefix.to_vec();
    out.extend_from_slice(b"FORM");
    put_be32(&mut out, body.len() as u32);
    out.extend(body);
    out
}

/// MIDI：一个音轨时格式 0，否则格式 1；每个音轨是 `tracks` 字节随机事件加「音轨结束」；`alien` 时头块之后夹一个不认识的块。
pub fn midi(tracks: &[usize], alien: bool) -> Vec<u8> {
    let mut noise = Noise::new(0x004D_4944);
    let mut out = b"MThd\0\0\0\x06".to_vec();
    put_be16(&mut out, if tracks.len() == 1 { 0 } else { 1 });
    put_be16(&mut out, tracks.len() as u16);
    put_be16(&mut out, 480);
    if alien {
        out.extend_from_slice(b"XFIH\0\0\0\x05");
        out.extend(noise.bytes(5));
    }
    for &len in tracks {
        out.extend_from_slice(b"MTrk");
        put_be32(&mut out, len as u32 + 4);
        out.extend(noise.bytes(len));
        out.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
    }
    out
}

/// FLV：文件头、第一个长度 0，之后 `tags` 个标签（先脚本数据，再视频、音频交替），每个后面跟着它的长度。
pub fn flv(tags: usize) -> Vec<u8> {
    let mut noise = Noise::new(0x0046_4C56);
    let mut out = b"FLV\x01\x05\0\0\0\x09\0\0\0\0".to_vec();
    for index in 0..tags {
        let kind = match index {
            0 => 18,
            index if index % 2 == 1 => 9,
            _ => 8,
        };
        let len = 20 + (noise.next_u64() % 400) as usize;
        out.push(kind);
        out.extend_from_slice(&(len as u32).to_be_bytes()[1..]);
        out.extend_from_slice(&((index * 40) as u32).to_be_bytes()[1..]);
        out.extend_from_slice(&[0, 0, 0, 0]); // 时间戳扩展、流 ID
        out.extend(noise.bytes(len));
        put_be32(&mut out, 11 + len as u32);
    }
    out
}

/// ASF 样本里文件属性对象的位置：头对象的第一个子对象。
pub const ASF_FILE_PROPERTIES_AT: usize = 30;

/// ASF：头对象里依次是文件属性、流属性（`video` 时是视频流，否则音频流）、一个不认识的对象；
/// 之后是带 `packets` 字节数据的数据对象与一个索引对象。文件属性记着整个文件的长度。
pub fn asf(video: bool, packets: usize) -> Vec<u8> {
    use crate::asf::{
        AUDIO_MEDIA, DATA_OBJECT, FILE_PROPERTIES, HEADER_OBJECT, STREAM_PROPERTIES, VIDEO_MEDIA,
    };
    let mut noise = Noise::new(0x0041_5346);
    let object = |guid: &[u8; 16], body: &[u8]| {
        let mut out = guid.to_vec();
        out.extend_from_slice(&(24 + body.len() as u64).to_le_bytes());
        out.extend_from_slice(body);
        out
    };
    let mut stream = vec![0u8; 78 - 24];
    stream[..16].copy_from_slice(if video { &VIDEO_MEDIA } else { &AUDIO_MEDIA });
    let stream = object(&STREAM_PROPERTIES, &stream);
    let other = object(&[0x11; 16], &noise.bytes(40));
    let mut data = noise.bytes(16); // 文件 ID
    data.extend_from_slice(&3u64.to_le_bytes()); // 包数
    data.extend_from_slice(&[1, 1]);
    data.extend(noise.bytes(packets));
    let data = object(&DATA_OBJECT, &data);
    let index = object(&[0x22; 16], &noise.bytes(32));
    let header_len = 30 + 104 + stream.len() + other.len();
    let file_size = header_len + data.len() + index.len();

    let mut properties = noise.bytes(16); // 文件 ID
    properties.extend_from_slice(&(file_size as u64).to_le_bytes());
    properties.extend(noise.bytes(40)); // 创建时间、包数、播放与发送时长、预卷
    put32(&mut properties, 0x02); // 可定位，不是广播
    properties.extend(noise.bytes(12));
    let properties = object(&FILE_PROPERTIES, &properties);

    let mut out = HEADER_OBJECT.to_vec();
    out.extend_from_slice(&(header_len as u64).to_le_bytes());
    put32(&mut out, 3);
    out.extend_from_slice(&[1, 2]);
    out.extend(properties);
    out.extend(stream);
    out.extend(other);
    out.extend(data);
    out.extend(index);
    out
}

/// CAB：一个文件夹、`files` 个文件，一个 CFDATA 块装 `data_len` 字节（不压缩）。
pub fn cab(files: usize, data_len: usize) -> Vec<u8> {
    let mut noise = Noise::new(0x0043_4142);
    let names: Vec<Vec<u8>> = (0..files)
        .map(|index| format!("file{index}.txt\0").into_bytes())
        .collect();
    let files_at = 36 + 8;
    let data_at = files_at + names.iter().map(|name| 16 + name.len()).sum::<usize>();
    let total = data_at + 8 + data_len;
    let mut out = b"MSCF\0\0\0\0".to_vec();
    put32(&mut out, total as u32);
    put32(&mut out, 0);
    put32(&mut out, files_at as u32);
    put32(&mut out, 0);
    out.extend_from_slice(&[3, 1]);
    put16(&mut out, 1);
    put16(&mut out, files as u16);
    put16(&mut out, 0); // 标志
    put16(&mut out, 0x1234); // 分卷组号
    put16(&mut out, 0);
    // CFFOLDER
    put32(&mut out, data_at as u32);
    put16(&mut out, 1);
    put16(&mut out, 0); // 不压缩
    let per_file = data_len / files.max(1);
    for (index, name) in names.iter().enumerate() {
        put32(&mut out, per_file as u32);
        put32(&mut out, (index * per_file) as u32);
        put16(&mut out, 0);
        put16(&mut out, 0x5A21);
        put16(&mut out, 0x6000);
        put16(&mut out, 0x20);
        out.extend_from_slice(name);
    }
    // CFDATA
    put32(&mut out, 0);
    put16(&mut out, data_len as u16);
    put16(&mut out, data_len as u16);
    out.extend(noise.bytes(data_len));
    out
}

/// TAR：每项是（名字、长度），名字以 `/` 结尾的是目录；`gnu` 时用 GNU 的标识；
/// 结尾两个全 0 块，`padded` 时再补 0 到 10240 字节的整数倍。
pub fn tar(entries: &[(&str, usize)], padded: bool, gnu: bool) -> Vec<u8> {
    let mut noise = Noise::new(0x0054_4152);
    let mut out = Vec::new();
    for &(name, size) in entries {
        let directory = name.ends_with('/');
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(if directory {
            b"0000755\0"
        } else {
            b"0000644\0"
        });
        header[108..116].copy_from_slice(b"0001750\0");
        header[116..124].copy_from_slice(b"0001750\0");
        header[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
        header[136..148].copy_from_slice(b"14000000000\0");
        header[156] = if directory { b'5' } else { b'0' };
        if gnu {
            header[257..265].copy_from_slice(b"ustar  \0");
        } else {
            header[257..265].copy_from_slice(b"ustar\x0000");
        }
        header[265..269].copy_from_slice(b"user");
        header[148..156].copy_from_slice(b"        ");
        let sum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        out.extend_from_slice(&header);
        out.extend(noise.bytes(size));
        out.resize(out.len().next_multiple_of(512), 0);
    }
    out.extend_from_slice(&[0; 1024]);
    if padded {
        out.resize(out.len().next_multiple_of(10240), 0);
    }
    out
}

/// PE 映像的布局：`sections` 是各节的原始数据长度（0 表示只有未初始化数据），
/// `certificate` 大于 0 时在最后放这么长的证书表，`symbols` 是 COFF 符号个数与字符串表长度。
pub struct PeLayout {
    pub plus: bool,
    pub dll: bool,
    pub subsystem: u16,
    pub sections: Vec<usize>,
    pub certificate: usize,
    pub symbols: Option<(u32, u32)>,
}

impl Default for PeLayout {
    fn default() -> Self {
        Self {
            plus: false,
            dll: false,
            subsystem: 2,
            sections: vec![700, 300],
            certificate: 0,
            symbols: None,
        }
    }
}

/// PE：DOS 头、PE 头在 0x80，节按 0x200 对齐依次排在头后面，之后是符号表与字符串表、证书表。
pub fn pe(layout: &PeLayout) -> Vec<u8> {
    const LFANEW: usize = 0x80;
    const ALIGN: usize = 0x200;
    let mut noise = Noise::new(0x0050_4500);
    let optional_len: usize = if layout.plus { 0xF0 } else { 0xE0 };
    let count = layout.sections.len();
    let headers_len = (LFANEW + 24 + optional_len + 40 * count).next_multiple_of(ALIGN);
    let mut sections = Vec::new();
    let mut at = headers_len;
    for &size in &layout.sections {
        let len = size.next_multiple_of(ALIGN);
        sections.push((if len == 0 { 0 } else { at }, len));
        at += len;
    }
    let symbols_at = at;
    let (symbols_len, strings_len) = layout.symbols.map_or((0, 0), |(count, strings)| {
        (18 * count as usize, strings as usize)
    });
    let mut end = symbols_at + symbols_len + strings_len;
    let certificate_at = end.next_multiple_of(8);
    if layout.certificate > 0 {
        end = certificate_at + layout.certificate;
    }
    let mut out = noise.bytes(end);
    out[..LFANEW].fill(0);
    out[..2].copy_from_slice(b"MZ");
    out[0x3C..0x40].copy_from_slice(&(LFANEW as u32).to_le_bytes());
    let mut headers = b"PE\0\0".to_vec();
    put16(&mut headers, if layout.plus { 0x8664 } else { 0x014C });
    put16(&mut headers, count as u16);
    put32(&mut headers, 0x6000_0000);
    put32(
        &mut headers,
        if layout.symbols.is_some() {
            symbols_at as u32
        } else {
            0
        },
    );
    put32(&mut headers, layout.symbols.map_or(0, |(count, _)| count));
    put16(&mut headers, optional_len as u16);
    put16(&mut headers, 0x0102 | if layout.dll { 0x2000 } else { 0 });
    let mut optional = vec![0u8; optional_len];
    let magic: u16 = if layout.plus { 0x20B } else { 0x10B };
    optional[..2].copy_from_slice(&magic.to_le_bytes());
    optional[32..36].copy_from_slice(&0x1000u32.to_le_bytes());
    optional[36..40].copy_from_slice(&(ALIGN as u32).to_le_bytes());
    optional[56..60].copy_from_slice(&(at as u32).to_le_bytes());
    optional[60..64].copy_from_slice(&(headers_len as u32).to_le_bytes());
    optional[68..70].copy_from_slice(&layout.subsystem.to_le_bytes());
    let directories = if layout.plus { 112 } else { 96 };
    optional[directories - 4..directories].copy_from_slice(&16u32.to_le_bytes());
    if layout.certificate > 0 {
        let entry = directories + 8 * 4;
        optional[entry..entry + 4].copy_from_slice(&(certificate_at as u32).to_le_bytes());
        optional[entry + 4..entry + 8].copy_from_slice(&(layout.certificate as u32).to_le_bytes());
    }
    headers.extend(optional);
    for (index, &(raw_at, raw_len)) in sections.iter().enumerate() {
        let mut section = format!(".s{index}\0\0\0\0\0").into_bytes();
        section.truncate(8);
        put32(&mut section, 0x1000);
        put32(&mut section, 0x1000 * (index as u32 + 1));
        put32(&mut section, raw_len as u32);
        put32(&mut section, raw_at as u32);
        section.extend_from_slice(&[0; 12]);
        put32(&mut section, 0x6000_0020);
        headers.extend(section);
    }
    out[LFANEW..LFANEW + headers.len()].copy_from_slice(&headers);
    out[LFANEW + headers.len()..headers_len].fill(0);
    if layout.symbols.is_some() {
        let strings_at = symbols_at + symbols_len;
        out[strings_at..strings_at + 4].copy_from_slice(&(strings_len as u32).to_le_bytes());
    }
    out
}

/// sfnt 的偏移表与表记录；`tables` 是（标签、位置、长度）。
fn sfnt_directory(version: &[u8; 4], tables: &[(&[u8; 4], usize, usize)]) -> Vec<u8> {
    let count = tables.len() as u16;
    let selector = 15 - count.leading_zeros() as u16;
    let range = (1u16 << selector) * 16;
    let mut out = version.to_vec();
    for value in [count, range, selector, count * 16 - range] {
        put_be16(&mut out, value);
    }
    for &(tag, at, len) in tables {
        out.extend_from_slice(tag);
        put_be32(&mut out, 0); // 校验和
        put_be32(&mut out, at as u32);
        put_be32(&mut out, len as u32);
    }
    out
}

/// 一张表的内容：`head` 表照规范写版本与魔数，其余随机。
fn sfnt_table(noise: &mut Noise, tag: &[u8; 4], len: usize) -> Vec<u8> {
    let mut table = noise.bytes(len);
    if tag == b"head" {
        table[..4].copy_from_slice(&[0, 1, 0, 0]);
        table[12..16].copy_from_slice(&0x5F0F_3CF5u32.to_be_bytes());
    }
    table
}

/// TrueType / OpenType：`tables` 是（标签、长度），按顺序排在表记录之后，每张按 4 字节对齐。
pub fn sfnt(version: &[u8; 4], tables: &[(&[u8; 4], usize)]) -> Vec<u8> {
    let mut noise = Noise::new(0x0054_5446);
    let mut at = 12 + 16 * tables.len();
    let mut placed = Vec::new();
    for &(tag, len) in tables {
        placed.push((tag, at, len));
        at = (at + len).next_multiple_of(4);
    }
    let mut out = sfnt_directory(version, &placed);
    for &(tag, table_at, len) in &placed {
        out.resize(table_at, 0);
        out.extend(sfnt_table(&mut noise, tag, len));
    }
    out.resize(out.len().next_multiple_of(4), 0);
    out
}

/// 字体集：`fonts` 个字体，各有 cmap、glyf、head 三张表；`signature` 时用 2.0 版的头，在最后放数字签名表。
pub fn ttc(fonts: usize, signature: bool) -> Vec<u8> {
    let mut noise = Noise::new(0x0054_5443);
    let header_len = 12 + 4 * fonts + if signature { 12 } else { 0 };
    let directory_len = 12 + 16 * 3;
    let mut out = vec![0u8; header_len + fonts * directory_len];
    out[..4].copy_from_slice(b"ttcf");
    out[4..6].copy_from_slice(&(if signature { 2u16 } else { 1 }).to_be_bytes());
    out[8..12].copy_from_slice(&(fonts as u32).to_be_bytes());
    for font in 0..fonts {
        let directory_at = header_len + font * directory_len;
        out[12 + 4 * font..16 + 4 * font].copy_from_slice(&(directory_at as u32).to_be_bytes());
        let mut placed = Vec::new();
        for (tag, len) in [
            (b"cmap", 40 + 8 * font),
            (b"glyf", 500 + 100 * font),
            (b"head", 54),
        ] {
            let at = out.len();
            out.extend(sfnt_table(&mut noise, tag, len));
            out.resize(out.len().next_multiple_of(4), 0);
            placed.push((tag, at, len));
        }
        let directory = sfnt_directory(b"\0\x01\0\0", &placed);
        out[directory_at..directory_at + directory_len].copy_from_slice(&directory);
    }
    if signature {
        let dsig_at = out.len();
        out.extend(noise.bytes(100));
        let at = 12 + 4 * fonts;
        out[at..at + 4].copy_from_slice(b"DSIG");
        out[at + 4..at + 8].copy_from_slice(&100u32.to_be_bytes());
        out[at + 8..at + 12].copy_from_slice(&(dsig_at as u32).to_be_bytes());
    }
    out
}

/// WOFF / WOFF2：一共 `len` 字节，头里写着这个长度，一张表；没有元数据与私有数据。
pub fn woff(woff2: bool, len: usize) -> Vec<u8> {
    let mut noise = Noise::new(0x0057_4F46);
    let mut out = noise.bytes(len);
    out[..4].copy_from_slice(if woff2 { b"wOF2" } else { b"wOFF" });
    out[4..8].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    out[8..12].copy_from_slice(&(len as u32).to_be_bytes());
    out[12..14].copy_from_slice(&1u16.to_be_bytes());
    out[14..16].fill(0);
    out[16..20].copy_from_slice(&128u32.to_be_bytes());
    if woff2 {
        out[20..24].copy_from_slice(&(len as u32 - 58).to_be_bytes());
        out[28..48].fill(0);
    } else {
        out[24..44].fill(0);
        let table = len as u32 - 64;
        out[44..48].copy_from_slice(b"glyf");
        out[48..52].copy_from_slice(&64u32.to_be_bytes());
        out[52..56].copy_from_slice(&table.to_be_bytes());
        out[56..60].copy_from_slice(&(table + 10).to_be_bytes());
    }
    out
}

/// ICO / CUR：每个图像是（是否 PNG、数据长度），依次紧跟在目录后面；不是 PNG 的以位图信息头开始。
pub fn ico(images: &[(bool, usize)], cursor: bool) -> Vec<u8> {
    let mut noise = Noise::new(0x0049_434F);
    let mut out = vec![0, 0, if cursor { 2 } else { 1 }, 0];
    put16(&mut out, images.len() as u16);
    let mut at = 6 + 16 * images.len();
    let mut data = Vec::new();
    for &(png, len) in images {
        out.extend_from_slice(&[32, 32, 0, 0]);
        if cursor {
            put16(&mut out, 5);
            put16(&mut out, 7);
        } else {
            put16(&mut out, 1);
            put16(&mut out, 32);
        }
        put32(&mut out, len as u32);
        put32(&mut out, at as u32);
        let mut image = noise.bytes(len);
        if png {
            image[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        } else {
            image[..4].copy_from_slice(&40u32.to_le_bytes());
        }
        data.extend(image);
        at += len;
    }
    out.extend(data);
    out
}

/// Photoshop 文档的布局：PSB、合并图像用 RLE、通道数、有没有图像资源。图像 20 × 10、每通道 8 位、RGB。
pub struct PsdLayout {
    pub big: bool,
    pub rle: bool,
    pub channels: u16,
    pub resources: bool,
}

impl Default for PsdLayout {
    fn default() -> Self {
        Self {
            big: false,
            rle: false,
            channels: 3,
            resources: true,
        }
    }
}

pub fn psd(layout: &PsdLayout) -> Vec<u8> {
    let (width, height) = (20usize, 10usize);
    let mut noise = Noise::new(0x0038_4250);
    let mut out = b"8BPS".to_vec();
    put_be16(&mut out, if layout.big { 2 } else { 1 });
    out.extend_from_slice(&[0; 6]);
    put_be16(&mut out, layout.channels);
    put_be32(&mut out, height as u32);
    put_be32(&mut out, width as u32);
    put_be16(&mut out, 8);
    put_be16(&mut out, 3);
    put_be32(&mut out, 0); // 颜色模式数据
    if layout.resources {
        let mut resources = b"8BIM".to_vec();
        put_be16(&mut resources, 0x0408);
        resources.extend_from_slice(&[0, 0]); // 空名字，补齐到偶数
        put_be32(&mut resources, 20);
        resources.extend(noise.bytes(20));
        put_be32(&mut out, resources.len() as u32);
        out.extend(resources);
    } else {
        put_be32(&mut out, 0);
    }
    if layout.big {
        out.extend_from_slice(&60u64.to_be_bytes());
    } else {
        put_be32(&mut out, 60);
    }
    out.extend(noise.bytes(60));
    let rows = usize::from(layout.channels) * height;
    if layout.rle {
        put_be16(&mut out, 1);
        let counts: Vec<usize> = (0..rows)
            .map(|_| 2 + (noise.next_u64() % 20) as usize)
            .collect();
        for &count in &counts {
            if layout.big {
                put_be32(&mut out, count as u32);
            } else {
                put_be16(&mut out, count as u16);
            }
        }
        out.extend(noise.bytes(counts.iter().sum()));
    } else {
        put_be16(&mut out, 0);
        out.extend(noise.bytes(rows * width));
    }
    out
}

/// [`psd`] 样本里合并图像数据（压缩方式字段）的位置。
pub fn psd_image_at(sample: &[u8]) -> usize {
    let big = sample[5] == 2;
    let mut at = 26;
    at += 4 + be32_at(sample, at) as usize;
    at += 4 + be32_at(sample, at) as usize;
    if big {
        at + 8 + u64::from_be_bytes(sample[at..at + 8].try_into().unwrap()) as usize
    } else {
        at + 4 + be32_at(sample, at) as usize
    }
}

/// 初值为 0、最后不取反的 CRC-32，逐位算（PST 文件头的 CRC 用它）。
pub fn raw_crc32(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// Outlook 数据文件：一共 `len` 字节，ANSI 或 Unicode 格式的文件头记着这个长度，`ost` 时是脱机文件。
pub fn pst(unicode: bool, ost: bool, len: usize) -> Vec<u8> {
    let mut noise = Noise::new(0x0050_5354);
    let mut out = noise.bytes(len);
    out[..4].copy_from_slice(b"!BDN");
    out[8..10].copy_from_slice(if ost { b"SO" } else { b"SM" });
    out[10..12].copy_from_slice(&(if unicode { 23u16 } else { 14 }).to_le_bytes());
    out[12..14].copy_from_slice(&19u16.to_le_bytes());
    out[14..16].copy_from_slice(&[1, 1]);
    if unicode {
        out[184..192].copy_from_slice(&(len as u64).to_le_bytes());
    } else {
        out[168..172].copy_from_slice(&(len as u32).to_le_bytes());
    }
    let crc = raw_crc32(&out[8..8 + 471]);
    out[4..8].copy_from_slice(&crc.to_le_bytes());
    out
}

/// 未压缩的 SWF：`tags` 是各标签的数据长度（63 以上用长标签头），最后是 End 标签；文件头写着总长度。
pub fn swf(tags: &[usize]) -> Vec<u8> {
    let mut noise = Noise::new(0x0053_5746);
    let mut out = b"FWS\x0A\0\0\0\0".to_vec();
    // 舞台大小：每个坐标 15 位，5 + 60 = 65 位，占 9 字节
    out.push(15 << 3);
    out.extend(noise.bytes(8));
    out.extend_from_slice(&[0x00, 0x18]); // 帧率 24
    put16(&mut out, 1);
    for (index, &len) in tags.iter().enumerate() {
        let code = 9 + index as u16;
        if len >= 0x3F {
            put16(&mut out, (code << 6) | 0x3F);
            put32(&mut out, len as u32);
        } else {
            put16(&mut out, (code << 6) | len as u16);
        }
        out.extend(noise.bytes(len));
    }
    put16(&mut out, 0);
    let total = out.len() as u32;
    out[4..8].copy_from_slice(&total.to_le_bytes());
    out
}

/// 富士 RAF：文件头之后依次是 `jpeg` 字节的 JPEG 预览、200 字节的 CFA 头、`cfa` 字节的 CFA 数据。
pub fn raf(jpeg: usize, cfa: usize) -> Vec<u8> {
    let mut noise = Noise::new(0x0052_4146);
    let jpeg_at = 0x94;
    let cfa_header_at = jpeg_at + jpeg;
    let cfa_at = cfa_header_at + 200;
    let mut out = noise.bytes(cfa_at + cfa);
    out[..16].copy_from_slice(b"FUJIFILMCCD-RAW ");
    out[0x10..0x14].copy_from_slice(b"0201");
    out[0x14..0x1C].copy_from_slice(b"FF129502");
    out[0x1C..0x3C].fill(0);
    out[0x1C..0x20].copy_from_slice(b"X-T3");
    out[0x3C..0x40].copy_from_slice(b"0100");
    for (index, (at, len)) in [(jpeg_at, jpeg), (cfa_header_at, 200), (cfa_at, cfa)]
        .into_iter()
        .enumerate()
    {
        let field = 0x54 + 8 * index;
        out[field..field + 4].copy_from_slice(&(at as u32).to_be_bytes());
        out[field + 4..field + 8].copy_from_slice(&(len as u32).to_be_bytes());
    }
    out[jpeg_at..jpeg_at + 4].copy_from_slice(&[0xFF, 0xD8, 0xFF, 0xE1]);
    out
}

/// EVTX：文件头块与 `chunks` 个块。
pub fn evtx(chunks: u16) -> Vec<u8> {
    let mut noise = Noise::new(0x0045_5654);
    let mut out = vec![0u8; 4096];
    out[..8].copy_from_slice(b"ElfFile\0");
    out[16..24].copy_from_slice(&u64::from(chunks - 1).to_le_bytes());
    out[24..32].copy_from_slice(&(u64::from(chunks) * 100 + 1).to_le_bytes());
    out[32..36].copy_from_slice(&128u32.to_le_bytes());
    out[36..38].copy_from_slice(&1u16.to_le_bytes());
    out[38..40].copy_from_slice(&3u16.to_le_bytes());
    out[40..42].copy_from_slice(&4096u16.to_le_bytes());
    out[42..44].copy_from_slice(&chunks.to_le_bytes());
    let crc = crc32(&[&out[..120]]);
    out[124..128].copy_from_slice(&crc.to_le_bytes());
    for _ in 0..chunks {
        let mut chunk = noise.bytes(65536);
        chunk[..8].copy_from_slice(b"ElfChnk\0");
        out.extend(chunk);
    }
    out
}

/// 注册表配置单元：基本块与 `bins` 个 4096 字节的 hive bin。
pub fn regf(bins: u32) -> Vec<u8> {
    let mut noise = Noise::new(0x0052_4547);
    let mut out = vec![0u8; 4096];
    out[..4].copy_from_slice(b"regf");
    out[4..8].copy_from_slice(&7u32.to_le_bytes());
    out[8..12].copy_from_slice(&7u32.to_le_bytes());
    out[20..24].copy_from_slice(&1u32.to_le_bytes());
    out[24..28].copy_from_slice(&5u32.to_le_bytes());
    out[32..36].copy_from_slice(&1u32.to_le_bytes());
    out[36..40].copy_from_slice(&0x20u32.to_le_bytes());
    out[40..44].copy_from_slice(&(bins * 4096).to_le_bytes());
    out[44..48].copy_from_slice(&1u32.to_le_bytes());
    for (index, unit) in "SYSTEM".encode_utf16().enumerate() {
        out[48 + 2 * index..50 + 2 * index].copy_from_slice(&unit.to_le_bytes());
    }
    let mut checksum = out[..508]
        .as_chunks::<4>()
        .0
        .iter()
        .fold(0u32, |sum, word| sum ^ u32::from_le_bytes(*word));
    if checksum == u32::MAX {
        checksum -= 1;
    } else if checksum == 0 {
        checksum = 1;
    }
    out[508..512].copy_from_slice(&checksum.to_le_bytes());
    for index in 0..bins {
        let mut bin = noise.bytes(4096);
        bin[..4].copy_from_slice(b"hbin");
        bin[4..8].copy_from_slice(&(index * 4096).to_le_bytes());
        bin[8..12].copy_from_slice(&4096u32.to_le_bytes());
        out.extend(bin);
    }
    out
}

/// CHM：版本 2 或 3 的文件头、头段 0、目录段（`ITSP` 头加一段随机数据），之后 `content` 字节的内容。
pub fn chm(version: u32, content: usize) -> Vec<u8> {
    let mut noise = Noise::new(0x0043_484D);
    let header_len = if version == 3 { 0x60 } else { 0x58 };
    let section0_at = header_len;
    let directory_at = section0_at + 0x18;
    let directory_len = 0x54 + 200;
    let total = directory_at + directory_len + content;
    let mut out = noise.bytes(total);
    out[..4].copy_from_slice(b"ITSF");
    out[4..8].copy_from_slice(&version.to_le_bytes());
    out[8..12].copy_from_slice(&(header_len as u32).to_le_bytes());
    out[0x0C..0x10].copy_from_slice(&1u32.to_le_bytes());
    out[0x38..0x40].copy_from_slice(&(section0_at as u64).to_le_bytes());
    out[0x40..0x48].copy_from_slice(&0x18u64.to_le_bytes());
    out[0x48..0x50].copy_from_slice(&(directory_at as u64).to_le_bytes());
    out[0x50..0x58].copy_from_slice(&(directory_len as u64).to_le_bytes());
    if version == 3 {
        out[0x58..0x60].copy_from_slice(&((directory_at + directory_len) as u64).to_le_bytes());
    }
    out[section0_at..section0_at + 0x18].fill(0);
    out[section0_at..section0_at + 4].copy_from_slice(&0x01FEu32.to_le_bytes());
    out[section0_at + 8..section0_at + 16].copy_from_slice(&(total as u64).to_le_bytes());
    out[directory_at..directory_at + 4].copy_from_slice(b"ITSP");
    out
}

/// EMF：头记录、`records` 条画线记录、带 `palette` 项调色板的结束记录。
pub fn emf(records: usize, palette: usize) -> Vec<u8> {
    let mut noise = Noise::new(0x0045_4D46);
    let mut out = Vec::new();
    put32(&mut out, 1);
    put32(&mut out, 88);
    out.extend(noise.bytes(32)); // 外框与画框
    out.extend_from_slice(b" EMF");
    put32(&mut out, 0x0001_0000);
    put32(&mut out, 0); // 文件字节数，最后填
    put32(&mut out, records as u32 + 2);
    put16(&mut out, 1);
    put16(&mut out, 0);
    put32(&mut out, 0);
    put32(&mut out, 0);
    put32(&mut out, palette as u32);
    out.extend_from_slice(&[0x80, 7, 0, 0, 0x38, 4, 0, 0]); // 设备像素
    out.extend_from_slice(&[0x40, 1, 0, 0, 0xB4, 0, 0, 0]); // 设备毫米
    for _ in 0..records {
        put32(&mut out, 54); // EMR_LINETO
        put32(&mut out, 16);
        out.extend(noise.bytes(8));
    }
    let eof_len = 20 + 4 * palette;
    put32(&mut out, 14);
    put32(&mut out, eof_len as u32);
    put32(&mut out, palette as u32);
    put32(&mut out, if palette > 0 { 16 } else { 0 });
    out.extend(noise.bytes(4 * palette));
    put32(&mut out, eof_len as u32);
    let total = out.len() as u32;
    out[48..52].copy_from_slice(&total.to_le_bytes());
    out
}

/// WMF：`placeable` 时带可放置头；META_HEADER、`records` 条画线记录、结束记录。
pub fn wmf(placeable: bool, records: usize) -> Vec<u8> {
    let mut noise = Noise::new(0x0057_4D46);
    let mut body = Vec::new();
    put16(&mut body, 1);
    put16(&mut body, 9);
    put16(&mut body, 0x0300);
    put32(&mut body, 0); // 总字数，最后填
    put16(&mut body, 1);
    put32(&mut body, if records > 0 { 5 } else { 3 });
    put16(&mut body, 0);
    for _ in 0..records {
        put32(&mut body, 5);
        put16(&mut body, 0x0213); // META_LINETO
        body.extend(noise.bytes(4));
    }
    put32(&mut body, 3);
    put16(&mut body, 0);
    let words = (body.len() / 2) as u32;
    body[6..10].copy_from_slice(&words.to_le_bytes());
    if !placeable {
        return body;
    }
    let mut out = vec![0xD7, 0xCD, 0xC6, 0x9A, 0, 0];
    for value in [0u16, 0, 1000, 800, 1440] {
        put16(&mut out, value);
    }
    put32(&mut out, 0);
    let checksum = out
        .as_chunks::<2>()
        .0
        .iter()
        .fold(0u16, |sum, word| sum ^ u16::from_le_bytes(*word));
    put16(&mut out, checksum);
    out.extend(body);
    out
}
