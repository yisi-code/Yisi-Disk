//! FAT12 / FAT16 / FAT32：引导扇区（BPB）、目录（含长文件名）、删除项识别。依据微软公开的 FAT 规范。

use yisi_part::bytes::{le16, le32};

use crate::clusters::{Clusters, FatWidth, Link};
use crate::entry::{ClusterRef, Entry, FileRef, Listing, Location, State, dos_time};
use crate::reader::FileReader;
use crate::region::{ChunkCache, Region};
use crate::{FsError, FsKind, MAX_DIR_BYTES, MAX_ENTRIES};

const ENTRY: usize = 32;
const ATTR_READ_ONLY_ETC: u8 = 0x0F;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const DELETED: u8 = 0xE5;
/// 名字首字节确实是 0xE5 时，目录项里存的是 0x05。
const KANJI_E5: u8 = 0x05;
/// 长文件名最多 20 个目录项（255 个字符）。
const MAX_LFN_PARTS: usize = 20;

/// 引导扇区里的几何参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub width: FatWidth,
    pub bytes_per_sector: u64,
    pub sectors_per_cluster: u64,
    pub reserved: u64,
    pub fats: u64,
    pub root_entries: u64,
    pub total_sectors: u64,
    pub fat_sectors: u64,
    pub root_cluster: u32,
    pub active_fat: u64,
    pub cluster_count: u32,
    pub label: Option<String>,
}

impl Geometry {
    fn root_dir_sectors(&self) -> u64 {
        (self.root_entries * ENTRY as u64).div_ceil(self.bytes_per_sector)
    }

    fn first_data_sector(&self) -> u64 {
        self.reserved + self.fats * self.fat_sectors + self.root_dir_sectors()
    }
}

/// 解析引导扇区；参数不自洽时返回 `None`。类型按簇数判定（规范第 3.5 节），
/// 只是 BPB 里 16 位的 FAT 扇区数为 0 时按 FAT32 处理（小容量的 FAT32 也存在）。
pub(crate) fn parse_boot(sector: &[u8]) -> Option<Geometry> {
    let bytes_per_sector = u64::from(le16(sector, 11)?);
    let sectors_per_cluster = u64::from(*sector.get(13)?);
    let reserved = u64::from(le16(sector, 14)?);
    let fats = u64::from(*sector.get(16)?);
    let root_entries = u64::from(le16(sector, 17)?);
    let total16 = u64::from(le16(sector, 19)?);
    let media = *sector.get(21)?;
    let fat16_sectors = u64::from(le16(sector, 22)?);
    let total32 = u64::from(le32(sector, 32)?);
    if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096)
        || !sectors_per_cluster.is_power_of_two()
        || sectors_per_cluster > 128
        || reserved == 0
        || !matches!(fats, 1 | 2)
        || !(media == 0xF0 || media >= 0xF8)
        || le16(sector, 510) != Some(0xAA55)
    {
        return None;
    }
    let total_sectors = if total16 != 0 { total16 } else { total32 };
    let is_fat32 = fat16_sectors == 0;
    let fat_sectors = if is_fat32 {
        u64::from(le32(sector, 36)?)
    } else {
        fat16_sectors
    };
    if total_sectors == 0 || fat_sectors == 0 || (is_fat32 && root_entries != 0) {
        return None;
    }
    let mut geometry = Geometry {
        width: FatWidth::Fat32,
        bytes_per_sector,
        sectors_per_cluster,
        reserved,
        fats,
        root_entries,
        total_sectors,
        fat_sectors,
        root_cluster: 0,
        active_fat: 0,
        cluster_count: 0,
        label: None,
    };
    let data_sectors = total_sectors.checked_sub(geometry.first_data_sector())?;
    let clusters = data_sectors / sectors_per_cluster;
    geometry.width = if is_fat32 {
        FatWidth::Fat32
    } else if clusters < 4085 {
        FatWidth::Fat12
    } else if clusters < 65525 {
        FatWidth::Fat16
    } else {
        return None;
    };
    // FAT 本身装得下的表项数也限制了簇数
    let fat_bits: u64 = match geometry.width {
        FatWidth::Fat12 => 12,
        FatWidth::Fat16 => 16,
        _ => 32,
    };
    let fat_entries = fat_sectors * bytes_per_sector * 8 / fat_bits;
    let clusters = clusters.min(fat_entries.saturating_sub(2)).min(0x0FFF_FFF5);
    if clusters == 0 {
        return None;
    }
    geometry.cluster_count = u32::try_from(clusters).ok()?;

    let label_at = if is_fat32 {
        let flags = le16(sector, 40)?;
        // 第 7 位置 1 表示不镜像，只用低 4 位指定的那一份 FAT
        if flags & 0x80 != 0 {
            geometry.active_fat = u64::from(flags & 0x0F);
            if geometry.active_fat >= fats {
                return None;
            }
        }
        geometry.root_cluster = le32(sector, 44)?;
        66
    } else {
        38
    };
    if sector.get(label_at) == Some(&0x29) {
        geometry.label = sector
            .get(label_at + 5..label_at + 16)
            .and_then(short_label);
    }
    Some(geometry)
}

pub(crate) struct Fat {
    kind: FsKind,
    clusters: Clusters,
    /// FAT12/16 的根目录区：分区内偏移与字节数；FAT32 为 `None`，根目录在簇里。
    fixed_root: Option<(u64, u64)>,
    root_cluster: u32,
    label: Option<String>,
}

impl Fat {
    /// 按引导扇区 `boot`（主的或备份的）打开。
    pub fn open(region: Region, boot: &[u8]) -> Result<Self, FsError> {
        let geometry = parse_boot(boot).ok_or(FsError::Invalid("FAT 引导扇区的参数不自洽"))?;
        let bps = geometry.bytes_per_sector;
        let fat_bytes = geometry.fat_sectors * bps;
        let fat_start = (geometry.reserved + geometry.active_fat * geometry.fat_sectors) * bps;
        let heap_offset = geometry.first_data_sector() * bps;
        let cluster_size = geometry.sectors_per_cluster * bps;
        if heap_offset >= region.length() {
            return Err(FsError::Invalid("FAT 的数据区在分区之外"));
        }
        // 分区被截短时，簇区只算到分区末尾
        let fits = (region.length() - heap_offset) / cluster_size;
        let count = u64::from(geometry.cluster_count).min(fits);
        let count = u32::try_from(count).map_err(|_| FsError::Invalid("簇数过大"))?;
        let kind = match geometry.width {
            FatWidth::Fat12 => FsKind::Fat12,
            FatWidth::Fat16 => FsKind::Fat16,
            _ => FsKind::Fat32,
        };
        let fixed_root = (kind != FsKind::Fat32).then(|| {
            let offset = (geometry.reserved + geometry.fats * geometry.fat_sectors) * bps;
            (offset, geometry.root_entries * ENTRY as u64)
        });
        let clusters = Clusters {
            fat: ChunkCache::new(region.clone(), fat_start, fat_bytes),
            region,
            heap_offset,
            cluster_size,
            count,
            width: geometry.width,
        };
        if kind == FsKind::Fat32 && !clusters.is_valid(geometry.root_cluster) {
            return Err(FsError::Invalid("FAT32 根目录的簇号不合法"));
        }
        let mut fat = Self {
            kind,
            clusters,
            fixed_root,
            root_cluster: geometry.root_cluster,
            label: geometry.label,
        };
        // 根目录里的卷标比引导扇区里的准（改卷标时 Windows 只改根目录）
        if let Ok((bytes, _)) = fat.dir_bytes(&fat.root())
            && let Some(label) = root_label(&bytes)
        {
            fat.label = Some(label);
        }
        Ok(fat)
    }

    pub fn kind(&self) -> FsKind {
        self.kind
    }

    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    pub fn cluster_size(&self) -> u64 {
        self.clusters.cluster_size
    }

    pub fn root(&self) -> FileRef {
        FileRef {
            size: 0,
            is_dir: true,
            deleted: false,
            location: Location::Clusters(ClusterRef {
                first_cluster: if self.fixed_root.is_some() {
                    0
                } else {
                    self.root_cluster
                },
                valid_size: 0,
                contiguous: false,
            }),
        }
    }

    pub fn list(&self, dir: &FileRef) -> Result<Listing, FsError> {
        if !dir.is_dir {
            return Err(FsError::BadRef);
        }
        let (bytes, overwritten) = self.dir_bytes(dir)?;
        let raw = parse_dir(&bytes, dir.deleted);
        let mut entries = Vec::with_capacity(raw.entries.len());
        for item in raw.entries {
            entries.push(self.entry(item)?);
        }
        Ok(Listing {
            entries,
            truncated: raw.truncated,
            overwritten,
        })
    }

    pub fn open_file(&self, file: &FileRef) -> Result<FileReader, FsError> {
        if file.is_dir {
            return Err(FsError::BadRef);
        }
        let at = file.clusters()?;
        let needed = self.clusters.clusters_for(file.size);
        let runs = if at.contiguous {
            self.clusters.contiguous(at.first_cluster, needed).0
        } else {
            self.clusters.chain(at.first_cluster, needed)?.runs
        };
        Ok(FileReader::new(&self.clusters, &runs, file.size, file.size))
    }

    /// 目录的原始内容；第二个值为 `true` 表示这个已删除的目录已被覆盖。
    fn dir_bytes(&self, dir: &FileRef) -> Result<(Vec<u8>, bool), FsError> {
        if dir.is_fixed_root() {
            let Some((offset, length)) = self.fixed_root else {
                return Err(FsError::BadRef);
            };
            let mut bytes = vec![0u8; length as usize];
            self.clusters.region.read_exact(offset, &mut bytes)?;
            return Ok((bytes, false));
        }
        let first = dir.clusters()?.first_cluster;
        let cs = self.clusters.cluster_size;
        let max_clusters = (MAX_DIR_BYTES / cs).max(1);
        if !dir.deleted {
            let chain = self.clusters.chain(first, max_clusters)?;
            return Ok((self.clusters.read_runs(&chain.runs, MAX_DIR_BYTES)?, false));
        }

        // 已删除的目录：簇链已清零，只能从首簇起往后找连续的空闲簇，
        // 直到遇到目录结束标记或已被占用的簇。首簇开头必须还是 `.` 与 `..`，否则已被覆盖。
        let mut bytes = self.first_cluster(first)?;
        if !starts_with_dots(&bytes) {
            return Ok((Vec::new(), true));
        }
        let mut cluster = first;
        let mut count = 1u64;
        while count < max_clusters && !has_end_marker(&bytes) {
            let Some(next) = cluster
                .checked_add(1)
                .filter(|&next| self.clusters.is_valid(next))
            else {
                break;
            };
            if self.clusters.link(next)? != Link::Free {
                break;
            }
            let (run, _) = self.clusters.contiguous(next, 1);
            bytes.extend(self.clusters.read_runs(&run, cs)?);
            cluster = next;
            count += 1;
        }
        Ok((bytes, false))
    }

    fn entry(&self, raw: RawEntry) -> Result<Entry, FsError> {
        let deleted = raw.deleted;
        let size = if raw.is_dir { 0 } else { raw.size };
        let file = FileRef {
            size,
            is_dir: raw.is_dir,
            deleted,
            location: Location::Clusters(ClusterRef {
                first_cluster: raw.first_cluster,
                valid_size: size,
                contiguous: deleted,
            }),
        };
        let state = if deleted {
            // 早年的 Windows 删除 FAT32 文件时会清掉首簇的高 16 位。高位为 0、簇数又超过 16 位时，
            // 按低位找到的簇已被占用，说不准是真被覆盖了，还是首簇本来就不在这里
            let ambiguous = self.kind == FsKind::Fat32
                && raw.cluster_high == 0
                && u64::from(self.clusters.count) + 2 > 0xFFFF;
            match self.deleted_state(&file, raw.first_cluster)? {
                State::Overwritten if ambiguous => State::Unknown,
                state => state,
            }
        } else {
            State::Live
        };
        Ok(Entry {
            name: raw.name,
            short_name: Some(raw.short_name),
            name_uncertain: raw.name_uncertain,
            size: file.size,
            modified: raw.modified,
            created: raw.created,
            state,
            position: (raw.is_dir || size > 0)
                .then(|| self.clusters.offset(raw.first_cluster))
                .flatten(),
            unsupported: None,
            file,
        })
    }

    fn deleted_state(&self, file: &FileRef, first: u32) -> Result<State, FsError> {
        if !file.is_dir && file.size == 0 {
            return Ok(State::Intact);
        }
        let needed = if file.is_dir {
            1
        } else {
            self.clusters.clusters_for(file.size)
        };
        let (runs, complete) = self.clusters.contiguous(first, needed);
        if !complete {
            return Ok(State::Unknown);
        }
        for run in &runs {
            for step in 0..run.count {
                if self.clusters.link(run.first + step)? != Link::Free {
                    return Ok(State::Overwritten);
                }
            }
        }
        // 目录的簇虽然空闲，开头却已不是 `.` 与 `..`：内容已不在（被覆盖，或删除前就没写到盘上）
        if file.is_dir && !starts_with_dots(&self.first_cluster(first)?) {
            return Ok(State::Overwritten);
        }
        Ok(State::Intact)
    }

    fn first_cluster(&self, cluster: u32) -> Result<Vec<u8>, FsError> {
        let (run, _) = self.clusters.contiguous(cluster, 1);
        self.clusters.read_runs(&run, self.clusters.cluster_size)
    }
}

/// 子目录的第一个簇以 `.` 与 `..` 两项开头。
fn starts_with_dots(bytes: &[u8]) -> bool {
    bytes.get(..11) == Some(b".          ".as_slice())
        && bytes.get(32..43) == Some(b"..         ".as_slice())
}

/// 目录里有没有「此后再无目录项」的标记（首字节为 0）。
fn has_end_marker(bytes: &[u8]) -> bool {
    bytes
        .as_chunks::<ENTRY>()
        .0
        .iter()
        .any(|entry| entry[0] == 0)
}

/// 解析出、还没判断可恢复性的目录项。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawEntry {
    name: String,
    short_name: String,
    name_uncertain: bool,
    is_dir: bool,
    deleted: bool,
    first_cluster: u32,
    cluster_high: u16,
    size: u64,
    modified: Option<String>,
    created: Option<String>,
}

struct RawListing {
    entries: Vec<RawEntry>,
    truncated: bool,
}

/// 一个长文件名目录项：序号字节（删除后是 0xE5）、校验和、13 个 UTF-16 单元。
struct LfnPart {
    order: u8,
    checksum: u8,
    units: [u16; 13],
}

/// 解析目录内容。`parent_deleted` 为真时，所在目录已被删除，里面的项一律按已删除处理（簇链也已释放）。
fn parse_dir(bytes: &[u8], parent_deleted: bool) -> RawListing {
    let mut entries = Vec::new();
    let mut pending: Vec<LfnPart> = Vec::new();
    for raw in bytes.as_chunks::<ENTRY>().0 {
        let (Some(&first), Some(&attr)) = (raw.first(), raw.get(11)) else {
            break;
        };
        if first == 0 {
            break;
        }
        if attr & 0x3F == ATTR_READ_ONLY_ETC {
            if let Some(part) = lfn_part(raw) {
                if first & 0x40 != 0 && first != DELETED {
                    pending.clear();
                }
                pending.push(part);
                if pending.len() > MAX_LFN_PARTS {
                    pending.clear();
                }
            }
            continue;
        }
        let parts = std::mem::take(&mut pending);
        if attr & ATTR_VOLUME_ID != 0 || attr & 0xC0 != 0 {
            continue;
        }
        let Some(short) = raw.get(..11) else {
            continue;
        };
        if short == b".          " || short == b"..         " {
            continue;
        }
        let deleted_here = first == DELETED;
        if deleted_here && !plausible_short(short) {
            continue;
        }
        if entries.len() >= MAX_ENTRIES {
            return RawListing {
                entries,
                truncated: true,
            };
        }
        entries.push(raw_entry(raw, attr, deleted_here, parent_deleted, &parts));
    }
    RawListing {
        entries,
        truncated: false,
    }
}

fn raw_entry(
    raw: &[u8],
    attr: u8,
    deleted_here: bool,
    parent_deleted: bool,
    parts: &[LfnPart],
) -> RawEntry {
    let mut short: [u8; 11] = raw
        .get(..11)
        .and_then(|bytes| bytes.try_into().ok())
        .unwrap_or([b' '; 11]);
    if short[0] == KANJI_E5 {
        short[0] = DELETED;
    }
    let mut long = None;
    let mut name_uncertain = false;
    if deleted_here {
        // 删除时首字节被改成 0xE5，原来的首字母丢了。有长文件名时按校验和反推，否则用 `_` 代替
        let recovered = parts
            .first()
            .and_then(|part| recover_first_byte(&short, part.checksum))
            .filter(|&byte| plausible_first_byte(byte));
        match recovered {
            Some(byte) if lfn_consistent(parts, true) => {
                short[0] = byte;
                long = long_name(parts);
            }
            _ => {
                short[0] = b'_';
                name_uncertain = true;
            }
        }
    } else if lfn_consistent(parts, false)
        && parts.first().map(|part| part.checksum) == Some(short_checksum(&short))
    {
        long = long_name(parts);
    }
    let lowercase_base = raw.get(12).is_some_and(|&flags| flags & 0x08 != 0);
    let lowercase_ext = raw.get(12).is_some_and(|&flags| flags & 0x10 != 0);
    let (short_name, non_ascii) = short_display(&short, lowercase_base, lowercase_ext);
    if long.is_none() && non_ascii {
        name_uncertain = true;
    }
    let cluster_high = le16(raw, 20).unwrap_or(0);
    let cluster_low = le16(raw, 26).unwrap_or(0);
    let date_time = |date_at, time_at| dos_time(le16(raw, date_at)?, le16(raw, time_at)?);
    RawEntry {
        name: long.unwrap_or_else(|| short_name.clone()),
        short_name,
        name_uncertain,
        is_dir: attr & ATTR_DIRECTORY != 0,
        deleted: deleted_here || parent_deleted,
        first_cluster: u32::from(cluster_high) << 16 | u32::from(cluster_low),
        cluster_high,
        size: u64::from(le32(raw, 28).unwrap_or(0)),
        modified: date_time(24, 22),
        created: date_time(16, 14),
    }
}

fn lfn_part(raw: &[u8]) -> Option<LfnPart> {
    let mut units = [0u16; 13];
    let positions = (1..11)
        .step_by(2)
        .chain((14..26).step_by(2))
        .chain((28..32).step_by(2));
    for (slot, at) in units.iter_mut().zip(positions) {
        *slot = le16(raw, at)?;
    }
    Some(LfnPart {
        order: *raw.first()?,
        checksum: *raw.get(13)?,
        units,
    })
}

/// 长文件名的各段是否属于同一个短文件名：校验和一致、段数合理；未删除的还要序号从 `0x40 | n` 递减到 1。
fn lfn_consistent(parts: &[LfnPart], deleted: bool) -> bool {
    let Some(first) = parts.first() else {
        return false;
    };
    if parts.iter().any(|part| part.checksum != first.checksum) {
        return false;
    }
    if deleted {
        return parts.iter().all(|part| part.order == DELETED);
    }
    let count = parts.len();
    first.order & 0x40 != 0
        && usize::from(first.order & 0x1F) == count
        && parts
            .iter()
            .enumerate()
            .all(|(index, part)| usize::from(part.order & 0x1F) == count - index)
}

/// 各段在目录里是倒着存的（最后一段在前），拼起来到 0 为止；`0xFFFF` 是填充。
fn long_name(parts: &[LfnPart]) -> Option<String> {
    let mut units = Vec::with_capacity(parts.len() * 13);
    for part in parts.iter().rev() {
        units.extend_from_slice(&part.units);
    }
    if let Some(end) = units.iter().position(|&unit| unit == 0) {
        units.truncate(end);
    }
    while units.last() == Some(&0xFFFF) {
        units.pop();
    }
    let name = String::from_utf16_lossy(&units);
    (!name.is_empty()).then_some(name)
}

/// 短文件名（11 字节）的校验和，长文件名的每一段都记着它。
pub(crate) fn short_checksum(short: &[u8; 11]) -> u8 {
    short
        .iter()
        .fold(0u8, |sum, &byte| sum.rotate_right(1).wrapping_add(byte))
}

/// 首字节换成多少，短文件名的校验和才等于 `checksum`。校验和对首字节是一一对应的，答案唯一。
fn recover_first_byte(short: &[u8; 11], checksum: u8) -> Option<u8> {
    let mut candidate = *short;
    (0..=u8::MAX).find(|&byte| {
        candidate[0] = byte;
        short_checksum(&candidate) == checksum
    })
}

/// 短文件名里允许出现的首字节：可打印、非小写、不是规范禁止的符号。
fn plausible_first_byte(byte: u8) -> bool {
    byte >= 0x80 || (byte > 0x20 && !byte.is_ascii_lowercase() && !forbidden(byte) && byte != b'.')
}

fn forbidden(byte: u8) -> bool {
    b"\"*+,/:;<=>?[\\]|".contains(&byte)
}

/// 已删除目录项的短文件名是否像样：首字节之后没有控制字符、小写字母、禁用符号。用来过滤已删除目录里的垃圾。
fn plausible_short(short: &[u8]) -> bool {
    short.iter().skip(1).all(|&byte| {
        byte >= 0x80 || (byte >= 0x20 && !byte.is_ascii_lowercase() && !forbidden(byte))
    })
}

/// 8.3 名字显示成 `NAME.EXT`，按大小写标记转小写；非 ASCII 字节（OEM 代码页）显示为 `_`。
fn short_display(short: &[u8; 11], lowercase_base: bool, lowercase_ext: bool) -> (String, bool) {
    let mut non_ascii = false;
    let mut part = |bytes: &[u8], lower: bool| -> String {
        let length = bytes
            .iter()
            .rposition(|&byte| byte != b' ')
            .map_or(0, |end| end + 1);
        bytes
            .iter()
            .take(length)
            .map(|&byte| {
                if byte >= 0x80 {
                    non_ascii = true;
                    '_'
                } else if lower {
                    char::from(byte.to_ascii_lowercase())
                } else {
                    char::from(byte)
                }
            })
            .collect()
    };
    let (base, ext) = short.split_at(8);
    let base = part(base, lowercase_base);
    let ext = part(ext, lowercase_ext);
    let name = if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    };
    (name, non_ascii)
}

/// 根目录里未删除的卷标项。
fn root_label(bytes: &[u8]) -> Option<String> {
    for raw in bytes.as_chunks::<ENTRY>().0 {
        let (&first, &attr) = (raw.first()?, raw.get(11)?);
        if first == 0 {
            return None;
        }
        if first != DELETED && attr & 0x3F == ATTR_VOLUME_ID {
            return short_label(raw.get(..11)?);
        }
    }
    None
}

/// 11 字节、空格补齐的卷标；`NO NAME` 是「没有卷标」。
fn short_label(bytes: &[u8]) -> Option<String> {
    let text: String = bytes
        .iter()
        .map(|&byte| {
            if byte.is_ascii_graphic() || byte == b' ' {
                char::from(byte)
            } else {
                '_'
            }
        })
        .collect();
    let text = text.trim_end();
    (!text.is_empty() && text != "NO NAME").then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_checksum_matches_the_specification_example() {
        // 规范里的算法：sum = ((sum & 1) << 7) + (sum >> 1) + byte
        let short = *b"FOO     BAR";
        let mut sum = 0u8;
        for byte in short {
            sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(byte);
        }
        assert_eq!(short_checksum(&short), sum);
    }

    #[test]
    fn first_byte_is_recovered_from_the_checksum() {
        let short = *b"REPORT~1DOC";
        let checksum = short_checksum(&short);
        let mut deleted = short;
        deleted[0] = DELETED;
        assert_eq!(recover_first_byte(&deleted, checksum), Some(b'R'));
    }

    #[test]
    fn short_names_respect_case_flags() {
        assert_eq!(short_display(b"README  TXT", true, false).0, "readme.TXT");
        assert_eq!(short_display(b"NOEXT      ", false, false).0, "NOEXT");
        assert_eq!(
            short_display(
                &[
                    0xC4, b'A', b' ', b' ', b' ', b' ', b' ', b' ', b'T', b'X', b'T'
                ],
                false,
                false
            ),
            ("_A.TXT".to_owned(), true)
        );
    }
}
