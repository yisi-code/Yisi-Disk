//! 只读打开一块真实的物理盘：列出分区表、识别各分区的文件系统，能浏览的分区列出根目录；
//! NTFS 分区再扫一遍 MFT，报告用时与找到的已删除项数。不读文件内容。需要管理员权限，在管理员 PowerShell 里手动运行，设备路径由环境变量给出：
//! `$env:YISI_FS_TEST_DEVICE = '\\.\PhysicalDrive0'; cargo test -p yisi-fs --test real_disk -- --ignored --nocapture`
//! 不对报过读错误的盘运行。
#![cfg(windows)]

use std::sync::Arc;
use std::time::Instant;

use yisi_fs::Volume;
use yisi_io::{BlockDevice, WindowsDisk};

#[test]
#[ignore = "读真实磁盘，需要管理员权限并设置 YISI_FS_TEST_DEVICE"]
fn physical_disk_partitions_are_listed_and_roots_browsed() {
    let path = std::env::var("YISI_FS_TEST_DEVICE").expect("设置 YISI_FS_TEST_DEVICE");
    let device: Arc<dyn BlockDevice> = Arc::new(WindowsDisk::open(&path).unwrap());
    let table = yisi_part::read_table(device.as_ref()).unwrap();
    println!(
        "{path}：{} 字节，扇区 {}，{:?}，说明 {:?}",
        device.size(),
        device.sector_size(),
        table.scheme,
        table.notes
    );
    assert!(!table.partitions.is_empty());
    for partition in &table.partitions {
        let kind = yisi_fs::detect(device.as_ref(), partition.offset, partition.length).unwrap();
        println!(
            "  分区 {}：偏移 {}，长度 {}，用途 {:?}，{:?}",
            partition.number,
            partition.offset,
            partition.length,
            partition.role(),
            kind
        );
        if !kind.is_browsable() {
            continue;
        }
        let volume = Volume::open(Arc::clone(&device), partition.offset, partition.length).unwrap();
        if volume.needs_scan() {
            let started = Instant::now();
            let mut last = (0, 0);
            let summary = volume
                .scan(&mut |done, total| {
                    last = (done, total);
                    true
                })
                .unwrap()
                .unwrap();
            let elapsed = started.elapsed();
            println!(
                "    扫描 MFT：读 {} 字节，用时 {elapsed:?}（{:.0} MiB/s），{summary:?}",
                last.1,
                last.1 as f64 / 1048576.0 / elapsed.as_secs_f64().max(0.001)
            );
        }
        let listing = volume.list(&volume.root()).unwrap();
        println!(
            "    卷标 {:?}，簇 {} 字节，根目录 {} 项",
            volume.label(),
            volume.cluster_size(),
            listing.entries.len()
        );
        for entry in listing.entries.iter().take(20) {
            println!(
                "      {}{} {} 字节 {:?}{}",
                entry.name,
                if entry.is_dir() { "/" } else { "" },
                entry.size,
                entry.state,
                entry
                    .modified
                    .as_deref()
                    .map(|time| format!(" {time}"))
                    .unwrap_or_default()
            );
        }
    }
}
