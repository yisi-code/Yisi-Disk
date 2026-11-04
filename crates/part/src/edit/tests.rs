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
    assert!(matches!(
        plan(&extended, &add(6000, 100)),
        Err(EditError::OverlapsExtended)
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

    let mut logical = samples::mbr_disk(8192 * 512, &[MbrEntry::new(0x0F, 1000, 3000)]);
    samples::ebr_chain(&mut logical, 512, 1000, &[(0x83, 64, 100)]);
    assert!(matches!(
        plan(
            &logical,
            &TableEdit::SetType {
                number: 5,
                mbr_type: 0x07,
                gpt_type: BASIC_DATA
            }
        ),
        Err(EditError::Unsupported(_))
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
