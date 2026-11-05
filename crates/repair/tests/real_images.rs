//! 对 Windows 自己格式化、写入、删除过的镜像（由 `crates/fs/tests/make-test-images.ps1` 生成）做一致性检查：
//! 这些卷是干净的，检查不应报出任何问题（报出的就是误报）。另外抹掉分区表与 NTFS 卷的主备引导扇区，
//! 丢失分区搜索要按 `$MFT` 的结构找回这些卷，文件照样列得出。
//!
//! 手动运行（PowerShell）：
//! `$env:YISI_FS_TEST_IMAGES = '<镜像目录>'; cargo test -p yisi-repair --test real_images -- --ignored --nocapture`

use std::fs;
use std::sync::Arc;
use std::time::Instant;

use yisi_fs::samples::MemoryDisk;
use yisi_fs::{BootCopy, FsKind, Volume};
use yisi_io::{BlockDevice, RawImage, ScanControl};
use yisi_part::{Partition, PartitionKind};
use yisi_repair::boot_rebuild::overlay_rebuilt;
use yisi_repair::check::{self, Control, Stage};
use yisi_repair::search::{self, Candidate, FoundBy, SearchControl, SearchMode};

struct Quiet;

impl Control for Quiet {
    fn checkpoint(&self) -> bool {
        true
    }

    fn progress(&self, _stage: Stage, _done: u64, _total: u64) {}
}

#[test]
#[ignore = "需要先用 make-test-images.ps1 生成镜像，并设置 YISI_FS_TEST_IMAGES"]
fn windows_formatted_volumes_check_clean() {
    let dir = std::env::var("YISI_FS_TEST_IMAGES").expect("设置 YISI_FS_TEST_IMAGES 为镜像目录");
    let mut images: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "vhd"))
        .collect();
    images.sort();
    assert!(!images.is_empty(), "{dir} 里没有 .vhd");
    let mut false_positives = Vec::new();
    for image in images {
        println!("== {}", image.display());
        let device: Arc<dyn BlockDevice> = Arc::new(RawImage::open(&image, 512).unwrap());
        let table = yisi_part::read_table(device.as_ref()).unwrap();
        for partition in &table.partitions {
            let started = Instant::now();
            let report = check::check(&device, partition.offset, partition.length, &Quiet).unwrap();
            println!(
                "  分区 {}：{:?}，簇 {} 字节 × {}，已占用 {}，文件 {}、目录 {}，用时 {} ms",
                partition.number,
                report.fs,
                report.cluster_size,
                report.cluster_count,
                report.used_clusters,
                report.files,
                report.directories,
                started.elapsed().as_millis()
            );
            println!("    检查了 {:?}，没查 {:?}", report.checked, report.skipped);
            for issue in &report.issues {
                println!("    问题 {issue:?}");
            }
            // 没有文件系统的分区（GPT 的 Microsoft 保留分区）不算误报
            if report.fs != FsKind::Unknown && !report.issues.is_empty() {
                false_positives.push(format!(
                    "{} 分区 {}：{:?}，{} 个问题",
                    image.display(),
                    partition.number,
                    report.fs,
                    report.issues.len()
                ));
            }
        }
    }
    assert!(false_positives.is_empty(), "{false_positives:#?}");
}

impl ScanControl for Quiet {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn progress(&self, _done: u64, _total: u64) {}
}

impl SearchControl for Quiet {
    fn found(&self, _candidates: &[Candidate], _truncated: bool) {}
}

fn root_names(device: Arc<dyn BlockDevice>, offset: u64, length: u64) -> Vec<String> {
    let volume = Volume::open_with(device, offset, length, BootCopy::Primary).unwrap();
    let mut names: Vec<String> = volume
        .list(&volume.root())
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    names.sort();
    names
}

#[test]
#[ignore = "需要先用 make-test-images.ps1 生成镜像，并设置 YISI_FS_TEST_IMAGES"]
fn ntfs_without_boot_sectors_or_table_is_found_by_structure() {
    let dir = std::env::var("YISI_FS_TEST_IMAGES").expect("设置 YISI_FS_TEST_IMAGES 为镜像目录");
    let mut tried = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|ext| ext != "vhd") {
            continue;
        }
        let mut image = fs::read(&path).unwrap();
        let original: Arc<dyn BlockDevice> = Arc::new(MemoryDisk::new(image.clone(), 512));
        let table = yisi_part::read_table(original.as_ref()).unwrap();
        let ntfs: Vec<Partition> = table
            .partitions
            .iter()
            .filter(|partition| {
                Volume::open_with(
                    Arc::clone(&original),
                    partition.offset,
                    partition.length,
                    BootCopy::Primary,
                )
                .is_ok_and(|volume| volume.kind() == FsKind::Ntfs)
            })
            .cloned()
            .collect();
        if ntfs.is_empty() {
            continue;
        }
        println!("== {}", path.display());
        // 抹掉开头 1 MiB 里的分区表（MBR、GPT 主表）与各 NTFS 卷的主备引导扇区（备份在分区最后一个扇区）
        image[..34 * 512].fill(0);
        let footer = image.len() - 512;
        let backup_gpt = footer - 33 * 512;
        image[backup_gpt..footer].fill(0);
        for partition in &ntfs {
            let start = partition.offset as usize;
            let last = (partition.offset + partition.length) as usize - 512;
            image[start..start + 512].fill(0);
            image[last..last + 512].fill(0);
        }
        let device: Arc<dyn BlockDevice> = Arc::new(MemoryDisk::new(image, 512));
        let started = Instant::now();
        let report = search::search(&device, SearchMode::Deep, &Quiet);
        println!(
            "  深度搜索 {} ms：{:#?}",
            started.elapsed().as_millis(),
            report.candidates
        );
        for partition in &ntfs {
            let found = report
                .candidates
                .iter()
                .find(|candidate| candidate.offset == partition.offset)
                .unwrap_or_else(|| panic!("分区 {} 没找回", partition.number));
            assert_eq!(
                (found.kind, found.found_by, found.rebuilt),
                (FsKind::Ntfs, FoundBy::Structure, true)
            );
            // 长度按簇数算：不超过原分区，差不到一个簇加一个扇区
            assert!(found.length <= partition.length, "{found:?}");
            let cluster = Volume::open_with(
                Arc::clone(&original),
                partition.offset,
                partition.length,
                BootCopy::Primary,
            )
            .unwrap()
            .cluster_size();
            assert!(partition.length - found.length < cluster + 512, "{found:?}");
            let rebuilt = Partition {
                number: 0,
                offset: found.offset,
                length: found.length,
                kind: PartitionKind::Whole,
            };
            let view = overlay_rebuilt(&device, &[rebuilt], 0);
            assert_eq!(
                root_names(view, found.offset, found.length),
                root_names(Arc::clone(&original), partition.offset, partition.length)
            );
            tried += 1;
        }
    }
    assert!(tried > 0, "{dir} 里没有 NTFS 分区");
}
