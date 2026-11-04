//! 测试样本：丢失分区搜索用的整盘镜像。由 `yisi-fs` 与 `yisi-part` 的样本生成器拼出，只在测试里使用，越界时直接 panic。
#![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]

use yisi_fs::boot::exfat_boot_checksum;
use yisi_fs::format::{FatWidth, fat_entry_span, put_fat_entry};
use yisi_fs::samples::{ExfatImage, FatImage, FatType, Layout, NTFS_ROOT, NtfsImage, pattern};
use yisi_part::samples::{MbrEntry, mbr_disk};

const SECTOR: usize = 512;
const MIB: usize = 1 << 20;

/// 镜像里各分区的位置与写进去的文件。
pub struct SearchDisk {
    pub image: Vec<u8>,
    /// FAT32（MBR 第 1 项），带第 6 扇区的备份引导扇区，卷标 `DATA32`，根目录下有 `notes.txt`。
    pub fat32: (u64, u64),
    /// NTFS（第 2 项），末尾扇区是备份引导扇区，根目录下有 `report.bin` 与 `inner.img`。
    pub ntfs: (u64, u64),
    /// exFAT（第 3 项），自带备份引导区，卷标 `EXDATA`，根目录下有 `photo.dat`。
    pub exfat: (u64, u64),
    /// `inner.img` 的内容是一个 FAT12 卷（根目录下有 `nested.txt`），位置落在 NTFS 分区里面：
    /// 引导扇区和 FAT 都对得上，搜索会找到它，并标出与 NTFS 分区重叠。
    pub nested_fat12: (u64, u64),
    /// exFAT 之后的空闲区里有一个孤零零的 FAT16 引导扇区（后面没有 FAT），搜索不应把它当成分区。
    pub stray_boot: u64,
    pub notes: Vec<u8>,
    pub report: Vec<u8>,
    pub photo: Vec<u8>,
    pub nested: Vec<u8>,
}

impl SearchDisk {
    /// 清空分区表（第 0 扇区整个清零）。
    pub fn clear_table(&mut self) {
        self.image[..SECTOR].fill(0);
    }

    /// 把分区开头的主引导扇区清零，只剩备份。
    pub fn wipe_primary_boot(&mut self, offset: u64) {
        let at = offset as usize;
        self.image[at..at + SECTOR].fill(0);
    }
}

/// 56 MiB 的 MBR 磁盘：FAT32 在 1 MiB 处、NTFS 在 37 MiB 处、exFAT 在 46 MiB 处，一个接一个；
/// 54.5 MiB 处有一个孤立的 FAT16 引导扇区。
pub fn search_disk() -> SearchDisk {
    let notes = pattern(11, 5000);
    let report = pattern(22, 70_000);
    let photo = pattern(33, 9000);
    let nested = pattern(44, 1500);

    let mut fat32 = FatImage::new(FatType::Fat32);
    fat32.set_label(b"DATA32     ");
    fat32.add_file(fat32.root(), "notes.txt", &notes, Layout::Contiguous);
    // FAT32 样本自带第 6 扇区的备份引导扇区
    let fat32 = fat32.finish();

    let mut fat12 = FatImage::new(FatType::Fat12);
    fat12.add_file(fat12.root(), "nested.txt", &nested, Layout::Contiguous);
    let fat12 = fat12.finish();

    let mut ntfs = NtfsImage::new();
    ntfs.add_file(NTFS_ROOT, "report.bin", &report);
    let inner = ntfs.add_file(NTFS_ROOT, "inner.img", &fat12);
    let inner_offset = (ntfs.first_lcn(inner) * ntfs.cluster_size()) as u64;
    let mut ntfs = ntfs.finish();
    // 备份引导扇区在卷的最后一个扇区
    let boot = ntfs[..SECTOR].to_vec();
    let last = ntfs.len() - SECTOR;
    ntfs[last..].copy_from_slice(&boot);

    let mut exfat = ExfatImage::new();
    exfat.set_label("EXDATA");
    exfat.add_file(exfat.root(), "photo.dat", &photo, Layout::Contiguous);
    let exfat = exfat.finish();

    let fat32_at = MIB;
    let ntfs_at = 37 * MIB;
    let exfat_at = 46 * MIB;
    let stray_at = 54 * MIB + MIB / 2;
    let sectors = |len: usize| u32::try_from(len / SECTOR).unwrap();
    let mut image = mbr_disk(
        56 * MIB,
        &[
            MbrEntry::new(0x0C, sectors(fat32_at), sectors(fat32.len())),
            MbrEntry::new(0x07, sectors(ntfs_at), sectors(ntfs.len())),
            MbrEntry::new(0x07, sectors(exfat_at), sectors(exfat.len())),
        ],
    );
    image[fat32_at..fat32_at + fat32.len()].copy_from_slice(&fat32);
    image[ntfs_at..ntfs_at + ntfs.len()].copy_from_slice(&ntfs);
    image[exfat_at..exfat_at + exfat.len()].copy_from_slice(&exfat);
    let stray = FatImage::new(FatType::Fat16).finish();
    image[stray_at..stray_at + SECTOR].copy_from_slice(&stray[..SECTOR]);

    let span = |at: usize, len: usize| (at as u64, len as u64);
    SearchDisk {
        fat32: span(fat32_at, fat32.len()),
        ntfs: span(ntfs_at, ntfs.len()),
        exfat: span(exfat_at, exfat.len()),
        nested_fat12: (ntfs_at as u64 + inner_offset, fat12.len() as u64),
        stray_boot: stray_at as u64,
        image,
        notes,
        report,
        photo,
        nested,
    }
}

/// 样本 FAT 镜像里 FAT 的位置（按引导扇区读）：保留扇区数、每份 FAT 的扇区数、份数。
pub fn fat_geometry(image: &[u8]) -> (usize, usize, usize) {
    let reserved = u16::from_le_bytes([image[14], image[15]]) as usize;
    let fats = image[16] as usize;
    let fat16 = u16::from_le_bytes([image[22], image[23]]) as usize;
    let sectors = if fat16 != 0 {
        fat16
    } else {
        u32::from_le_bytes(image[36..40].try_into().unwrap()) as usize
    };
    (reserved, sectors, fats)
}

fn width(kind: FatType) -> FatWidth {
    match kind {
        FatType::Fat12 => FatWidth::Fat12,
        FatType::Fat16 => FatWidth::Fat16,
        FatType::Fat32 => FatWidth::Fat32,
    }
}

/// 改样本 FAT 镜像里的表项；`copy` 为 `None` 时各份都改。
pub fn set_fat(image: &mut [u8], kind: FatType, cluster: u32, value: u32, copy: Option<usize>) {
    let (reserved, sectors, fats) = fat_geometry(image);
    let width = width(kind);
    let (at, _) = fat_entry_span(width, cluster);
    for copy in copy.map_or(0..fats, |copy| copy..copy + 1) {
        let table = (reserved + copy * sectors) * SECTOR;
        put_fat_entry(width, cluster, value, &mut image[table + at as usize..]);
    }
}

/// 样本 FAT 镜像里表项的原始值（第一份）。
pub fn fat_entry(image: &[u8], kind: FatType, cluster: u32, copy: usize) -> u32 {
    let (reserved, sectors, _) = fat_geometry(image);
    let width = width(kind);
    let (at, len) = fat_entry_span(width, cluster);
    let table = (reserved + copy * sectors) * SECTOR + at as usize;
    yisi_fs::format::fat_entry_raw(width, cluster, &image[table..table + len]).unwrap()
}

pub fn end_marker(kind: FatType) -> u32 {
    width(kind).end_of_chain()
}

/// 样本 exFAT 镜像的簇区起点（字节）。
pub fn exfat_heap(image: &[u8]) -> usize {
    u32::from_le_bytes(image[88..92].try_into().unwrap()) as usize * SECTOR
}

/// 改样本 exFAT 镜像的分配位图（在 2 号簇）。
pub fn set_bitmap(image: &mut [u8], cluster: u32, allocated: bool) {
    let at = exfat_heap(image) + (cluster as usize - 2) / 8;
    let bit = 1u8 << ((cluster - 2) % 8);
    if allocated {
        image[at] |= bit;
    } else {
        image[at] &= !bit;
    }
}

/// 改完样本 exFAT 的主引导区后重算它的校验和。
pub fn reseal_exfat_boot(image: &mut [u8]) {
    let checksum = exfat_boot_checksum(&image[..11 * SECTOR]);
    for word in image[11 * SECTOR..12 * SECTOR].as_chunks_mut::<4>().0 {
        word.copy_from_slice(&checksum.to_le_bytes());
    }
}
