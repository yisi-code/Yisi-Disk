//! 修复引导扇区的计划：在搜索用的整盘样本（FAT32、NTFS、exFAT 各一个分区，都有备份）上改坏主引导扇区或备份，
//! 生成计划、把写入落到样本上，再诊断必须是「正常」，并且写下去的正是另一份的内容。
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;

use yisi_fs::samples::MemoryDisk;
use yisi_io::BlockDevice;

use super::*;
use crate::boot::diagnose;
use crate::samples::{SearchDisk, search_disk};

const SECTOR: usize = 512;

fn device(image: &[u8]) -> Arc<dyn BlockDevice> {
    Arc::new(MemoryDisk::new(image.to_vec(), SECTOR as u32))
}

/// 三个分区：编号、起点、长度、备份引导扇区在分区里的偏移、复制的字节数。
fn partitions(disk: &SearchDisk) -> [(u32, usize, usize, usize, usize); 3] {
    let (fat32, fat32_len) = disk.fat32;
    let (ntfs, ntfs_len) = disk.ntfs;
    let (exfat, exfat_len) = disk.exfat;
    [
        (1, fat32 as usize, fat32_len as usize, 6 * SECTOR, SECTOR),
        (
            2,
            ntfs as usize,
            ntfs_len as usize,
            ntfs_len as usize - SECTOR,
            SECTOR,
        ),
        (
            3,
            exfat as usize,
            exfat_len as usize,
            12 * SECTOR,
            12 * SECTOR,
        ),
    ]
}

fn apply(image: &mut [u8], plan: &BootFixPlan) {
    for write in &plan.writes {
        let at = write.offset as usize;
        assert_eq!(&image[at..at + write.before.len()], write.before.as_slice());
        image[at..at + write.after.len()].copy_from_slice(&write.after);
    }
}

fn verdicts(image: &[u8]) -> Vec<Verdict> {
    diagnose(&device(image))
        .unwrap()
        .partitions
        .iter()
        .map(|partition| partition.verdict)
        .collect()
}

#[test]
fn sample_starts_healthy() {
    let disk = search_disk();
    assert_eq!(verdicts(&disk.image), vec![Verdict::Healthy; 3]);
}

#[test]
fn copies_the_backup_over_a_wiped_primary() {
    let original = search_disk();
    for (number, start, _, backup, length) in partitions(&original) {
        let mut disk = search_disk();
        disk.image[start..start + length].fill(0);
        let plan = plan_boot_fix(&device(&disk.image), number, BootFix::PrimaryFromBackup).unwrap();
        assert_eq!(plan.from, (start + backup) as u64, "分区 {number}");
        assert_eq!(plan.to, start as u64);
        assert_eq!(plan.length, length as u64);
        assert_eq!(plan.writes.len(), 1);
        assert!(plan.writes[0].before.iter().all(|&byte| byte == 0));

        apply(&mut disk.image, &plan);
        assert_eq!(
            verdicts(&disk.image),
            vec![Verdict::Healthy; 3],
            "分区 {number}"
        );
        assert_eq!(
            disk.image[start..start + length],
            original.image[start + backup..start + backup + length],
            "分区 {number}"
        );
        // 只改了这一段
        let mut expected = original.image.clone();
        expected[start..start + length]
            .copy_from_slice(&original.image[start + backup..start + backup + length]);
        assert!(disk.image == expected, "分区 {number}");
    }
}

#[test]
fn rewrites_a_damaged_backup_from_the_primary() {
    let original = search_disk();
    for (number, start, _, backup, length) in partitions(&original) {
        let mut disk = search_disk();
        let at = start + backup;
        // 只改坏备份的签名，其余照旧
        disk.image[at + 510] = 0;
        assert_eq!(
            verdicts(&disk.image)[number as usize - 1],
            Verdict::BackupDamaged
        );
        let plan = plan_boot_fix(&device(&disk.image), number, BootFix::BackupFromPrimary).unwrap();
        assert_eq!(plan.from, start as u64, "分区 {number}");
        assert_eq!(plan.to, at as u64);
        assert_eq!(plan.length, length as u64);

        apply(&mut disk.image, &plan);
        assert_eq!(
            verdicts(&disk.image),
            vec![Verdict::Healthy; 3],
            "分区 {number}"
        );
        assert_eq!(
            disk.image[at..at + length],
            disk.image[start..start + length],
            "分区 {number}"
        );
    }
}

#[test]
fn refuses_when_the_diagnosis_does_not_match_the_direction() {
    let disk = search_disk();
    let healthy = device(&disk.image);
    for fix in [BootFix::PrimaryFromBackup, BootFix::BackupFromPrimary] {
        assert!(matches!(
            plan_boot_fix(&healthy, 1, fix),
            Err(BootFixError::NotApplicable(Verdict::Healthy))
        ));
    }
    let mut wiped = search_disk();
    let (start, _) = wiped.fat32;
    wiped.wipe_primary_boot(start);
    assert!(matches!(
        plan_boot_fix(&device(&wiped.image), 1, BootFix::BackupFromPrimary),
        Err(BootFixError::NotApplicable(Verdict::PrimaryDamaged))
    ));
    assert!(matches!(
        plan_boot_fix(&healthy, 9, BootFix::PrimaryFromBackup),
        Err(BootFixError::NoPartition(9))
    ));
}

#[test]
fn refuses_when_the_target_cannot_be_read() {
    struct Unreadable {
        base: MemoryDisk,
        bad: u64,
    }
    impl BlockDevice for Unreadable {
        fn size(&self) -> u64 {
            self.base.size()
        }
        fn sector_size(&self) -> u32 {
            512
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, yisi_io::IoError> {
            let end = offset + buf.len() as u64;
            if offset <= self.bad && self.bad < end {
                return Err(yisi_io::IoError::UnexpectedEof {
                    offset,
                    expected: buf.len(),
                    read: 0,
                });
            }
            self.base.read_at(offset, buf)
        }
    }
    let disk = search_disk();
    let (start, _) = disk.ntfs;
    // 主引导扇区读不出（诊断为主坏备好），要写的位置也就读不出：没有写前内容就不给计划
    let device: Arc<dyn BlockDevice> = Arc::new(Unreadable {
        base: MemoryDisk::new(disk.image, 512),
        bad: start,
    });
    let result = plan_boot_fix(&device, 2, BootFix::PrimaryFromBackup);
    assert!(
        matches!(result, Err(BootFixError::Unreadable(at)) if at == start),
        "{result:?}"
    );
}

#[test]
fn rebuilds_when_both_copies_are_gone() {
    let original = search_disk();
    // 三个分区的主、备都清掉：每个分区都能重建，写下去后诊断是「正常」，根目录里看得到样本的文件
    let names = ["notes.txt", "report.bin", "photo.dat"];
    for ((number, start, _, backup, length), name) in partitions(&original).into_iter().zip(names) {
        let mut disk = search_disk();
        disk.image[start..start + length].fill(0);
        disk.image[start + backup..start + backup + length].fill(0);
        // MBR 类型 0x07 不分 NTFS 与 exFAT：两处都清空后可能是「认不出」
        assert!(
            matches!(
                verdicts(&disk.image)[number as usize - 1],
                Verdict::BothDamaged | Verdict::Unrecognized
            ),
            "分区 {number}"
        );
        let plan =
            plan_boot_fix(&device(&disk.image), number, BootFix::Rebuild { serial: 7 }).unwrap();
        assert_eq!((plan.from, plan.to), (start as u64, start as u64));
        assert_eq!(plan.writes.len(), 2, "分区 {number}");
        let rebuilt = plan.rebuilt.as_ref().unwrap();
        assert!(
            rebuilt.root.iter().any(|entry| entry == name),
            "分区 {number}：{:?}",
            rebuilt.root
        );
        assert!(rebuilt.root_count >= rebuilt.root.len());
        assert!(
            rebuilt
                .fields
                .iter()
                .any(|(field, _)| *field == "volumeSerial")
        );
        apply(&mut disk.image, &plan);
        assert_eq!(
            verdicts(&disk.image),
            vec![Verdict::Healthy; 3],
            "分区 {number}"
        );
    }
    // 还有一份好的就不重建
    assert!(matches!(
        plan_boot_fix(&device(&original.image), 1, BootFix::Rebuild { serial: 7 }),
        Err(BootFixError::NotApplicable(Verdict::Healthy))
    ));
}
