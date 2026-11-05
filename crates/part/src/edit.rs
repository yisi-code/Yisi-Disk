//! 改写分区表：加一个分区，改一个分区的类型，或者用一份完好的 GPT 修好另一份。只生成改写后的扇区与写前内容
//! （`yisi_io::SectorWrite`），不写盘。
//!
//! MBR：加主分区放进第一个空的主分区表项（不建扩展分区）；整个落在扩展分区里的分区加成逻辑分区：在它前面的空隙写一个
//! 新 EBR、挂进链里（原来的 EBR 残留还在就沿用）。改类型改主分区表项或逻辑分区所在 EBR 的第 0 项。
//! GPT 改主表的一个表项，重算表项校验和，主表头与备份表头、备份表项区都按改后的主表重写（备份表头记的
//! 位置按主表头的 `AlternateLBA`）。分区表本身有问题（主 GPT 坏了、按别的扇区大小写成、扩展分区链断了）时不改；
//! 只是备份 GPT 坏了不妨碍（改写时按主表重写备份）。

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use yisi_io::{BlockDevice, IoError, SectorWrite};

use crate::bytes::le32;
use crate::crc32::crc32;
use crate::table::{BOOT_SIGNATURE, SIGNATURE_OFFSET};
use crate::{Guid, Note, PartError, PartitionKind, Scheme, gpt, mbr, read_table};

/// GPT 表项里的名字（Windows 磁盘管理建的数据分区也是这个名字）。
pub const GPT_DATA_NAME: &str = "Basic data partition";

/// 新 EBR 放在逻辑分区起点前多少个扇区：1 MiB（现在的分区工具）、64 KiB（Windows 在小盘上）、63 个扇区
/// （按柱面分的老盘）、1 个扇区。先找原来那个 EBR 的残留，找不到再用空着的扇区。
const EBR_GAPS: [u64; 4] = [2048, 128, 63, 1];
/// MBR 扇区里分区表项区的起点（之前是引导代码，不动）。
const TABLE_AREA: usize = 446;

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
    /// 把编号为 `number` 的分区的类型改成 `mbr_type`（MBR 主分区或逻辑分区）或 `gpt_type`（GPT）。
    SetType {
        number: u32,
        mbr_type: u8,
        gpt_type: Guid,
    },
}

/// 修 GPT 的方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GptRepair {
    /// 主 GPT 坏了：按磁盘末尾的备份重写主表头与主表项（表项放在 LBA 2）。
    PrimaryFromBackup,
    /// 备份 GPT 坏了：按主表重写备份表项与备份表头。
    BackupFromPrimary,
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
    #[error("位置或长度超出了主引导记录（MBR）能记的范围")]
    TooLarge,
    #[error("分区类型已经是这个值")]
    Unchanged,
    #[error("逻辑分区前面没有空着的扇区放扩展引导记录（EBR）")]
    NoRoomForEbr,
    #[error("主备两份 GUID 分区表（GPT）都完好，不用修")]
    GptIntact,
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
    // 只是备份 GPT 坏了不妨碍：改写时按主表重写备份
    if let Some(note) = table
        .notes
        .iter()
        .find(|note| **note != Note::BackupGptInvalid)
    {
        return Err(EditError::Unsupported(note_reason(note)));
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

fn note_reason(note: &Note) -> &'static str {
    match note {
        Note::PrimaryGptInvalid => "主 GUID 分区表（GPT）已损坏，现在用的是备份：先修复 GPT",
        Note::BackupGptInvalid => "备份 GUID 分区表（GPT）已损坏",
        Note::GptUnreadable => "GUID 分区表（GPT）读不出",
        Note::SectorSizeMismatch { .. } => "分区表按别的扇区大小写成",
        Note::ExtendedChainBroken => "扩展分区的链断了",
        Note::PartitionTruncated { .. } | Note::PartitionOutside { .. } => "有分区超出了设备末尾",
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
            // 扩展分区本身不在分区列表里，单独查：整个落在里面的加成逻辑分区，压着边界的不行
            if let Some(extended) = mbr::extended_entry(&before) {
                let start = u64::from(extended.start) * sector;
                let size = u64::from(extended.sectors) * sector;
                if overlaps(offset, length, start, size) {
                    if offset > start && offset + length <= start + size {
                        return plan_add_logical(device, &extended, offset, length, mbr_type);
                    }
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
                return plan_logical_type(device, &before, number, mbr_type);
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

/// 第 `number` 号分区（逻辑分区从 5 起，按链上的顺序）所在的 EBR。
fn logical_ebr(device: &dyn BlockDevice, first: &[u8], number: u32) -> Result<mbr::Ebr, EditError> {
    let sector = u64::from(device.sector_size());
    let extended = mbr::extended_entry(first).ok_or(EditError::NoPartition(number))?;
    let (chain, _) = mbr::ebr_chain(device, u64::from(extended.start), sector);
    let index = number
        .checked_sub(5)
        .ok_or(EditError::NoPartition(number))? as usize;
    chain
        .into_iter()
        .filter(|ebr| ebr.logical().is_some())
        .nth(index)
        .ok_or(EditError::NoPartition(number))
}

/// 改逻辑分区的类型：它所在 EBR 的第 0 项。
fn plan_logical_type(
    device: &dyn BlockDevice,
    first: &[u8],
    number: u32,
    mbr_type: u8,
) -> Result<Vec<SectorWrite>, EditError> {
    let sector = u64::from(device.sector_size());
    let ebr = logical_ebr(device, first, number)?;
    if mbr::type_at(&ebr.sector, 0) == Some(mbr_type) {
        return Err(EditError::Unchanged);
    }
    let mut after = ebr.sector.clone();
    mbr::set_type(&mut after, 0, mbr_type).ok_or(EditError::NoPartition(number))?;
    Ok(vec![SectorWrite {
        offset: ebr.lba * sector,
        before: ebr.sector,
        after,
    }])
}

/// 把整个落在扩展分区里的分区加成逻辑分区。它前面要有一个逻辑分区（或链开头的 EBR 还空着）：
/// 新 EBR 写在它前面的空隙里（原来的 EBR 残留还在就沿用），前一个 EBR 的第 1 项改指新 EBR，新 EBR 接上原来的下一个。
fn plan_add_logical(
    device: &dyn BlockDevice,
    extended: &mbr::Entry,
    offset: u64,
    length: u64,
    mbr_type: u8,
) -> Result<Vec<SectorWrite>, EditError> {
    let sector = u64::from(device.sector_size());
    let extended_start = u64::from(extended.start);
    let start = offset / sector;
    let sectors = length / sector;
    let end = start + sectors;
    let (chain, broken) = mbr::ebr_chain(device, extended_start, sector);
    if broken {
        return Err(EditError::Unsupported("扩展分区的链断了"));
    }
    let to_u32 = |value: u64| u32::try_from(value).map_err(|_| EditError::TooLarge);
    // 链开头的 EBR 还没有逻辑分区（扩展分区里的逻辑分区都删掉了）：直接写进它
    if let Some(head) = chain.first()
        && head.logical().is_none()
    {
        let mut after = head.sector.clone();
        mbr::put_entry(
            &mut after,
            0,
            0x00,
            mbr_type,
            to_u32(start - head.lba)?,
            to_u32(sectors)?,
        )
        .ok_or(EditError::OutOfRange)?;
        if let Some(next) = head.next()
            && extended_start + u64::from(next.start) < end
        {
            return Err(EditError::OutOfRange);
        }
        return Ok(vec![SectorWrite {
            offset: head.lba * sector,
            before: head.sector.clone(),
            after,
        }]);
    }
    // 接在哪个 EBR 后面：逻辑分区结束在新分区起点之前的最后一个
    let ends: Vec<Option<u64>> = chain
        .iter()
        .map(|ebr| {
            ebr.logical()
                .map(|logical| ebr.lba + u64::from(logical.start) + u64::from(logical.sectors))
        })
        .collect();
    let index = (0..chain.len())
        .rev()
        .find(|&index| {
            ends.get(index)
                .copied()
                .flatten()
                .is_some_and(|stop| stop <= start)
        })
        .ok_or(EditError::Unsupported(
            "要写回的分区在第一个逻辑分区之前，这里不改链的开头",
        ))?;
    let previous = chain.get(index).ok_or(EditError::OutOfRange)?;
    let floor = ends
        .get(index)
        .copied()
        .flatten()
        .ok_or(EditError::OutOfRange)?;
    if let Some(following) = chain.get(index + 1)
        && end > following.lba
    {
        return Err(EditError::OutOfRange);
    }
    // 新 EBR 的位置：原来那个 EBR 的残留（签名在、第 0 项正好指向这个分区）优先，其次是空隙里空着的扇区
    let mut remnant = None;
    let mut blank = None;
    for gap in EBR_GAPS {
        let Some(lba) = start.checked_sub(gap) else {
            continue;
        };
        if lba < floor || lba <= previous.lba || lba <= extended_start {
            continue;
        }
        let mut bytes = vec![0u8; sector as usize];
        device.read_exact_at(lba * sector, &mut bytes)?;
        if le_signature(&bytes)
            && mbr::entry_start_and_sectors(&bytes, 0)
                == Some((to_u32(start - lba)?, to_u32(sectors)?))
        {
            remnant = Some((lba, bytes));
            break;
        }
        if blank.is_none() && bytes.iter().all(|&byte| byte == 0) {
            blank = Some((lba, bytes));
        }
    }
    let (lba, before) = remnant.or(blank).ok_or(EditError::NoRoomForEbr)?;
    let mut fresh = before.clone();
    fresh
        .get_mut(TABLE_AREA..SIGNATURE_OFFSET)
        .ok_or(EditError::OutOfRange)?
        .fill(0);
    mbr::put_entry(
        &mut fresh,
        0,
        0x00,
        mbr_type,
        to_u32(start - lba)?,
        to_u32(sectors)?,
    )
    .ok_or(EditError::OutOfRange)?;
    // 接上原来的下一个 EBR；前一个本是链尾时第 1 项留空，不抄类型已清零的旧链接残留的起点与长度
    if previous.next().is_some() {
        mbr::copy_entry(&previous.sector, 1, &mut fresh, 1).ok_or(EditError::OutOfRange)?;
    }
    fresh
        .get_mut(SIGNATURE_OFFSET..SIGNATURE_OFFSET + 2)
        .ok_or(EditError::OutOfRange)?
        .copy_from_slice(&BOOT_SIGNATURE.to_le_bytes());
    // 前一个 EBR 改指新 EBR：起点相对扩展分区开头，长度到新逻辑分区的末尾
    let mut linked = previous.sector.clone();
    mbr::put_entry(
        &mut linked,
        1,
        0x00,
        0x05,
        to_u32(lba - extended_start)?,
        to_u32(end - lba)?,
    )
    .ok_or(EditError::OutOfRange)?;
    let mut writes = vec![
        SectorWrite {
            offset: previous.lba * sector,
            before: previous.sector.clone(),
            after: linked,
        },
        SectorWrite {
            offset: lba * sector,
            before,
            after: fresh,
        },
    ];
    writes.sort_by_key(|write| write.offset);
    Ok(merge_adjacent(writes))
}

fn le_signature(sector: &[u8]) -> bool {
    crate::bytes::le16(sector, SIGNATURE_OFFSET) == Some(BOOT_SIGNATURE)
}

fn plan_gpt(device: &dyn BlockDevice, edit: &TableEdit) -> Result<Vec<SectorWrite>, EditError> {
    let sector = u64::from(device.sector_size());
    let primary =
        gpt::read_primary(device)?.ok_or(EditError::Unsupported("主 GUID 分区表（GPT）读不出或校验和不对"))?;
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
    gpt_writes(
        device,
        &primary.header,
        entries,
        primary.entries_lba,
        primary.alternate_lba,
        primary.last_usable,
    )
}

/// 用一份完好的 GPT 修好另一份。只读设备。两份都完好时报 [`EditError::GptIntact`]；
/// 备份表头不在磁盘最后一个扇区（盘换过容量）、主表项放不下时不修。
pub fn plan_gpt_repair(
    device: &dyn BlockDevice,
) -> Result<(GptRepair, Vec<SectorWrite>), EditError> {
    let table = read_table(device)?;
    if table.scheme != Scheme::Gpt {
        return Err(EditError::Unsupported(
            "不是 GUID 分区表（GPT），或主备两份都读不出",
        ));
    }
    if let Some(note) = table
        .notes
        .iter()
        .find(|note| matches!(note, Note::SectorSizeMismatch { .. }))
    {
        return Err(EditError::Unsupported(note_reason(note)));
    }
    let sector = u64::from(device.sector_size());
    let last = (device.size() / sector)
        .checked_sub(1)
        .ok_or(EditError::OutOfRange)?;
    match gpt::read_primary(device)? {
        Some(primary) => {
            if gpt::read_copy(device, primary.alternate_lba)?.is_some() {
                return Err(EditError::GptIntact);
            }
            if primary.alternate_lba != last {
                return Err(EditError::Unsupported(
                    "主表记的备份位置不在磁盘最后一个扇区（盘可能换过容量）",
                ));
            }
            let writes = gpt_writes(
                device,
                &primary.header,
                primary.entries.clone(),
                primary.entries_lba,
                primary.alternate_lba,
                primary.last_usable,
            )?;
            Ok((GptRepair::BackupFromPrimary, writes))
        }
        None => {
            let backup = gpt::read_copy(device, last)?.ok_or(EditError::Unsupported(
                "磁盘最后一个扇区上没有完好的备份 GUID 分区表（GPT）",
            ))?;
            // 主表项按规范放在 LBA 2，要在第一个可用扇区之前放得下
            let entry_sectors = (backup.entries.len() as u64).div_ceil(sector);
            if 2 + entry_sectors > backup.first_usable {
                return Err(EditError::Unsupported("主表项放不下"));
            }
            let mut header = backup.header.clone();
            put_u64(&mut header, gpt::MY_LBA_AT, 1)?;
            put_u64(&mut header, gpt::ALTERNATE_LBA_AT, last)?;
            put_u64(&mut header, gpt::ENTRIES_LBA_AT, 2)?;
            let writes = gpt_writes(
                device,
                &header,
                backup.entries.clone(),
                2,
                last,
                backup.last_usable,
            )?;
            Ok((GptRepair::PrimaryFromBackup, writes))
        }
    }
}

/// 按主表头（`header` 是 LBA 1 处那份的样子，表项校验和这里重算）与表项写出主备两份 GPT：主表头、主表项区、
/// 备份表项区（紧挨在备份表头之前）、备份表头。内容没变的段去掉。
fn gpt_writes(
    device: &dyn BlockDevice,
    header: &[u8],
    entries: Vec<u8>,
    entries_lba: u64,
    alternate_lba: u64,
    last_usable: u64,
) -> Result<Vec<SectorWrite>, EditError> {
    let sector = u64::from(device.sector_size());
    if !(entries.len() as u64).is_multiple_of(sector) {
        return Err(EditError::Unsupported("表项区不是整数个扇区"));
    }
    let entries_crc = crc32(&[&entries]);
    let entry_sectors = entries.len() as u64 / sector;
    let backup_entries_lba = alternate_lba
        .checked_sub(entry_sectors)
        .filter(|&lba| lba > last_usable)
        .ok_or(EditError::Unsupported("备份表的位置不对"))?;
    if le32(header, gpt::HEADER_SIZE_AT).is_none() {
        return Err(EditError::Unsupported("表头太短"));
    }

    let mut primary_header = header.to_vec();
    put_u32(&mut primary_header, gpt::ENTRIES_CRC_AT, entries_crc)?;
    gpt::seal_header(&mut primary_header).ok_or(EditError::Unsupported("表头太短"))?;
    let mut backup_header = primary_header.clone();
    put_u64(&mut backup_header, gpt::MY_LBA_AT, alternate_lba)?;
    put_u64(&mut backup_header, gpt::ALTERNATE_LBA_AT, 1)?;
    put_u64(&mut backup_header, gpt::ENTRIES_LBA_AT, backup_entries_lba)?;
    gpt::seal_header(&mut backup_header).ok_or(EditError::Unsupported("表头太短"))?;

    let mut writes = Vec::new();
    for (lba, content) in [
        (1, header_sector(device, 1, &primary_header)?),
        (entries_lba, entries.clone()),
        (backup_entries_lba, entries),
        (
            alternate_lba,
            header_sector(device, alternate_lba, &backup_header)?,
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
