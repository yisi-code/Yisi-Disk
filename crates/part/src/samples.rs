//! 测试样本：内存里的块设备、按规范拼出的 MBR / EBR / GPT 镜像、固定种子的伪随机数。
//! 只在测试里使用，越界时直接 panic。
#![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]

use yisi_io::{BlockDevice, IoError};

use crate::Guid;
use crate::crc32::crc32;

/// 内存里的块设备。
pub struct MemoryDisk {
    data: Vec<u8>,
    sector_size: u32,
}

impl MemoryDisk {
    pub fn new(data: Vec<u8>, sector_size: u32) -> Self {
        Self { data, sector_size }
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

impl BlockDevice for MemoryDisk {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(self.data.len());
        let end = start.saturating_add(buf.len()).min(self.data.len());
        buf[..end - start].copy_from_slice(&self.data[start..end]);
        Ok(end - start)
    }
}

/// 固定种子的伪随机数（xorshift64*），同一种子每次得到相同的序列。
pub struct Noise(u64);

impl Noise {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `0..bound` 之间的数；`bound` 为 0 时返回 0。
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }

    /// 随机改掉 `data` 里的 `count` 个字节。
    pub fn mutate(&mut self, data: &mut [u8], count: usize) {
        if data.is_empty() {
            return;
        }
        for _ in 0..count {
            let at = self.below(data.len() as u64) as usize;
            data[at] = self.next_u64() as u8;
        }
    }
}

/// MBR 的一个表项。
#[derive(Debug, Clone, Copy)]
pub struct MbrEntry {
    pub type_id: u8,
    pub start: u32,
    pub sectors: u32,
    pub bootable: bool,
}

impl MbrEntry {
    pub fn new(type_id: u8, start: u32, sectors: u32) -> Self {
        Self {
            type_id,
            start,
            sectors,
            bootable: false,
        }
    }

    pub fn bootable(mut self) -> Self {
        self.bootable = true;
        self
    }
}

fn put_mbr_entry(sector: &mut [u8], slot: usize, entry: &MbrEntry) {
    let at = 446 + slot * 16;
    sector[at] = if entry.bootable { 0x80 } else { 0 };
    sector[at + 4] = entry.type_id;
    sector[at + 8..at + 12].copy_from_slice(&entry.start.to_le_bytes());
    sector[at + 12..at + 16].copy_from_slice(&entry.sectors.to_le_bytes());
}

/// `size` 字节的镜像，开头是带 `entries`（最多 4 项）的 MBR，位置按 512 字节扇区。
pub fn mbr_disk(size: usize, entries: &[MbrEntry]) -> Vec<u8> {
    let mut image = vec![0u8; size];
    for (slot, entry) in entries.iter().enumerate() {
        put_mbr_entry(&mut image[..512], slot, entry);
    }
    image[510] = 0x55;
    image[511] = 0xAA;
    image
}

/// 从扩展分区开头（`extended_start` 扇区）起写一串 EBR。每项是（类型、逻辑分区距所在 EBR 的扇区数、扇区数），
/// 下一个 EBR 紧跟在上一个逻辑分区之后。
pub fn ebr_chain(
    image: &mut [u8],
    sector: usize,
    extended_start: u32,
    logicals: &[(u8, u32, u32)],
) {
    let mut ebr = extended_start;
    for (index, &(type_id, gap, sectors)) in logicals.iter().enumerate() {
        let at = ebr as usize * sector;
        let block = &mut image[at..at + sector];
        put_mbr_entry(block, 0, &MbrEntry::new(type_id, gap, sectors));
        let next = ebr + gap + sectors;
        if let Some(&(_, next_gap, next_sectors)) = logicals.get(index + 1) {
            put_mbr_entry(
                block,
                1,
                &MbrEntry::new(0x05, next - extended_start, next_gap + next_sectors),
            );
        }
        block[510] = 0x55;
        block[511] = 0xAA;
        ebr = next;
    }
}

/// GPT 的一个表项，位置是首尾 LBA（含）。
#[derive(Debug, Clone)]
pub struct GptEntry {
    pub type_guid: Guid,
    pub first: u64,
    pub last: u64,
    pub name: String,
}

impl GptEntry {
    pub fn new(type_guid: Guid, first: u64, last: u64, name: &str) -> Self {
        Self {
            type_guid,
            first,
            last,
            name: name.to_owned(),
        }
    }
}

const GPT_ENTRY_COUNT: usize = 128;
const GPT_ENTRY_SIZE: usize = 128;

/// `sectors` 个扇区的 GPT 镜像：保护性 MBR、主表（LBA 1 起）、备份表（末尾），校验和都正确。
pub fn gpt_disk(sectors: u64, sector_size: u32, entries: &[GptEntry]) -> Vec<u8> {
    let sector = sector_size as usize;
    let mut image = vec![0u8; sectors as usize * sector];
    let protective = MbrEntry::new(0xEE, 1, u32::try_from(sectors - 1).unwrap_or(u32::MAX));
    put_mbr_entry(&mut image[..512], 0, &protective);
    image[510] = 0x55;
    image[511] = 0xAA;

    let mut table = vec![0u8; GPT_ENTRY_COUNT * GPT_ENTRY_SIZE];
    for (index, entry) in entries.iter().enumerate() {
        let at = index * GPT_ENTRY_SIZE;
        let slot = &mut table[at..at + GPT_ENTRY_SIZE];
        slot[..16].copy_from_slice(&entry.type_guid.0);
        slot[16..32].copy_from_slice(&[index as u8 + 1; 16]);
        slot[32..40].copy_from_slice(&entry.first.to_le_bytes());
        slot[40..48].copy_from_slice(&entry.last.to_le_bytes());
        for (unit_index, unit) in entry.name.encode_utf16().take(36).enumerate() {
            slot[56 + unit_index * 2..58 + unit_index * 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
    let table_sectors = (table.len() / sector) as u64;
    let last = sectors - 1;
    let backup_entries = last - table_sectors;
    let table_crc = crc32(&[&table]);

    for (my_lba, alternate, entries_lba) in [(1, last, 2), (last, 1, backup_entries)] {
        let mut header = vec![0u8; 92];
        header[..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        header[24..32].copy_from_slice(&my_lba.to_le_bytes());
        header[32..40].copy_from_slice(&alternate.to_le_bytes());
        header[40..48].copy_from_slice(&(2 + table_sectors).to_le_bytes());
        header[48..56].copy_from_slice(&(backup_entries - 1).to_le_bytes());
        header[56..72].copy_from_slice(&[0x5A; 16]);
        header[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        header[80..84].copy_from_slice(&(GPT_ENTRY_COUNT as u32).to_le_bytes());
        header[84..88].copy_from_slice(&(GPT_ENTRY_SIZE as u32).to_le_bytes());
        header[88..92].copy_from_slice(&table_crc.to_le_bytes());
        let header_crc = crc32(&[&header]);
        header[16..20].copy_from_slice(&header_crc.to_le_bytes());

        let at = my_lba as usize * sector;
        image[at..at + 92].copy_from_slice(&header);
        let at = entries_lba as usize * sector;
        image[at..at + table.len()].copy_from_slice(&table);
    }
    image
}

/// 一个参数自洽的 FAT 引导扇区（只有 BPB，没有文件系统），用来测试「第 0 扇区是引导扇区不是 MBR」。
pub fn fat_like_boot_sector() -> Vec<u8> {
    let mut sector = vec![0u8; 512];
    sector[..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    sector[3..11].copy_from_slice(b"MSDOS5.0");
    sector[11..13].copy_from_slice(&512u16.to_le_bytes());
    sector[13] = 4;
    sector[14..16].copy_from_slice(&4u16.to_le_bytes());
    sector[16] = 2;
    sector[21] = 0xF8;
    sector[510] = 0x55;
    sector[511] = 0xAA;
    sector
}
