//! 自动识别分区表：GPT、MBR（含扩展分区）、无分区表（整块盘就是一个文件系统）。

use yisi_io::{BlockDevice, IoError};

use crate::bytes::le16;
use crate::{Guid, gpt, mbr};

/// 引导扇区与 MBR 共用的结尾标志 `55 AA`。
pub(crate) const BOOT_SIGNATURE: u16 = 0xAA55;
pub(crate) const SIGNATURE_OFFSET: usize = 510;

#[derive(Debug, thiserror::Error)]
pub enum PartError {
    #[error("读取分区表失败")]
    Io(#[from] IoError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Mbr,
    Gpt,
    /// 没有分区表：整块设备当作一个分区，交给文件系统识别。
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionTable {
    pub scheme: Scheme,
    /// 按在表里的顺序排列（MBR 先主分区、再逻辑分区）。
    pub partitions: Vec<Partition>,
    /// 读表时发现、但不妨碍列出分区的问题。
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// 分区编号：MBR 主分区 1–4、逻辑分区从 5 起；GPT 是表项序号加 1；无分区表时为 1。
    pub number: u32,
    /// 起始字节偏移。
    pub offset: u64,
    /// 字节长度，已截到设备末尾。
    pub length: u64,
    pub kind: PartitionKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartitionKind {
    Mbr {
        type_id: u8,
        bootable: bool,
        logical: bool,
    },
    Gpt {
        type_guid: Guid,
        unique_guid: Guid,
        attributes: u64,
        name: String,
    },
    /// 无分区表时的整块设备。
    Whole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// 主 GPT 损坏（签名、校验和或表项校验和不对），改用磁盘末尾的备份 GPT。
    PrimaryGptInvalid,
    /// 有保护性 MBR，但主备 GPT 都读不出，按 MBR 列出。
    GptUnreadable,
    /// 分区表按 4096 字节扇区写成，与设备报告的扇区大小不同（常见于 USB 硬盘盒换过的盘或镜像）。
    SectorSizeMismatch { table: u32, device: u32 },
    /// 扩展分区的链断了或成环，只列出断点之前的逻辑分区。
    ExtendedChainBroken,
    /// 分区超出设备末尾，长度已截短。
    PartitionTruncated { number: u32 },
    /// 分区起点在设备末尾之外，没有列出。
    PartitionOutside { number: u32 },
}

/// 读分区表。识别不出任何分区表时返回 [`Scheme::None`] 和覆盖整块设备的一个分区。
pub fn read_table(device: &dyn BlockDevice) -> Result<PartitionTable, PartError> {
    let sector_size = device.sector_size();
    let mut first = vec![0u8; sector_size.max(512) as usize];
    let read = device.read_at(0, &mut first)?;
    first.truncate(read);

    let mut notes = Vec::new();
    let has_signature = le16(&first, SIGNATURE_OFFSET) == Some(BOOT_SIGNATURE);
    let entries = if has_signature {
        mbr::primary_entries(&first)
    } else {
        Vec::new()
    };

    if entries
        .iter()
        .any(|entry| entry.type_id == mbr::GPT_PROTECTIVE)
    {
        if let Some(found) = gpt::read(device, &mut notes)? {
            return Ok(finish(device, Scheme::Gpt, found, notes));
        }
        notes.push(Note::GptUnreadable);
    }

    if has_signature && !looks_like_boot_sector(&first) && !entries.is_empty() {
        let found = mbr::read(device, &first, &mut notes)?;
        return Ok(finish(device, Scheme::Mbr, found, notes));
    }

    let whole = Partition {
        number: 1,
        offset: 0,
        length: device.size(),
        kind: PartitionKind::Whole,
    };
    Ok(PartitionTable {
        scheme: Scheme::None,
        partitions: vec![whole],
        notes,
    })
}

/// 截到设备末尾，起点在设备之外的不列出。
fn finish(
    device: &dyn BlockDevice,
    scheme: Scheme,
    found: Vec<Partition>,
    mut notes: Vec<Note>,
) -> PartitionTable {
    let size = device.size();
    let mut partitions = Vec::with_capacity(found.len());
    for mut partition in found {
        let Some(room) = size.checked_sub(partition.offset).filter(|&room| room > 0) else {
            notes.push(Note::PartitionOutside {
                number: partition.number,
            });
            continue;
        };
        if partition.length > room {
            partition.length = room;
            notes.push(Note::PartitionTruncated {
                number: partition.number,
            });
        }
        partitions.push(partition);
    }
    PartitionTable {
        scheme,
        partitions,
        notes,
    }
}

/// 第 0 扇区是文件系统的引导扇区而不是 MBR（U 盘直接格式化、不分区的情况）。
///
/// 两者都以 `55 AA` 结尾。引导扇区以跳转指令开头，并且要么带 exFAT / NTFS 的标识，
/// 要么 BPB 的各项参数自洽；MBR 的引导代码一般不以跳转指令开头，即使是，BPB 的位置也不是合理的参数。
pub(crate) fn looks_like_boot_sector(sector: &[u8]) -> bool {
    let jump = matches!(sector.first(), Some(0xEB | 0xE9));
    if !jump {
        return false;
    }
    let oem = sector.get(3..11);
    if oem == Some(b"EXFAT   ".as_slice()) || oem == Some(b"NTFS    ".as_slice()) {
        return true;
    }
    let bytes_per_sector = le16(sector, 11).unwrap_or(0);
    let sectors_per_cluster = sector.get(13).copied().unwrap_or(0);
    let reserved = le16(sector, 14).unwrap_or(0);
    let fats = sector.get(16).copied().unwrap_or(0);
    let media = sector.get(21).copied().unwrap_or(0);
    matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096)
        && sectors_per_cluster.is_power_of_two()
        && reserved > 0
        && matches!(fats, 1 | 2)
        && (media == 0xF0 || media >= 0xF8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples::{self, MbrEntry, MemoryDisk};

    #[test]
    fn blank_device_is_one_whole_partition() {
        let disk = MemoryDisk::new(vec![0; 64 * 512], 512);
        let table = read_table(&disk).unwrap();
        assert_eq!(table.scheme, Scheme::None);
        assert_eq!(table.partitions.len(), 1);
        assert_eq!(table.partitions[0].length, 64 * 512);
    }

    #[test]
    fn boot_sector_at_zero_is_not_taken_for_an_mbr() {
        let mut image = vec![0u8; 64 * 512];
        image[..512].copy_from_slice(&samples::fat_like_boot_sector());
        let table = read_table(&MemoryDisk::new(image, 512)).unwrap();
        assert_eq!(table.scheme, Scheme::None);
    }

    #[test]
    fn partitions_past_the_end_are_truncated_or_dropped() {
        let image = samples::mbr_disk(
            100 * 512,
            &[MbrEntry::new(0x0C, 10, 200), MbrEntry::new(0x07, 150, 10)],
        );
        let table = read_table(&MemoryDisk::new(image, 512)).unwrap();
        assert_eq!(table.scheme, Scheme::Mbr);
        assert_eq!(table.partitions.len(), 1);
        assert_eq!(table.partitions[0].length, 90 * 512);
        assert!(
            table
                .notes
                .contains(&Note::PartitionTruncated { number: 1 })
        );
        assert!(table.notes.contains(&Note::PartitionOutside { number: 2 }));
    }
}
