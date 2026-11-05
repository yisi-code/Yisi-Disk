//! 复合文档（微软 [MS-CFB]，Office 97–2003 的 doc / xls / ppt、Outlook 的 msg、安装包 msi 都是它）：
//! 512 字节的文件头之后是一个个扇区（版本 3 每个 512 字节，版本 4 每个 4096 字节，第 n 个扇区在
//! `(n + 1) << 扇区位数`）。FAT 记着每个扇区的用途，所在的扇区列在文件头的 DIFAT 表里（前 109 个），
//! 更多的在 DIFAT 扇区链里。文件长度就是最后一个被用到的扇区的结尾：读完全部 FAT 扇区，
//! 找出不是「空闲」的最大扇区号。
//!
//! 按内容改用扩展名：读第一个目录扇区里的各项名字，`WordDocument` 是 doc，`Workbook` / `Book` 是 xls，
//! `PowerPoint Document` 是 ppt，`__substg1.0_…` / `__nameid_version1.0` 是 msg；根项的 CLSID 是 MSI 的是 msi；
//! 都不是时存成 ole（复合文档，类型不明）。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, ReadQueue, Step, Structure, Walker};
use yisi_part::bytes::{le16, le32};

const MAGIC: &[u8] = b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1";
/// 文件头里 DIFAT 表的位置与项数。
const HEADER_DIFAT_AT: usize = 76;
const HEADER_DIFAT_ENTRIES: u32 = 109;
/// FAT 里的特殊值：比它们小的是普通扇区号。空闲扇区是 FREESECT。
const MAX_REGULAR_SECTOR: u32 = 0xFFFF_FFFA;
const FREE_SECTOR: u32 = 0xFFFF_FFFF;
const DIRECTORY_ENTRY_LEN: usize = 128;
/// MSI 安装包根项的 CLSID {000C1084-0000-0000-C000-000000000046}，按存储顺序。
const MSI_CLSID: [u8; 16] = [
    0x84, 0x10, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

pub struct Ole;

static INFO: FormatInfo = FormatInfo {
    extension: "doc",
    variants: &["xls", "ppt", "msg", "msi", "ole"],
    description: "Office 97-2003 document (compound file)",
    max_size: 4 << 30,
    enabled_by_default: true,
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: MAGIC,
}];

impl FileFormat for Ole {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let major = le16(block, 26)?;
        let shift = le16(block, 30)?;
        let plausible = le16(block, 28)? == 0xFFFE
            && matches!((major, shift), (3, 9) | (4, 12))
            && le16(block, 32)? == 6;
        if !plausible {
            return None;
        }
        let fat_sectors = le32(block, 44)?;
        let first_directory = le32(block, 48)?;
        let first_difat = le32(block, 68)?;
        let difat_sectors = le32(block, 72)?;
        let sector_size = 1u64 << shift;
        if fat_sectors == 0 || u64::from(fat_sectors) * sector_size > INFO.max_size {
            return None;
        }
        let mut structure = OleStructure {
            shift,
            entries_per_sector: (sector_size / 4) as u32,
            fat_sectors,
            fat_seen: 0,
            difat_left: difat_sectors,
            queue: ReadQueue::default(),
            current: None,
            last_used: None,
            extension: None,
        };
        // 文件头里的 DIFAT 表：FAT 扇区按顺序是第 0、1、2…… 个
        for index in 0..fat_sectors.min(HEADER_DIFAT_ENTRIES) {
            let sector = le32(block, HEADER_DIFAT_AT + index as usize * 4)?;
            structure.fat_sector(index, sector)?;
        }
        if fat_sectors > HEADER_DIFAT_ENTRIES {
            structure.difat_sector(HEADER_DIFAT_ENTRIES, first_difat)?;
        }
        if first_directory <= MAX_REGULAR_SECTOR {
            structure
                .queue
                .push(structure.position(first_directory), Item::Directory);
        }
        Some(Box::new(Walker::new(INFO.max_size, structure)))
    }
}

#[derive(Clone, Copy)]
enum Item {
    /// 第 `index` 个 FAT 扇区。
    Fat { index: u32 },
    /// DIFAT 扇区，列着从第 `first_index` 个起的 FAT 扇区。
    Difat { first_index: u32 },
    /// 第一个目录扇区。
    Directory,
}

struct OleStructure {
    shift: u16,
    entries_per_sector: u32,
    fat_sectors: u32,
    /// 读过的 FAT 扇区数。
    fat_seen: u32,
    /// 还没读的 DIFAT 扇区数（文件头记的），防止链绕圈。
    difat_left: u32,
    queue: ReadQueue<Item>,
    current: Option<Item>,
    /// 用到的最大扇区号。
    last_used: Option<u64>,
    extension: Option<&'static str>,
}

impl OleStructure {
    fn sector_size(&self) -> u64 {
        1 << self.shift
    }

    fn position(&self, sector: u32) -> u64 {
        (u64::from(sector) + 1) << self.shift
    }

    /// 排上第 `index` 个 FAT 扇区；扇区号不合法时整个文件不认。
    fn fat_sector(&mut self, index: u32, sector: u32) -> Option<()> {
        (sector <= MAX_REGULAR_SECTOR)
            .then(|| self.queue.push(self.position(sector), Item::Fat { index }))
    }

    fn difat_sector(&mut self, first_index: u32, sector: u32) -> Option<()> {
        if sector > MAX_REGULAR_SECTOR || self.difat_left == 0 {
            return None;
        }
        self.difat_left -= 1;
        self.queue
            .push(self.position(sector), Item::Difat { first_index });
        Some(())
    }

    fn note_used(&mut self, sector: u64) {
        self.last_used = Some(self.last_used.map_or(sector, |last| last.max(sector)));
    }

    /// 第一个目录扇区里各项的名字（UTF-16LE）与根项的 CLSID。
    fn classify(&mut self, bytes: &[u8]) {
        let mut found = None;
        for (index, entry) in bytes.chunks(DIRECTORY_ENTRY_LEN).enumerate() {
            let name_len = le16(entry, 64).map_or(0, usize::from).min(64);
            let name: Vec<u16> = entry
                .get(..name_len.saturating_sub(2))
                .unwrap_or_default()
                .chunks(2)
                .filter_map(|pair| le16(pair, 0))
                .collect();
            let name = String::from_utf16_lossy(&name);
            let kind = match name.as_str() {
                "WordDocument" => Some("doc"),
                "Workbook" | "Book" => Some("xls"),
                "PowerPoint Document" => Some("ppt"),
                "__nameid_version1.0" => Some("msg"),
                _ if name.starts_with("__substg1.0_") => Some("msg"),
                _ if index == 0 && entry.get(80..96) == Some(MSI_CLSID.as_slice()) => Some("msi"),
                _ => None,
            };
            if let Some(kind) = kind {
                found = Some(kind);
                break;
            }
        }
        self.extension = Some(found.unwrap_or("ole"));
    }

    fn next(&mut self, cursor: u64) -> Step {
        while let Some((at, item)) = self.queue.pop() {
            if at < cursor {
                // FAT 扇区已经过去了：读不到它就算不出长度
                if matches!(item, Item::Fat { .. } | Item::Difat { .. }) {
                    return Step::Invalid;
                }
                continue;
            }
            self.current = Some(item);
            return Step::Need {
                at,
                len: self.sector_size() as usize,
            };
        }
        match self.last_used {
            Some(last) if self.fat_seen == self.fat_sectors => {
                Step::Done(((last + 2) << self.shift).max(cursor + self.sector_size()))
            }
            _ => Step::Invalid,
        }
    }
}

impl Structure for OleStructure {
    fn first(&self) -> Step {
        match self.queue.first_at() {
            Some(at) => Step::Need {
                at,
                len: self.sector_size() as usize,
            },
            None => Step::Invalid,
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        let item = match self.current.take() {
            Some(item) => item,
            // 第一步：header_check 排在最前面的那一样
            None => match self.queue.pop() {
                Some((_, item)) => item,
                None => return Step::Invalid,
            },
        };
        match item {
            Item::Fat { index } => {
                self.fat_seen += 1;
                let base = u64::from(index) * u64::from(self.entries_per_sector);
                let last = (0..self.entries_per_sector as usize).rev().find(|&entry| {
                    le32(bytes, entry * 4).is_some_and(|value| value != FREE_SECTOR)
                });
                if let Some(entry) = last {
                    self.note_used(base + entry as u64);
                }
            }
            Item::Difat { first_index } => {
                let listed = self.entries_per_sector - 1;
                for offset in 0..listed {
                    let index = first_index + offset;
                    if index >= self.fat_sectors {
                        break;
                    }
                    let Some(sector) = le32(bytes, offset as usize * 4) else {
                        return Step::Invalid;
                    };
                    if self.fat_sector(index, sector).is_none() {
                        return Step::Invalid;
                    }
                }
                if first_index + listed < self.fat_sectors {
                    let Some(next) = le32(bytes, listed as usize * 4) else {
                        return Step::Invalid;
                    };
                    if self.difat_sector(first_index + listed, next).is_none() {
                        return Step::Invalid;
                    }
                }
            }
            Item::Directory => self.classify(bytes),
        }
        self.next(at)
    }

    fn extension(&self) -> Option<&'static str> {
        // doc 是格式本身的扩展名
        self.extension.filter(|&extension| extension != "doc")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::{check_all_splits, run_keeping};

    fn read(data: &[u8]) -> Option<(u64, Option<&'static str>)> {
        let mut recovering = Ole.header_check(data)?;
        let length = run_keeping(&mut recovering, data, 300)?;
        Some((length, recovering.extension()))
    }

    #[test]
    fn length_is_the_end_of_the_last_used_sector() {
        for (version, sectors) in [(3, 20), (3, 300), (4, 12)] {
            let sample = samples::ole(version, sectors, "WordDocument", false);
            let mut data = sample.clone();
            data.extend_from_slice(&[0x55; 700]);
            check_all_splits(
                || Ole.header_check(&data).unwrap(),
                &data,
                Some(sample.len() as u64),
            );
        }
    }

    #[test]
    fn large_files_list_fat_sectors_in_difat_sectors() {
        // 版本 3 每个 FAT 扇区管 128 个扇区，超过 109 个 FAT 扇区要用 DIFAT 扇区
        let sample = samples::ole(3, 128 * 112, "Workbook", false);
        assert_eq!(read(&sample), Some((sample.len() as u64, Some("xls"))));
    }

    #[test]
    fn stream_names_pick_the_extension() {
        for (name, msi, expected) in [
            ("WordDocument", false, None),
            ("Workbook", false, Some("xls")),
            ("PowerPoint Document", false, Some("ppt")),
            ("__substg1.0_0037001F", false, Some("msg")),
            ("SomethingElse", true, Some("msi")),
            ("SomethingElse", false, Some("ole")),
        ] {
            let sample = samples::ole(3, 40, name, msi);
            assert_eq!(
                read(&sample),
                Some((sample.len() as u64, expected)),
                "{name}"
            );
        }
    }

    #[test]
    fn broken_headers_and_truncation_are_rejected() {
        let sample = samples::ole(3, 40, "WordDocument", false);
        let mut wrong_order = sample.clone();
        wrong_order[28] = 0;
        assert!(Ole.header_check(&wrong_order).is_none(), "字节序标记不对");
        let mut wrong_shift = sample.clone();
        wrong_shift[30] = 12;
        assert!(
            Ole.header_check(&wrong_shift).is_none(),
            "版本 3 配 4096 字节扇区"
        );
        let truncated = &sample[..sample.len() - 600];
        check_all_splits(|| Ole.header_check(truncated).unwrap(), truncated, None);
    }
}
