//! 扫一遍 MFT：每条记录留一份摘要（序号、是否在用、长度、数据位置、存放方式、可信度），
//! 已删除的记录另外留下名字与父目录引用，扫完按父目录挂回目录树。
//! 之后读 `$LogFile`（[`logfile`](super::logfile)），把 MFT 里已找不到的已删除项一并挂上。

use std::collections::{HashMap, HashSet};

use yisi_part::bytes::{le32, le64};

use super::logfile::{self, LogFile, Logged};
use super::record::{
    ATTRIBUTE_LIST, Body, DATA, NAMESPACE_DOS, Record, STANDARD_INFORMATION, decode_runs, split_ref,
};
use super::stream::Stream;
use super::wof::wof_algorithm;
use super::{
    BITMAP_RECORD, Ntfs, Value, file_names, reparse_point, truncated_on_delete, unsupported,
};
use crate::entry::{Entry, FileAttributes, FileRef, Location, Stamp, State, Unsupported};
use crate::{FsError, ScanStage, ScanSummary};

/// 一次读这么多字节的 MFT 或分配位图。
const BLOCK: u64 = 1 << 20;
/// MFT 记录超过这么多条时不扫（摘要表每条 24 字节，约 384 MiB）。
const MAX_RECORDS: u64 = 16 << 20;
/// 已删除的名字最多留这么多个。
const MAX_DELETED_NAMES: usize = 4 << 20;
/// 分配位图不超过这么大时整份读进内存，否则按块缓存着读。
const MAX_LOADED_BITMAP: u64 = 512 << 20;

/// 摘要的标志。
const VALID: u8 = 0x01;
const IN_USE: u8 = 0x02;
const DIRECTORY: u8 = 0x04;
const BASE: u8 = 0x08;
/// 在用的文件有属性列表、数据流不在基本记录里：等扫到它的扩展记录再补长度与位置。
const NEEDS_EXTENSION: u8 = 0x10;
const NO_POSITION: u64 = u64::MAX;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Summary {
    pub size: u64,
    position: u64,
    sequence: u16,
    flags: u8,
    /// 低 4 位：存放方式（0 为支持）；高 4 位：可信度（只对已删除的记录有意义）。
    detail: u8,
}

impl Summary {
    const INVALID: Self = Self {
        size: 0,
        position: NO_POSITION,
        sequence: 0,
        flags: 0,
        detail: 0,
    };

    fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    pub fn position(&self) -> Option<u64> {
        (self.position != NO_POSITION).then_some(self.position)
    }

    pub fn unsupported(&self) -> Option<Unsupported> {
        match self.detail & 0x0F {
            1 => Some(Unsupported::Compressed),
            2 => Some(Unsupported::Encrypted),
            3 => Some(Unsupported::SystemCompressed),
            4 => Some(Unsupported::Reparse),
            _ => None,
        }
    }

    fn set_unsupported(&mut self, reason: Option<Unsupported>) {
        let code = match reason {
            None => 0,
            Some(Unsupported::Compressed) => 1,
            Some(Unsupported::Encrypted) => 2,
            Some(Unsupported::SystemCompressed) => 3,
            Some(Unsupported::Reparse) => 4,
        };
        self.detail = (self.detail & 0xF0) | code;
    }

    fn state(&self) -> State {
        match self.detail >> 4 {
            1 => State::Intact,
            2 => State::Overwritten,
            3 => State::Unknown,
            4 => State::Cleared,
            _ => State::Live,
        }
    }

    fn set_state(&mut self, state: State) {
        let code = match state {
            State::Live => 0,
            State::Intact => 1,
            State::Overwritten => 2,
            State::Unknown => 3,
            State::Cleared => 4,
        };
        self.detail = (self.detail & 0x0F) | (code << 4);
    }

    fn set_data(&mut self, data: Option<&Value>, cluster_size: u64) {
        match data {
            None => {
                self.size = 0;
                self.position = NO_POSITION;
            }
            Some(Value::Resident {
                bytes, position, ..
            }) => {
                self.size = bytes.len() as u64;
                self.position = position.unwrap_or(NO_POSITION);
            }
            Some(Value::NonResident { extents, size, .. }) => {
                self.size = *size;
                self.position = extents
                    .first()
                    .and_then(|extent| extent.lcn)
                    .and_then(|lcn| lcn.checked_mul(cluster_size))
                    .unwrap_or(NO_POSITION);
            }
        }
    }
}

/// 已删除记录的一个名字（有硬链接的文件可能有几个）。
struct DeletedName {
    record: u64,
    parent: u64,
    parent_sequence: u16,
    name: String,
    short_name: Option<String>,
    created: u64,
    modified: u64,
    /// `$FILE_NAME` 里的文件属性。
    attributes: u32,
}

/// 在用的扩展记录里找到的数据流开头一段：基本记录里没有数据流时用它补长度与位置。
struct ExtensionData {
    size: u64,
    position: u64,
    /// 压缩了、又解不开（[`super::record::Attribute::undecodable_compression`]）。
    undecodable_compression: bool,
    encrypted: bool,
}

/// 从 `$LogFile` 找回的一个名字。
struct LoggedName {
    record: u64,
    sequence: u16,
    parent: u64,
    parent_sequence: u16,
    name: String,
    short_name: Option<String>,
    created: u64,
    modified: u64,
    attributes: u32,
    /// 长度、数据位置、存放方式、可信度（按删除前的运行表判断）；`DIRECTORY` 标志表示目录。
    summary: Summary,
}

/// 挂在某个目录下的一项：MFT 里的已删除名字，或从日志找回的名字。
#[derive(Clone, Copy)]
enum Child {
    Mft(u32),
    Logged(u32),
}

pub(crate) struct Scan {
    pub summary: ScanSummary,
    records: Vec<Summary>,
    names: Vec<DeletedName>,
    /// 父目录记录号 → 挂在它下面的已删除名字（`names` 的下标）。
    children: HashMap<u64, Vec<u32>>,
    orphans: Vec<u32>,
    logged: Vec<LoggedName>,
    /// 从日志找回、内容读得出的文件删除前的记录映像：（记录号，序号）→ 映像。
    logged_images: HashMap<(u64, u16), Vec<u8>>,
    /// 目录列出时的（记录号，序号）→ 另外挂在它下面的项：从日志找回的名字，以及上级目录只在日志里找得到的
    /// MFT 已删除名字。
    extra_children: HashMap<(u64, u16), Vec<Child>>,
    logged_orphans: Vec<u32>,
}

/// 簇的分配位图：一位一个簇，1 为已占用。
enum ClusterBitmap {
    Loaded(Vec<u8>),
    Streamed(Stream),
}

impl ClusterBitmap {
    /// `count` 个从 `lcn` 起的簇里有没有已占用的；超出位图时为 `None`。
    fn any_allocated(&self, lcn: u64, count: u64) -> Result<Option<bool>, FsError> {
        let Some(end) = lcn.checked_add(count) else {
            return Ok(None);
        };
        let mut cluster = lcn;
        while cluster < end {
            let Some(byte) = self.byte(cluster / 8)? else {
                return Ok(None);
            };
            if cluster.is_multiple_of(8) && end - cluster >= 8 {
                if byte != 0 {
                    return Ok(Some(true));
                }
                cluster += 8;
                continue;
            }
            if byte & (1 << (cluster % 8)) != 0 {
                return Ok(Some(true));
            }
            cluster += 1;
        }
        Ok(Some(false))
    }

    fn byte(&self, index: u64) -> Result<Option<u8>, FsError> {
        match self {
            Self::Loaded(bytes) => Ok(bytes.get(index as usize).copied()),
            Self::Streamed(stream) => {
                if index >= stream.size() {
                    return Ok(None);
                }
                let mut byte = [0u8];
                stream.read_cached(index, &mut byte)?;
                let [value] = byte;
                Ok(Some(value))
            }
        }
    }
}

impl Scan {
    /// 先读分配位图，再按 1 MiB 一段顺序读 MFT。`progress` 返回 `false` 时停下，结果为 `None`。
    pub fn run(
        ntfs: &Ntfs,
        progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<Option<Self>, FsError> {
        ntfs.set_scan_stage(ScanStage::Mft);
        let record_count = ntfs.record_count;
        if record_count > MAX_RECORDS {
            return Err(FsError::Invalid(
                "主文件表（MFT）记录超过 1600 万条，暂不支持扫描",
            ));
        }
        let record_size = ntfs.record_size;
        let bitmap_record = ntfs
            .record(BITMAP_RECORD)?
            .ok_or(FsError::Invalid("$Bitmap 的记录读不出"))?;
        let bitmap_value = ntfs
            .value(BITMAP_RECORD, &bitmap_record, DATA, &[])?
            .ok_or(FsError::Invalid("$Bitmap 没有数据流"))?;
        let bitmap_length = ntfs.cluster_count.div_ceil(8);
        let mft_bytes = record_count * record_size;
        let total = bitmap_length.saturating_add(mft_bytes);
        let mut done = 0u64;

        let bitmap = match bitmap_value {
            Value::Resident { bytes, .. } => ClusterBitmap::Loaded(bytes),
            Value::NonResident { extents, size, .. } => {
                let stream = ntfs.stream(extents, size.min(bitmap_length));
                if stream.size() > MAX_LOADED_BITMAP {
                    done += bitmap_length;
                    ClusterBitmap::Streamed(stream)
                } else {
                    let mut bytes = vec![0u8; stream.size() as usize];
                    for (index, block) in bytes.chunks_mut(BLOCK as usize).enumerate() {
                        // 读不出的部分按已占用算：宁可把删除的文件判成「已被覆盖」，也不误判成完整
                        if stream.read_at(index as u64 * BLOCK, block).is_err() {
                            block.fill(0xFF);
                        }
                        done += block.len() as u64;
                        if !progress(done, total) {
                            return Ok(None);
                        }
                    }
                    ClusterBitmap::Loaded(bytes)
                }
            }
        };

        let mut builder = Builder {
            ntfs,
            bitmap: &bitmap,
            records: Vec::with_capacity(record_count as usize),
            names: Vec::new(),
            extensions: HashMap::new(),
            pending: Vec::new(),
            unreadable: 0,
            logged: Vec::new(),
            logged_images: HashMap::new(),
            log_records: 0,
        };
        let per_block = (BLOCK / record_size).max(1);
        let mut buffer = Vec::new();
        let mut first = 0u64;
        while first < record_count {
            let count = per_block.min(record_count - first);
            buffer.resize((count * record_size) as usize, 0);
            if ntfs.mft.read_at(first * record_size, &mut buffer).is_ok() {
                for (index, raw) in buffer.chunks_exact(record_size as usize).enumerate() {
                    builder.add(first + index as u64, raw.to_vec());
                }
            } else {
                // 整段读不出时逐条重读，只放弃真正读不出的记录
                for number in first..first + count {
                    let mut raw = vec![0u8; record_size as usize];
                    if ntfs.mft.read_at(number * record_size, &mut raw).is_ok() {
                        builder.add(number, raw);
                    } else {
                        builder.unreadable += 1;
                        builder.records.push(Summary::INVALID);
                    }
                }
            }
            first += count;
            done += count * record_size;
            if !progress(done, total) {
                return Ok(None);
            }
        }
        // 日志读不出（没有 $LogFile、重启区坏了）不影响扫描的结果，只是没有从日志找回的项
        ntfs.set_scan_stage(ScanStage::Log);
        let log = match logfile::read(ntfs, progress) {
            Ok(Some(log)) => log,
            Ok(None) => return Ok(None),
            Err(_) => LogFile::default(),
        };
        builder.add_logged(log);
        Ok(Some(builder.finish()))
    }

    /// 记录读得出，却已不是索引项指的那个文件（已删除、序号变了、成了扩展记录）。读不出的记录不算。
    pub fn is_stale(&self, record: u64, sequence: u16) -> bool {
        usize::try_from(record)
            .ok()
            .and_then(|index| self.records.get(index))
            .is_some_and(|summary| {
                summary.has(VALID)
                    && (!summary.has(IN_USE) || !summary.has(BASE) || summary.sequence != sequence)
            })
    }

    /// 在用、序号对得上的基本记录的摘要。
    pub fn live(&self, record: u64, sequence: u16) -> Option<&Summary> {
        const LIVE_BASE: u8 = VALID | IN_USE | BASE;
        self.records
            .get(usize::try_from(record).ok()?)
            .filter(|summary| {
                summary.flags & LIVE_BASE == LIVE_BASE && summary.sequence == sequence
            })
    }

    /// 挂在目录 `record`（序号 `sequence`）下的已删除项，含从日志找回的。
    pub fn child_entries(&self, record: u64, sequence: u16) -> Vec<Entry> {
        let matches = usize::try_from(record)
            .ok()
            .and_then(|index| self.records.get(index))
            .is_some_and(|summary| summary.has(DIRECTORY) && summary.sequence == sequence);
        let mut entries: Vec<Entry> = if matches {
            self.children
                .get(&record)
                .map(|children| {
                    children
                        .iter()
                        .filter_map(|&index| self.entry(index))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        entries.extend(self.logged_children(record, sequence));
        entries
    }

    /// 另外挂在目录 `record`（列出时的序号 `sequence`）下的项；从日志找回的目录只有这些。
    pub fn logged_children(&self, record: u64, sequence: u16) -> Vec<Entry> {
        self.extra_children
            .get(&(record, sequence))
            .map(|children| {
                children
                    .iter()
                    .filter_map(|&child| match child {
                        Child::Mft(index) => self.entry(index),
                        Child::Logged(index) => self.logged_entry(index),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn orphan_entries(&self) -> Vec<Entry> {
        self.orphans
            .iter()
            .filter_map(|&index| self.entry(index))
            .chain(
                self.logged_orphans
                    .iter()
                    .filter_map(|&index| self.logged_entry(index)),
            )
            .collect()
    }

    /// 从日志找回、内容读得出的文件删除前的记录映像。
    pub fn logged_image(&self, record: u64, sequence: u16) -> Option<&[u8]> {
        self.logged_images
            .get(&(record, sequence))
            .map(Vec::as_slice)
    }

    fn logged_entry(&self, index: u32) -> Option<Entry> {
        let name = self.logged.get(index as usize)?;
        let summary = &name.summary;
        let is_dir = summary.has(DIRECTORY);
        let size = if is_dir { 0 } else { summary.size };
        Some(Entry {
            name: name.name.clone(),
            short_name: name.short_name.clone(),
            name_uncertain: false,
            size,
            modified: Stamp::Nt(name.modified).text(),
            created: Stamp::Nt(name.created).text(),
            state: summary.state(),
            position: summary.position(),
            unsupported: summary.unsupported(),
            attributes: FileAttributes::from_bits(name.attributes),
            file: FileRef {
                size,
                is_dir,
                deleted: true,
                location: Location::Logged {
                    record: name.record,
                    sequence: name.sequence,
                },
                modified: (name.modified != 0).then_some(Stamp::Nt(name.modified)),
            },
        })
    }

    fn entry(&self, index: u32) -> Option<Entry> {
        let name = self.names.get(index as usize)?;
        let summary = self.records.get(usize::try_from(name.record).ok()?)?;
        let is_dir = summary.has(DIRECTORY);
        let size = if is_dir { 0 } else { summary.size };
        Some(Entry {
            name: name.name.clone(),
            short_name: name.short_name.clone(),
            name_uncertain: false,
            size,
            modified: Stamp::Nt(name.modified).text(),
            created: Stamp::Nt(name.created).text(),
            state: if is_dir {
                State::Intact
            } else {
                summary.state()
            },
            position: summary.position(),
            unsupported: summary.unsupported(),
            attributes: FileAttributes::from_bits(name.attributes),
            file: FileRef {
                size,
                is_dir,
                deleted: true,
                location: Location::Mft {
                    record: name.record,
                    sequence: summary.sequence,
                },
                modified: (name.modified != 0).then_some(Stamp::Nt(name.modified)),
            },
        })
    }
}

struct Builder<'a> {
    ntfs: &'a Ntfs,
    bitmap: &'a ClusterBitmap,
    records: Vec<Summary>,
    names: Vec<DeletedName>,
    extensions: HashMap<u64, ExtensionData>,
    /// 有属性列表的已删除记录：扫完再跟着属性列表去读扩展记录。
    pending: Vec<u64>,
    unreadable: u64,
    logged: Vec<LoggedName>,
    logged_images: HashMap<(u64, u16), Vec<u8>>,
    log_records: u64,
}

impl Builder<'_> {
    fn add(&mut self, number: u64, raw: Vec<u8>) {
        let Some(record) = Record::parse(raw, number) else {
            self.records.push(Summary::INVALID);
            return;
        };
        let mut summary = Summary {
            size: 0,
            position: NO_POSITION,
            sequence: record.sequence,
            flags: VALID
                | if record.in_use() { IN_USE } else { 0 }
                | if record.is_dir() { DIRECTORY } else { 0 },
            detail: 0,
        };
        if !record.is_base() {
            if record.in_use()
                && let Some(data) = extension_data(&record, self.ntfs.cluster_size)
            {
                self.extensions.insert(split_ref(record.base).0, data);
            }
            self.records.push(summary);
            return;
        }
        summary.flags |= BASE;
        let has_list = record.find(ATTRIBUTE_LIST, &[]).is_some();
        if !has_list {
            self.describe(number, &record, &mut summary);
        } else if record.in_use() {
            // 在用的文件只要长度与位置：基本记录里有数据流开头一段就用它，否则等扩展记录
            match extension_data(&record, self.ntfs.cluster_size) {
                Some(data) => apply_extension(&mut summary, &data, &record),
                None if !record.is_dir() => summary.flags |= NEEDS_EXTENSION,
                None => {}
            }
        } else {
            self.pending.push(number);
        }
        if !record.in_use() {
            self.add_names(number, &record);
        }
        self.records.push(summary);
    }

    /// 没有属性列表（或扫完后补做）的记录：数据流、存放方式，已删除的再判断可信度。
    fn describe(&self, number: u64, record: &Record, summary: &mut Summary) {
        match self.ntfs.describe(number, record) {
            Ok(described) => {
                summary.set_data(described.data.as_ref(), self.ntfs.cluster_size);
                summary.set_unsupported(described.unsupported);
                if truncated_on_delete(record, described.data.as_ref()) {
                    summary.set_state(State::Unknown);
                } else if !record.in_use() {
                    let state = self
                        .deleted_state(described.data.as_ref())
                        .unwrap_or(State::Unknown);
                    summary.set_state(state);
                }
            }
            // 属性列表或运行表对不上：删除的文件说不准能不能找回
            Err(_) => summary.set_state(State::Unknown),
        }
    }

    fn deleted_state(&self, data: Option<&Value>) -> Result<State, FsError> {
        let Some(Value::NonResident {
            extents,
            size,
            compressed,
            ..
        }) = data
        else {
            // 没有数据流，或内容就在记录里
            return Ok(State::Intact);
        };
        let cluster_size = self.ntfs.cluster_size;
        let needed = size.div_ceil(cluster_size);
        let mut covered = 0u64;
        for extent in extents {
            if covered >= needed {
                break;
            }
            let count = extent.length.min(needed - covered);
            covered += count;
            let Some(lcn) = extent.lcn else {
                continue;
            };
            if lcn.saturating_add(count) > self.ntfs.cluster_count {
                return Ok(State::Unknown);
            }
            match self.bitmap.any_allocated(lcn, count)? {
                Some(true) => return Ok(State::Overwritten),
                Some(false) => {}
                None => return Ok(State::Unknown),
            }
        }
        // 压缩文件的运行表本来就比长度短
        Ok(if covered < needed && !compressed {
            State::Unknown
        } else {
            State::Intact
        })
    }

    fn add_names(&mut self, number: u64, record: &Record) {
        let names = file_names(record);
        let short = names
            .iter()
            .find(|name| name.namespace == NAMESPACE_DOS)
            .map(|name| name.name.clone());
        let (created, modified) = standard_times(record);
        let long: Vec<_> = names
            .iter()
            .filter(|name| name.namespace != NAMESPACE_DOS)
            .collect();
        // 只有 DOS 名（不该有，但扫到写坏的记录时可能）就用它
        let chosen = if long.is_empty() {
            names.iter().collect()
        } else {
            long
        };
        for name in chosen {
            if self.names.len() >= MAX_DELETED_NAMES {
                return;
            }
            self.names.push(DeletedName {
                record: number,
                parent: name.parent,
                parent_sequence: name.parent_sequence,
                name: name.name.clone(),
                short_name: short.clone().filter(|short| *short != name.name),
                created: if created == 0 { name.created } else { created },
                modified: if modified == 0 {
                    name.modified
                } else {
                    modified
                },
                attributes: name.flags,
            });
        }
    }

    fn finish(mut self) -> Scan {
        // 在用文件的数据流在扩展记录里
        for (number, summary) in self.records.iter_mut().enumerate() {
            if summary.has(NEEDS_EXTENSION)
                && let Some(data) = self.extensions.get(&(number as u64))
            {
                summary.size = data.size;
                summary.position = data.position;
                if data.encrypted {
                    summary.set_unsupported(Some(Unsupported::Encrypted));
                } else if data.undecodable_compression {
                    summary.set_unsupported(Some(Unsupported::Compressed));
                }
            }
        }
        // 有属性列表的已删除记录：跟着属性列表读扩展记录
        for number in std::mem::take(&mut self.pending) {
            let Some(index) = usize::try_from(number).ok() else {
                continue;
            };
            let mut summary = self.records.get(index).copied().unwrap_or(Summary::INVALID);
            match self.ntfs.record(number) {
                Ok(Some(record)) => self.describe(number, &record, &mut summary),
                _ => summary.set_state(State::Unknown),
            }
            if let Some(slot) = self.records.get_mut(index) {
                *slot = summary;
            }
        }

        // 按父目录挂（规则见 mft_parent）；MFT 里找不到、日志里找回了的上级目录也认。
        // 都找不到的，或父记录就是自己的，放进「找不到原目录」
        let logged_dirs: HashSet<(u64, u16)> = self
            .logged
            .iter()
            .filter(|name| name.summary.has(DIRECTORY))
            .map(|name| (name.record, name.sequence))
            .collect();
        let logged_parent = |parent: u64, sequence: u16, own: (u64, u16)| {
            (parent != own.0 && logged_dirs.contains(&(parent, sequence)))
                .then_some((parent, sequence))
        };
        let mut children: HashMap<u64, Vec<u32>> = HashMap::new();
        let mut extra_children: HashMap<(u64, u16), Vec<Child>> = HashMap::new();
        let mut orphans = Vec::new();
        for (index, name) in self.names.iter().enumerate() {
            let index = index as u32;
            if name.parent != name.record
                && mft_parent(&self.records, name.parent, name.parent_sequence).is_some()
            {
                children.entry(name.parent).or_default().push(index);
            } else if let Some(key) =
                logged_parent(name.parent, name.parent_sequence, (name.record, 0))
            {
                extra_children
                    .entry(key)
                    .or_default()
                    .push(Child::Mft(index));
            } else {
                orphans.push(index);
            }
        }
        let mut logged_orphans = Vec::new();
        for (index, name) in self.logged.iter().enumerate() {
            let index = index as u32;
            let own = (name.record, name.sequence);
            let key = mft_parent(&self.records, name.parent, name.parent_sequence)
                .filter(|_| name.parent != name.record)
                .map(|sequence| (name.parent, sequence))
                .or_else(|| logged_parent(name.parent, name.parent_sequence, own));
            match key {
                Some(key) => extra_children
                    .entry(key)
                    .or_default()
                    .push(Child::Logged(index)),
                None => logged_orphans.push(index),
            }
        }
        let deleted = self
            .records
            .iter()
            .filter(|summary| summary.flags & (VALID | IN_USE | BASE) == VALID | BASE)
            .count() as u64;
        let in_use = self
            .records
            .iter()
            .filter(|summary| summary.flags & (VALID | IN_USE) == VALID | IN_USE)
            .count() as u64;
        Scan {
            summary: ScanSummary {
                records: self.records.len() as u64,
                in_use,
                deleted,
                deleted_names: self.names.len() as u64,
                orphans: (orphans.len() + logged_orphans.len()) as u64,
                unreadable: self.unreadable,
                free_bytes: 0,
                log_records: self.log_records,
                log_names: self.logged.len() as u64,
            },
            records: self.records,
            names: self.names,
            children,
            orphans,
            logged: self.logged,
            logged_images: self.logged_images,
            extra_children,
            logged_orphans,
        }
    }

    /// 日志里找到的文件：MFT 里还在的（同一条记录、同一个序号）以 MFT 为准，其余的按删除前的记录映像判断长度、
    /// 数据位置与可信度，只有名字的列出、读不出。
    fn add_logged(&mut self, log: LogFile) {
        self.log_records = log.records;
        let named: HashSet<u64> = self.names.iter().map(|name| name.record).collect();
        for found in log.found {
            if self.in_mft(&found, &named) {
                continue;
            }
            let (summary, image, times) = self.logged_summary(&found);
            if let Some(image) = image {
                self.logged_images
                    .insert((found.record, found.sequence), image);
            }
            for name in found.names {
                if self.logged.len() >= MAX_DELETED_NAMES {
                    return;
                }
                let (created, modified) = times.unwrap_or((name.created, name.modified));
                self.logged.push(LoggedName {
                    record: found.record,
                    sequence: found.sequence,
                    parent: name.parent,
                    parent_sequence: name.parent_sequence,
                    short_name: found.short_name.clone().filter(|short| *short != name.name),
                    name: name.name,
                    created,
                    modified,
                    attributes: name.flags,
                    summary,
                });
            }
        }
    }

    /// 这个（记录号，序号）在 MFT 里还在：在用的记录序号相同；已删除的记录序号相同或大 1（释放时加 1），且列出了名字。
    fn in_mft(&self, found: &Logged, named: &HashSet<u64>) -> bool {
        let Some(current) = usize::try_from(found.record)
            .ok()
            .and_then(|index| self.records.get(index))
            .filter(|current| current.has(VALID) && current.has(BASE))
        else {
            return false;
        };
        if current.has(IN_USE) {
            current.sequence == found.sequence
        } else {
            (current.sequence == found.sequence
                || current.sequence == found.sequence.wrapping_add(1))
                && named.contains(&found.record)
        }
    }

    /// 从日志找回的一项的摘要、读得出时的记录映像、记录里的创建与修改时间。
    /// 读不出（只有名字、常驻内容不在日志里、记录映像对不上）的文件记为「位置已不知道」（[`State::Cleared`]）。
    fn logged_summary(&self, found: &Logged) -> (Summary, Option<Vec<u8>>, Option<(u64, u64)>) {
        let mut summary = Summary {
            size: found.names.first().map_or(0, |name| name.size),
            position: NO_POSITION,
            sequence: found.sequence,
            flags: VALID | BASE | if found.is_dir { DIRECTORY } else { 0 },
            detail: 0,
        };
        let record = found
            .image
            .as_ref()
            .and_then(|image| Record::parse_image(image.clone(), found.record));
        let times = record
            .as_ref()
            .map(standard_times)
            .filter(|&times| times != (0, 0));
        if found.is_dir {
            summary.size = 0;
            summary.set_state(State::Intact);
            return (summary, None, times);
        }
        summary.set_state(State::Cleared);
        let (Some(record), Some(image)) = (record, &found.image) else {
            return (summary, None, times);
        };
        let Ok(described) = self.ntfs.describe(found.record, &record) else {
            return (summary, None, times);
        };
        summary.set_data(described.data.as_ref(), self.ntfs.cluster_size);
        summary.set_unsupported(described.unsupported);
        match &described.data {
            // 常驻的内容：位置是现在 MFT 里那条记录的，已不是这个文件；内容的改动不进日志
            Some(Value::Resident { .. }) => {
                summary.position = NO_POSITION;
                if found.resident_unknown {
                    return (summary, None, times);
                }
                summary.set_state(State::Intact);
            }
            data => {
                let state = self.deleted_state(data.as_ref()).unwrap_or(State::Unknown);
                summary.set_state(state);
            }
        }
        (summary, Some(image.clone()), times)
    }
}

/// 名字的父目录在 MFT 里、挂得上时，父目录列出时的序号：父目录还在用时序号必须相等；父目录也已删除时，
/// 删除会让序号加 1，两种都认。父记录已被别的文件重用、不是目录时为 `None`。
fn mft_parent(records: &[Summary], parent: u64, parent_sequence: u16) -> Option<u16> {
    let summary = records
        .get(usize::try_from(parent).ok()?)
        .filter(|parent| parent.has(VALID) && parent.has(BASE) && parent.has(DIRECTORY))?;
    let attached = if summary.has(IN_USE) {
        summary.sequence == parent_sequence
    } else {
        summary.sequence == parent_sequence || summary.sequence == parent_sequence.wrapping_add(1)
    };
    attached.then_some(summary.sequence)
}

fn apply_extension(summary: &mut Summary, data: &ExtensionData, record: &Record) {
    summary.size = data.size;
    summary.position = data.position;
    let attributes = standard_attributes(record);
    let reparse = reparse_point(record);
    summary.set_unsupported(unsupported(
        data.undecodable_compression,
        data.encrypted || attributes & super::FILE_ENCRYPTED != 0,
        reparse.and_then(|value| le32(value, 0)),
        reparse.and_then(wof_algorithm).is_some(),
        data.size,
        record.is_dir(),
    ));
}

/// 记录里无名 `$DATA` 的开头一段（虚拟簇号从 0 起）的长度与位置。
fn extension_data(record: &Record, cluster_size: u64) -> Option<ExtensionData> {
    record.attributes().find_map(|attribute| {
        if attribute.kind != DATA || !attribute.name_is(&[]) {
            return None;
        }
        let (size, position) = match &attribute.body {
            Body::NonResident(body) if body.lowest_vcn == 0 => {
                let first = decode_runs(body.runs, 0)?.into_iter().next();
                let position = first
                    .and_then(|extent| extent.lcn)
                    .and_then(|lcn| lcn.checked_mul(cluster_size))
                    .unwrap_or(NO_POSITION);
                (body.size, position)
            }
            Body::Resident { value, .. } => (value.len() as u64, NO_POSITION),
            Body::NonResident(_) => return None,
        };
        Some(ExtensionData {
            size,
            position,
            undecodable_compression: attribute.undecodable_compression(cluster_size),
            encrypted: attribute.encrypted(),
        })
    })
}

fn standard_times(record: &Record) -> (u64, u64) {
    record
        .find(STANDARD_INFORMATION, &[])
        .and_then(|attribute| attribute.resident_value())
        .map_or((0, 0), |value| {
            (le64(value, 0).unwrap_or(0), le64(value, 8).unwrap_or(0))
        })
}

fn standard_attributes(record: &Record) -> u32 {
    record
        .find(STANDARD_INFORMATION, &[])
        .and_then(|attribute| attribute.resident_value())
        .and_then(|value| le32(value, 0x20))
        .unwrap_or(0)
}
