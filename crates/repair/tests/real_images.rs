//! 对 Windows 自己格式化、写入、删除过的镜像（由 `crates/fs/tests/make-test-images.ps1` 生成）做一致性检查：
//! 这些卷是干净的，检查不应报出任何问题（报出的就是误报）。
//!
//! 手动运行（PowerShell）：
//! `$env:YISI_FS_TEST_IMAGES = '<镜像目录>'; cargo test -p yisi-repair --test real_images -- --ignored --nocapture`

use std::fs;
use std::time::Instant;

use yisi_io::{BlockDevice, RawImage};
use yisi_repair::check::{self, Control, FsType, Stage};

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
        let device = RawImage::open(&image, 512).unwrap();
        let table = yisi_part::read_table(&device as &dyn BlockDevice).unwrap();
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
            if report.fs != FsType::Unknown && !report.issues.is_empty() {
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
