//! exFAT：引导扇区、分配位图、目录项集合（含校验和）、删除项识别。依据微软公开的 exFAT 规范。

use std::sync::{Arc, Mutex, PoisonError};

use yisi_part::bytes::{le16, le32, le64};

use crate::clusters::{Chain, Clusters, FatWidth, Run};
use crate::entry::{ClusterRef, Entry, FileRef, Listing, Location, State, dos_time};
use crate::reader::FileReader;
use crate::region::{ChunkCache, Region};
use crate::{FsError, MAX_DIR_BYTES, MAX_ENTRIES};

const ENTRY: usize = 32;
/// 目录项类型的第 7 位：1 为使用中，删除时清零。
const IN_USE: u8 = 0x80;
const TYPE_BITMAP: u8 = 0x81;
const TYPE_LABEL: u8 = 0x83;
const TYPE_FILE: u8 = 0x85;
const TYPE_STREAM: u8 = 0xC0;
const TYPE_NAME: u8 = 0xC1;
const ATTR_DIRECTORY: u16 = 0x10;
/// 流扩展项的标志：第 1 位为「不走 FAT 链」（簇连续存放）。
const NO_FAT_CHAIN: u8 = 0x02;
/// 分配位图一次读进内存的上限（对应 20 亿个簇），超过时按块缓存。
const MAX_LOADED_BITMAP: u64 = 256 << 20;

/// 引导扇区里的几何参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub fat_offset: u64,
    pub fat_length: u64,
    pub heap_offset: u64,
    pub cluster_count: u32,
    pub root_cluster: u32,
    pub sector_shift: u32,
    pub cluster_shift: u32,
    pub use_second_fat: bool,
}

pub(crate) fn parse_boot(sector: &[u8]) -> Option<Geometry> {
    if sector.get(3..11) != Some(b"EXFAT   ".as_slice()) || le16(sector, 510) != Some(0xAA55) {
        return None;
    }
    // 规范要求 BPB 的旧字段区（11 到 63 字节）全为 0
    if sector.get(11..64)?.iter().any(|&byte| byte != 0) {
        return None;
    }
    let sector_shift = u32::from(*sector.get(108)?);
    let cluster_shift = u32::from(*sector.get(109)?);
    let fats = *sector.get(110)?;
    if !(9..=12).contains(&sector_shift)
        || cluster_shift > 25 - sector_shift
        || !matches!(fats, 1 | 2)
    {
        return None;
    }
    let geometry = Geometry {
        fat_offset: u64::from(le32(sector, 80)?),
        fat_length: u64::from(le32(sector, 84)?),
        heap_offset: u64::from(le32(sector, 88)?),
        cluster_count: le32(sector, 92)?,
        root_cluster: le32(sector, 96)?,
        sector_shift,
        cluster_shift,
        use_second_fat: fats == 2 && le16(sector, 106)? & 0x01 != 0,
    };
    let valid = geometry.fat_offset >= 24
        && geometry.fat_length > 0
        && geometry.heap_offset >= geometry.fat_offset + geometry.fat_length * u64::from(fats)
        && geometry.cluster_count > 0
        && geometry.cluster_count <= 0xFFFF_FFF5
        && geometry.root_cluster >= 2;
    valid.then_some(geometry)
}

/// 分配位图：第 n 位（从 2 号簇算起）为 1 表示已分配。
enum Bitmap {
    Cached(ChunkCache),
    Loaded(Vec<u8>),
}

pub(crate) struct Exfat {
    clusters: Clusters,
    root_cluster: u32,
    label: Option<String>,
    /// 分配位图的位置，第一次需要时才读。
    bitmap_runs: Vec<Run>,
    bitmap_length: u64,
    bitmap: Mutex<Option<Arc<Bitmap>>>,
}

impl Exfat {
    /// 按引导扇区 `boot`（主的或备份的）打开。
    pub fn open(region: Region, boot: &[u8]) -> Result<Self, FsError> {
        let geometry = parse_boot(boot).ok_or(FsError::Invalid("exFAT 引导扇区的参数不自洽"))?;
        let sector_size = 1u64 << geometry.sector_shift;
        let cluster_size = sector_size << geometry.cluster_shift;
        let fat_start = if geometry.use_second_fat {
            geometry.fat_offset + geometry.fat_length
        } else {
            geometry.fat_offset
        } * sector_size;
        let heap_offset = geometry.heap_offset * sector_size;
        if heap_offset >= region.length() {
            return Err(FsError::Invalid("exFAT 的簇区在分区之外"));
        }
        let fits = (region.length() - heap_offset) / cluster_size;
        let count = u64::from(geometry.cluster_count).min(fits);
        let count = u32::try_from(count).map_err(|_| FsError::Invalid("簇数过大"))?;
        let clusters = Clusters {
            fat: ChunkCache::new(region.clone(), fat_start, geometry.fat_length * sector_size),
            region,
            heap_offset,
            cluster_size,
            count,
            width: FatWidth::Exfat,
        };
        if !clusters.is_valid(geometry.root_cluster) {
            return Err(FsError::Invalid("exFAT 根目录的簇号不合法"));
        }
        let mut volume = Self {
            clusters,
            root_cluster: geometry.root_cluster,
            label: None,
            bitmap_runs: Vec::new(),
            bitmap_length: 0,
            bitmap: Mutex::new(None),
        };
        // 根目录里的分配位图项与卷标项
        let root = volume.dir_bytes(&volume.root())?;
        for raw in root.as_chunks::<ENTRY>().0 {
            match raw.first().copied() {
                Some(0) | None => break,
                Some(TYPE_BITMAP) if volume.bitmap_runs.is_empty() => {
                    let first = le32(raw, 20).unwrap_or(0);
                    let length = le64(raw, 24).unwrap_or(0);
                    let needed = volume.clusters.clusters_for(length);
                    let chain = volume.clusters.chain(first, needed)?;
                    if chain.complete && chain.len() == needed {
                        volume.bitmap_runs = chain.runs;
                        volume.bitmap_length = length;
                    }
                }
                Some(TYPE_LABEL) => {
                    let count = usize::from(raw.get(1).copied().unwrap_or(0)).min(11);
                    let units: Vec<u16> = (0..count)
                        .filter_map(|index| le16(raw, 2 + index * 2))
                        .collect();
                    let label = String::from_utf16_lossy(&units);
                    if !label.is_empty() {
                        volume.label = Some(label);
                    }
                }
                _ => {}
            }
        }
        if volume.bitmap_runs.is_empty() || volume.bitmap_length * 8 < u64::from(count) {
            return Err(FsError::Invalid("exFAT 根目录里没有可用的分配位图"));
        }
        Ok(volume)
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
                first_cluster: self.root_cluster,
                valid_size: 0,
                contiguous: false,
            }),
        }
    }

    pub fn list(&self, dir: &FileRef) -> Result<Listing, FsError> {
        if !dir.is_dir {
            return Err(FsError::BadRef);
        }
        let overwritten = dir.deleted && self.is_allocated(dir.clusters()?.first_cluster)?;
        if overwritten {
            return Ok(Listing {
                entries: Vec::new(),
                truncated: false,
                overwritten: true,
            });
        }
        let bytes = self.dir_bytes(dir)?;
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut index = 0;
        let count = bytes.len() / ENTRY;
        while index < count {
            let Some(raw) = entry_at(&bytes, index) else {
                break;
            };
            let kind = raw.first().copied().unwrap_or(0);
            if kind == 0 {
                break;
            }
            if kind & 0x7F != TYPE_FILE & 0x7F {
                index += 1;
                continue;
            }
            match file_set(&bytes, index) {
                Some(set) => {
                    if entries.len() >= MAX_ENTRIES {
                        truncated = true;
                        break;
                    }
                    index += set.entries;
                    entries.push(self.entry(set, dir.deleted)?);
                }
                None => index += 1,
            }
        }
        Ok(Listing {
            entries,
            truncated,
            overwritten: false,
        })
    }

    pub fn open_file(&self, file: &FileRef) -> Result<FileReader, FsError> {
        if file.is_dir {
            return Err(FsError::BadRef);
        }
        let runs = self.runs(file)?;
        Ok(FileReader::new(
            &self.clusters,
            &runs,
            file.size,
            file.clusters()?.valid_size,
        ))
    }

    /// 文件或目录占用的簇：连续存放的直接算，否则沿 FAT 链找。
    fn runs(&self, file: &FileRef) -> Result<Vec<Run>, FsError> {
        let at = file.clusters()?;
        let needed = self.clusters.clusters_for(file.size);
        Ok(if at.contiguous {
            self.clusters.contiguous(at.first_cluster, needed).0
        } else {
            self.clusters.chain(at.first_cluster, needed)?.runs
        })
    }

    fn dir_bytes(&self, dir: &FileRef) -> Result<Vec<u8>, FsError> {
        let limit = if dir.size == 0 {
            // 根目录没有记录长度，沿链读到结束
            MAX_DIR_BYTES
        } else {
            dir.size.min(MAX_DIR_BYTES)
        };
        let at = dir.clusters()?;
        let needed = self.clusters.clusters_for(limit);
        let runs = if at.contiguous {
            self.clusters.contiguous(at.first_cluster, needed).0
        } else {
            self.clusters.chain(at.first_cluster, needed)?.runs
        };
        self.clusters.read_runs(&runs, limit)
    }

    fn entry(&self, set: FileSet, parent_deleted: bool) -> Result<Entry, FsError> {
        let deleted = set.deleted || parent_deleted;
        let is_dir = set.attributes & ATTR_DIRECTORY != 0;
        let mut at = ClusterRef {
            first_cluster: set.first_cluster,
            valid_size: set.valid_length.min(set.data_length),
            contiguous: set.no_fat_chain,
        };
        let state = if deleted {
            let (state, contiguous) = self.deleted_state(set.data_length, at)?;
            at.contiguous = contiguous;
            state
        } else {
            State::Live
        };
        let file = FileRef {
            size: set.data_length,
            is_dir,
            deleted,
            location: Location::Clusters(at),
        };
        Ok(Entry {
            name: set.name,
            short_name: None,
            name_uncertain: false,
            size: if is_dir { 0 } else { file.size },
            modified: set.modified,
            created: set.created,
            state,
            position: (is_dir || file.size > 0)
                .then(|| self.clusters.offset(at.first_cluster))
                .flatten(),
            unsupported: None,
            file,
        })
    }

    /// 已删除文件的可恢复性，以及读它时是否按连续存放处理。
    ///
    /// 删除只清分配位图，FAT 表项一般还在：走 FAT 链的文件先沿链找，链完整就照链读；
    /// 链断了只能假设连续存放，判为无法判断。
    fn deleted_state(&self, size: u64, at: ClusterRef) -> Result<(State, bool), FsError> {
        if size == 0 {
            return Ok((State::Intact, at.contiguous));
        }
        let needed = self.clusters.clusters_for(size);
        let (chain, contiguous, trusted) = if at.contiguous {
            let (runs, complete) = self.clusters.contiguous(at.first_cluster, needed);
            (Chain { runs, complete }, true, true)
        } else {
            let chain = self.clusters.chain(at.first_cluster, needed)?;
            if chain.complete && chain.len() == needed {
                (chain, false, true)
            } else {
                let (runs, complete) = self.clusters.contiguous(at.first_cluster, needed);
                (Chain { runs, complete }, true, false)
            }
        };
        if !chain.complete {
            return Ok((State::Unknown, contiguous));
        }
        for cluster in chain.clusters() {
            if self.is_allocated(cluster)? {
                return Ok((State::Overwritten, contiguous));
            }
        }
        Ok((
            if trusted {
                State::Intact
            } else {
                State::Unknown
            },
            contiguous,
        ))
    }

    fn is_allocated(&self, cluster: u32) -> Result<bool, FsError> {
        if !self.clusters.is_valid(cluster) {
            return Ok(false);
        }
        let bit = u64::from(cluster - 2);
        let byte = match &*self.bitmap()? {
            Bitmap::Loaded(bytes) => bytes.get((bit / 8) as usize).copied(),
            Bitmap::Cached(cache) => cache.bytes::<1>(bit / 8)?.map(|[byte]| byte),
        };
        Ok(byte.is_some_and(|byte| byte & (1 << (bit % 8)) != 0))
    }

    fn bitmap(&self) -> Result<Arc<Bitmap>, FsError> {
        let mut slot = self.bitmap.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(bitmap) = slot.as_ref() {
            return Ok(Arc::clone(bitmap));
        }
        let bitmap = match self.bitmap_runs.as_slice() {
            // 连续存放（几乎总是这样）：按块缓存，用到哪里读哪里
            [run] => {
                let start = self.clusters.offset(run.first).ok_or(FsError::BadRef)?;
                Bitmap::Cached(ChunkCache::new(
                    self.clusters.region.clone(),
                    start,
                    self.bitmap_length,
                ))
            }
            runs if self.bitmap_length <= MAX_LOADED_BITMAP => {
                Bitmap::Loaded(self.clusters.read_runs(runs, self.bitmap_length)?)
            }
            _ => return Err(FsError::Invalid("exFAT 分配位图过大且不连续")),
        };
        let bitmap = Arc::new(bitmap);
        *slot = Some(Arc::clone(&bitmap));
        Ok(bitmap)
    }
}

fn entry_at(bytes: &[u8], index: usize) -> Option<&[u8]> {
    let start = index.checked_mul(ENTRY)?;
    bytes.get(start..start.checked_add(ENTRY)?)
}

/// 一组目录项（文件项 + 流扩展项 + 文件名项）解析出的内容。
struct FileSet {
    /// 这一组占几个目录项。
    entries: usize,
    deleted: bool,
    attributes: u16,
    name: String,
    first_cluster: u32,
    data_length: u64,
    valid_length: u64,
    no_fat_chain: bool,
    modified: Option<String>,
    created: Option<String>,
}

/// 从第 `index` 项起解析一组目录项。类型次序、使用中标记要一致，校验和要对得上（删除的组按删除前的类型字节计算），
/// 否则返回 `None`：已删除目录项的空间可能已被新的目录项部分覆盖。
fn file_set(bytes: &[u8], index: usize) -> Option<FileSet> {
    let primary = entry_at(bytes, index)?;
    let kind = *primary.first()?;
    let deleted = kind & IN_USE == 0;
    let secondary = usize::from(*primary.get(1)?);
    if !(2..=18).contains(&secondary) {
        return None;
    }
    let mut checksum = 0u16;
    let mut name_units = Vec::new();
    let mut stream = None;
    for offset in 0..=secondary {
        let raw = entry_at(bytes, index + offset)?;
        let entry_kind = *raw.first()?;
        if (entry_kind & IN_USE == 0) != deleted {
            return None;
        }
        let in_use_kind = entry_kind | IN_USE;
        match (offset, in_use_kind) {
            (0, TYPE_FILE) => {}
            (1, TYPE_STREAM) => stream = Some(raw),
            (2.., TYPE_NAME) => {
                name_units.extend((0..15).filter_map(|unit| le16(raw, 2 + unit * 2)));
            }
            // 厂商扩展项等：只参与校验和
            (2.., kind) if kind & 0x40 != 0 => {}
            _ => return None,
        }
        for (position, &byte) in raw.iter().enumerate() {
            if offset == 0 && (position == 2 || position == 3) {
                continue;
            }
            let byte = if position == 0 { in_use_kind } else { byte };
            checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
        }
    }
    if Some(checksum) != le16(primary, 2) {
        return None;
    }
    let stream = stream?;
    let name_length = usize::from(*stream.get(3)?);
    if name_length == 0 || name_length > name_units.len() {
        return None;
    }
    name_units.truncate(name_length);
    let timestamp = |at| {
        let value = le32(primary, at)?;
        dos_time((value >> 16) as u16, value as u16)
    };
    Some(FileSet {
        entries: secondary + 1,
        deleted,
        attributes: le16(primary, 4)?,
        name: String::from_utf16_lossy(&name_units),
        first_cluster: le32(stream, 20)?,
        data_length: le64(stream, 24)?,
        valid_length: le64(stream, 8)?,
        no_fat_chain: stream
            .get(1)
            .is_some_and(|&flags| flags & NO_FAT_CHAIN != 0),
        modified: timestamp(12),
        created: timestamp(8),
    })
}
