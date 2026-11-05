//! MBR 与扩展分区（EBR 链）。位置一律以设备的逻辑扇区为单位。

use std::collections::BTreeSet;

use yisi_io::BlockDevice;

use crate::bytes::{le16, le32};
use crate::table::{BOOT_SIGNATURE, SIGNATURE_OFFSET};
use crate::{Note, PartError, Partition, PartitionKind};

pub(crate) const GPT_PROTECTIVE: u8 = 0xEE;
/// 扩展分区的类型：CHS 扩展、LBA 扩展、Linux 扩展。
pub(crate) const EXTENDED: [u8; 3] = [0x05, 0x0F, 0x85];
use crate::boot_sector::MBR_TABLE_OFFSET as TABLE_OFFSET;
const ENTRY_SIZE: usize = 16;
/// CHS 地址写成「超出 CHS 能表示的范围，按 LBA 找」：磁头 254、扇区 63、柱面 1023。
const CHS_BEYOND: [u8; 3] = [0xFE, 0xFF, 0xFF];
/// 逻辑分区最多列这么多个，挡住构造出来的超长链。
const MAX_LOGICAL: u32 = 128;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Entry {
    pub status: u8,
    pub type_id: u8,
    /// 起始扇区（主分区相对磁盘开头，逻辑分区相对所在的 EBR）。
    pub start: u32,
    pub sectors: u32,
}

/// 第 0 扇区里的 4 个表项中不为空、状态字节合法（0x00 或 0x80）的那些。
pub(crate) fn primary_entries(sector: &[u8]) -> Vec<Entry> {
    (0..4)
        .filter_map(|slot| entry(sector, slot))
        .filter(|entry| matches!(entry.status, 0x00 | 0x80))
        .collect()
}

/// 第 `slot` 个表项是空的（类型为 0 或扇区数为 0）。
pub(crate) fn slot_is_free(sector: &[u8], slot: usize) -> bool {
    slot < 4 && entry(sector, slot).is_none()
}

/// 写一个表项（主分区表或 EBR 里的）。CHS 字段一律写成按 LBA 的标记，起点与扇区数按 LBA 记。
/// 位置超出扇区时返回 `None`，什么都不写。
pub(crate) fn put_entry(
    sector: &mut [u8],
    slot: usize,
    status: u8,
    type_id: u8,
    start: u32,
    sectors: u32,
) -> Option<()> {
    let at = TABLE_OFFSET.checked_add(slot.checked_mul(ENTRY_SIZE)?)?;
    let [s0, s1, s2, s3] = start.to_le_bytes();
    let [n0, n1, n2, n3] = sectors.to_le_bytes();
    let [c0, c1, c2] = CHS_BEYOND;
    let entry = [
        status, c0, c1, c2, type_id, c0, c1, c2, s0, s1, s2, s3, n0, n1, n2, n3,
    ];
    sector
        .get_mut(at..at.checked_add(ENTRY_SIZE)?)?
        .copy_from_slice(&entry);
    Some(())
}

/// 第 `slot` 个表项的起始扇区与扇区数（空的表项为 `None`）。
pub(crate) fn entry_start_and_sectors(sector: &[u8], slot: usize) -> Option<(u32, u32)> {
    entry(sector, slot).map(|entry| (entry.start, entry.sectors))
}

/// 把 `from` 的第 `slot` 个表项原样（16 字节）抄到 `to` 的第 `into` 个表项。
pub(crate) fn copy_entry(from: &[u8], slot: usize, to: &mut [u8], into: usize) -> Option<()> {
    let source = TABLE_OFFSET.checked_add(slot.checked_mul(ENTRY_SIZE)?)?;
    let target = TABLE_OFFSET.checked_add(into.checked_mul(ENTRY_SIZE)?)?;
    let bytes: [u8; ENTRY_SIZE] = from.get(source..source + ENTRY_SIZE)?.try_into().ok()?;
    to.get_mut(target..target + ENTRY_SIZE)?
        .copy_from_slice(&bytes);
    Some(())
}

/// 改第 `slot` 个表项的类型字节。
pub(crate) fn set_type(sector: &mut [u8], slot: usize, type_id: u8) -> Option<()> {
    let at = TABLE_OFFSET
        .checked_add(slot.checked_mul(ENTRY_SIZE)?)?
        .checked_add(4)?;
    *sector.get_mut(at)? = type_id;
    Some(())
}

/// 第 `slot` 个表项的类型字节。
pub(crate) fn type_at(sector: &[u8], slot: usize) -> Option<u8> {
    entry(sector, slot).map(|entry| entry.type_id)
}

fn entry(sector: &[u8], slot: usize) -> Option<Entry> {
    let at = TABLE_OFFSET + slot * ENTRY_SIZE;
    let entry = Entry {
        status: *sector.get(at)?,
        type_id: *sector.get(at + 4)?,
        start: le32(sector, at + 8)?,
        sectors: le32(sector, at + 12)?,
    };
    (entry.type_id != 0 && entry.sectors != 0).then_some(entry)
}

pub(crate) fn read(
    device: &dyn BlockDevice,
    first: &[u8],
    notes: &mut Vec<Note>,
) -> Result<Vec<Partition>, PartError> {
    let sector_size = u64::from(device.sector_size());
    let mut partitions = Vec::new();
    let mut extended = None;
    for (slot, entry) in (0..4u32).filter_map(|slot| {
        entry(first, slot as usize)
            .filter(|entry| matches!(entry.status, 0x00 | 0x80))
            .map(|entry| (slot, entry))
    }) {
        if EXTENDED.contains(&entry.type_id) {
            // 只认第一个扩展分区，多个扩展分区本身就不合规范
            extended.get_or_insert(u64::from(entry.start));
            continue;
        }
        if let Some(partition) =
            partition(slot + 1, u64::from(entry.start), entry, sector_size, false)
        {
            partitions.push(partition);
        }
    }
    if let Some(extended_start) = extended {
        read_logical(device, extended_start, sector_size, &mut partitions, notes);
    }
    Ok(partitions)
}

/// 扩展分区链上的一个 EBR：位置（扇区）与内容。
pub(crate) struct Ebr {
    pub lba: u64,
    pub sector: Vec<u8>,
}

impl Ebr {
    /// 这个 EBR 描述的逻辑分区（第 0 项），起点相对这个 EBR。
    pub fn logical(&self) -> Option<Entry> {
        entry(&self.sector, 0)
    }

    /// 指向下一个 EBR 的第 1 项（类型是扩展分区），起点相对扩展分区开头。
    pub fn next(&self) -> Option<Entry> {
        entry(&self.sector, 1).filter(|next| EXTENDED.contains(&next.type_id))
    }
}

/// 第 0 扇区里的扩展分区表项（只认第一个，多个扩展分区本身就不合规范）。
pub(crate) fn extended_entry(first: &[u8]) -> Option<Entry> {
    primary_entries(first)
        .into_iter()
        .find(|entry| EXTENDED.contains(&entry.type_id))
}

/// 从扩展分区开头起沿 EBR 链读出各个 EBR。链断了（读不出、没有签名、成环、过长）时停在断点，第二项为 `true`。
pub(crate) fn ebr_chain(
    device: &dyn BlockDevice,
    extended_start: u64,
    sector_size: u64,
) -> (Vec<Ebr>, bool) {
    let mut chain = Vec::new();
    let mut lba = extended_start;
    let mut visited = BTreeSet::new();
    loop {
        let mut sector = vec![0u8; sector_size as usize];
        let readable = lba
            .checked_mul(sector_size)
            .is_some_and(|offset| device.read_exact_at(offset, &mut sector).is_ok());
        if !visited.insert(lba)
            || chain.len() as u32 >= MAX_LOGICAL
            || !readable
            || le16(&sector, SIGNATURE_OFFSET) != Some(BOOT_SIGNATURE)
        {
            return (chain, true);
        }
        let ebr = Ebr { lba, sector };
        let next = ebr.next();
        chain.push(ebr);
        match next {
            Some(next) => match extended_start.checked_add(u64::from(next.start)) {
                Some(next_lba) => lba = next_lba,
                None => return (chain, true),
            },
            None => return (chain, false),
        }
    }
}

/// 沿 EBR 链读出逻辑分区。链断了时只列断点之前的，记一条说明。
fn read_logical(
    device: &dyn BlockDevice,
    extended_start: u64,
    sector_size: u64,
    partitions: &mut Vec<Partition>,
    notes: &mut Vec<Note>,
) {
    let (chain, broken) = ebr_chain(device, extended_start, sector_size);
    let mut number = 5u32;
    for ebr in &chain {
        if let Some(logical) = ebr.logical()
            && let Some(start) = ebr.lba.checked_add(u64::from(logical.start))
            && let Some(partition) = partition(number, start, logical, sector_size, true)
        {
            partitions.push(partition);
            number += 1;
        }
    }
    if broken {
        notes.push(Note::ExtendedChainBroken);
    }
}

fn partition(
    number: u32,
    start_sector: u64,
    entry: Entry,
    sector_size: u64,
    logical: bool,
) -> Option<Partition> {
    Some(Partition {
        number,
        offset: start_sector.checked_mul(sector_size)?,
        length: u64::from(entry.sectors).checked_mul(sector_size)?,
        kind: PartitionKind::Mbr {
            type_id: entry.type_id,
            bootable: entry.status == 0x80,
            logical,
        },
    })
}

#[cfg(test)]
mod tests {
    use crate::samples::{self, MbrEntry, MemoryDisk};
    use crate::{Note, PartitionKind, Scheme, read_table};

    #[test]
    fn primary_and_logical_partitions_are_listed_in_order() {
        let mut image = samples::mbr_disk(
            4096 * 512,
            &[
                MbrEntry::new(0x0C, 2048, 100).bootable(),
                MbrEntry::new(0x0F, 2200, 1800),
            ],
        );
        samples::ebr_chain(&mut image, 512, 2200, &[(0x07, 64, 300), (0x83, 32, 500)]);
        let table = read_table(&MemoryDisk::new(image, 512)).unwrap();
        assert_eq!(table.scheme, Scheme::Mbr);
        assert!(table.notes.is_empty(), "{:?}", table.notes);
        let summary: Vec<_> = table
            .partitions
            .iter()
            .map(|partition| {
                (
                    partition.number,
                    partition.offset / 512,
                    partition.length / 512,
                )
            })
            .collect();
        // 第一个逻辑分区在扩展分区开头的 EBR 之后 64 扇区；第二个 EBR 紧跟第一个逻辑分区
        assert_eq!(
            summary,
            [(1, 2048, 100), (5, 2264, 300), (6, 2200 + 364 + 32, 500)]
        );
        assert!(matches!(
            table.partitions[0].kind,
            PartitionKind::Mbr {
                type_id: 0x0C,
                bootable: true,
                logical: false
            }
        ));
        assert!(matches!(
            table.partitions[2].kind,
            PartitionKind::Mbr {
                type_id: 0x83,
                logical: true,
                ..
            }
        ));
    }

    #[test]
    fn looping_ebr_chain_stops_with_a_note() {
        let mut image = samples::mbr_disk(4096 * 512, &[MbrEntry::new(0x05, 1000, 2000)]);
        samples::ebr_chain(&mut image, 512, 1000, &[(0x07, 64, 100)]);
        // 让第一个 EBR 的「下一个」指回它自己
        let next = 1000 * 512 + 446 + 16;
        image[next + 4] = 0x05;
        image[next + 8..next + 12].copy_from_slice(&0u32.to_le_bytes());
        image[next + 12..next + 16].copy_from_slice(&10u32.to_le_bytes());
        let table = read_table(&MemoryDisk::new(image, 512)).unwrap();
        assert_eq!(table.partitions.len(), 1);
        assert!(table.notes.contains(&Note::ExtendedChainBroken));
    }
}
