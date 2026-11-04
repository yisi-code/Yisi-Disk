//! 重建引导扇区：在搜索用的整盘样本上把分区的主、备引导扇区都清掉，推算出的参数与原来的一致（推不出来、
//! 也不影响打开的几项除外），按它打开卷读得出文件；结构缺了的推不出来。
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use std::io::Read;

use yisi_fs::samples::{FatImage, FatType, Layout, MemoryDisk, pattern};
use yisi_fs::{BootCopy, Volume};
use yisi_part::read_table;

use super::*;
use crate::samples::{SearchDisk, search_disk};

const SECTOR: usize = 512;
const SERIAL: u64 = 0x0BAD_F00D_1234_5678;

fn device(image: &[u8]) -> Arc<dyn BlockDevice> {
    Arc::new(MemoryDisk::new(image.to_vec(), SECTOR as u32))
}

fn partition(image: &[u8], number: u32) -> Partition {
    read_table(device(image).as_ref())
        .unwrap()
        .partitions
        .into_iter()
        .find(|partition| partition.number == number)
        .unwrap()
}

fn apply(image: &mut [u8], writes: &[SectorWrite]) {
    for write in writes {
        let at = write.offset as usize;
        assert_eq!(&image[at..at + write.before.len()], write.before.as_slice());
        image[at..at + write.after.len()].copy_from_slice(&write.after);
    }
}

/// 分区 `number` 根目录下 `name` 的内容（按主引导扇区打开）。
fn read_file(image: &[u8], number: u32, name: &str) -> Vec<u8> {
    let partition = partition(image, number);
    let volume = Volume::open_with(
        device(image),
        partition.offset,
        partition.length,
        BootCopy::Primary,
    )
    .unwrap();
    let listing = volume.list(&volume.root()).unwrap();
    let entry = listing
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("根目录里没有 {name}"));
    let mut data = Vec::new();
    volume
        .open_file(&entry.file)
        .unwrap()
        .read_to_end(&mut data)
        .unwrap();
    data
}

/// 两份引导扇区的关键字段，去掉 `skip` 里这几项后逐项比较。
fn same_fields(kind: FsKind, original: &[u8], rebuilt: &[u8], skip: &[&str]) {
    let keep = |fields: Vec<(&'static str, u64)>| -> Vec<(&'static str, u64)> {
        fields
            .into_iter()
            .filter(|(field, _)| !skip.contains(field))
            .collect()
    };
    assert_eq!(
        keep(bpb::significant_fields(kind, rebuilt)),
        keep(bpb::significant_fields(kind, original))
    );
}

/// 清掉分区里 `ranges` 这几段（分区里的偏移、长度）。
fn wipe(disk: &mut SearchDisk, offset: u64, ranges: &[(usize, usize)]) {
    for &(at, len) in ranges {
        let start = offset as usize + at;
        disk.image[start..start + len].fill(0);
    }
}

#[test]
fn fat32_is_rebuilt_from_its_two_fats() {
    let mut disk = search_disk();
    let (offset, _) = disk.fat32;
    let original = disk.image[offset as usize..offset as usize + SECTOR].to_vec();
    wipe(&mut disk, offset, &[(0, SECTOR), (6 * SECTOR, SECTOR)]);
    let part = partition(&disk.image, 1);
    for kind in [FsKind::Fat32, FsKind::Unknown] {
        let rebuilt = rebuild(&device(&disk.image), &part, kind, SERIAL).unwrap();
        assert_eq!(rebuilt.kind, FsKind::Fat32);
        // 样本没写隐藏扇区数（Windows 写分区起点），卷序列号推不出来
        same_fields(
            FsKind::Fat32,
            &original,
            &rebuilt.boot,
            &["hiddenSectors", "volumeSerial"],
        );
        assert_eq!(rebuilt.label.as_deref(), Some("DATA32"));
        // 引导扇区连同 FSInfo 写在开头，备份写在第 6 扇区
        let at: Vec<u64> = rebuilt
            .writes
            .iter()
            .map(|write| write.offset - offset)
            .collect();
        assert_eq!(at, [0, 6 * SECTOR as u64]);
    }
    let rebuilt = rebuild(&device(&disk.image), &part, FsKind::Fat32, SERIAL).unwrap();
    apply(&mut disk.image, &rebuilt.writes);
    assert_eq!(read_file(&disk.image, 1, "notes.txt"), disk.notes);
    assert_eq!(
        diagnose_partition(&device(&disk.image), &part).verdict,
        Verdict::Healthy
    );
}

#[test]
fn exfat_is_rebuilt_from_its_root_directory() {
    let mut disk = search_disk();
    let (offset, _) = disk.exfat;
    let original = disk.image[offset as usize..offset as usize + SECTOR].to_vec();
    wipe(&mut disk, offset, &[(0, 24 * SECTOR)]);
    let part = partition(&disk.image, 3);
    let rebuilt = rebuild(&device(&disk.image), &part, FsKind::Unknown, SERIAL).unwrap();
    assert_eq!(rebuilt.kind, FsKind::Exfat);
    // 样本没写分区起点；FAT 长度取够用的最小值（样本多留了一个扇区）
    same_fields(
        FsKind::Exfat,
        &original,
        &rebuilt.boot,
        &["partitionOffset", "volumeSerial", "fatLength"],
    );
    assert_eq!(rebuilt.label.as_deref(), Some("EXDATA"));
    assert_eq!(rebuilt.writes.len(), 2);
    apply(&mut disk.image, &rebuilt.writes);
    assert_eq!(read_file(&disk.image, 3, "photo.dat"), disk.photo);
    assert_eq!(
        diagnose_partition(&device(&disk.image), &part).verdict,
        Verdict::Healthy
    );
}

#[test]
fn ntfs_is_rebuilt_from_its_master_file_table() {
    let mut disk = search_disk();
    let (offset, length) = disk.ntfs;
    let original = disk.image[offset as usize..offset as usize + SECTOR].to_vec();
    wipe(
        &mut disk,
        offset,
        &[(0, SECTOR), (length as usize - SECTOR, SECTOR)],
    );
    let part = partition(&disk.image, 2);
    let rebuilt = rebuild(&device(&disk.image), &part, FsKind::Unknown, SERIAL).unwrap();
    assert_eq!(rebuilt.kind, FsKind::Ntfs);
    same_fields(
        FsKind::Ntfs,
        &original,
        &rebuilt.boot,
        &["hiddenSectors", "volumeSerial", "volumeSerialHigh"],
    );
    let at: Vec<u64> = rebuilt
        .writes
        .iter()
        .map(|write| write.offset - offset)
        .collect();
    assert_eq!(at, [0, length - SECTOR as u64]);
    apply(&mut disk.image, &rebuilt.writes);
    assert_eq!(read_file(&disk.image, 2, "report.bin"), disk.report);
    assert_eq!(
        diagnose_partition(&device(&disk.image), &part).verdict,
        Verdict::Healthy
    );
}

#[test]
fn sub_directories_confirm_the_cluster_size() {
    // 4 个扇区一簇、根目录下有子目录：每簇扇区数要让子目录的首簇处正好是 `.` 目录项
    let mut sample = FatImage::new(FatType::Fat32);
    let dir = sample.mkdir(sample.root(), "photos");
    sample.add_file(dir.as_dir(), "a.jpg", &pattern(5, 3000), Layout::Contiguous);
    let mut image = sample.finish();
    let original = image[..SECTOR].to_vec();
    image[..SECTOR].fill(0);
    image[6 * SECTOR..7 * SECTOR].fill(0);
    let part = Partition {
        number: 1,
        offset: 0,
        length: image.len() as u64,
        kind: yisi_part::PartitionKind::Whole,
    };
    let rebuilt = rebuild(&device(&image), &part, FsKind::Fat32, SERIAL).unwrap();
    same_fields(FsKind::Fat32, &original, &rebuilt.boot, &["volumeSerial"]);

    // 子目录首簇处被改掉：对不上，不给结果
    let dir_at = sample_dir_offset(&original, dir.first_cluster());
    image[dir_at] = b'X';
    assert!(matches!(
        rebuild(&device(&image), &part, FsKind::Fat32, SERIAL),
        Err(RebuildError::Ambiguous(_))
    ));
}

/// 样本 FAT32 里簇 `cluster` 的字节偏移（按原来的引导扇区算）。
fn sample_dir_offset(boot: &[u8], cluster: u32) -> usize {
    let geometry = bpb::check_fat(boot, None).geometry.unwrap();
    let data = geometry.reserved + geometry.fats * geometry.fat_sectors;
    ((data + u64::from(cluster - 2) * geometry.sectors_per_cluster) * 512) as usize
}

#[test]
fn missing_structures_are_not_guessed() {
    let mut disk = search_disk();
    // FAT32 的第二份 FAT 也没了
    let (offset, _) = disk.fat32;
    let geometry = bpb::check_fat(&disk.image[offset as usize..offset as usize + SECTOR], None)
        .geometry
        .unwrap();
    let second = (geometry.reserved + geometry.fat_sectors) as usize * SECTOR;
    wipe(
        &mut disk,
        offset,
        &[(0, SECTOR), (6 * SECTOR, SECTOR), (second, SECTOR)],
    );
    assert!(matches!(
        rebuild(
            &device(&disk.image),
            &partition(&disk.image, 1),
            FsKind::Fat32,
            SERIAL
        ),
        Err(RebuildError::NotFound(_))
    ));
    // 整个分区都是空的：三种都推不出来
    let (offset, length) = disk.exfat;
    wipe(&mut disk, offset, &[(0, length as usize)]);
    assert!(matches!(
        rebuild(
            &device(&disk.image),
            &partition(&disk.image, 3),
            FsKind::Unknown,
            SERIAL
        ),
        Err(RebuildError::NotFound(_))
    ));
    // FAT12/16 不重建
    assert!(matches!(
        rebuild(
            &device(&disk.image),
            &partition(&disk.image, 3),
            FsKind::Fat16,
            SERIAL
        ),
        Err(RebuildError::Unsupported(FsKind::Fat16))
    ));
}

#[test]
fn rebuilt_partitions_are_overlaid_for_browsing() {
    let mut disk = search_disk();
    let (offset, _) = disk.exfat;
    wipe(&mut disk, offset, &[(0, 24 * SECTOR)]);
    let source = device(&disk.image);
    let partitions = read_table(source.as_ref()).unwrap().partitions;
    let view = overlay_rebuilt(&source, &partitions, SERIAL);
    // 叠加视图上按主引导扇区打得开，源本身没变
    let part = partition(&disk.image, 3);
    let volume = Volume::open_with(view, part.offset, part.length, BootCopy::Primary).unwrap();
    assert_eq!(volume.label(), Some("EXDATA"));
    assert!(
        disk.image[offset as usize..offset as usize + 24 * SECTOR]
            .iter()
            .all(|&b| b == 0)
    );
}

#[test]
fn exfat_fat_length_follows_windows() {
    // Windows 格式化的 160 MiB exFAT（实测）：卷长度 327680 扇区、8 个扇区一簇、FAT 从第 128 扇区起、
    // 簇区从第 512 扇区起、40896 簇，引导扇区里记的 FAT 长度是 328
    assert_eq!(
        exfat_fat_length(327_680, 8, 512, 40_896, 512 - 128),
        Some(328)
    );
    // 簇区之前放不下 Windows 的写法时取够用的最小值，最小值也放不下时不给
    assert_eq!(exfat_fat_length(327_680, 8, 512, 40_896, 325), Some(320));
    assert_eq!(exfat_fat_length(327_680, 8, 512, 40_896, 319), None);
}
