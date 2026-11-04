//! 改写分区表：加一个分区，或改一个分区的类型。只生成改写后的扇区与写前内容（`yisi_io::SectorWrite`），不写盘。
//!
//! MBR 只改第 0 扇区：加分区放进第一个空的主分区表项（不建扩展分区），改类型只改主分区的类型字节。
//! GPT 改主表的一个表项，重算表项校验和，主表头与备份表头、备份表项区都按改后的主表重写（备份表头记的
//! 位置按主表头的 `AlternateLBA`）。分区表本身有问题（主 GPT 坏了、按别的扇区大小写成、扩展分区链断了）时不改。

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use yisi_io::{BlockDevice, IoError, SectorWrite};

use crate::crc32::crc32;
use crate::{Guid, Note, PartError, PartitionKind, Scheme, gpt, mbr, read_table};

/// GPT 表项里的名字（Windows 磁盘管理建的数据分区也是这个名字）。
pub const GPT_DATA_NAME: &str = "Basic data partition";

/// 怎么改分区表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableEdit {
    /// 加一个分区：起点与长度（字节，按扇区对齐）。MBR 用 `mbr_type`，GPT 用 `gpt_type` 与名字 [`GPT_DATA_NAME`]。
    Add {
        offset: u64,
        length: u64,
        mbr_type: u8,
        gpt_type: Guid,
    },
    /// 把编号为 `number` 的分区的类型改成 `mbr_type`（MBR 主分区）或 `gpt_type`（GPT）。
    SetType {
        number: u32,
        mbr_type: u8,
        gpt_type: Guid,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error(transparent)]
    Table(#[from] PartError),
    #[error("读分区表区域失败")]
    Io(#[from] IoError),
    #[error("这块设备没有分区表")]
    NoTable,
    #[error("这份分区表现在不能改：{0}")]
    Unsupported(&'static str),
    #[error("分区表里没有空的表项")]
    NoFreeSlot,
    #[error("新分区与分区 {0} 重叠")]
    Overlaps(u32),
    #[error("新分区与扩展分区重叠")]
    OverlapsExtended,
    #[error("新分区超出了设备或分区表可用的范围")]
    OutOfRange,
    #[error("位置或长度没有按扇区对齐")]
    Unaligned,
    #[error("分区表里没有分区 {0}")]
    NoPartition(u32),
    #[error("位置或长度超出了 MBR 能记的范围")]
    TooLarge,
    #[error("分区类型已经是这个值")]
    Unchanged,
}

/// 生成改写分区表要写的各段（按偏移排序、互不重叠）。只读设备。
pub fn plan_table_edit(
    device: &dyn BlockDevice,
    edit: &TableEdit,
) -> Result<Vec<SectorWrite>, EditError> {
    let table = read_table(device)?;
    if table.scheme == Scheme::None {
        return Err(EditError::NoTable);
    }
    if let Some(note) = table.notes.first() {
        let reason = match note {
            Note::PrimaryGptInvalid => "主 GPT 已损坏，现在用的是备份",
            Note::GptUnreadable => "GPT 读不出",
            Note::SectorSizeMismatch { .. } => "分区表按别的扇区大小写成",
            Note::ExtendedChainBroken => "扩展分区的链断了",
            Note::PartitionTruncated { .. } | Note::PartitionOutside { .. } => {
                "有分区超出了设备末尾"
            }
        };
        return Err(EditError::Unsupported(reason));
    }
    let sector = u64::from(device.sector_size());
    if let TableEdit::Add { offset, length, .. } = *edit {
        if offset % sector != 0 || length % sector != 0 || length == 0 {
            return Err(EditError::Unaligned);
        }
        let end = offset.checked_add(length).ok_or(EditError::OutOfRange)?;
        if offset < sector || end > device.size() {
            return Err(EditError::OutOfRange);
        }
        if let Some(other) = table
            .partitions
            .iter()
            .find(|partition| overlaps(offset, length, partition.offset, partition.length))
        {
            return Err(EditError::Overlaps(other.number));
        }
    }
    match table.scheme {
        Scheme::Mbr => plan_mbr(device, edit, &table.partitions),
        Scheme::Gpt => plan_gpt(device, edit),
        Scheme::None => Err(EditError::NoTable),
    }
}

fn plan_mbr(
    device: &dyn BlockDevice,
    edit: &TableEdit,
    partitions: &[crate::Partition],
) -> Result<Vec<SectorWrite>, EditError> {
    let sector = u64::from(device.sector_size());
    let mut before = vec![0u8; device.sector_size() as usize];
    device.read_exact_at(0, &mut before)?;
    let mut after = before.clone();
    match *edit {
        TableEdit::Add {
            offset,
            length,
            mbr_type,
            ..
        } => {
            // 扩展分区本身不在分区列表里，单独查重叠
            for entry in mbr::primary_entries(&before) {
                let start = u64::from(entry.start) * sector;
                let size = u64::from(entry.sectors) * sector;
                if mbr::EXTENDED.contains(&entry.type_id) && overlaps(offset, length, start, size) {
                    return Err(EditError::OverlapsExtended);
                }
            }
            let start = u32::try_from(offset / sector).map_err(|_| EditError::TooLarge)?;
            let sectors = u32::try_from(length / sector).map_err(|_| EditError::TooLarge)?;
            start.checked_add(sectors).ok_or(EditError::TooLarge)?;
            let slot = (0..4)
                .find(|&slot| mbr::slot_is_free(&before, slot))
                .ok_or(EditError::NoFreeSlot)?;
            mbr::put_entry(&mut after, slot, 0x00, mbr_type, start, sectors)
                .ok_or(EditError::OutOfRange)?;
        }
        TableEdit::SetType {
            number, mbr_type, ..
        } => {
            let partition = partitions
                .iter()
                .find(|partition| partition.number == number)
                .ok_or(EditError::NoPartition(number))?;
            if matches!(partition.kind, PartitionKind::Mbr { logical: true, .. }) {
                return Err(EditError::Unsupported(
                    "逻辑分区的类型在扩展分区的链里，这里不改",
                ));
            }
            let slot = usize::try_from(number)
                .ok()
                .and_then(|number| number.checked_sub(1))
                .filter(|&slot| slot < 4)
                .ok_or(EditError::NoPartition(number))?;
            if mbr::type_at(&before, slot) == Some(mbr_type) {
                return Err(EditError::Unchanged);
            }
            mbr::set_type(&mut after, slot, mbr_type).ok_or(EditError::NoPartition(number))?;
        }
    }
    Ok(vec![SectorWrite {
        offset: 0,
        before,
        after,
    }])
}

fn plan_gpt(device: &dyn BlockDevice, edit: &TableEdit) -> Result<Vec<SectorWrite>, EditError> {
    let sector = u64::from(device.sector_size());
    let primary =
        gpt::read_primary(device)?.ok_or(EditError::Unsupported("主 GPT 读不出或校验和不对"))?;
    let mut entries = primary.entries.clone();
    match *edit {
        TableEdit::Add {
            offset,
            length,
            gpt_type,
            ..
        } => {
            let first = offset / sector;
            let last = (offset + length) / sector - 1;
            if first < primary.first_usable || last > primary.last_usable {
                return Err(EditError::OutOfRange);
            }
            let slot = entries
                .chunks_exact_mut(primary.entry_size)
                .find(|slot| {
                    slot.get(..16)
                        .is_some_and(|guid| guid.iter().all(|&byte| byte == 0))
                })
                .ok_or(EditError::NoFreeSlot)?;
            gpt::put_entry(
                slot,
                gpt_type,
                random_guid(offset),
                first,
                last,
                0,
                GPT_DATA_NAME,
            )
            .ok_or(EditError::OutOfRange)?;
        }
        TableEdit::SetType {
            number, gpt_type, ..
        } => {
            let index = usize::try_from(number)
                .ok()
                .and_then(|number| number.checked_sub(1))
                .ok_or(EditError::NoPartition(number))?;
            let slot = entries
                .chunks_exact_mut(primary.entry_size)
                .nth(index)
                .ok_or(EditError::NoPartition(number))?;
            let current = slot.get_mut(..16).ok_or(EditError::NoPartition(number))?;
            if current.iter().all(|&byte| byte == 0) {
                return Err(EditError::NoPartition(number));
            }
            if *current == gpt_type.0 {
                return Err(EditError::Unchanged);
            }
            current.copy_from_slice(&gpt_type.0);
        }
    }

    let entries_crc = crc32(&[&entries]);
    let entry_sectors = u64::try_from(entries.len()).unwrap_or(u64::MAX) / sector;
    if !(entries.len() as u64).is_multiple_of(sector) {
        return Err(EditError::Unsupported("表项区不是整数个扇区"));
    }
    let backup_entries_lba = primary
        .alternate_lba
        .checked_sub(entry_sectors)
        .filter(|&lba| lba > primary.last_usable)
        .ok_or(EditError::Unsupported("备份表的位置不对"))?;

    let mut primary_header = primary.header.clone();
    put_u32(&mut primary_header, gpt::ENTRIES_CRC_AT, entries_crc)?;
    gpt::seal_header(&mut primary_header).ok_or(EditError::Unsupported("表头太短"))?;
    let mut backup_header = primary_header.clone();
    put_u64(&mut backup_header, gpt::MY_LBA_AT, primary.alternate_lba)?;
    put_u64(&mut backup_header, gpt::ALTERNATE_LBA_AT, 1)?;
    put_u64(&mut backup_header, gpt::ENTRIES_LBA_AT, backup_entries_lba)?;
    gpt::seal_header(&mut backup_header).ok_or(EditError::Unsupported("表头太短"))?;

    let mut writes = Vec::new();
    for (lba, content) in [
        (1, header_sector(device, 1, &primary_header)?),
        (primary.entries_lba, entries.clone()),
        (backup_entries_lba, entries),
        (
            primary.alternate_lba,
            header_sector(device, primary.alternate_lba, &backup_header)?,
        ),
    ] {
        let offset = lba.checked_mul(sector).ok_or(EditError::OutOfRange)?;
        let mut before = vec![0u8; content.len()];
        device.read_exact_at(offset, &mut before)?;
        writes.push(SectorWrite {
            offset,
            before,
            after: content,
        });
    }
    writes.sort_by_key(|write| write.offset);
    Ok(merge_adjacent(writes))
}

/// `lba` 处的扇区，开头换成表头，之后的部分照旧。
fn header_sector(device: &dyn BlockDevice, lba: u64, header: &[u8]) -> Result<Vec<u8>, EditError> {
    let sector = u64::from(device.sector_size());
    let mut bytes = vec![0u8; device.sector_size() as usize];
    device.read_exact_at(
        lba.checked_mul(sector).ok_or(EditError::OutOfRange)?,
        &mut bytes,
    )?;
    bytes
        .get_mut(..header.len())
        .ok_or(EditError::Unsupported("表头比扇区还长"))?
        .copy_from_slice(header);
    Ok(bytes)
}

/// 首尾相接的段合成一段；内容没变的段去掉。
fn merge_adjacent(writes: Vec<SectorWrite>) -> Vec<SectorWrite> {
    let mut merged: Vec<SectorWrite> = Vec::new();
    for write in writes {
        if let Some(last) = merged.last_mut()
            && last.offset.checked_add(last.len()) == Some(write.offset)
        {
            last.before.extend_from_slice(&write.before);
            last.after.extend_from_slice(&write.after);
            continue;
        }
        merged.push(write);
    }
    merged.retain(|write| write.before != write.after);
    merged
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) -> Result<(), EditError> {
    bytes
        .get_mut(at..at + 4)
        .ok_or(EditError::Unsupported("表头太短"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) -> Result<(), EditError> {
    bytes
        .get_mut(at..at + 8)
        .ok_or(EditError::Unsupported("表头太短"))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn overlaps(a_offset: u64, a_length: u64, b_offset: u64, b_length: u64) -> bool {
    a_offset < b_offset.saturating_add(b_length) && b_offset < a_offset.saturating_add(a_length)
}

/// 新表项的唯一 GUID（第 4 版、随机）。随机数取自系统给每个 `RandomState` 的随机种子，只求不重复。
fn random_guid(salt: u64) -> Guid {
    let state = RandomState::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let mut bytes = [0u8; 16];
    for (index, chunk) in bytes.as_chunks_mut::<8>().0.iter_mut().enumerate() {
        let mut hasher = state.build_hasher();
        hasher.write_usize(index);
        hasher.write_u64(salt);
        hasher.write_u128(nanos);
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    // 磁盘上第三段是小端：版本号在第 7 字节的高 4 位；变体在第 8 字节的高 2 位
    let [.., b7, b8, _, _, _, _, _, _, _] = &mut bytes;
    *b7 = (*b7 & 0x0F) | 0x40;
    *b8 = (*b8 & 0x3F) | 0x80;
    Guid(bytes)
}

#[cfg(test)]
mod tests;
