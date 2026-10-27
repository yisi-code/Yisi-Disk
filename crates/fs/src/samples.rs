//! 测试样本：按微软规范拼出的 FAT12/16/32 与 exFAT 镜像（NTFS 的在 `ntfs` 子模块），可以建目录、写文件（连续或碎片）、删除、
//! 在删除的文件原来的位置写新文件。只在测试里使用，越界时直接 panic。
#![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

pub use yisi_part::samples::{MemoryDisk, Noise};

use crate::fat::short_checksum;

mod ntfs;
pub use ntfs::{NtfsImage, ROOT as NTFS_ROOT, TAG_WOF};

const SECTOR: usize = 512;
/// FAT32 样本的备份引导扇区。
const FAT32_BACKUP_SECTOR: usize = 6;
/// 样本里所有时间戳：2024-03-05 13:45:30。
pub const SAMPLE_TIME: &str = "2024-03-05 13:45:30";
const DOS_DATE: u16 = ((2024 - 1980) << 9) | (3 << 5) | 5;
const DOS_TIME: u16 = (13 << 11) | (45 << 5) | 15;

/// 样本里的一个目录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dir(usize);

/// 写进样本的一个文件或目录：它的目录项在哪里、占了哪些簇，删除时要用。
#[derive(Debug, Clone)]
pub struct Placed {
    dir: usize,
    /// 第一个目录项（长文件名或文件项）的序号，以及共几项。
    first_entry: usize,
    entry_count: usize,
    pub clusters: Vec<u32>,
    /// 新建的目录在样本里的编号。
    pub created_dir: Option<Dir>,
}

impl Placed {
    pub fn first_cluster(&self) -> u32 {
        self.clusters.first().copied().unwrap_or(0)
    }

    pub fn as_dir(&self) -> Dir {
        self.created_dir.expect("不是目录")
    }
}

/// 簇的分配方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Contiguous,
    /// 每用一个簇就空出一个簇。
    Fragmented,
    /// 从指定的簇起连续存放（用来模拟「新文件写进了已删除文件的位置」）。
    At(u32),
}

enum DirLocation {
    /// FAT12/16 的根目录区：分区内偏移与能放的目录项数。
    Fixed {
        offset: usize,
        capacity: usize,
    },
    Clusters(Vec<u32>),
}

struct DirState {
    location: DirLocation,
    used: usize,
    short_names: BTreeSet<[u8; 11]>,
}

// ───────────────────────────── FAT ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatType {
    Fat12,
    Fat16,
    Fat32,
}

pub struct FatImage {
    kind: FatType,
    image: Vec<u8>,
    sectors_per_cluster: usize,
    reserved: usize,
    fat_sectors: usize,
    heap_offset: usize,
    cluster_count: u32,
    fat: Vec<u32>,
    next: u32,
    dirs: Vec<DirState>,
}

impl FatImage {
    /// 簇数按规范落在各自类型的范围里：FAT12 约 3900 簇、FAT16 约 34000 簇、FAT32 约 68000 簇（512 字节一簇）。
    pub fn new(kind: FatType) -> Self {
        let (total, spc, root_entries, reserved) = match kind {
            FatType::Fat12 => (4000usize, 1usize, 224usize, 1usize),
            FatType::Fat16 => (70_000, 2, 512, 4),
            FatType::Fat32 => (70_000, 1, 0, 32),
        };
        let bits = match kind {
            FatType::Fat12 => 12,
            FatType::Fat16 => 16,
            FatType::Fat32 => 32,
        };
        let root_sectors = (root_entries * 32).div_ceil(SECTOR);
        let rough = (total - reserved - root_sectors) / spc + 2;
        let fat_sectors = (rough * bits / 8).div_ceil(SECTOR);
        let first_data = reserved + 2 * fat_sectors + root_sectors;
        let cluster_count = ((total - first_data) / spc) as u32;

        let mut image = vec![0u8; total * SECTOR];
        let boot = &mut image[..SECTOR];
        boot[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        boot[3..11].copy_from_slice(b"MSDOS5.0");
        boot[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        boot[13] = spc as u8;
        boot[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
        boot[16] = 2;
        boot[17..19].copy_from_slice(&(root_entries as u16).to_le_bytes());
        if total < 0x10000 {
            boot[19..21].copy_from_slice(&(total as u16).to_le_bytes());
        } else {
            boot[32..36].copy_from_slice(&(total as u32).to_le_bytes());
        }
        boot[21] = 0xF8;
        let label_at = if kind == FatType::Fat32 {
            boot[36..40].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
            boot[44..48].copy_from_slice(&2u32.to_le_bytes());
            // FSInfo 在第 1 扇区，备份引导扇区在第 6 扇区（`finish` 时复制过去）
            boot[48..50].copy_from_slice(&1u16.to_le_bytes());
            boot[50..52].copy_from_slice(&(FAT32_BACKUP_SECTOR as u16).to_le_bytes());
            66
        } else {
            boot[22..24].copy_from_slice(&(fat_sectors as u16).to_le_bytes());
            38
        };
        boot[label_at] = 0x29;
        boot[label_at + 1..label_at + 5].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        boot[label_at + 5..label_at + 16].copy_from_slice(b"BOOTLABEL  ");
        boot[label_at + 16..label_at + 24].copy_from_slice(match kind {
            FatType::Fat12 => b"FAT12   ",
            FatType::Fat16 => b"FAT16   ",
            FatType::Fat32 => b"FAT32   ",
        });
        boot[510] = 0x55;
        boot[511] = 0xAA;

        let mut fat = vec![0u32; cluster_count as usize + 2];
        fat[0] = 0x0FFF_FFF8;
        fat[1] = 0x0FFF_FFFF;
        let mut sample = Self {
            kind,
            image,
            sectors_per_cluster: spc,
            reserved,
            fat_sectors,
            heap_offset: first_data * SECTOR,
            cluster_count,
            fat,
            next: 2,
            dirs: Vec::new(),
        };
        let root = if kind == FatType::Fat32 {
            let cluster = sample.allocate(1, Layout::Contiguous);
            DirLocation::Clusters(cluster)
        } else {
            DirLocation::Fixed {
                offset: (reserved + 2 * fat_sectors) * SECTOR,
                capacity: root_entries,
            }
        };
        sample.dirs.push(DirState {
            location: root,
            used: 0,
            short_names: BTreeSet::new(),
        });
        sample
    }

    pub fn root(&self) -> Dir {
        Dir(0)
    }

    pub fn cluster_size(&self) -> usize {
        self.sectors_per_cluster * SECTOR
    }

    pub fn cluster_count(&self) -> u32 {
        self.cluster_count
    }

    /// 根目录里的卷标项。
    pub fn set_label(&mut self, label: &[u8; 11]) {
        let mut entry = [0u8; 32];
        entry[..11].copy_from_slice(label);
        entry[11] = 0x08;
        self.push_entries(0, &[entry]);
    }

    pub fn add_file(&mut self, parent: Dir, name: &str, data: &[u8], layout: Layout) -> Placed {
        let count = data.len().div_ceil(self.cluster_size());
        let clusters = self.allocate(count, layout);
        for (index, &cluster) in clusters.iter().enumerate() {
            let chunk = &data
                [index * self.cluster_size()..data.len().min((index + 1) * self.cluster_size())];
            let at = self.cluster_offset(cluster);
            self.image[at..at + chunk.len()].copy_from_slice(chunk);
        }
        self.place(parent, name, false, false, clusters, data.len() as u32)
    }

    /// 只写 8.3 短文件名、不写长文件名的文件（名字必须本来就是大写的 8.3）。
    pub fn add_short_file(&mut self, parent: Dir, name: &str, data: &[u8]) -> Placed {
        let count = data.len().div_ceil(self.cluster_size());
        let clusters = self.allocate(count, Layout::Contiguous);
        for (index, &cluster) in clusters.iter().enumerate() {
            let chunk = &data
                [index * self.cluster_size()..data.len().min((index + 1) * self.cluster_size())];
            let at = self.cluster_offset(cluster);
            self.image[at..at + chunk.len()].copy_from_slice(chunk);
        }
        self.place(parent, name, false, true, clusters, data.len() as u32)
    }

    pub fn mkdir(&mut self, parent: Dir, name: &str) -> Placed {
        let clusters = self.allocate(1, Layout::Contiguous);
        // `..` 指向根目录时记 0（FAT32 也一样）
        let parent_cluster = match &self.dirs[parent.0].location {
            DirLocation::Clusters(clusters) if parent.0 != 0 => clusters[0],
            _ => 0,
        };
        let index = self.dirs.len();
        self.dirs.push(DirState {
            location: DirLocation::Clusters(clusters.clone()),
            used: 0,
            short_names: BTreeSet::new(),
        });
        let dot = short_entry(*b".          ", 0x10, clusters[0], 0);
        let dotdot = short_entry(*b"..         ", 0x10, parent_cluster, 0);
        self.push_entries(index, &[dot, dotdot]);
        let mut placed = self.place(parent, name, true, false, clusters, 0);
        placed.created_dir = Some(Dir(index));
        placed
    }

    /// 像系统删除那样：目录项首字节改成 0xE5（长文件名各段也是），簇链清零。
    pub fn delete(&mut self, placed: &Placed) {
        for index in placed.first_entry..placed.first_entry + placed.entry_count {
            let at = self.entry_offset(placed.dir, index);
            self.image[at] = 0xE5;
        }
        for &cluster in &placed.clusters {
            self.fat[cluster as usize] = 0;
        }
    }

    /// 把一个簇清零，模拟数据还没写到盘上就被删除的目录或文件。
    pub fn zero_cluster(&mut self, cluster: u32) {
        let at = self.cluster_offset(cluster);
        let size = self.cluster_size();
        self.image[at..at + size].fill(0);
    }

    /// 改写目录项（短文件名项）里的字节，模拟各种系统的差异或损坏。
    pub fn patch_short_entry(&mut self, placed: &Placed, offset: usize, bytes: &[u8]) {
        let at = self.entry_offset(placed.dir, placed.first_entry + placed.entry_count - 1);
        self.image[at + offset..at + offset + bytes.len()].copy_from_slice(bytes);
    }

    pub fn finish(mut self) -> Vec<u8> {
        let bytes_per_fat = self.fat_sectors * SECTOR;
        let mut table = vec![0u8; bytes_per_fat];
        for (cluster, &value) in self.fat.iter().enumerate() {
            match self.kind {
                FatType::Fat12 => {
                    let at = cluster + cluster / 2;
                    let value = (value & 0x0FFF) as u16;
                    let mut pair = u16::from_le_bytes([table[at], table[at + 1]]);
                    pair = if cluster % 2 == 1 {
                        (pair & 0x000F) | (value << 4)
                    } else {
                        (pair & 0xF000) | value
                    };
                    table[at..at + 2].copy_from_slice(&pair.to_le_bytes());
                }
                FatType::Fat16 => {
                    table[cluster * 2..cluster * 2 + 2]
                        .copy_from_slice(&((value & 0xFFFF) as u16).to_le_bytes());
                }
                FatType::Fat32 => {
                    table[cluster * 4..cluster * 4 + 4].copy_from_slice(&value.to_le_bytes());
                }
            }
        }
        for copy in 0..2 {
            let at = (self.reserved + copy * self.fat_sectors) * SECTOR;
            self.image[at..at + bytes_per_fat].copy_from_slice(&table);
        }
        if self.kind == FatType::Fat32 {
            // FSInfo 扇区：三处签名，空闲簇数与下一个空闲簇记为「未知」
            let fs_info = &mut self.image[SECTOR..2 * SECTOR];
            fs_info[..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
            fs_info[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
            fs_info[488..496].fill(0xFF);
            fs_info[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
            let backup = FAT32_BACKUP_SECTOR * SECTOR;
            self.image.copy_within(..SECTOR, backup);
        }
        self.image
    }

    fn end_marker(&self) -> u32 {
        match self.kind {
            FatType::Fat12 => 0xFFF,
            FatType::Fat16 => 0xFFFF,
            FatType::Fat32 => 0x0FFF_FFFF,
        }
    }

    fn allocate(&mut self, count: usize, layout: Layout) -> Vec<u32> {
        let mut clusters = Vec::with_capacity(count);
        let mut cluster = match layout {
            Layout::At(first) => first,
            _ => self.next,
        };
        for _ in 0..count {
            assert!(self.fat[cluster as usize] == 0, "簇 {cluster} 已被占用");
            clusters.push(cluster);
            cluster += if layout == Layout::Fragmented { 2 } else { 1 };
        }
        for pair in clusters.windows(2) {
            self.fat[pair[0] as usize] = pair[1];
        }
        if let Some(&last) = clusters.last() {
            self.fat[last as usize] = self.end_marker();
        }
        if !matches!(layout, Layout::At(_)) {
            self.next = cluster;
        }
        clusters
    }

    fn cluster_offset(&self, cluster: u32) -> usize {
        self.heap_offset + (cluster as usize - 2) * self.cluster_size()
    }

    fn place(
        &mut self,
        parent: Dir,
        name: &str,
        is_dir: bool,
        short_only: bool,
        clusters: Vec<u32>,
        size: u32,
    ) -> Placed {
        let short = if short_only {
            to_short(name)
        } else {
            self.unique_short(parent.0, name)
        };
        self.dirs[parent.0].short_names.insert(short);
        let first_cluster = clusters.first().copied().unwrap_or(0);
        let mut entries = Vec::new();
        if !short_only {
            entries.extend(lfn_entries(name, short_checksum(&short)));
        }
        entries.push(short_entry(
            short,
            if is_dir { 0x10 } else { 0x20 },
            first_cluster,
            size,
        ));
        let first_entry = self.push_entries(parent.0, &entries);
        Placed {
            dir: parent.0,
            first_entry,
            entry_count: entries.len(),
            clusters,
            created_dir: None,
        }
    }

    fn unique_short(&self, dir: usize, name: &str) -> [u8; 11] {
        let taken = &self.dirs[dir].short_names;
        let (base, ext) = split_name(name);
        let clean = |text: &str, len: usize| -> Vec<u8> {
            text.bytes()
                .filter(|byte| byte.is_ascii_alphanumeric())
                .map(|byte| byte.to_ascii_uppercase())
                .take(len)
                .collect()
        };
        let base = clean(base, 6);
        let ext = clean(ext, 3);
        for number in 1.. {
            let tail = format!("~{number}");
            let mut short = [b' '; 11];
            let keep = base.len().min(8 - tail.len());
            short[..keep].copy_from_slice(&base[..keep]);
            short[keep..keep + tail.len()].copy_from_slice(tail.as_bytes());
            short[8..8 + ext.len()].copy_from_slice(&ext);
            if !taken.contains(&short) {
                return short;
            }
        }
        unreachable!()
    }

    /// 在目录末尾追加若干项，目录满了就加一个簇。返回第一项的序号。
    fn push_entries(&mut self, dir: usize, entries: &[[u8; 32]]) -> usize {
        let first = self.dirs[dir].used;
        for entry in entries {
            let index = self.dirs[dir].used;
            let capacity_full = match &self.dirs[dir].location {
                DirLocation::Fixed { capacity, .. } => {
                    assert!(index < *capacity, "根目录满了");
                    false
                }
                DirLocation::Clusters(clusters) => {
                    index * 32 >= clusters.len() * self.cluster_size()
                }
            };
            if capacity_full {
                let new = self.allocate(1, Layout::Contiguous)[0];
                if let DirLocation::Clusters(clusters) = &mut self.dirs[dir].location {
                    let last = *clusters.last().unwrap();
                    self.fat[last as usize] = new;
                    clusters.push(new);
                }
            }
            let at = self.entry_offset(dir, index);
            self.image[at..at + 32].copy_from_slice(entry);
            self.dirs[dir].used += 1;
        }
        first
    }

    fn entry_offset(&self, dir: usize, index: usize) -> usize {
        match &self.dirs[dir].location {
            DirLocation::Fixed { offset, .. } => offset + index * 32,
            DirLocation::Clusters(clusters) => {
                let per_cluster = self.cluster_size() / 32;
                self.cluster_offset(clusters[index / per_cluster]) + (index % per_cluster) * 32
            }
        }
    }
}

fn split_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot + 1..]),
        _ => (name, ""),
    }
}

fn to_short(name: &str) -> [u8; 11] {
    let (base, ext) = split_name(name);
    assert!(base.len() <= 8 && ext.len() <= 3, "{name} 不是 8.3 名字");
    let mut short = [b' '; 11];
    short[..base.len()].copy_from_slice(base.as_bytes());
    short[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
    short
}

fn short_entry(short: [u8; 11], attr: u8, first_cluster: u32, size: u32) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[..11].copy_from_slice(&short);
    entry[11] = attr;
    entry[14..16].copy_from_slice(&DOS_TIME.to_le_bytes());
    entry[16..18].copy_from_slice(&DOS_DATE.to_le_bytes());
    entry[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    entry[22..24].copy_from_slice(&DOS_TIME.to_le_bytes());
    entry[24..26].copy_from_slice(&DOS_DATE.to_le_bytes());
    entry[26..28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
    entry
}

/// 长文件名各段，按在目录里的顺序（最后一段在前）。
fn lfn_entries(name: &str, checksum: u8) -> Vec<[u8; 32]> {
    let mut units: Vec<u16> = name.encode_utf16().collect();
    if !units.len().is_multiple_of(13) {
        units.push(0);
        while !units.len().is_multiple_of(13) {
            units.push(0xFFFF);
        }
    }
    let parts = units.len() / 13;
    let positions: Vec<usize> = (1..11)
        .step_by(2)
        .chain((14..26).step_by(2))
        .chain((28..32).step_by(2))
        .collect();
    (0..parts)
        .rev()
        .map(|part| {
            let mut entry = [0u8; 32];
            entry[0] = (part as u8 + 1) | if part + 1 == parts { 0x40 } else { 0 };
            entry[11] = 0x0F;
            entry[13] = checksum;
            for (slot, &at) in positions.iter().enumerate() {
                entry[at..at + 2].copy_from_slice(&units[part * 13 + slot].to_le_bytes());
            }
            entry
        })
        .collect()
}

// ───────────────────────────── exFAT ─────────────────────────────

const EXFAT_SPC_SHIFT: u32 = 3;
const EXFAT_TOTAL_SECTORS: usize = 16 * 1024;
const EXFAT_FAT_OFFSET: usize = 32;
/// 目录固定占 4 个簇。
const EXFAT_DIR_CLUSTERS: usize = 4;

pub struct ExfatImage {
    image: Vec<u8>,
    fat_length: usize,
    heap_offset: usize,
    cluster_count: u32,
    fat: Vec<u32>,
    bitmap: Vec<u8>,
    next: u32,
    dirs: Vec<DirState>,
}

impl ExfatImage {
    /// 8 MiB、4 KiB 一簇。2 号簇是分配位图，3 号簇是大写表，根目录从 4 号簇起、走 FAT 链。
    pub fn new() -> Self {
        let cluster_size = SECTOR << EXFAT_SPC_SHIFT;
        let rough_clusters = EXFAT_TOTAL_SECTORS * SECTOR / cluster_size;
        let fat_length = ((rough_clusters + 2) * 4).div_ceil(SECTOR);
        let heap_sector = (EXFAT_FAT_OFFSET + fat_length).next_multiple_of(1 << EXFAT_SPC_SHIFT);
        let cluster_count = ((EXFAT_TOTAL_SECTORS - heap_sector) >> EXFAT_SPC_SHIFT) as u32;
        let mut sample = Self {
            image: vec![0u8; EXFAT_TOTAL_SECTORS * SECTOR],
            fat_length,
            heap_offset: heap_sector * SECTOR,
            cluster_count,
            fat: vec![0u32; cluster_count as usize + 2],
            bitmap: vec![0u8; (cluster_count as usize).div_ceil(8)],
            next: 2,
            dirs: Vec::new(),
        };
        sample.fat[0] = 0xFFFF_FFF8;
        sample.fat[1] = 0xFFFF_FFFF;
        let bitmap = sample.allocate(1, Layout::Contiguous, true);
        let upcase = sample.allocate(1, Layout::Contiguous, true);
        let root = sample.allocate(EXFAT_DIR_CLUSTERS, Layout::Contiguous, true);
        assert_eq!((bitmap[0], upcase[0], root[0]), (2, 3, 4));
        sample.dirs.push(DirState {
            location: DirLocation::Clusters(root),
            used: 0,
            short_names: BTreeSet::new(),
        });
        let mut bitmap_entry = [0u8; 32];
        bitmap_entry[0] = 0x81;
        bitmap_entry[20..24].copy_from_slice(&2u32.to_le_bytes());
        bitmap_entry[24..32].copy_from_slice(&(sample.bitmap.len() as u64).to_le_bytes());
        let mut upcase_entry = [0u8; 32];
        upcase_entry[0] = 0x82;
        upcase_entry[20..24].copy_from_slice(&3u32.to_le_bytes());
        upcase_entry[24..32].copy_from_slice(&128u64.to_le_bytes());
        sample.push_entries(0, &[bitmap_entry, upcase_entry]);
        sample
    }

    pub fn root(&self) -> Dir {
        Dir(0)
    }

    pub fn cluster_size(&self) -> usize {
        SECTOR << EXFAT_SPC_SHIFT
    }

    pub fn set_label(&mut self, label: &str) {
        let units: Vec<u16> = label.encode_utf16().collect();
        let mut entry = [0u8; 32];
        entry[0] = 0x83;
        entry[1] = units.len() as u8;
        for (index, unit) in units.iter().enumerate() {
            entry[2 + index * 2..4 + index * 2].copy_from_slice(&unit.to_le_bytes());
        }
        self.push_entries(0, &[entry]);
    }

    /// `layout` 为连续时写「不走 FAT 链」标记、不写 FAT（与 Windows 一致），否则写 FAT 链。
    pub fn add_file(&mut self, parent: Dir, name: &str, data: &[u8], layout: Layout) -> Placed {
        let count = data.len().div_ceil(self.cluster_size());
        let no_fat_chain = layout != Layout::Fragmented;
        let clusters = self.allocate(count, layout, !no_fat_chain);
        for (index, &cluster) in clusters.iter().enumerate() {
            let chunk = &data
                [index * self.cluster_size()..data.len().min((index + 1) * self.cluster_size())];
            let at = self.cluster_offset(cluster);
            self.image[at..at + chunk.len()].copy_from_slice(chunk);
        }
        self.place(
            parent,
            name,
            0x20,
            clusters,
            data.len() as u64,
            no_fat_chain,
        )
    }

    pub fn mkdir(&mut self, parent: Dir, name: &str) -> Placed {
        let clusters = self.allocate(EXFAT_DIR_CLUSTERS, Layout::Contiguous, false);
        let index = self.dirs.len();
        self.dirs.push(DirState {
            location: DirLocation::Clusters(clusters.clone()),
            used: 0,
            short_names: BTreeSet::new(),
        });
        let size = (EXFAT_DIR_CLUSTERS * self.cluster_size()) as u64;
        let mut placed = self.place(parent, name, 0x10, clusters, size, true);
        placed.created_dir = Some(Dir(index));
        placed
    }

    /// 像 Windows 删除那样：目录项类型的「使用中」位清零，分配位图清零，FAT 表项不动。
    pub fn delete(&mut self, placed: &Placed) {
        for index in placed.first_entry..placed.first_entry + placed.entry_count {
            let at = self.entry_offset(placed.dir, index);
            self.image[at] &= 0x7F;
        }
        for &cluster in &placed.clusters {
            let bit = cluster as usize - 2;
            self.bitmap[bit / 8] &= !(1 << (bit % 8));
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        let mut boot = [0u8; SECTOR];
        boot[..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
        boot[3..11].copy_from_slice(b"EXFAT   ");
        boot[72..80].copy_from_slice(&(EXFAT_TOTAL_SECTORS as u64).to_le_bytes());
        boot[80..84].copy_from_slice(&(EXFAT_FAT_OFFSET as u32).to_le_bytes());
        boot[84..88].copy_from_slice(&(self.fat_length as u32).to_le_bytes());
        boot[88..92].copy_from_slice(&((self.heap_offset / SECTOR) as u32).to_le_bytes());
        boot[92..96].copy_from_slice(&self.cluster_count.to_le_bytes());
        boot[96..100].copy_from_slice(&4u32.to_le_bytes());
        boot[100..104].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        boot[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
        boot[108] = 9;
        boot[109] = EXFAT_SPC_SHIFT as u8;
        boot[110] = 1;
        boot[111] = 0x80;
        boot[510] = 0x55;
        boot[511] = 0xAA;
        // 主引导区与备份引导区：引导扇区、8 个扩展引导扇区（只有结尾签名）、OEM、保留、校验和扇区
        let mut region = vec![0u8; 12 * SECTOR];
        region[..SECTOR].copy_from_slice(&boot);
        for sector in 1..9 {
            region[sector * SECTOR + 510] = 0x55;
            region[sector * SECTOR + 511] = 0xAA;
        }
        let mut checksum = 0u32;
        for (index, &byte) in region[..11 * SECTOR].iter().enumerate() {
            if matches!(index, 106 | 107 | 112) {
                continue;
            }
            checksum = checksum.rotate_right(1).wrapping_add(u32::from(byte));
        }
        for word in region[11 * SECTOR..].as_chunks_mut::<4>().0 {
            word.copy_from_slice(&checksum.to_le_bytes());
        }
        self.image[..12 * SECTOR].copy_from_slice(&region);
        self.image[12 * SECTOR..24 * SECTOR].copy_from_slice(&region);

        let fat_at = EXFAT_FAT_OFFSET * SECTOR;
        for (cluster, &value) in self.fat.iter().enumerate() {
            self.image[fat_at + cluster * 4..fat_at + cluster * 4 + 4]
                .copy_from_slice(&value.to_le_bytes());
        }
        let bitmap_at = self.cluster_offset(2);
        self.image[bitmap_at..bitmap_at + self.bitmap.len()].copy_from_slice(&self.bitmap);
        self.image
    }

    fn allocate(&mut self, count: usize, layout: Layout, chain: bool) -> Vec<u32> {
        let mut clusters = Vec::with_capacity(count);
        let mut cluster = match layout {
            Layout::At(first) => first,
            _ => self.next,
        };
        for _ in 0..count {
            let bit = cluster as usize - 2;
            assert!(
                self.bitmap[bit / 8] & (1 << (bit % 8)) == 0,
                "簇 {cluster} 已被占用"
            );
            self.bitmap[bit / 8] |= 1 << (bit % 8);
            clusters.push(cluster);
            cluster += if layout == Layout::Fragmented { 2 } else { 1 };
        }
        if chain {
            for pair in clusters.windows(2) {
                self.fat[pair[0] as usize] = pair[1];
            }
            if let Some(&last) = clusters.last() {
                self.fat[last as usize] = 0xFFFF_FFFF;
            }
        }
        if !matches!(layout, Layout::At(_)) {
            self.next = cluster;
        }
        clusters
    }

    fn cluster_offset(&self, cluster: u32) -> usize {
        self.heap_offset + (cluster as usize - 2) * self.cluster_size()
    }

    fn place(
        &mut self,
        parent: Dir,
        name: &str,
        attributes: u16,
        clusters: Vec<u32>,
        size: u64,
        no_fat_chain: bool,
    ) -> Placed {
        let units: Vec<u16> = name.encode_utf16().collect();
        let name_entries = units.len().div_ceil(15);
        let mut set = vec![[0u8; 32]; 2 + name_entries];
        set[0][0] = 0x85;
        set[0][1] = (1 + name_entries) as u8;
        set[0][4..6].copy_from_slice(&attributes.to_le_bytes());
        let stamp = (u32::from(DOS_DATE) << 16) | u32::from(DOS_TIME);
        for at in [8, 12, 16] {
            set[0][at..at + 4].copy_from_slice(&stamp.to_le_bytes());
        }
        set[1][0] = 0xC0;
        set[1][1] = 0x01 | if no_fat_chain { 0x02 } else { 0 };
        set[1][3] = units.len() as u8;
        set[1][8..16].copy_from_slice(&size.to_le_bytes());
        set[1][20..24].copy_from_slice(&clusters.first().copied().unwrap_or(0).to_le_bytes());
        set[1][24..32].copy_from_slice(&size.to_le_bytes());
        for (index, unit) in units.iter().enumerate() {
            let entry = &mut set[2 + index / 15];
            entry[0] = 0xC1;
            let at = 2 + (index % 15) * 2;
            entry[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
        let mut checksum = 0u16;
        for (entry_index, entry) in set.iter().enumerate() {
            for (position, &byte) in entry.iter().enumerate() {
                if entry_index == 0 && (position == 2 || position == 3) {
                    continue;
                }
                checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
            }
        }
        set[0][2..4].copy_from_slice(&checksum.to_le_bytes());
        let first_entry = self.push_entries(parent.0, &set);
        Placed {
            dir: parent.0,
            first_entry,
            entry_count: set.len(),
            clusters,
            created_dir: None,
        }
    }

    fn push_entries(&mut self, dir: usize, entries: &[[u8; 32]]) -> usize {
        let first = self.dirs[dir].used;
        for entry in entries {
            let index = self.dirs[dir].used;
            assert!(
                index * 32 < EXFAT_DIR_CLUSTERS * self.cluster_size(),
                "目录满了"
            );
            let at = self.entry_offset(dir, index);
            self.image[at..at + 32].copy_from_slice(entry);
            self.dirs[dir].used += 1;
        }
        first
    }

    fn entry_offset(&self, dir: usize, index: usize) -> usize {
        let DirLocation::Clusters(clusters) = &self.dirs[dir].location else {
            unreachable!()
        };
        let per_cluster = self.cluster_size() / 32;
        self.cluster_offset(clusters[index / per_cluster]) + (index % per_cluster) * 32
    }
}

impl Default for ExfatImage {
    fn default() -> Self {
        Self::new()
    }
}

/// 可辨认的文件内容：每 4 个字节是「种子、偏移」的组合，错位或读错簇都能看出来。
pub fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| (index as u32).wrapping_mul(2_654_435_761).to_le_bytes()[index % 4] ^ seed)
        .collect()
}
