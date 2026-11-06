//! exFAT：引导扇区、分配位图、目录项集合（含校验和）、删除项识别。依据微软公开的 exFAT 规范。

use std::sync::{Arc, Mutex, PoisonError};

use yisi_part::bytes::{le32, le64};

use crate::bpb::ExfatBpb;
use crate::clusters::{Chain, Clusters, FatWidth, Run};
use crate::entry::{ClusterRef, Entry, FileAttributes, FileRef, Listing, Location, Stamp, State};
use crate::format::{
    ATTR_DIRECTORY, DIR_ENTRY_SIZE as ENTRY, EXFAT_IN_USE, EXFAT_TYPE_BITMAP, EXFAT_TYPE_FILE,
    EXFAT_TYPE_LABEL, ExfatSet, exfat_entry_set, exfat_label,
};
use crate::orphans::{FreeSpace, Look, ScanSlot};
use crate::reader::FileReader;
use crate::region::{ChunkCache, Region};
use crate::{FsError, MAX_DIR_BYTES, MAX_ENTRIES, ScanSummary, bpb};

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
    /// 读空闲簇找出的、原来的目录已找不到的文件夹。
    scan: ScanSlot,
}

impl Exfat {
    /// 按引导扇区 `boot`（主的或备份的）打开。
    pub fn open(region: Region, boot: &[u8]) -> Result<Self, FsError> {
        let geometry = parse_boot(boot).ok_or(FsError::Invalid(
            "扩展文件分配表（exFAT）引导扇区的参数不自洽",
        ))?;
        let heap_offset = geometry.heap_offset_bytes();
        if heap_offset >= region.length() {
            return Err(FsError::Invalid("扩展文件分配表（exFAT）的簇区在分区之外"));
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
            return Err(FsError::Invalid(
                "扩展文件分配表（exFAT）根目录的簇号不合法",
            ));
        }
        let mut volume = Self {
            clusters,
            root_cluster: geometry.root_cluster,
            label: None,
            bitmap_runs: Vec::new(),
            bitmap_length: 0,
            bitmap: Mutex::new(None),
            scan: ScanSlot::default(),
        };
        // 根目录里的分配位图项与卷标项
        let (root, _) = volume.dir_contents(&volume.root())?;
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
                Some(EXFAT_TYPE_LABEL) => {
                    if let Some(label) = exfat_label(raw) {
                        volume.label = Some(label);
                    }
                }
                _ => {}
            }
        }
        if volume.bitmap_runs.is_empty() || volume.bitmap_length * 8 < u64::from(count) {
            return Err(FsError::Invalid(
                "扩展文件分配表（exFAT）根目录里没有可用的分配位图",
            ));
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
            modified: None,
        }
    }

    pub fn list(&self, dir: &FileRef) -> Result<Listing, FsError> {
        self.list_runs(dir).map(|(listing, _)| listing)
    }

    /// 扫一遍空闲簇，找出原来的目录已找不到的文件夹（[`crate::orphans`]）。
    pub fn scan(
        &self,
        progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<Option<ScanSummary>, FsError> {
        self.scan.run(self, progress)
    }

    pub fn is_scanned(&self) -> bool {
        self.scan.get().is_some()
    }

    fn list_dir(&self, dir: &FileRef) -> Result<(Listing, Vec<Run>), FsError> {
        if !dir.is_dir {
            return Err(FsError::BadRef);
        }
        if dir.is_orphans() {
            return Ok((self.scan.listing(&self.clusters), Vec::new()));
        }
        let overwritten = dir.deleted && self.is_allocated(dir.clusters()?.first_cluster)?;
        if overwritten {
            let listing = Listing {
                entries: Vec::new(),
                truncated: false,
                overwritten: true,
            };
            return Ok((listing, Vec::new()));
        }
        let (bytes, runs) = self.dir_contents(dir)?;
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
        if !dir.deleted
            && dir.location == self.root().location
            && let Some(orphans) = self.scan.root_entry()
        {
            entries.push(orphans);
        }
        let listing = Listing {
            entries,
            truncated,
            overwritten: false,
        };
        Ok((listing, runs))
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

    /// 目录的内容与它占的簇。
    fn dir_contents(&self, dir: &FileRef) -> Result<(Vec<u8>, Vec<Run>), FsError> {
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
        let bytes = self.clusters.read_runs(&runs, limit)?;
        Ok((bytes, runs))
    }

    /// 整簇都是目录项：至少有一组完整、校验和对得上的文件目录项集合（[`exfat_entry_set`]，各项取值合理，见
    /// [`Self::set_ok`]）；开头可以是上一个簇里那一组剩下的次项，末尾可以是跨到下一个簇的一组的开头（在的那几项
    /// 都要是使用中标记一致的次项）；结束标记之后全为 0；此外不许有别的东西。
    fn look_dir(&self, bytes: &[u8]) -> Look {
        let slots = bytes.as_chunks::<ENTRY>().0;
        let mut sets = 0u32;
        let mut ended = false;
        let mut index = 0;
        while let Some(raw) = slots.get(index) {
            let kind = raw[0];
            if ended || kind == 0 {
                if raw.iter().any(|&byte| byte != 0) {
                    return Look::Other;
                }
                ended = true;
                index += 1;
                continue;
            }
            if kind & 0x7F == EXFAT_TYPE_FILE & 0x7F {
                let set = exfat_entry_set(slots, index);
                if set.problems.is_empty() && set.has_stream && self.set_ok(&set) {
                    sets = sets.saturating_add(1);
                    index += set.entries;
                    continue;
                }
                let recorded = usize::from(raw[1]);
                let spills = (2..=18).contains(&recorded)
                    && index + recorded >= slots.len()
                    && slots
                        .get(index + 1..)
                        .unwrap_or_default()
                        .iter()
                        .all(|next| next[0] & 0x40 != 0 && (next[0] ^ kind) & EXFAT_IN_USE == 0);
                if spills {
                    break;
                }
                return Look::Other;
            }
            // 上一个簇里那一组剩下的次项：流扩展、文件名、厂商扩展与分配项
            let leading = sets == 0 && matches!(kind & 0x7F, 0x40 | 0x41 | 0x60 | 0x61);
            if !leading {
                return Look::Other;
            }
            index += 1;
        }
        if sets == 0 {
            return Look::Other;
        }
        Look::Body {
            ended,
            entries: sets,
            strong: true,
        }
    }

    /// 一组目录项的取值合理：属性只用规范定义的几位；长度为 0 时首簇为 0，否则首簇合法、长度装得进簇区、
    /// 有效数据长度不超过长度；目录的长度是整簇、不超过规范上限 256 MiB。
    fn set_ok(&self, set: &ExfatSet) -> bool {
        const ATTRIBUTE_BITS: u16 = 0x37;
        const MAX_DIR_LENGTH: u64 = 256 << 20;
        if set.attributes & !ATTRIBUTE_BITS != 0 || set.valid_length > set.data_length {
            return false;
        }
        if set.data_length == 0 {
            return set.first_cluster == 0;
        }
        let fits = self.clusters.is_valid(set.first_cluster)
            && self.clusters.clusters_for(set.data_length) <= u64::from(self.clusters.count);
        let is_dir = set.attributes & u16::from(ATTR_DIRECTORY) != 0;
        fits && (!is_dir
            || (set.data_length.is_multiple_of(self.clusters.cluster_size)
                && set.data_length <= MAX_DIR_LENGTH))
    }

    fn entry(&self, set: ExfatSet, parent_deleted: bool) -> Result<Entry, FsError> {
        let deleted = set.deleted || parent_deleted;
        let is_dir = set.attributes & u16::from(ATTR_DIRECTORY) != 0;
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
        let modified = Stamp::Dos {
            stamp: set.modified,
            utc_offset: set.modified_utc_offset,
        };
        let file = FileRef {
            size: set.data_length,
            is_dir,
            deleted,
            location: Location::Clusters(at),
            modified: Some(modified),
        };
        Ok(Entry {
            name: set.name,
            short_name: None,
            name_uncertain: false,
            size: if is_dir { 0 } else { file.size },
            modified: modified.text(),
            created: Stamp::Dos {
                stamp: set.created,
                utc_offset: 0,
            }
            .text(),
            state,
            position: (is_dir || file.size > 0)
                .then(|| self.clusters.offset(at.first_cluster))
                .flatten(),
            unsupported: None,
            attributes: FileAttributes::from_bits(u32::from(set.attributes)),
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
            _ => {
                return Err(FsError::Invalid(
                    "扩展文件分配表（exFAT）分配位图过大且不连续",
                ));
            }
        };
        let bitmap = Arc::new(bitmap);
        *slot = Some(Arc::clone(&bitmap));
        Ok(bitmap)
    }
}

impl FreeSpace for Exfat {
    fn clusters(&self) -> &Clusters {
        &self.clusters
    }

    fn root(&self) -> FileRef {
        Exfat::root(self)
    }

    fn list_runs(&self, dir: &FileRef) -> Result<(Listing, Vec<Run>), FsError> {
        self.list_dir(dir)
    }

    fn is_free(&self, cluster: u32) -> Result<bool, FsError> {
        Ok(!self.is_allocated(cluster)?)
    }

    fn look(&self, _cluster: u32, bytes: &[u8]) -> Look {
        self.look_dir(bytes)
    }

    /// 子目录占的簇：不走 FAT 链的连续存放；其余的沿 FAT 链（删除后表项一般还在），链读不出时只算首簇。
    fn subdirs(&self, bytes: &[u8]) -> Vec<Run> {
        let slots = bytes.as_chunks::<ENTRY>().0;
        let mut runs = Vec::new();
        let mut index = 0;
        while let Some(raw) = slots.get(index) {
            if raw[0] == 0 {
                break;
            }
            if raw[0] & 0x7F != EXFAT_TYPE_FILE & 0x7F {
                index += 1;
                continue;
            }
            let set = exfat_entry_set(slots, index);
            if !set.problems.is_empty() || !set.has_stream {
                index += 1;
                continue;
            }
            index += set.entries;
            if set.attributes & u16::from(ATTR_DIRECTORY) == 0
                || !self.clusters.is_valid(set.first_cluster)
            {
                continue;
            }
            let needed = self.clusters.clusters_for(set.data_length).max(1);
            let found = if set.no_fat_chain {
                self.clusters.contiguous(set.first_cluster, needed).0
            } else {
                self.clusters
                    .chain(set.first_cluster, needed)
                    .map(|chain| chain.runs)
                    .unwrap_or_default()
            };
            if found.is_empty() {
                runs.push(Run {
                    first: set.first_cluster,
                    count: 1,
                });
            }
            runs.extend(found);
        }
        runs
    }
}
