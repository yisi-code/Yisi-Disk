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
