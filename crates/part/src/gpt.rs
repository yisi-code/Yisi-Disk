//! GPT：主表在 LBA 1，备份表头在最后一个 LBA。头与分区表项各有 CRC-32，任何一项对不上都不采用。

use yisi_io::BlockDevice;

use crate::bytes::{le32, le64};
use crate::crc32::crc32;
use crate::{Guid, Note, PartError, Partition, PartitionKind};

pub(crate) const SIGNATURE: &[u8; 8] = b"EFI PART";
/// 表头里各字段的偏移（UEFI 规范第 5 章）。
pub(crate) const HEADER_SIZE_AT: usize = 12;
pub(crate) const HEADER_CRC_AT: usize = 16;
pub(crate) const MY_LBA_AT: usize = 24;
pub(crate) const ALTERNATE_LBA_AT: usize = 32;
pub(crate) const FIRST_USABLE_AT: usize = 40;
pub(crate) const LAST_USABLE_AT: usize = 48;
pub(crate) const ENTRIES_LBA_AT: usize = 72;
pub(crate) const ENTRY_COUNT_AT: usize = 80;
pub(crate) const ENTRY_SIZE_AT: usize = 84;
pub(crate) const ENTRIES_CRC_AT: usize = 88;
/// 规范里表头的大小。
pub(crate) const HEADER_SIZE: usize = 92;
const MIN_HEADER_SIZE: u32 = HEADER_SIZE as u32;
const MIN_ENTRY_SIZE: u32 = 128;
/// 表项区最多读这么多字节（规范的默认是 128 项 × 128 字节 = 16 KiB），挡住构造出来的巨大表项数。
const MAX_ENTRIES_BYTES: u64 = 1 << 20;
/// 表项里名字的位置与长度（36 个 UTF-16 单元）。
const NAME_OFFSET: usize = 56;
const NAME_BYTES: usize = 72;

struct Header {
    entries_lba: u64,
    entry_count: u32,
    entry_size: u32,
    entries_crc: u32,
    alternate_lba: u64,
}

/// 依次尝试设备的扇区大小下的主表、备份表；设备报告 512 字节扇区时再按 4096 字节扇区试一次。
/// 都读不出时返回 `None`。
pub(crate) fn read(
    device: &dyn BlockDevice,
    notes: &mut Vec<Note>,
) -> Result<Option<Vec<Partition>>, PartError> {
    let device_sector = device.sector_size();
    let mut sizes = vec![device_sector];
    if device_sector == 512 {
        sizes.push(4096);
    }
    for sector_size in sizes {
        let sector = u64::from(sector_size);
        let primary = header_at(device, sector, 1)?;
        let from_primary = match &primary {
            Some(header) => entries(device, sector, header)?,
            None => None,
        };
        let found = match from_primary {
            Some(found) => {
                // 主表完好时也看一眼备份：坏了另记一条，分区照主表列出
                if let Some(header) = &primary
                    && !copy_ok(device, sector, header.alternate_lba)?
                {
                    notes.push(Note::BackupGptInvalid);
                }
                Some(found)
            }
            None => {
                // 备份表头的位置：主表头里记着；主表头坏了就取设备最后一个扇区
                let last = device.size() / sector;
                let alternate = primary
                    .as_ref()
                    .map(|header| header.alternate_lba)
                    .filter(|&lba| lba > 1 && lba < last)
                    .or_else(|| last.checked_sub(1));
                let backup = match alternate {
                    Some(lba) if lba > 1 => header_at(device, sector, lba)?,
                    _ => None,
                };
                let found = match backup {
                    Some(header) => entries(device, sector, &header)?,
                    None => None,
                };
                if found.is_some() {
                    notes.push(Note::PrimaryGptInvalid);
                }
                found
            }
        };
        if let Some(found) = found {
            if sector_size != device_sector {
                notes.push(Note::SectorSizeMismatch {
                    table: sector_size,
                    device: device_sector,
                });
            }
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// `lba` 处有一份完好的 GPT：表头与表项区的校验和都对。位置在设备之外时不算。
fn copy_ok(device: &dyn BlockDevice, sector: u64, lba: u64) -> Result<bool, PartError> {
    let last = device.size() / sector;
    if lba <= 1 || lba >= last {
        return Ok(false);
    }
    Ok(match header_at(device, sector, lba)? {
        Some(header) => entries(device, sector, &header)?.is_some(),
        None => false,
    })
}

/// 读 `lba` 处的表头并校验：签名、表头大小、表头 CRC、表头记录的自身位置、表项大小。
fn header_at(device: &dyn BlockDevice, sector: u64, lba: u64) -> Result<Option<Header>, PartError> {
    let Some(offset) = lba.checked_mul(sector) else {
        return Ok(None);
    };
    let mut bytes = vec![0u8; sector as usize];
    let read = device.read_at(offset, &mut bytes)?;
    bytes.truncate(read);
    if bytes.get(..8) != Some(SIGNATURE.as_slice()) {
        return Ok(None);
    }
    let Some(header_size) = le32(&bytes, HEADER_SIZE_AT) else {
        return Ok(None);
    };
    if header_size < MIN_HEADER_SIZE || u64::from(header_size) > sector {
        return Ok(None);
    }
    let Some(mut checked) = bytes.get(..header_size as usize).map(<[u8]>::to_vec) else {
        return Ok(None);
    };
    let stored_crc = le32(&checked, HEADER_CRC_AT);
    if let Some(field) = checked.get_mut(HEADER_CRC_AT..HEADER_CRC_AT + 4) {
        field.fill(0);
    }
    if stored_crc != Some(crc32(&[&checked])) || le64(&bytes, MY_LBA_AT) != Some(lba) {
        return Ok(None);
    }
    let header = (|| {
        Some(Header {
            alternate_lba: le64(&bytes, ALTERNATE_LBA_AT)?,
            entries_lba: le64(&bytes, ENTRIES_LBA_AT)?,
            entry_count: le32(&bytes, ENTRY_COUNT_AT)?,
            entry_size: le32(&bytes, ENTRY_SIZE_AT)?,
            entries_crc: le32(&bytes, ENTRIES_CRC_AT)?,
        })
    })();
    Ok(header.filter(|header| {
        header.entry_size >= MIN_ENTRY_SIZE && header.entry_size.is_multiple_of(8)
    }))
}

/// 读表项区并核对 CRC，列出类型不为零的表项。
fn entries(
    device: &dyn BlockDevice,
    sector: u64,
    header: &Header,
) -> Result<Option<Vec<Partition>>, PartError> {
    let total = u64::from(header.entry_count) * u64::from(header.entry_size);
    let Some(offset) = header.entries_lba.checked_mul(sector) else {
        return Ok(None);
    };
    if total > MAX_ENTRIES_BYTES {
        return Ok(None);
    }
    let mut bytes = vec![0u8; total as usize];
    if device.read_at(offset, &mut bytes)? != bytes.len() || crc32(&[&bytes]) != header.entries_crc
    {
        return Ok(None);
    }
    let mut partitions = Vec::new();
    for (index, entry) in bytes.chunks_exact(header.entry_size as usize).enumerate() {
        if let Some(partition) = entry_partition(index, entry, sector) {
            partitions.push(partition);
        }
    }
    Ok(Some(partitions))
}

/// 填上表头的校验和：先把校验和字段清零，再对整个表头（`header` 就是表头那么长）算 CRC-32。
pub(crate) fn seal_header(header: &mut [u8]) -> Option<()> {
    header.get_mut(HEADER_CRC_AT..HEADER_CRC_AT + 4)?.fill(0);
    let crc = crc32(&[header]);
    header
        .get_mut(HEADER_CRC_AT..HEADER_CRC_AT + 4)?
        .copy_from_slice(&crc.to_le_bytes());
    Some(())
}

/// 写一个表项（先清空整项）：类型、唯一 GUID、首尾 LBA（含）、属性、名字（最多 36 个 UTF-16 单元）。
pub(crate) fn put_entry(
    slot: &mut [u8],
    type_guid: Guid,
    unique_guid: Guid,
    first: u64,
    last: u64,
    attributes: u64,
    name: &str,
) -> Option<()> {
    slot.get_mut(..NAME_OFFSET + NAME_BYTES)?;
    slot.fill(0);
    slot.get_mut(..16)?.copy_from_slice(&type_guid.0);
    slot.get_mut(16..32)?.copy_from_slice(&unique_guid.0);
    slot.get_mut(32..40)?.copy_from_slice(&first.to_le_bytes());
    slot.get_mut(40..48)?.copy_from_slice(&last.to_le_bytes());
    slot.get_mut(48..56)?
        .copy_from_slice(&attributes.to_le_bytes());
    for (index, unit) in name.encode_utf16().take(NAME_BYTES / 2).enumerate() {
        let at = NAME_OFFSET + index * 2;
        slot.get_mut(at..at + 2)?
            .copy_from_slice(&unit.to_le_bytes());
    }
    Some(())
}

/// 一份 GPT（表头与它的表项区），改写、修复分区表时用。表头与表项的校验和都对才返回。
pub(crate) struct GptCopy {
    /// 表头那么长的字节（不含扇区里表头之后的部分）。
    pub header: Vec<u8>,
    pub entries: Vec<u8>,
    pub entries_lba: u64,
    pub entry_size: usize,
    pub alternate_lba: u64,
    pub first_usable: u64,
    pub last_usable: u64,
}

/// 按设备的扇区大小读主 GPT。读不出、校验和不对时为 `None`。
pub(crate) fn read_primary(device: &dyn BlockDevice) -> Result<Option<GptCopy>, PartError> {
    read_copy(device, 1)
}

/// 按设备的扇区大小读 `lba` 处的一份 GPT（主表在 LBA 1，备份在主表头记的 `AlternateLBA`）。
pub(crate) fn read_copy(device: &dyn BlockDevice, lba: u64) -> Result<Option<GptCopy>, PartError> {
    let sector = u64::from(device.sector_size());
    if lba == 0 || lba >= device.size() / sector {
        return Ok(None);
    }
    let Some(header) = header_at(device, sector, lba)? else {
        return Ok(None);
    };
    let total = u64::from(header.entry_count) * u64::from(header.entry_size);
    if total > MAX_ENTRIES_BYTES {
        return Ok(None);
    }
    let mut bytes = vec![0u8; sector as usize];
    if device.read_at(lba * sector, &mut bytes)? != bytes.len() {
        return Ok(None);
    }
    let size = le32(&bytes, HEADER_SIZE_AT).map_or(0, |size| size as usize);
    let (Some(first_usable), Some(last_usable), Some(head)) = (
        le64(&bytes, FIRST_USABLE_AT),
        le64(&bytes, LAST_USABLE_AT),
        bytes.get(..size),
    ) else {
        return Ok(None);
    };
    let head = head.to_vec();
    let Some(offset) = header.entries_lba.checked_mul(sector) else {
        return Ok(None);
    };
    let mut entries = vec![0u8; total as usize];
    if device.read_at(offset, &mut entries)? != entries.len()
        || crc32(&[&entries]) != header.entries_crc
    {
        return Ok(None);
    }
    Ok(Some(GptCopy {
        header: head,
        entries,
        entries_lba: header.entries_lba,
        entry_size: header.entry_size as usize,
        alternate_lba: header.alternate_lba,
        first_usable,
        last_usable,
    }))
}

fn entry_partition(index: usize, entry: &[u8], sector: u64) -> Option<Partition> {
    let type_guid = Guid(entry.get(..16)?.try_into().ok()?);
    if type_guid.is_zero() {
        return None;
    }
    let first = le64(entry, 32)?;
    let last = le64(entry, 40)?;
    let sectors = last.checked_sub(first)?.checked_add(1)?;
    let name_units: Vec<u16> = entry
        .get(NAME_OFFSET..NAME_OFFSET + NAME_BYTES)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .take_while(|&unit| unit != 0)
        .collect();
    Some(Partition {
        number: u32::try_from(index).ok()?.checked_add(1)?,
        offset: first.checked_mul(sector)?,
        length: sectors.checked_mul(sector)?,
        kind: PartitionKind::Gpt {
            type_guid,
            unique_guid: Guid(entry.get(16..32)?.try_into().ok()?),
            attributes: le64(entry, 48)?,
            name: String::from_utf16_lossy(&name_units),
        },
    })
}

#[cfg(test)]
mod tests {
    use crate::samples::{self, GptEntry, MemoryDisk};
    use crate::{Guid, Note, PartitionKind, Scheme, read_table};

    const BASIC_DATA: Guid = Guid::from_text("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7");

    fn sample(sector: u32) -> Vec<u8> {
        samples::gpt_disk(
            8192,
            sector,
            &[
                GptEntry::new(BASIC_DATA, 34 + 6, 100, "数据"),
                GptEntry::new(BASIC_DATA, 200, 400, "second"),
            ],
        )
    }

    /// 每个分区：编号、起始字节、字节长度、名字。
    type Listed = Vec<(u32, u64, u64, String)>;

    fn summary(image: Vec<u8>, sector: u32) -> (Scheme, Listed, Vec<Note>) {
        let table = read_table(&MemoryDisk::new(image, sector)).unwrap();
        let partitions = table
            .partitions
            .iter()
            .map(|partition| {
                let PartitionKind::Gpt { name, .. } = &partition.kind else {
                    panic!("应为 GPT 分区");
                };
                (
                    partition.number,
                    partition.offset,
                    partition.length,
                    name.clone(),
                )
            })
            .collect();
        (table.scheme, partitions, table.notes)
    }

    #[test]
    fn primary_table_is_read_with_names() {
        let (scheme, partitions, notes) = summary(sample(512), 512);
        assert_eq!(scheme, Scheme::Gpt);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(
            partitions,
            [
                (1, 40 * 512, 61 * 512, "数据".to_owned()),
                (2, 200 * 512, 201 * 512, "second".to_owned()),
            ]
        );
    }

    #[test]
    fn damaged_primary_falls_back_to_backup() {
        let mut image = sample(512);
        image[512 + 16] ^= 0xFF; // 主表头的校验和
        let (scheme, partitions, notes) = summary(image, 512);
        assert_eq!(scheme, Scheme::Gpt);
        assert_eq!(partitions.len(), 2);
        assert_eq!(notes, [Note::PrimaryGptInvalid]);

        // 主表项被改：表头完好但表项校验和不对，同样改用备份
        let mut image = sample(512);
        image[2 * 512 + 40] ^= 0x01;
        let (_, partitions, notes) = summary(image, 512);
        assert_eq!(partitions[0].1, 40 * 512);
        assert_eq!(notes, [Note::PrimaryGptInvalid]);
    }

    #[test]
    fn both_tables_damaged_is_reported_and_listed_as_mbr() {
        let mut image = sample(512);
        image[512] = b'X';
        let last = image.len() - 512;
        image[last] = b'X';
        let table = read_table(&MemoryDisk::new(image, 512)).unwrap();
        assert_eq!(table.scheme, Scheme::Mbr);
        assert!(table.notes.contains(&Note::GptUnreadable));
    }

    #[test]
    fn table_written_with_4k_sectors_is_found_on_a_512_device() {
        let (scheme, partitions, notes) = summary(sample(4096), 512);
        assert_eq!(scheme, Scheme::Gpt);
        assert_eq!(partitions[0].1, 40 * 4096);
        assert_eq!(
            notes,
            [Note::SectorSizeMismatch {
                table: 4096,
                device: 512
            }]
        );
    }
}
