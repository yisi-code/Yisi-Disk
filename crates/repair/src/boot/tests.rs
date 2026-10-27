//! 引导扇区诊断：用按规范拼出的 FAT32、exFAT、NTFS 样本放进 MBR 分区，改坏主引导扇区、备份或分区表，核对结论。

use std::sync::Arc;

use yisi_fs::FsKind;
use yisi_fs::samples::{
    ExfatImage, FatImage, FatType, Layout, MemoryDisk, NTFS_ROOT, Noise, NtfsImage, pattern,
};
use yisi_io::BlockDevice;
use yisi_part::samples::{self as part_samples, MbrEntry};

use super::{Issue, PartitionReport, TableIssue, Verdict, diagnose};

const SECTOR: usize = 512;
/// 分区之间、开头与结尾都留 1 MiB。
const GAP: usize = 2048;

/// 一个分区：MBR 类型、卷的内容、分区表里记的扇区数（不给时与卷一样长）。
struct Part {
    type_id: u8,
    image: Vec<u8>,
    sectors: Option<usize>,
}

fn part(type_id: u8, image: Vec<u8>) -> Part {
    Part {
        type_id,
        image,
        sectors: None,
    }
}

fn disk(parts: &[Part]) -> Arc<dyn BlockDevice> {
    let mut entries = Vec::new();
    let mut start = GAP;
    let mut placed = Vec::new();
    for part in parts {
        let volume = part.image.len() / SECTOR;
        let listed = part.sectors.unwrap_or(volume);
        entries.push(MbrEntry::new(part.type_id, start as u32, listed as u32));
        placed.push(start);
        start += volume.max(listed) + GAP;
    }
    let mut image = part_samples::mbr_disk(start * SECTOR, &entries);
    for (part, start) in parts.iter().zip(placed) {
        image[start * SECTOR..start * SECTOR + part.image.len()].copy_from_slice(&part.image);
    }
    Arc::new(MemoryDisk::new(image, SECTOR as u32))
}

fn reports(parts: &[Part]) -> Vec<PartitionReport> {
    diagnose(&disk(parts)).unwrap().partitions
}

fn fat32() -> Vec<u8> {
    let mut sample = FatImage::new(FatType::Fat32);
    let root = sample.root();
    sample.set_label(b"FAT32 DISK ");
    sample.add_file(root, "a.bin", &pattern(1, 5000), Layout::Contiguous);
    sample.finish()
}

fn exfat() -> Vec<u8> {
    let mut sample = ExfatImage::new();
    let root = sample.root();
    sample.set_label("EXFAT DISK");
    sample.add_file(root, "a.bin", &pattern(2, 9000), Layout::Contiguous);
    sample.finish()
}

fn ntfs() -> Vec<u8> {
    let mut sample = NtfsImage::new();
    sample.set_label("NTFS DISK");
    sample.add_file(NTFS_ROOT, "a.bin", &pattern(3, 20_000));
    sample.finish()
}

/// 备份引导扇区在卷里的位置。
fn backup_at(kind: FsKind, image: &[u8]) -> usize {
    match kind {
        FsKind::Fat32 => 6 * SECTOR,
        FsKind::Exfat => 12 * SECTOR,
        _ => image.len() - SECTOR,
    }
}

#[test]
fn healthy_partitions_are_reported_as_such() {
    let all = reports(&[
        part(0x0C, fat32()),
        part(0x07, exfat()),
        part(0x07, ntfs()),
        part(0x06, FatImage::new(FatType::Fat16).finish()),
    ]);
    let kinds: Vec<FsKind> = all.iter().map(|report| report.filesystem).collect();
    assert_eq!(
        kinds,
        [FsKind::Fat32, FsKind::Exfat, FsKind::Ntfs, FsKind::Fat16]
    );
    for report in &all {
        assert_eq!(report.verdict, Verdict::Healthy, "{report:#?}");
        assert!(report.primary.issues.is_empty());
        assert!(report.primary.opened());
        assert!(report.differences.is_empty(), "{report:#?}");
        assert!(report.table_issues.is_empty(), "{report:#?}");
    }
    for report in &all[..3] {
        let backup = report.backup.as_ref().unwrap();
        assert!(backup.is_valid() && backup.opened(), "{report:#?}");
        assert_eq!(backup.label, report.primary.label);
    }
    assert_eq!(all[0].backup.as_ref().unwrap().offset, 6 * 512);
    assert_eq!(all[1].backup.as_ref().unwrap().offset, 12 * 512);
    assert_eq!(all[2].primary.label.as_deref(), Some("NTFS DISK"));
    // FAT16 没有备份
    assert!(all[3].backup.is_none());
}

#[test]
fn broken_primary_with_intact_backup_can_be_browsed_from_the_backup() {
    for (kind, image) in [
        (FsKind::Fat32, fat32()),
        (FsKind::Exfat, exfat()),
        (FsKind::Ntfs, ntfs()),
    ] {
        // 清零：认不出是什么，要靠备份认出来
        let mut zeroed = image.clone();
        zeroed[..SECTOR].fill(0);
        // 改坏扇区大小：主引导扇区还认得出，主备有一个字段不同
        let mut skewed = image.clone();
        skewed[11..13].copy_from_slice(&3u16.to_le_bytes());
        if kind == FsKind::Exfat {
            skewed[108] = 3;
        }
        let all = reports(&[part(0x07, zeroed), part(0x07, skewed)]);
        for report in &all {
            assert_eq!(report.filesystem, kind, "{report:#?}");
            assert_eq!(report.verdict, Verdict::PrimaryDamaged, "{report:#?}");
            let backup = report.backup.as_ref().unwrap();
            assert!(backup.is_valid() && backup.opened(), "{report:#?}");
            assert_eq!(backup.offset as usize, backup_at(kind, &image));
            assert!(report.primary.open.is_none());
        }
        assert_eq!(all[0].primary.issues, [Issue::Blank]);
        assert!(all[0].differences.is_empty());
        let skewed = &all[1];
        assert!(
            skewed
                .primary
                .issues
                .iter()
                .any(|issue| matches!(issue, Issue::BadBytesPerSector(_))),
            "{skewed:#?}"
        );
        assert!(
            skewed
                .differences
                .iter()
                .any(|difference| difference.field == "bytesPerSector"),
            "{skewed:#?}"
        );
    }
}

#[test]
fn both_copies_broken() {
    // NTFS：主引导扇区的 MFT 位置在卷外，备份清零
    let mut ntfs_image = ntfs();
    ntfs_image[0x30..0x38].copy_from_slice(&u64::MAX.to_le_bytes());
    let last = ntfs_image.len() - SECTOR;
    ntfs_image[last..].fill(0);
    // exFAT：主备的引导区各改一个字节，校验和都不对
    let mut exfat_image = exfat();
    exfat_image[SECTOR + 7] ^= 0xFF;
    exfat_image[13 * SECTOR + 7] ^= 0xFF;
    // FAT32：主备都清零，只能按分区类型推测
    let mut fat_image = fat32();
    fat_image[..SECTOR].fill(0);
    fat_image[6 * SECTOR..7 * SECTOR].fill(0);
    // FAT16 没有备份
    let mut fat16 = FatImage::new(FatType::Fat16).finish();
    fat16[13] = 3;

    let all = reports(&[
        part(0x07, ntfs_image),
        part(0x07, exfat_image),
        part(0x0C, fat_image),
        part(0x06, fat16),
    ]);
    let kinds: Vec<FsKind> = all.iter().map(|report| report.filesystem).collect();
    assert_eq!(
        kinds,
        [FsKind::Ntfs, FsKind::Exfat, FsKind::Fat32, FsKind::Fat16]
    );
    for report in &all {
        assert_eq!(report.verdict, Verdict::BothDamaged, "{report:#?}");
        assert!(report.table_issues.is_empty());
    }
    assert_eq!(all[0].primary.issues, [Issue::MftOutside(u64::MAX)]);
    assert_eq!(all[0].backup.as_ref().unwrap().issues, [Issue::Blank]);
    for copy in [&all[1].primary, all[1].backup.as_ref().unwrap()] {
        assert!(
            matches!(
                copy.issues.as_slice(),
                [Issue::ChecksumMismatch {
                    stored: Some(_),
                    ..
                }]
            ),
            "{copy:#?}"
        );
    }
    assert_eq!(all[2].primary.issues, [Issue::Blank]);
    assert_eq!(all[2].backup.as_ref().unwrap().issues, [Issue::Blank]);
    assert_eq!(all[3].primary.issues, [Issue::BadSectorsPerCluster(3)]);
    assert!(all[3].backup.is_none());
}

#[test]
fn damaged_backup_alone_is_reported() {
    let mut image = fat32();
    image[6 * SECTOR + 16] = 7;
    let all = reports(&[part(0x0C, image)]);
    assert_eq!(all[0].verdict, Verdict::BackupDamaged);
    assert!(all[0].primary.opened());
    assert_eq!(
        all[0].backup.as_ref().unwrap().issues,
        [Issue::BadFatCount(7)]
    );
    assert_eq!(all[0].differences.len(), 1);
    assert_eq!(
        (
            all[0].differences[0].field,
            all[0].differences[0].primary,
            all[0].differences[0].backup
        ),
        ("fatCount", 2, 7)
    );
}

#[test]
fn partition_table_disagreeing_with_the_boot_sector() {
    let ntfs_image = ntfs();
    let ntfs_sectors = ntfs_image.len() / SECTOR;
    let fat_image = fat32();
    let fat_sectors = fat_image.len() / SECTOR;
    let all = reports(&[
        // 分区表的长度被改小了：NTFS 的卷与备份都超出分区
        Part {
            type_id: 0x07,
            image: ntfs_image.clone(),
            sectors: Some(ntfs_sectors - 100),
        },
        // 分区表的长度被改大了 4 MiB
        Part {
            type_id: 0x0C,
            image: fat_image,
            sectors: Some(fat_sectors + 8192),
        },
        // NTFS 的分区标成了 FAT32
        part(0x0C, ntfs_image),
    ]);
    let shrunk = &all[0];
    assert_eq!(shrunk.filesystem, FsKind::Ntfs);
    assert_eq!(
        shrunk.table_issues,
        [TableIssue::VolumeLarger {
            volume: (ntfs_sectors as u64 - 1) * 512,
            partition: (ntfs_sectors as u64 - 100) * 512,
        }]
    );
    // 分区最后一个扇区不是备份：主引导扇区正常，备份看起来坏了
    assert_eq!(shrunk.verdict, Verdict::BackupDamaged, "{shrunk:#?}");

    let grown = &all[1];
    assert_eq!(grown.verdict, Verdict::Healthy, "{grown:#?}");
    assert_eq!(
        grown.table_issues,
        [TableIssue::VolumeSmaller {
            volume: fat_sectors as u64 * 512,
            partition: (fat_sectors as u64 + 8192) * 512,
        }]
    );

    let mislabelled = &all[2];
    assert_eq!(mislabelled.filesystem, FsKind::Ntfs);
    assert_eq!(mislabelled.verdict, Verdict::Healthy);
    assert_eq!(mislabelled.table_issues, [TableIssue::TypeMismatch]);
}

#[test]
fn empty_and_unsupported_partitions() {
    let mut bitlocker = vec![0u8; 4 << 20];
    bitlocker[3..11].copy_from_slice(b"-FVE-FS-");
    bitlocker[510] = 0x55;
    bitlocker[511] = 0xAA;
    let all = reports(&[part(0x07, vec![0u8; 4 << 20]), part(0x07, bitlocker)]);
    assert_eq!(all[0].filesystem, FsKind::Unknown);
    assert_eq!(all[0].verdict, Verdict::Unrecognized);
    assert!(all[0].backup.is_none());
    assert_eq!(all[1].filesystem, FsKind::BitLocker);
    assert_eq!(all[1].verdict, Verdict::Unsupported);
}

#[test]
fn mutated_boot_sectors_never_panic() {
    let samples = [fat32(), exfat(), ntfs()];
    let mut noise = Noise::new(23);
    for _ in 0..60 {
        for (index, sample) in samples.iter().enumerate() {
            let mut image = sample.clone();
            let count = 1 + noise.below(16) as usize;
            noise.mutate(&mut image[..SECTOR], count);
            let backup = backup_at([FsKind::Fat32, FsKind::Exfat, FsKind::Ntfs][index], sample);
            let count = noise.below(16) as usize;
            noise.mutate(&mut image[backup..backup + SECTOR], count);
            let type_id = [0x0C, 0x07, 0x07][index];
            let report = &reports(&[part(type_id, image)])[0];
            // 说备份可用时，备份一定打得开
            if report.verdict == Verdict::PrimaryDamaged {
                assert!(
                    report
                        .backup
                        .as_ref()
                        .is_some_and(|backup| backup.is_valid())
                );
            }
        }
    }
}
