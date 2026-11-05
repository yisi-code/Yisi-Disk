//! 改写分区表的计划：在搜索用的整盘样本上抹掉一个表项、或把类型改错，生成计划、写进样本，再读分区表与诊断。
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;

use yisi_fs::samples::MemoryDisk;
use yisi_io::BlockDevice;
use yisi_part::PartitionKind;

use super::*;
use crate::boot::{Verdict, diagnose};
use crate::samples::search_disk;

/// MBR 第 `slot` 个表项的位置。
fn slot_at(slot: usize) -> usize {
    446 + slot * 16
}

fn device(image: &[u8]) -> Arc<dyn BlockDevice> {
    Arc::new(MemoryDisk::new(image.to_vec(), 512))
}

fn apply(image: &mut [u8], writes: &[SectorWrite]) {
    for write in writes {
        let at = write.offset as usize;
        assert_eq!(&image[at..at + write.before.len()], write.before.as_slice());
        image[at..at + write.after.len()].copy_from_slice(&write.after);
    }
}

#[test]
fn a_lost_partition_is_written_back_with_the_type_of_its_filesystem() {
    let mut disk = search_disk();
    let (offset, length) = disk.exfat;
    // 抹掉第 3 个表项（exFAT）：搜索会把它报成丢失
    disk.image[slot_at(2)..slot_at(3)].fill(0);
    assert_eq!(
        read_table(device(&disk.image).as_ref())
            .unwrap()
            .partitions
            .len(),
        2
    );

    let plan = plan_table_fix(&device(&disk.image), TableFix::AddLost { offset, length }).unwrap();
    assert_eq!(plan.filesystem, FsKind::Exfat);
    assert_eq!(plan.scheme, Scheme::Mbr);
    assert_eq!(
        (
            plan.partition.as_ref().unwrap().number,
            plan.partition.as_ref().unwrap().offset,
            plan.partition.as_ref().unwrap().length
        ),
        (3, offset, length)
    );
    assert!(matches!(
        plan.partition.as_ref().unwrap().kind,
        PartitionKind::Mbr { type_id: 0x07, .. }
    ));
    apply(&mut disk.image, &plan.writes);
    let verdicts: Vec<Verdict> = diagnose(&device(&disk.image))
        .unwrap()
        .partitions
        .iter()
        .map(|partition| partition.verdict)
        .collect();
    assert_eq!(verdicts, [Verdict::Healthy; 3]);
}

#[test]
fn a_lost_partition_whose_primary_boot_sector_is_gone_is_found_by_its_backup() {
    let mut disk = search_disk();
    let (offset, length) = disk.fat32;
    disk.image[slot_at(0)..slot_at(1)].fill(0);
    disk.wipe_primary_boot(offset);
    let plan = plan_table_fix(&device(&disk.image), TableFix::AddLost { offset, length }).unwrap();
    assert_eq!(plan.filesystem, FsKind::Fat32);
    assert!(matches!(
        plan.partition.as_ref().unwrap().kind,
        PartitionKind::Mbr { type_id: 0x0C, .. }
    ));
}

#[test]
fn places_without_a_filesystem_or_already_in_the_table_are_refused() {
    let disk = search_disk();
    let device = device(&disk.image);
    assert!(matches!(
        plan_table_fix(
            &device,
            TableFix::AddLost {
                offset: disk.stray_boot + (1 << 20),
                length: 1 << 20
            }
        ),
        Err(TableFixError::NoFilesystem)
    ));
    let (offset, length) = disk.exfat;
    assert!(matches!(
        plan_table_fix(&device, TableFix::AddLost { offset, length }),
        Err(TableFixError::Edit(EditError::Overlaps(3)))
    ));
}

#[test]
fn a_mismatched_partition_type_is_corrected() {
    let mut disk = search_disk();
    // 把 FAT32 分区的类型改成 Linux（0x83）：诊断报类型不符
    disk.image[slot_at(0) + 4] = 0x83;
    let plan = plan_table_fix(&device(&disk.image), TableFix::SetType { partition: 1 }).unwrap();
    assert_eq!(plan.filesystem, FsKind::Fat32);
    assert!(matches!(
        plan.partition.as_ref().unwrap().kind,
        PartitionKind::Mbr { type_id: 0x0C, .. }
    ));
    apply(&mut disk.image, &plan.writes);
    let report = diagnose(&device(&disk.image)).unwrap();
    assert!(report.partitions[0].table_issues.is_empty());

    assert!(matches!(
        plan_table_fix(&device(&disk.image), TableFix::SetType { partition: 1 }),
        Err(TableFixError::NotApplicable(1))
    ));
    assert!(matches!(
        plan_table_fix(&device(&disk.image), TableFix::SetType { partition: 9 }),
        Err(TableFixError::NoPartition(9))
    ));
}

#[test]
fn a_damaged_primary_gpt_is_rebuilt_from_the_backup() {
    use yisi_part::samples::{GptEntry, gpt_disk};

    let good = gpt_disk(
        8192,
        512,
        &[
            GptEntry::new(BASIC_DATA, 2048, 3047, "a"),
            GptEntry::new(BASIC_DATA, 4096, 6143, "b"),
        ],
    );
    let mut image = good.clone();
    image[512 + 16] ^= 0xFF;
    let plan = plan_table_fix(&device(&image), TableFix::RepairGpt).unwrap();
    assert_eq!(plan.gpt, Some(GptRepair::PrimaryFromBackup));
    assert!(plan.partition.is_none());
    apply(&mut image, &plan.writes);
    assert_eq!(image, good);
    // 两份都完好：没什么可修的
    assert!(matches!(
        plan_table_fix(&device(&good), TableFix::RepairGpt),
        Err(TableFixError::Edit(EditError::GptIntact))
    ));
}

#[test]
fn a_lost_logical_partition_is_written_back_into_the_extended_chain() {
    use yisi_fs::samples::{FatImage, FatType, Layout, pattern};
    use yisi_part::samples::{MbrEntry, ebr_chain, mbr_disk};

    // 扩展分区从第 2048 扇区起；第一个逻辑分区之后，第 20480 扇区起放一个 FAT16 卷，它的 EBR 已经没了
    let mut fat = FatImage::new(FatType::Fat16);
    fat.add_file(
        fat.root(),
        "notes.txt",
        &pattern(3, 3000),
        Layout::Contiguous,
    );
    let fat = fat.finish();
    let start = 20480usize;
    let sectors = fat.len() / 512;
    let total = start + sectors + 4096;
    let mut image = mbr_disk(
        total * 512,
        &[MbrEntry::new(0x0F, 2048, (total - 2048 - 8) as u32)],
    );
    ebr_chain(&mut image, 512, 2048, &[(0x0E, 2048, 4096)]);
    image[start * 512..start * 512 + fat.len()].copy_from_slice(&fat);
    let offset = (start * 512) as u64;
    let length = fat.len() as u64;
    assert_eq!(
        read_table(device(&image).as_ref())
            .unwrap()
            .partitions
            .len(),
        1
    );

    let plan = plan_table_fix(&device(&image), TableFix::AddLost { offset, length }).unwrap();
    let partition = plan.partition.as_ref().unwrap();
    assert_eq!((partition.number, partition.offset), (6, offset));
    assert!(matches!(
        partition.kind,
        PartitionKind::Mbr {
            type_id: 0x0E,
            logical: true,
            ..
        }
    ));
    // 两个 EBR：前一个改指新的，新的写在分区起点前 1 MiB
    assert_eq!(
        plan.writes
            .iter()
            .map(|write| write.offset / 512)
            .collect::<Vec<_>>(),
        [2048, (start - 2048) as u64]
    );
    apply(&mut image, &plan.writes);
    let table = read_table(device(&image).as_ref()).unwrap();
    assert!(table.notes.is_empty(), "{:?}", table.notes);
    assert_eq!(table.partitions.len(), 2);
}
