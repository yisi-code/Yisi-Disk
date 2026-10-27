//! exFAT：引导扇区、分配位图、目录项集合（含校验和）、删除项识别。依据微软公开的 exFAT 规范。

use std::sync::{Arc, Mutex, PoisonError};

use yisi_part::bytes::{le16, le32, le64};

use crate::bpb::ExfatBpb;
use crate::clusters::{Chain, Clusters, FatWidth, Run};
use crate::entry::{ClusterRef, Entry, FileRef, Listing, Location, State, dos_time};
use crate::format::{EXFAT_TYPE_BITMAP, EXFAT_TYPE_FILE, ExfatSet, exfat_entry_set};
use crate::reader::FileReader;
use crate::region::{ChunkCache, Region};
use crate::{FsError, MAX_DIR_BYTES, MAX_ENTRIES, bpb};

const ENTRY: usize = 32;
/// 目录项类型的第 7 位：1 为使用中，删除时清零。
const TYPE_LABEL: u8 = 0x83;
const ATTR_DIRECTORY: u16 = 0x10;
/// 分配位图一次读进内存的上限（对应 20 亿个簇），超过时按块缓存。
const MAX_LOADED_BITMAP: u64 = 256 << 20;

/// 解析引导扇区；有致命问题时返回 `None`（规则在 [`bpb`](crate::bpb)）。
pub(crate) fn parse_boot(sector: &[u8]) -> Option<ExfatBpb> {
    bpb::check_exfat(sector).geometry
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
        let heap_offset = geometry.heap_offset_bytes();
        if heap_offset >= region.length() {
            return Err(FsError::Invalid("exFAT 的簇区在分区之外"));
        }
        let count = u32::try_from(geometry.usable_clusters(region.length()))
            .map_err(|_| FsError::Invalid("簇数过大"))?;
        let clusters = Clusters {
            fat: ChunkCache::new(
                region.clone(),
                geometry.active_fat_offset(),
                geometry.fat_bytes(),
            ),
            region,
            heap_offset,
            cluster_size: geometry.cluster_size(),
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
                Some(EXFAT_TYPE_BITMAP) if volume.bitmap_runs.is_empty() => {
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
        let raw_entries = bytes.as_chunks::<ENTRY>().0;
        while let Some(raw) = raw_entries.get(index) {
            let kind = raw[0];
            if kind == 0 {
                break;
            }
            if kind & 0x7F != EXFAT_TYPE_FILE & 0x7F {
                index += 1;
                continue;
            }
            // 规则在 format，与文件系统检查共用；有问题的组不列出（已删除的组可能已被新目录项部分覆盖）
            let set = exfat_entry_set(raw_entries, index);
            if !set.problems.is_empty() || !set.has_stream {
                index += 1;
                continue;
            }
            if entries.len() >= MAX_ENTRIES {
                truncated = true;
                break;
            }
            index += set.entries;
            entries.push(self.entry(set, dir.deleted)?);
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

    fn entry(&self, set: ExfatSet, parent_deleted: bool) -> Result<Entry, FsError> {
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
            modified: timestamp(set.modified),
            created: timestamp(set.created),
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

/// exFAT 时间戳：日期在高 16 位、时间在低 16 位，各按 DOS 格式。
fn timestamp(value: u32) -> Option<String> {
    dos_time((value >> 16) as u16, value as u16)
}
