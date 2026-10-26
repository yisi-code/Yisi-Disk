//! 扫一遍 MFT：每条记录留一份摘要（序号、是否在用、长度、数据位置、存放方式、可信度），
//! 已删除的记录另外留下名字与父目录引用，扫完按父目录挂回目录树。

use std::collections::HashMap;

use yisi_part::bytes::{le32, le64};

use super::record::{
    ATTRIBUTE_LIST, Body, DATA, NAMESPACE_DOS, REPARSE_POINT, Record, STANDARD_INFORMATION,
    decode_runs, split_ref,
};
use super::stream::Stream;
use super::{BITMAP_RECORD, Ntfs, Value, file_names, truncated_on_delete, unsupported};
use crate::entry::{Entry, FileRef, Location, State, Unsupported, nt_time};
use crate::{FsError, ScanSummary};

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
            _ => State::Live,
        }
    }

    fn set_state(&mut self, state: State) {
        let code = match state {
            State::Live => 0,
            State::Intact => 1,
            State::Overwritten => 2,
            State::Unknown => 3,
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
}

/// 在用的扩展记录里找到的数据流开头一段：基本记录里没有数据流时用它补长度与位置。
struct ExtensionData {
    size: u64,
    position: u64,
    compressed: bool,
    encrypted: bool,
}

pub(crate) struct Scan {
    pub summary: ScanSummary,
    records: Vec<Summary>,
    names: Vec<DeletedName>,
    /// 父目录记录号 → 挂在它下面的已删除名字（`names` 的下标）。
    children: HashMap<u64, Vec<u32>>,
    orphans: Vec<u32>,
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
        let record_count = ntfs.record_count;
        if record_count > MAX_RECORDS {
            return Err(FsError::Invalid("MFT 记录超过 1600 万条，暂不支持扫描"));
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

    /// 挂在目录 `record`（序号 `sequence`）下的已删除项。
    pub fn child_entries(&self, record: u64, sequence: u16) -> Vec<Entry> {
        let matches = usize::try_from(record)
            .ok()
            .and_then(|index| self.records.get(index))
            .is_some_and(|summary| summary.has(DIRECTORY) && summary.sequence == sequence);
        if !matches {
            return Vec::new();
        }
        self.children
            .get(&record)
            .map(|children| {
                children
                    .iter()
                    .filter_map(|&index| self.entry(index))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn orphan_entries(&self) -> Vec<Entry> {
        self.orphans
            .iter()
            .filter_map(|&index| self.entry(index))
            .collect()
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
            modified: nt_time(name.modified),
            created: nt_time(name.created),
            state: if is_dir {
                State::Intact
            } else {
                summary.state()
            },
            position: summary.position(),
            unsupported: summary.unsupported(),
            file: FileRef {
                size,
                is_dir,
                deleted: true,
                location: Location::Mft {
                    record: name.record,
                    sequence: summary.sequence,
                },
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
                } else if data.compressed {
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

        // 按父目录挂：父目录还在用时序号必须相等；父目录也已删除时，删除会让序号加 1，两种都认。
        // 父记录已被别的文件重用、不是目录、或就是自己的，放进「找不到原目录」
        let mut children: HashMap<u64, Vec<u32>> = HashMap::new();
        let mut orphans = Vec::new();
        for (index, name) in self.names.iter().enumerate() {
            let parent = usize::try_from(name.parent)
                .ok()
                .and_then(|parent| self.records.get(parent))
                .filter(|parent| parent.has(VALID) && parent.has(BASE) && parent.has(DIRECTORY));
            let attached = name.parent != name.record
                && parent.is_some_and(|parent| {
                    if parent.has(IN_USE) {
                        parent.sequence == name.parent_sequence
                    } else {
                        parent.sequence == name.parent_sequence
                            || parent.sequence == name.parent_sequence.wrapping_add(1)
                    }
                });
            let index = index as u32;
            if attached {
                children.entry(name.parent).or_default().push(index);
            } else {
                orphans.push(index);
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
                orphans: orphans.len() as u64,
                unreadable: self.unreadable,
            },
            records: self.records,
            names: self.names,
            children,
            orphans,
        }
    }
}

fn apply_extension(summary: &mut Summary, data: &ExtensionData, record: &Record) {
    summary.size = data.size;
    summary.position = data.position;
    let attributes = standard_attributes(record);
    summary.set_unsupported(unsupported(
        data.compressed,
        data.encrypted || attributes & super::FILE_ENCRYPTED != 0,
        reparse_tag(record),
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
            compressed: attribute.compressed(),
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

fn reparse_tag(record: &Record) -> Option<u32> {
    record
        .find(REPARSE_POINT, &[])
        .and_then(|attribute| attribute.resident_value())
        .and_then(|value| le32(value, 0))
}
