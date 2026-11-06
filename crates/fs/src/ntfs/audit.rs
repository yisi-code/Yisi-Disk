//! NTFS 一致性核对（只读，文件系统检查用）：顺序读一遍 MFT，核对每条记录与 `$MFT` 的记录位图、文件与上级目录、
//! 目录索引里的项与记录；再把各记录运行表用到的簇排序，找出几个文件共用的簇，并与 `$Bitmap` 逐簇对比。
//! 这里只给出事实，问题怎么说、有多严重由检查决定。
//!
//! 卷正在使用时（系统盘），读记录与读位图之间文件可能被建、被删，结论会对不上。要报出的「会丢数据」那几类
//! （记录位图漏标、`$Bitmap` 漏标、交叉链接）在报出之前不走缓存再读一次那条记录与那几位，已经变了的不报。

use super::index;
use super::record::{BITMAP, Body, DATA, NAMESPACE_DOS, Record, decode_runs, split_ref};
use super::{BITMAP_RECORD, FIRST_USER_RECORD, MFT_RECORD, Ntfs, ROOT_RECORD, Value, file_names};
use crate::FsError;
use crate::region::Region;

/// 一次读这么多字节的 MFT 或分配位图。
const BLOCK: u64 = 1 << 20;
/// MFT 记录超过这么多条时不核对（每条留 4 字节状态，另有名字与父目录的引用）。
const MAX_RECORDS: u64 = 16 << 20;
/// 运行表里用到簇的段超过这么多时不做簇的核对（每段 24 字节，约 192 MiB）。
const MAX_CLAIMS: usize = 8 << 20;
/// 每类事实最多列出这么多项，其余只计数。
pub const MAX_LISTED: usize = 500;
/// 拼路径时最多往上找这么多层。
const MAX_DEPTH: usize = 256;

/// 每条记录的状态。
const VALID: u8 = 0x01;
const IN_USE: u8 = 0x02;
const DIRECTORY: u8 = 0x04;
const BASE: u8 = 0x08;
/// 目录的索引全部读出了：可以判断文件在不在里面。
const INDEX_COMPLETE: u8 = 0x10;

/// 核对的阶段，进度按阶段报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditStage {
    /// 读 MFT；单位是记录。
    Records,
    /// 读各目录的索引；单位是目录。
    Indexes,
    /// 与 `$Bitmap` 逐簇对比；单位是位图的字节。
    Clusters,
}

/// 至多 [`MAX_LISTED`] 项，`total` 含没有列出的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed<T> {
    pub items: Vec<T>,
    pub total: u64,
}

impl<T> Default for Listed<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            total: 0,
        }
    }
}

impl<T> Listed<T> {
    fn push(&mut self, item: T) {
        self.total += 1;
        if self.items.len() < MAX_LISTED {
            self.items.push(item);
        }
    }
}

/// 两个文件共用的簇：从 `first` 起 `count` 个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedClusters {
    pub first: u64,
    pub count: u64,
    pub path: String,
    pub other_path: String,
}

/// 文件用到、`$Bitmap` 里却是空闲的簇：从 `first` 起 `count` 个，`path` 是用到第一个簇的文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmarkedClusters {
    pub first: u64,
    pub count: u64,
    pub path: String,
}

/// 核对的结果。路径以 `/` 开头；上级目录找不到时以 `/…/` 开头，没有名字的记录写成 `$MFT#记录号`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NtfsAudit {
    pub cluster_count: u64,
    /// 在用的文件与目录（不含系统文件与根目录）。
    pub files: u64,
    pub directories: u64,
    /// MFT 里读不出的记录数。
    pub unreadable_records: u64,
    /// 记录位图记为在用、记录本身却坏了（签名、更新序列、记录头不对）的记录号。
    pub damaged_records: Listed<u64>,
    /// `$MFT` 的记录位图读出来了，下面两项才有意义。
    pub record_bitmap_checked: bool,
    /// 记录在用、记录位图里却是空闲的文件：系统可能把这条记录分给新文件。
    pub not_in_record_bitmap: Listed<String>,
    /// 记录位图记为在用、记录却没在用的条数。
    pub marked_free_records: u64,
    /// 上级目录不在用、不是目录或已换了别的文件。
    pub orphans: Listed<String>,
    /// 目录索引里指向已删除或已换了别的文件的项（目录路径加上项里的名字）。
    pub stale_index_entries: Listed<String>,
    /// 不在上级目录索引里的文件：浏览时看不到。
    pub missing_from_index: Listed<String>,
    /// 索引读不出或坏了的目录。
    pub unreadable_indexes: Listed<String>,
    /// 运行表解不开或指到卷外的文件。
    pub bad_runs: Listed<String>,
    /// 下面几项做了：运行表的段不太多、`$Bitmap` 读得出。
    pub clusters_checked: bool,
    /// `$Bitmap` 里记为已占用的簇数（簇的核对做了才有）。
    pub used_clusters: u64,
    pub shared: Listed<SharedClusters>,
    pub not_in_bitmap: Listed<UnmarkedClusters>,
    /// 已占用、没有记录用到的簇：共几个、分几段、第一段从哪个簇起。
    pub lost_clusters: u64,
    pub lost_ranges: u64,
    pub first_lost: Option<u64>,
    /// `$Bitmap` 有读不出的部分，那些簇没有对比。
    pub bitmap_unreadable: bool,
}

/// 运行表里用到簇的一段，`owner` 是它所属文件的基本记录。
#[derive(Clone, Copy)]
struct Claim {
    lcn: u64,
    length: u64,
    owner: u64,
}

impl Claim {
    fn end(&self) -> u64 {
        self.lcn + self.length
    }
}

#[derive(Default, Clone, Copy)]
struct State {
    flags: u8,
    sequence: u16,
}

impl State {
    fn has(self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    /// 在用的基本记录，且序号是 `sequence`：文件引用指的就是它。
    fn is_live(self, sequence: u16) -> bool {
        self.has(VALID) && self.has(IN_USE) && self.has(BASE) && self.sequence == sequence
    }
}

/// 一个名字：文件 `child` 在上级目录 `parent`（序号 `parent_sequence`）里。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Link {
    parent: u64,
    child: u64,
    parent_sequence: u16,
}

/// 按引导扇区 `boot` 打开 NTFS 卷并核对。`progress(阶段, 已做, 总量)` 返回 `false` 时停下，结果为 `None`。
/// 卷打不开、记录太多时返回错误。
pub fn audit_ntfs(
    region: Region,
    boot: &[u8],
    progress: &mut dyn FnMut(AuditStage, u64, u64) -> bool,
) -> Result<Option<NtfsAudit>, FsError> {
    let ntfs = Ntfs::open(region, boot)?;
    if ntfs.record_count > MAX_RECORDS {
        return Err(FsError::Invalid(
            "主文件表（MFT）记录超过 1600 万条，暂不核对",
        ));
    }
    Auditor::new(&ntfs).run(progress)
}

struct Auditor<'a> {
    ntfs: &'a Ntfs,
    audit: NtfsAudit,
    states: Vec<State>,
    links: Vec<Link>,
    claims: Vec<Claim>,
    too_many_claims: bool,
    bad_run_owners: Vec<u64>,
    /// 记录在用、记录位图里空闲的文件（基本记录号），全部记录读完才拼得出路径。
    unmarked_records: Listed<u64>,
}

impl<'a> Auditor<'a> {
    fn new(ntfs: &'a Ntfs) -> Self {
        Self {
            ntfs,
            audit: NtfsAudit {
                cluster_count: ntfs.cluster_count,
                ..NtfsAudit::default()
            },
            states: Vec::new(),
            links: Vec::new(),
            claims: Vec::new(),
            too_many_claims: false,
            bad_run_owners: Vec::new(),
            unmarked_records: Listed::default(),
        }
    }

    fn run(
        mut self,
        progress: &mut dyn FnMut(AuditStage, u64, u64) -> bool,
    ) -> Result<Option<NtfsAudit>, FsError> {
        let record_bitmap = self.record_bitmap();
        self.audit.record_bitmap_checked = record_bitmap.is_some();
        if !self.read_records(record_bitmap.as_deref(), progress) {
            return Ok(None);
        }
        self.links.sort_unstable();
        self.links.dedup();
        self.check_parents();
        if !self.check_indexes(progress) {
            return Ok(None);
        }
        self.bad_run_owners.sort_unstable();
        self.bad_run_owners.dedup();
        for owner in std::mem::take(&mut self.bad_run_owners) {
            let path = self.path(owner);
            self.audit.bad_runs.push(path);
        }
        if !self.too_many_claims && !self.check_clusters(progress) {
            return Ok(None);
        }
        Ok(Some(self.audit))
    }

    /// `$MFT` 的记录位图（一位一条记录）；读不出时为 `None`。
    fn record_bitmap(&self) -> Option<Vec<u8>> {
        let record = self.ntfs.record(MFT_RECORD).ok()??;
        match self.ntfs.value(MFT_RECORD, &record, BITMAP, &[]).ok()?? {
            Value::Resident { bytes, .. } => Some(bytes),
            Value::NonResident { extents, size, .. } => {
                let stream = self.ntfs.stream(extents, size);
                let mut bytes = vec![0u8; usize::try_from(stream.size()).ok()?];
                stream.read_at(0, &mut bytes).ok()?;
                Some(bytes)
            }
        }
    }

    /// 按 1 MiB 一段顺序读 MFT，整段读不出时逐条重读。被取消时返回 `false`。
    fn read_records(
        &mut self,
        bitmap: Option<&[u8]>,
        progress: &mut dyn FnMut(AuditStage, u64, u64) -> bool,
    ) -> bool {
        let ntfs = self.ntfs;
        let (count, size) = (ntfs.record_count, ntfs.record_size);
        self.states = Vec::with_capacity(count as usize);
        let marked = |number: u64| {
            bitmap.map(|bytes| {
                bytes
                    .get((number / 8) as usize)
                    .is_some_and(|byte| byte & (1 << (number % 8)) != 0)
            })
        };
        let per_block = (BLOCK / size).max(1);
        let mut buffer = Vec::new();
        let mut first = 0u64;
        while first < count {
            let batch = per_block.min(count - first);
            buffer.resize((batch * size) as usize, 0);
            if ntfs.mft.read_at(first * size, &mut buffer).is_ok() {
                for (index, raw) in buffer.chunks_exact(size as usize).enumerate() {
                    let number = first + index as u64;
                    self.add(number, Some(raw.to_vec()), marked(number));
                }
            } else {
                for number in first..first + batch {
                    let mut raw = vec![0u8; size as usize];
                    let raw = ntfs
                        .mft
                        .read_at(number * size, &mut raw)
                        .is_ok()
                        .then_some(raw);
                    self.add(number, raw, marked(number));
                }
            }
            first += batch;
            if !progress(AuditStage::Records, first, count) {
                return false;
            }
        }
        true
    }

    /// 一条记录：`raw` 为 `None` 是读不出，`marked` 是记录位图里的那一位（位图读不出时为 `None`）。
    fn add(&mut self, number: u64, raw: Option<Vec<u8>>, marked: Option<bool>) {
        let Some(raw) = raw else {
            self.audit.unreadable_records += 1;
            self.states.push(State::default());
            return;
        };
        let Some(record) = Record::parse(raw, number) else {
            if marked == Some(true) {
                self.audit.damaged_records.push(number);
            }
            self.states.push(State::default());
            return;
        };
        let base = record.is_base();
        let mut state = State {
            flags: VALID,
            sequence: record.sequence,
        };
        if base {
            state.flags |= BASE;
        }
        if record.is_dir() {
            state.flags |= DIRECTORY;
        }
        if !record.in_use() {
            if marked == Some(true) {
                self.audit.marked_free_records += 1;
            }
            self.states.push(state);
            return;
        }
        state.flags |= IN_USE;
        self.states.push(state);
        let owner = if base {
            number
        } else {
            split_ref(record.base).0
        };
        if marked == Some(false) {
            self.unmarked_records.push(owner);
        }
        if base {
            let names = file_names(&record);
            if number >= FIRST_USER_RECORD && !names.is_empty() {
                if record.is_dir() {
                    self.audit.directories += 1;
                } else {
                    self.audit.files += 1;
                }
            }
            if number != ROOT_RECORD {
                self.links.extend(names.iter().map(|name| Link {
                    parent: name.parent,
                    child: number,
                    parent_sequence: name.parent_sequence,
                }));
            }
        }
        self.add_claims(&record, owner);
    }

    /// 记录里各个非常驻属性的运行表用到的簇。
    fn add_claims(&mut self, record: &Record, owner: u64) {
        let cluster_count = self.ntfs.cluster_count;
        for attribute in record.attributes() {
            let Body::NonResident(body) = attribute.body else {
                continue;
            };
            let Some(extents) = decode_runs(body.runs, body.lowest_vcn) else {
                self.bad_run_owners.push(owner);
                continue;
            };
            for extent in extents {
                let Some(lcn) = extent.lcn else {
                    continue;
                };
                if lcn
                    .checked_add(extent.length)
                    .is_none_or(|end| end > cluster_count)
                {
                    self.bad_run_owners.push(owner);
                    continue;
                }
                if self.claims.len() >= MAX_CLAIMS {
                    self.too_many_claims = true;
                    self.claims = Vec::new();
                }
                if !self.too_many_claims && extent.length > 0 {
                    self.claims.push(Claim {
                        lcn,
                        length: extent.length,
                        owner,
                    });
                }
            }
        }
    }

    fn state(&self, record: u64) -> State {
        usize::try_from(record)
            .ok()
            .and_then(|index| self.states.get(index))
            .copied()
            .unwrap_or_default()
    }

    /// 上级目录要是在用、序号对得上的目录；记录位图漏标的文件此时换成路径。
    fn check_parents(&mut self) {
        let unmarked = std::mem::take(&mut self.unmarked_records);
        // 记录位图是一开始读的：之后新建的文件也会「在用、位图里空闲」，重读一次位图再判断
        let fresh = if unmarked.items.is_empty() {
            None
        } else {
            self.record_bitmap()
        };
        let mut dropped = 0u64;
        for &record in &unmarked.items {
            let marked_now = fresh.as_deref().is_some_and(|bytes| {
                bytes
                    .get((record / 8) as usize)
                    .is_some_and(|byte| byte & (1 << (record % 8)) != 0)
            });
            if marked_now || !self.still_live(record) {
                dropped += 1;
                continue;
            }
            let path = self.path(record);
            self.audit.not_in_record_bitmap.items.push(path);
        }
        self.audit.not_in_record_bitmap.total = unmarked.total - dropped;

        // 有几个名字（硬链接）的文件只列一次
        let mut orphans: Vec<u64> = self
            .links
            .iter()
            .filter(|link| {
                let parent = self.state(link.parent);
                !parent.is_live(link.parent_sequence) || !parent.has(DIRECTORY)
            })
            .map(|link| link.child)
            .collect();
        orphans.sort_unstable();
        orphans.dedup();
        for child in orphans {
            let path = self.path(child);
            self.audit.orphans.push(path);
        }
    }

    /// 读各目录的索引：项要指向在用、序号对得上的记录；文件要在上级目录的索引里。被取消时返回 `false`。
    fn check_indexes(&mut self, progress: &mut dyn FnMut(AuditStage, u64, u64) -> bool) -> bool {
        let directories: Vec<u64> = self
            .states
            .iter()
            .enumerate()
            .filter(|(_, state)| state.has(IN_USE) && state.has(BASE) && state.has(DIRECTORY))
            .map(|(index, _)| index as u64)
            .collect();
        let total = directories.len() as u64;
        // 索引里的（目录，文件）对
        let mut listed: Vec<(u64, u64)> = Vec::new();
        for (done, &number) in directories.iter().enumerate() {
            let record = match self.ntfs.record(number) {
                Ok(Some(record)) => record,
                _ => {
                    let path = self.path(number);
                    self.audit.unreadable_indexes.push(path);
                    continue;
                }
            };
            match index::read(self.ntfs, number, &record) {
                Ok((keys, truncated)) => {
                    for key in keys {
                        listed.push((number, key.record));
                        // 目录自己的「.」项（只有根目录有）不算
                        if key.record == number {
                            continue;
                        }
                        if !self.state(key.record).is_live(key.sequence) {
                            let path = join(&self.path(number), &key.name.name);
                            self.audit.stale_index_entries.push(path);
                        }
                    }
                    if !truncated && let Some(state) = self.states.get_mut(number as usize) {
                        state.flags |= INDEX_COMPLETE;
                    }
                }
                Err(_) => {
                    let path = self.path(number);
                    self.audit.unreadable_indexes.push(path);
                }
            }
            if !progress(AuditStage::Indexes, done as u64 + 1, total) {
                return false;
            }
        }
        listed.sort_unstable();
        listed.dedup();
        let mut missing: Vec<u64> = self
            .links
            .iter()
            .filter(|link| {
                let parent = self.state(link.parent);
                parent.is_live(link.parent_sequence)
                    && parent.has(DIRECTORY)
                    && parent.has(INDEX_COMPLETE)
                    && listed.binary_search(&(link.parent, link.child)).is_err()
            })
            .map(|link| link.child)
            .collect();
        missing.sort_unstable();
        missing.dedup();
        for child in missing {
            let path = self.path(child);
            self.audit.missing_from_index.push(path);
        }
        true
    }

    /// 簇的核对：排好序找几个文件共用的段，再与 `$Bitmap` 逐簇对比。被取消时返回 `false`。
    fn check_clusters(&mut self, progress: &mut dyn FnMut(AuditStage, u64, u64) -> bool) -> bool {
        let Some(bitmap) = self.cluster_bitmap() else {
            return true;
        };
        let mut claims = std::mem::take(&mut self.claims);
        claims.sort_unstable_by_key(|claim| (claim.lcn, claim.length));
        self.find_shared(&claims);
        let merged = merge(&claims);

        let cluster_count = self.ntfs.cluster_count;
        let length = bitmap.size().min(cluster_count.div_ceil(8));
        let mut compare = Compare::default();
        let mut next = 0usize;
        let mut buffer = Vec::new();
        let mut expected = Vec::new();
        let mut offset = 0u64;
        while offset < length {
            let count = BLOCK.min(length - offset);
            buffer.resize(count as usize, 0);
            let first_cluster = offset * 8;
            if bitmap.read_at(offset, &mut buffer).is_err() {
                self.audit.bitmap_unreadable = true;
                compare.close();
                offset += count;
                continue;
            }
            expected.clear();
            expected.resize(count as usize, 0);
            let end_cluster = (first_cluster + count * 8).min(cluster_count);
            while let Some(&(start, end)) = merged.get(next) {
                if start >= end_cluster {
                    break;
                }
                set_bits(
                    &mut expected,
                    start.max(first_cluster) - first_cluster,
                    end.min(end_cluster) - first_cluster,
                );
                if end > end_cluster {
                    break;
                }
                next += 1;
            }
            for (index, (&actual, &want)) in buffer.iter().zip(&expected).enumerate() {
                let cluster = first_cluster + index as u64 * 8;
                let valid = valid_mask(cluster, cluster_count);
                let actual = actual & valid;
                self.audit.used_clusters += u64::from(actual.count_ones());
                if actual == want {
                    compare.close();
                    continue;
                }
                for bit in 0..8u8 {
                    let mask = 1u8 << bit;
                    if valid & mask == 0 {
                        break;
                    }
                    let state = match (actual & mask != 0, want & mask != 0) {
                        (true, false) => Some(Mismatch::Lost),
                        (false, true) => Some(Mismatch::Unmarked),
                        _ => None,
                    };
                    compare.step(cluster + u64::from(bit), state);
                }
            }
            offset += count;
            if !progress(AuditStage::Clusters, offset, length) {
                return false;
            }
        }
        compare.close();
        self.audit.lost_clusters = compare.lost_clusters;
        self.audit.lost_ranges = compare.lost_ranges;
        self.audit.first_lost = compare.first_lost;
        let mut dropped = 0u64;
        for (first, count) in compare.unmarked.items {
            let owner = owner_of(&claims, first);
            let changed = owner.is_some_and(|owner| !self.still_live(owner))
                || !still_unmarked(&bitmap, first, count);
            if changed {
                dropped += 1;
                continue;
            }
            let path = owner.map_or_else(String::new, |owner| self.path(owner));
            self.audit
                .not_in_bitmap
                .items
                .push(UnmarkedClusters { first, count, path });
        }
        self.audit.not_in_bitmap.total = compare.unmarked.total - dropped;
        self.audit.clusters_checked = true;
        true
    }

    /// `$Bitmap` 的数据流；读不出记录或没有数据流时为 `None`（簇的核对不做）。
    fn cluster_bitmap(&self) -> Option<super::stream::Stream> {
        let record = self.ntfs.record(BITMAP_RECORD).ok()??;
        match self.ntfs.value(BITMAP_RECORD, &record, DATA, &[]).ok()?? {
            Value::NonResident { extents, size, .. } => Some(self.ntfs.stream(extents, size)),
            Value::Resident { .. } => None,
        }
    }

    /// 已按起点排序的段里，与前面某段重叠、又属于另一个文件的部分。
    fn find_shared(&mut self, claims: &[Claim]) {
        let mut reach: Option<Claim> = None;
        for claim in claims {
            if let Some(previous) = reach {
                if claim.lcn < previous.end()
                    && claim.owner != previous.owner
                    && self.still_live(previous.owner)
                    && self.still_live(claim.owner)
                {
                    let end = claim.end().min(previous.end());
                    let (path, other_path) = (self.path(previous.owner), self.path(claim.owner));
                    self.audit.shared.push(SharedClusters {
                        first: claim.lcn,
                        count: end - claim.lcn,
                        path,
                        other_path,
                    });
                }
                if claim.end() <= previous.end() {
                    continue;
                }
            }
            reach = Some(*claim);
        }
    }

    /// 这条记录现在还是读 MFT 时的那个在用的文件（不走缓存重读；读不出时按原来的结论）。
    fn still_live(&self, record: u64) -> bool {
        let size = self.ntfs.record_size;
        let Some(offset) = record.checked_mul(size) else {
            return false;
        };
        let mut raw = vec![0u8; size as usize];
        if self.ntfs.mft.read_at(offset, &mut raw).is_err() {
            return true;
        }
        let sequence = self.state(record).sequence;
        Record::parse(raw, record).is_some_and(|now| now.in_use() && now.sequence == sequence)
    }

    /// 记录的路径：沿 `$FILE_NAME` 里的上级目录往上找到根目录。
    fn path(&self, record: u64) -> String {
        let mut parts = Vec::new();
        let mut current = record;
        let mut complete = false;
        for _ in 0..MAX_DEPTH {
            if current == ROOT_RECORD {
                complete = true;
                break;
            }
            let Some(name) = self.name(current) else {
                if parts.is_empty() {
                    return format!("$MFT#{record}");
                }
                break;
            };
            parts.push(name.0);
            current = name.1;
        }
        parts.reverse();
        let joined = parts.join("/");
        if complete {
            format!("/{joined}")
        } else {
            format!("/…/{joined}")
        }
    }

    /// 记录的名字（不用 DOS 短名，除非只有它）与上级目录。
    fn name(&self, record: u64) -> Option<(String, u64)> {
        let record_data = self.ntfs.record(record).ok()??;
        let names = file_names(&record_data);
        let name = names
            .iter()
            .find(|name| name.namespace != NAMESPACE_DOS)
            .or_else(|| names.first())?;
        Some((name.name.clone(), name.parent))
    }
}

fn join(directory: &str, name: &str) -> String {
    if directory.ends_with('/') {
        format!("{directory}{name}")
    } else {
        format!("{directory}/{name}")
    }
}

/// 已按起点排序的段合并成互不重叠的 `[起, 止)`。
fn merge(claims: &[Claim]) -> Vec<(u64, u64)> {
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for claim in claims {
        match merged.last_mut() {
            Some(last) if claim.lcn <= last.1 => last.1 = last.1.max(claim.end()),
            _ => merged.push((claim.lcn, claim.end())),
        }
    }
    merged
}

/// 把 `buf` 里第 `start` 到 `end`（不含）位置为 1。
fn set_bits(buf: &mut [u8], start: u64, end: u64) {
    let mut bit = start;
    while bit < end {
        if bit.is_multiple_of(8) && end - bit >= 8 {
            let bytes = ((end - bit) / 8) as usize;
            let from = (bit / 8) as usize;
            if let Some(slice) = buf.get_mut(from..from + bytes) {
                slice.fill(0xFF);
            }
            bit += bytes as u64 * 8;
            continue;
        }
        if let Some(byte) = buf.get_mut((bit / 8) as usize) {
            *byte |= 1 << (bit % 8);
        }
        bit += 1;
    }
}

/// 从簇 `cluster` 起的一个字节里，哪几位还在卷里。
fn valid_mask(cluster: u64, cluster_count: u64) -> u8 {
    match cluster_count.saturating_sub(cluster) {
        0 => 0,
        left if left >= 8 => 0xFF,
        left => (1u8 << left) - 1,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mismatch {
    /// 已占用、没有记录用到。
    Lost,
    /// 有记录用到、位图里空闲。
    Unmarked,
}

/// 逐簇对比：正在累积的一段，与已结束的各段（用到的文件等对比完再找）。
#[derive(Default)]
struct Compare {
    open: Option<(Mismatch, u64, u64)>,
    lost_clusters: u64,
    lost_ranges: u64,
    first_lost: Option<u64>,
    unmarked: Listed<(u64, u64)>,
}

impl Compare {
    fn step(&mut self, cluster: u64, state: Option<Mismatch>) {
        if let (Some((kind, first, count)), Some(state)) = (&mut self.open, state)
            && *kind == state
            && *first + *count == cluster
        {
            *count += 1;
            return;
        }
        self.close();
        if let Some(state) = state {
            self.open = Some((state, cluster, 1));
        }
    }

    fn close(&mut self) {
        let Some((kind, first, count)) = self.open.take() else {
            return;
        };
        match kind {
            Mismatch::Lost => {
                self.lost_clusters += count;
                self.lost_ranges += 1;
                self.first_lost.get_or_insert(first);
            }
            Mismatch::Unmarked => self.unmarked.push((first, count)),
        }
    }
}

/// 重读 `$Bitmap`（不走缓存）：从 `first` 起的 `count` 个簇现在还都是空闲的。段太长（超过 1 MiB 的位图）或
/// 读不出时按原来的结论。
fn still_unmarked(bitmap: &super::stream::Stream, first: u64, count: u64) -> bool {
    let (start, end) = (first / 8, first.saturating_add(count).div_ceil(8));
    if end.saturating_sub(start) > BLOCK {
        return true;
    }
    let mut bytes = vec![0u8; (end - start) as usize];
    if bitmap.read_at(start, &mut bytes).is_err() {
        return true;
    }
    (first..first.saturating_add(count)).all(|cluster| {
        bytes
            .get((cluster / 8 - start) as usize)
            .is_some_and(|byte| byte & (1 << (cluster % 8)) == 0)
    })
}

/// 用到簇 `cluster` 的文件（按起点排序的段里往前找）。
fn owner_of(claims: &[Claim], cluster: u64) -> Option<u64> {
    let after = claims.partition_point(|claim| claim.lcn <= cluster);
    claims
        .get(..after)?
        .iter()
        .rev()
        .find(|claim| claim.end() > cluster)
        .map(|claim| claim.owner)
}
