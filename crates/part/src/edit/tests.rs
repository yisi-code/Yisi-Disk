//! 改写分区表：按计划把各段写进样本，再读分区表，核对加上的分区、改后的类型，以及 GPT 主备两份都对。
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use super::*;
use crate::samples::{self, GptEntry, MbrEntry, MemoryDisk};
use crate::{BASIC_DATA, Partition};

const SECTOR: u64 = 512;
const LINUX: Guid = Guid::from_text("0FC63DAF-8483-4772-8E79-3D69D8477DE4");

fn apply(image: &mut [u8], writes: &[SectorWrite]) {
    for write in writes {
        let at = write.offset as usize;
        assert_eq!(&image[at..at + write.before.len()], write.before.as_slice());
        image[at..at + write.after.len()].copy_from_slice(&write.after);
    }
}

fn plan(image: &[u8], edit: &TableEdit) -> Result<Vec<SectorWrite>, EditError> {
    plan_table_edit(&MemoryDisk::new(image.to_vec(), SECTOR as u32), edit)
}

fn table(image: &[u8]) -> crate::PartitionTable {
    read_table(&MemoryDisk::new(image.to_vec(), SECTOR as u32)).unwrap()
}

fn add(offset_sectors: u64, sectors: u64) -> TableEdit {
    TableEdit::Add {
        offset: offset_sectors * SECTOR,
        length: sectors * SECTOR,
        mbr_type: 0x07,
        gpt_type: BASIC_DATA,
    }
}

fn spans(partitions: &[Partition]) -> Vec<(u32, u64, u64)> {
    partitions
        .iter()
        .map(|partition| {
            (
                partition.number,
                partition.offset / SECTOR,
                partition.length / SECTOR,
            )
        })
        .collect()
}

fn mbr_sample() -> Vec<u8> {
    samples::mbr_disk(8192 * 512, &[MbrEntry::new(0x0C, 2048, 1000)])
}

#[test]
fn a_lost_partition_is_added_to_a_free_mbr_slot() {
    let mut image = mbr_sample();
    let writes = plan(&image, &add(4096, 2000)).unwrap();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].offset, 0);
    apply(&mut image, &writes);
    let table = table(&image);
    assert!(table.notes.is_empty(), "{:?}", table.notes);
    assert_eq!(spans(&table.partitions), [(1, 2048, 1000), (2, 4096, 2000)]);
    assert!(matches!(
        table.partitions[1].kind,
        PartitionKind::Mbr {
            type_id: 0x07,
            bootable: false,
            logical: false
        }
    ));
    // 起止的 CHS 写成按 LBA 的标记
    assert_eq!(&image[446 + 16 + 1..446 + 16 + 4], &[0xFE, 0xFF, 0xFF]);
}

#[test]
fn additions_that_do_not_fit_are_refused() {
    let image = mbr_sample();
    assert!(matches!(
        plan(&image, &add(2500, 100)),
        Err(EditError::Overlaps(1))
    ));
    assert!(matches!(
        plan(&image, &add(8000, 500)),
        Err(EditError::OutOfRange)
    ));
    assert!(matches!(
        plan(&image, &add(0, 100)),
        Err(EditError::OutOfRange)
    ));
    let unaligned = TableEdit::Add {
        offset: 4096 * SECTOR + 1,
        length: SECTOR,
        mbr_type: 0x07,
        gpt_type: BASIC_DATA,
    };
    assert!(matches!(
        plan(&image, &unaligned),
        Err(EditError::Unaligned)
    ));

    let full = samples::mbr_disk(
        8192 * 512,
        &[
            MbrEntry::new(0x07, 100, 100),
            MbrEntry::new(0x07, 300, 100),
            MbrEntry::new(0x07, 500, 100),
            MbrEntry::new(0x07, 700, 100),
        ],
    );
    assert!(matches!(
        plan(&full, &add(4096, 100)),
        Err(EditError::NoFreeSlot)
    ));

    let mut extended = samples::mbr_disk(
        8192 * 512,
        &[
            MbrEntry::new(0x0C, 2048, 1000),
            MbrEntry::new(0x0F, 4000, 3000),
        ],
    );
    samples::ebr_chain(&mut extended, 512, 4000, &[(0x07, 64, 100)]);
    // 压着扩展分区边界的不行（整个落在里面的加成逻辑分区，见下面）
    assert!(matches!(
        plan(&extended, &add(6900, 200)),
        Err(EditError::OverlapsExtended)
    ));
}

#[test]
fn a_lost_partition_inside_the_extended_partition_becomes_logical() {
    let mut image = samples::mbr_disk(
        8192 * 512,
        &[
            MbrEntry::new(0x0C, 2048, 1000),
            MbrEntry::new(0x0F, 4000, 3000),
        ],
    );
    samples::ebr_chain(&mut image, 512, 4000, &[(0x07, 64, 100)]);
    // 链尾的第 1 项只清了类型，起点与长度还是旧链接的
    let link = 4000 * 512 + 462;
    image[link + 8..link + 16].copy_from_slice(&[0xC0, 0x07, 0, 0, 0xE4, 0, 0, 0]);
    // 接在第一个逻辑分区之后：没有旧 EBR 的残留，新 EBR 放在起点前 128 个扇区（前 2048 个已在扩展分区之外）
    let writes = plan(&image, &add(6000, 100)).unwrap();
    assert_eq!(
        writes
            .iter()
            .map(|write| write.offset / SECTOR)
            .collect::<Vec<_>>(),
        [4000, 6000 - 128]
    );
    // 只改分区表项区与签名，引导代码区一个字节都不动
    for write in &writes {
        assert_eq!(write.before[..446], write.after[..446]);
    }
    apply(&mut image, &writes);
    let after = table(&image);
    assert!(after.notes.is_empty(), "{:?}", after.notes);
    assert_eq!(
        spans(&after.partitions),
        [(1, 2048, 1000), (5, 4064, 100), (6, 6000, 100)]
    );
    // 新的链尾不带旧链接的残留
    let tail = (6000 - 128) * 512 + 462;
    assert!(image[tail..tail + 16].iter().all(|&byte| byte == 0));

    // 再插在这两个逻辑分区之间：新 EBR 接上原来的下一个，后面的编号顺延
    let writes = plan(&image, &add(5000, 300)).unwrap();
    apply(&mut image, &writes);
    let after = table(&image);
    assert!(after.notes.is_empty(), "{:?}", after.notes);
    assert_eq!(
        spans(&after.partitions),
        [
            (1, 2048, 1000),
            (5, 4064, 100),
            (6, 5000, 300),
            (7, 6000, 100)
        ]
    );

    // 在第一个逻辑分区之前：不改链的开头
    assert!(matches!(
        plan(&image, &add(4010, 20)),
        Err(EditError::Unsupported(_))
    ));
    // 前面没有空着的扇区：每个候选位置都压着别的东西
    let mut image = samples::mbr_disk(8192 * 512, &[MbrEntry::new(0x0F, 1000, 6000)]);
    samples::ebr_chain(&mut image, 512, 1000, &[(0x07, 64, 100)]);
    for lba in [3000 - 2048, 3000 - 128, 3000 - 63, 2999] {
        image[lba * 512] = 0x33;
    }
    assert!(matches!(
        plan(&image, &add(3000, 100)),
        Err(EditError::NoRoomForEbr)
    ));
}

#[test]
fn mbr_partition_type_is_changed_in_place() {
    let mut image = mbr_sample();
    let set = |mbr_type| TableEdit::SetType {
        number: 1,
        mbr_type,
        gpt_type: BASIC_DATA,
    };
    assert!(matches!(
        plan(&image, &set(0x0C)),
        Err(EditError::Unchanged)
    ));
    let writes = plan(&image, &set(0x07)).unwrap();
    apply(&mut image, &writes);
    assert!(matches!(
        table(&image).partitions[0].kind,
        PartitionKind::Mbr { type_id: 0x07, .. }
    ));
    assert!(matches!(
        plan(
            &image,
            &TableEdit::SetType {
                number: 3,
                mbr_type: 0x07,
                gpt_type: BASIC_DATA
            }
        ),
        Err(EditError::NoPartition(3))
    ));

    // 逻辑分区：改它所在 EBR 的第 0 项
    let mut logical = samples::mbr_disk(8192 * 512, &[MbrEntry::new(0x0F, 1000, 3000)]);
    samples::ebr_chain(&mut logical, 512, 1000, &[(0x83, 64, 100), (0x83, 64, 100)]);
    let set_logical = TableEdit::SetType {
        number: 6,
        mbr_type: 0x07,
        gpt_type: BASIC_DATA,
    };
    let writes = plan(&logical, &set_logical).unwrap();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].offset / SECTOR, 1000 + 164);
    apply(&mut logical, &writes);
    let kinds: Vec<u8> = table(&logical)
        .partitions
        .iter()
        .map(|partition| match partition.kind {
            PartitionKind::Mbr { type_id, .. } => type_id,
            _ => 0,
        })
        .collect();
    assert_eq!(kinds, [0x83, 0x07]);
    assert!(matches!(
        plan(&logical, &set_logical),
        Err(EditError::Unchanged)
    ));
}

fn gpt_sample() -> Vec<u8> {
    samples::gpt_disk(8192, 512, &[GptEntry::new(BASIC_DATA, 2048, 3047, "first")])
}

#[test]
fn a_lost_partition_is_added_to_both_gpt_copies() {
    let mut image = gpt_sample();
    let writes = plan(&image, &add(4096, 1000)).unwrap();
    // 主表头与主表项相接、备份表项与备份表头相接，合成两段
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].offset, SECTOR);
    assert_eq!(writes[1].offset + writes[1].len(), 8192 * SECTOR);
    apply(&mut image, &writes);
    let table = table(&image);
    assert!(table.notes.is_empty(), "{:?}", table.notes);
    assert_eq!(spans(&table.partitions), [(1, 2048, 1000), (2, 4096, 1000)]);
    let PartitionKind::Gpt {
        type_guid,
        unique_guid,
        name,
        ..
    } = &table.partitions[1].kind
    else {
        panic!("应为 GPT 分区");
    };
    assert_eq!(*type_guid, BASIC_DATA);
    assert_eq!(name, GPT_DATA_NAME);
    assert!(!unique_guid.is_zero());
    assert_eq!(unique_guid.0[7] >> 4, 4);

    // 把主表头改坏：按备份表读出的也有新分区
    let mut primary_gone = image.clone();
    primary_gone[512 + 16] ^= 0xFF;
    let backup = super::read_table(&MemoryDisk::new(primary_gone, 512)).unwrap();
    assert_eq!(backup.notes, [Note::PrimaryGptInvalid]);
    assert_eq!(
        spans(&backup.partitions),
        [(1, 2048, 1000), (2, 4096, 1000)]
    );
}

#[test]
fn gpt_partition_type_is_changed_and_damaged_tables_are_left_alone() {
    let mut image = samples::gpt_disk(8192, 512, &[GptEntry::new(LINUX, 2048, 3047, "data")]);
    let set = TableEdit::SetType {
        number: 1,
        mbr_type: 0x07,
        gpt_type: BASIC_DATA,
    };
    let writes = plan(&image, &set).unwrap();
    apply(&mut image, &writes);
    assert!(matches!(
        &table(&image).partitions[0].kind,
        PartitionKind::Gpt { type_guid, .. } if *type_guid == BASIC_DATA
    ));
    assert!(matches!(plan(&image, &set), Err(EditError::Unchanged)));

    let mut damaged = gpt_sample();
    damaged[512 + 16] ^= 0xFF;
    assert!(matches!(
        plan(&damaged, &add(4096, 100)),
        Err(EditError::Unsupported(_))
    ));
    let whole = vec![0u8; 8192 * 512];
    assert!(matches!(
        plan(&whole, &add(4096, 100)),
        Err(EditError::NoTable)
    ));
}

#[test]
fn a_damaged_gpt_copy_is_rebuilt_from_the_other() {
    let disk = |image: &[u8]| MemoryDisk::new(image.to_vec(), SECTOR as u32);
    let good = gpt_sample();
    let listed = spans(&table(&good).partitions);
    assert!(matches!(
        plan_gpt_repair(&disk(&good)),
        Err(EditError::GptIntact)
    ));

    // 主表头坏了：按备份重写主表
    let mut image = good.clone();
    image[512 + 16] ^= 0xFF;
    assert_eq!(table(&image).notes, [Note::PrimaryGptInvalid]);
    // 主 GPT 坏了时不改写分区表，要先修 GPT
    assert!(matches!(
        plan(&image, &add(4096, 100)),
        Err(EditError::Unsupported(_))
    ));
    let (repair, writes) = plan_gpt_repair(&disk(&image)).unwrap();
    assert_eq!(repair, GptRepair::PrimaryFromBackup);
    assert!(writes.iter().all(|write| write.offset < 64 * 1024));
    apply(&mut image, &writes);
    let after = table(&image);
    assert!(after.notes.is_empty(), "{:?}", after.notes);
    assert_eq!(spans(&after.partitions), listed);
    assert_eq!(image, good);

    // 备份表项坏了：读的时候照主表列出、另记一条；按主表重写备份
    let mut image = good.clone();
    let backup_entries = image.len() - 33 * 512;
    image[backup_entries + 40] ^= 0x01;
    assert_eq!(table(&image).notes, [Note::BackupGptInvalid]);
    let (repair, writes) = plan_gpt_repair(&disk(&image)).unwrap();
    assert_eq!(repair, GptRepair::BackupFromPrimary);
    assert!(
        writes
            .iter()
            .all(|write| write.offset >= image.len() as u64 - 64 * 1024)
    );
    apply(&mut image, &writes);
    assert!(table(&image).notes.is_empty());
    assert_eq!(image, good);

    // 只是备份坏了不妨碍改写分区表：改写时备份按主表一起重写
    let mut image = good.clone();
    let header = image.len() - 512 + 16;
    image[header] ^= 0xFF;
    assert_eq!(table(&image).notes, [Note::BackupGptInvalid]);
    let writes = plan(&image, &add(4096, 100)).unwrap();
    apply(&mut image, &writes);
    let after = table(&image);
    assert!(after.notes.is_empty(), "{:?}", after.notes);
    assert_eq!(after.partitions.len(), listed.len() + 1);

    // 两份都坏了：读不出 GPT，修不了
    let mut image = good.clone();
    image[512] = b'X';
    let last = image.len() - 512;
    image[last] = b'X';
    assert!(matches!(
        plan_gpt_repair(&disk(&image)),
        Err(EditError::Unsupported(_))
    ));
}
