//! NTFS：引导扇区、MFT 记录、常驻与非常驻属性（含属性列表）、`$I30` 目录索引；
//! 扫一遍 MFT 找出「未使用」的记录，按父目录引用挂回目录树，用 `$Bitmap` 判断它们的簇是否已被占用。
//! 依据公开的 NTFS 结构资料实现，只读。

mod index;
mod record;
mod scan;
mod stream;

use std::sync::{Arc, Mutex, PoisonError};

use yisi_part::bytes::{le16, le32, le64};

use self::record::{
    ATTRIBUTE_LIST, Body, DATA, Extent, FILE_NAME, NAMESPACE_DOS, REPARSE_POINT, Record,
    STANDARD_INFORMATION, VOLUME_NAME, decode_runs, parse_file_name, split_ref,
};
use self::scan::Scan;
use self::stream::Stream;
use crate::entry::{Entry, FileRef, Listing, Location, State, Unsupported, nt_time};
use crate::reader::{FileReader, Piece};
use crate::region::Region;
use crate::{FsError, MAX_ENTRIES, ScanSummary};

/// 系统文件的记录号。
const MFT_RECORD: u64 = 0;
const VOLUME_RECORD: u64 = 3;
const ROOT_RECORD: u64 = 5;
const BITMAP_RECORD: u64 = 6;
/// 0 到 15 号记录留给系统文件，不列出。
const FIRST_USER_RECORD: u64 = 16;

/// 文件属性（`$STANDARD_INFORMATION` 与 `$FILE_NAME` 里的）。
const FILE_REPARSE_POINT: u32 = 0x0400;
const FILE_COMPRESSED: u32 = 0x0800;
const FILE_ENCRYPTED: u32 = 0x4000;
/// `$FILE_NAME` 里表示目录的位。
const FILE_NAME_DIRECTORY: u32 = 0x1000_0000;
/// 重分析标记：Windows 的系统压缩（WOF）、目录联接、符号链接。
const TAG_WOF: u32 = 0x8000_0017;
const TAG_MOUNT_POINT: u32 = 0xA000_0003;
const TAG_SYMLINK: u32 = 0xA000_000C;

/// 属性列表最多读这么多字节、跟这么多条扩展记录。
const MAX_ATTRIBUTE_LIST: u64 = 1 << 20;
const MAX_EXTENSIONS: usize = 4096;

/// 引导扇区里的几何参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub bytes_per_sector: u64,
    pub cluster_size: u64,
    pub total_bytes: u64,
    pub mft_lcn: u64,
    pub mirror_lcn: u64,
    pub record_size: u64,
}

/// 「每条记录几簇」这类字段：正数是簇数，负数 -n 表示 2^n 字节。
fn clusters_or_shift(raw: u8, cluster_size: u64) -> Option<u64> {
    let value = raw as i8;
    if value > 0 {
        u64::try_from(value).ok()?.checked_mul(cluster_size)
    } else {
        let shift = u32::from(value.unsigned_abs());
        (shift < 32).then(|| 1u64 << shift)
    }
}

pub(crate) fn parse_boot(sector: &[u8]) -> Option<Geometry> {
    if sector.get(3..11) != Some(b"NTFS    ".as_slice()) || le16(sector, 510) != Some(0xAA55) {
        return None;
    }
    let bytes_per_sector = u64::from(le16(sector, 0x0B)?);
    if !bytes_per_sector.is_power_of_two() || !(256..=4096).contains(&bytes_per_sector) {
        return None;
    }
    // 每簇扇区数：大于 0x80 时表示 2^(256-n)（64 KiB 以上的簇）
    let raw = *sector.get(0x0D)?;
    let sectors_per_cluster = if raw <= 0x80 {
        u64::from(raw)
    } else {
        1u64.checked_shl(256 - u32::from(raw))?
    };
    let cluster_size = bytes_per_sector.checked_mul(sectors_per_cluster)?;
    if cluster_size == 0 || !cluster_size.is_power_of_two() || cluster_size > 2 << 20 {
        return None;
    }
    let record_size = clusters_or_shift(*sector.get(0x40)?, cluster_size)?;
    if !record_size.is_power_of_two() || !(512..=65536).contains(&record_size) {
        return None;
    }
    let geometry = Geometry {
        bytes_per_sector,
        cluster_size,
        total_bytes: le64(sector, 0x28)?.checked_mul(bytes_per_sector)?,
        mft_lcn: le64(sector, 0x30)?,
        mirror_lcn: le64(sector, 0x38)?,
        record_size,
    };
    (geometry.total_bytes > 0).then_some(geometry)
}

/// 一个属性的内容（数据流、索引等）。
pub(crate) enum Value {
    /// 常驻：内容就在 MFT 记录里，`position` 是它在分区里的偏移。
    Resident {
        bytes: Vec<u8>,
        position: Option<u64>,
        compressed: bool,
        encrypted: bool,
    },
    NonResident {
        extents: Vec<Extent>,
        size: u64,
        initialized: u64,
        compressed: bool,
        encrypted: bool,
    },
}

impl Value {
    fn compressed(&self) -> bool {
        match self {
            Self::Resident { compressed, .. } | Self::NonResident { compressed, .. } => *compressed,
        }
    }

    fn encrypted(&self) -> bool {
        match self {
            Self::Resident { encrypted, .. } | Self::NonResident { encrypted, .. } => *encrypted,
        }
    }

    fn size(&self) -> u64 {
        match self {
            Self::Resident { bytes, .. } => bytes.len() as u64,
            Self::NonResident { size, .. } => *size,
        }
    }
}

/// 一条基本记录的数据流与存放方式。
pub(crate) struct Described {
    /// 无名 `$DATA`；目录或没有数据时为 `None`。
    pub data: Option<Value>,
    pub unsupported: Option<Unsupported>,
}

pub(crate) struct Ntfs {
    region: Region,
    cluster_size: u64,
    cluster_count: u64,
    record_size: u64,
    mft: Stream,
    record_count: u64,
    root_sequence: u16,
    label: Option<String>,
    scan: Mutex<Option<Arc<Scan>>>,
    /// 同一时间只扫一遍；第二个请求等第一个扫完，直接用它的结果。
    scan_lock: Mutex<()>,
}

impl Ntfs {
    /// 按引导扇区 `boot`（主的或备份的）打开。
    pub fn open(region: Region, boot: &[u8]) -> Result<Self, FsError> {
        let geometry = parse_boot(boot).ok_or(FsError::Invalid("NTFS 引导扇区的参数不自洽"))?;
        let cluster_size = geometry.cluster_size;
        let cluster_count = geometry.total_bytes.min(region.length()) / cluster_size;
        let record_size = geometry.record_size;

        // $MFT 自己的记录：主表读不出时用 $MFTMirr 里的副本
        let read_first = |lcn: u64| -> Option<Record> {
            let mut bytes = vec![0u8; record_size as usize];
            region
                .read_exact(lcn.checked_mul(cluster_size)?, &mut bytes)
                .ok()?;
            Record::parse(bytes, MFT_RECORD).filter(|record| record.in_use() && record.is_base())
        };
        let base = read_first(geometry.mft_lcn)
            .or_else(|| read_first(geometry.mirror_lcn))
            .ok_or(FsError::Invalid("$MFT 的记录读不出"))?;
        // 先用基本记录里的第一段运行表读出扩展记录，再拼出完整的运行表（$MFT 碎片很多时才有扩展记录）
        let first = base
            .attributes()
            .find_map(|attribute| match attribute.body {
                Body::NonResident(ref body) if attribute.kind == DATA && body.lowest_vcn == 0 => {
                    Some((decode_runs(body.runs, 0)?, body.size))
                }
                _ => None,
            })
            .ok_or(FsError::Invalid("$MFT 没有数据流"))?;
        let partial = Stream::new(region.clone(), cluster_size, first.0, first.1);
        let read_partial = |number: u64| -> Result<Option<Record>, FsError> {
            let mut bytes = vec![0u8; record_size as usize];
            match partial.read_at(number.saturating_mul(record_size), &mut bytes) {
                Ok(()) => Ok(Record::parse(bytes, number)),
                Err(_) => Ok(None),
            }
        };
        let Some(Value::NonResident { extents, size, .. }) = collect(
            &region,
            cluster_size,
            MFT_RECORD,
            &base,
            DATA,
            &[],
            &read_partial,
        )?
        else {
            return Err(FsError::Invalid("$MFT 的数据流不是非常驻的"));
        };
        let mft = Stream::new(region.clone(), cluster_size, extents, size);
        let mut ntfs = Self {
            region,
            cluster_size,
            cluster_count,
            record_size,
            record_count: size / record_size,
            mft,
            root_sequence: 0,
            label: None,
            scan: Mutex::new(None),
            scan_lock: Mutex::new(()),
        };
        let root = ntfs
            .record(ROOT_RECORD)?
            .filter(|root| root.in_use() && root.is_dir())
            .ok_or(FsError::Invalid("NTFS 根目录的记录读不出"))?;
        ntfs.root_sequence = root.sequence;
        if let Ok(Some(volume)) = ntfs.record(VOLUME_RECORD)
            && let Some(name) = volume
                .find(VOLUME_NAME, &[])
                .and_then(|attribute| attribute.resident_value())
        {
            let units: Vec<u16> = name
                .as_chunks::<2>()
                .0
                .iter()
                .map(|unit| u16::from_le_bytes(*unit))
                .collect();
            let label = String::from_utf16_lossy(&units);
            if !label.is_empty() {
                ntfs.label = Some(label);
            }
        }
        Ok(ntfs)
    }

    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    pub fn cluster_size(&self) -> u64 {
        self.cluster_size
    }

    pub fn root(&self) -> FileRef {
        FileRef {
            size: 0,
            is_dir: true,
            deleted: false,
            location: Location::Mft {
                record: ROOT_RECORD,
                sequence: self.root_sequence,
            },
        }
    }

    pub fn needs_scan(&self) -> bool {
        self.scanned().is_none()
    }

    fn scanned(&self) -> Option<Arc<Scan>> {
        self.scan
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// 扫一遍 MFT，找出已删除的项。`progress(已处理字节, 总字节)` 返回 `false` 时停下，结果为 `None`。
    pub fn scan(
        &self,
        progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<Option<ScanSummary>, FsError> {
        let _guard = self
            .scan_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(done) = self.scanned() {
            return Ok(Some(done.summary));
        }
        let Some(result) = Scan::run(self, progress)? else {
            return Ok(None);
        };
        let summary = result.summary;
        *self.scan.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(result));
        Ok(Some(summary))
    }

    /// 读第 `number` 条 MFT 记录；超出 MFT、签名或更新序列不对时为 `None`。
    pub(crate) fn record(&self, number: u64) -> Result<Option<Record>, FsError> {
        if number >= self.record_count {
            return Ok(None);
        }
        let mut bytes = vec![0u8; self.record_size as usize];
        self.mft
            .read_cached(number * self.record_size, &mut bytes)?;
        Ok(Record::parse(bytes, number))
    }

    pub(crate) fn stream(&self, extents: Vec<Extent>, size: u64) -> Stream {
        Stream::new(self.region.clone(), self.cluster_size, extents, size)
    }

    /// 记录 `number` 的属性 `kind`（名字 `name`）的内容，跟着属性列表找扩展记录。
    pub(crate) fn value(
        &self,
        number: u64,
        record: &Record,
        kind: u32,
        name: &[u16],
    ) -> Result<Option<Value>, FsError> {
        let mut value = collect(
            &self.region,
            self.cluster_size,
            number,
            record,
            kind,
            name,
            &|other| self.record(other),
        )?;
        // 常驻内容在分区里的位置：记录在 MFT 里的位置加上内容在记录里的偏移
        if let Some(Value::Resident { position, .. }) = &mut value
            && let Some(inside) = *position
        {
            *position = number
                .checked_mul(self.record_size)
                .and_then(|offset| self.mft.disk_offset(offset))
                .and_then(|offset| offset.checked_add(inside));
        }
        Ok(value)
    }

    /// 基本记录的数据流与存放方式。属性列表对不上时返回错误。
    pub(crate) fn describe(&self, number: u64, record: &Record) -> Result<Described, FsError> {
        let data = if record.is_dir() {
            None
        } else {
            self.value(number, record, DATA, &[])?
        };
        let attributes = record
            .find(STANDARD_INFORMATION, &[])
            .and_then(|attribute| attribute.resident_value())
            .and_then(|value| le32(value, 0x20))
            .unwrap_or(0);
        let reparse_tag = record
            .find(REPARSE_POINT, &[])
            .and_then(|attribute| attribute.resident_value())
            .and_then(|value| le32(value, 0));
        let unsupported = unsupported(
            data.as_ref().is_some_and(Value::compressed),
            data.as_ref().is_some_and(Value::encrypted) || attributes & FILE_ENCRYPTED != 0,
            reparse_tag,
            data.as_ref().map_or(0, Value::size),
            record.is_dir(),
        );
        Ok(Described { data, unsupported })
    }

    pub fn list(&self, dir: &FileRef) -> Result<Listing, FsError> {
        if !dir.is_dir {
            return Err(FsError::BadRef);
        }
        let scan = self.scanned();
        let (record, sequence) = match dir.location {
            Location::Orphans => {
                let entries = scan
                    .as_ref()
                    .map(|scan| scan.orphan_entries())
                    .unwrap_or_default();
                return Ok(Listing {
                    entries,
                    truncated: false,
                    overwritten: false,
                });
            }
            Location::Mft { record, sequence } => (record, sequence),
            Location::Clusters(_) => return Err(FsError::BadRef),
        };
        let mut entries = Vec::new();
        let mut truncated = false;
        if !dir.deleted {
            let base = self
                .record(record)?
                .ok_or(FsError::Invalid("目录的 MFT 记录读不出"))?;
            if !base.in_use() || !base.is_dir() || base.sequence != sequence {
                return Err(FsError::Invalid("目录已不在原处：它的 MFT 记录已被重用"));
            }
            let (keys, partial) = index::read(self, record, &base)?;
            truncated = partial;
            entries = live_entries(&keys, record, scan.as_deref());
            if record == ROOT_RECORD
                && let Some(scan) = &scan
                && scan.summary.orphans > 0
            {
                entries.push(Entry {
                    name: "$Orphans".to_owned(),
                    short_name: None,
                    name_uncertain: false,
                    size: 0,
                    modified: None,
                    created: None,
                    state: State::Live,
                    position: None,
                    unsupported: None,
                    file: FileRef::orphans(),
                });
            }
        }
        if let Some(scan) = &scan {
            for entry in scan.child_entries(record, sequence) {
                if entries.len() >= MAX_ENTRIES {
                    truncated = true;
                    break;
                }
                entries.push(entry);
            }
        }
        Ok(Listing {
            entries,
            truncated,
            overwritten: false,
        })
    }

    pub fn open_file(&self, file: &FileRef) -> Result<FileReader, FsError> {
        let Location::Mft { record, sequence } = file.location else {
            return Err(FsError::BadRef);
        };
        if file.is_dir {
            return Err(FsError::BadRef);
        }
        let base = self
            .record(record)?
            .ok_or(FsError::Invalid("文件的 MFT 记录读不出"))?;
        if !base.is_base() || base.sequence != sequence || base.in_use() == file.deleted {
            return Err(FsError::Invalid("MFT 记录已被重用，内容已不是这个文件"));
        }
        let described = self.describe(record, &base)?;
        if let Some(reason) = described.unsupported {
            return Err(FsError::Unreadable(reason));
        }
        if truncated_on_delete(&base, described.data.as_ref()) {
            return Err(FsError::Invalid(
                "删除时 NTFS 先把这个文件截成了 0 字节，它的数据位置已不在 MFT 里",
            ));
        }
        Ok(match described.data {
            None => FileReader::from_pieces(self.region.clone(), Vec::new(), 0, 0),
            Some(Value::Resident { bytes, .. }) => {
                let size = bytes.len() as u64;
                FileReader::from_pieces(self.region.clone(), vec![Piece::Bytes(bytes)], size, size)
            }
            Some(Value::NonResident {
                extents,
                size,
                initialized,
                ..
            }) => {
                let pieces = self.stream(extents, size).pieces(size);
                FileReader::from_pieces(self.region.clone(), pieces, size, initialized)
            }
        })
    }
}

/// 已删除的文件有属性列表、数据流却是空的：Windows 删除这类（碎片多、运行表放不进一条记录的）文件时
/// 先把它截成 0 字节，运行表与扩展记录都清掉了，数据在哪里、有多长都已不在 MFT 里（记录里 `$FILE_NAME`
/// 的长度也常年不更新）。实测见 make-test-images.ps1 里的 listed-b.bin。
pub(crate) fn truncated_on_delete(record: &Record, data: Option<&Value>) -> bool {
    !record.in_use()
        && !record.is_dir()
        && record.find(ATTRIBUTE_LIST, &[]).is_some()
        && data.is_none_or(|value| value.size() == 0)
}

/// 存放方式是否暂不支持读出。目录只看链接类的重分析点（目录本身带压缩、加密属性只表示新文件的默认方式）。
fn unsupported(
    compressed: bool,
    encrypted: bool,
    reparse_tag: Option<u32>,
    size: u64,
    is_dir: bool,
) -> Option<Unsupported> {
    match reparse_tag {
        Some(TAG_MOUNT_POINT | TAG_SYMLINK) => return Some(Unsupported::Reparse),
        Some(TAG_WOF) if !is_dir => return Some(Unsupported::SystemCompressed),
        // 其他重分析点（云端文件等）：已下载到本地的，数据流里就是内容
        Some(_) if !is_dir && size == 0 => return Some(Unsupported::Reparse),
        _ => {}
    }
    if is_dir {
        None
    } else if encrypted {
        Some(Unsupported::Encrypted)
    } else if compressed {
        Some(Unsupported::Compressed)
    } else {
        None
    }
}

/// 从目录索引的项生成列表项。同一条记录有单独的 DOS 名时作为短文件名；
/// 扫描过 MFT 时，长度、数据位置与存放方式以记录为准；记录读得出、却已被删除或重用的旧索引项不列出。
fn live_entries(keys: &[index::Key], dir: u64, scan: Option<&Scan>) -> Vec<Entry> {
    let short_name = |record: u64| {
        keys.iter()
            .find(|key| key.record == record && key.name.namespace == NAMESPACE_DOS)
            .map(|key| key.name.name.clone())
    };
    let mut entries = Vec::new();
    for key in keys {
        if key.name.namespace == NAMESPACE_DOS
            || key.record == dir
            || key.record < FIRST_USER_RECORD
        {
            continue;
        }
        let name = &key.name;
        let is_dir = name.flags & FILE_NAME_DIRECTORY != 0;
        let mut size = if is_dir { 0 } else { name.size };
        let reparse = (name.flags & FILE_REPARSE_POINT != 0).then_some(name.reparse_tag);
        let mut unsupported = unsupported(
            name.flags & FILE_COMPRESSED != 0,
            name.flags & FILE_ENCRYPTED != 0,
            reparse,
            size,
            is_dir,
        );
        let mut position = None;
        if let Some(scan) = scan {
            if scan.is_stale(key.record, key.sequence) {
                continue;
            }
            // 记录读不出（坏扇区）时没有摘要，照索引里的列出
            if let Some(summary) = scan.live(key.record, key.sequence) {
                if !is_dir {
                    size = summary.size;
                }
                position = summary.position();
                unsupported = summary.unsupported();
            }
        }
        entries.push(Entry {
            name: name.name.clone(),
            short_name: short_name(key.record).filter(|short| *short != name.name),
            name_uncertain: false,
            size,
            modified: nt_time(name.modified),
            created: nt_time(name.created),
            state: State::Live,
            position,
            unsupported,
            file: FileRef {
                size,
                is_dir,
                deleted: false,
                location: Location::Mft {
                    record: key.record,
                    sequence: key.sequence,
                },
            },
        });
        if entries.len() >= MAX_ENTRIES {
            break;
        }
    }
    entries
}

/// 收集记录 `number` 里类型为 `kind`、名字为 `name` 的属性；有属性列表时跟到扩展记录里，
/// 把非常驻属性的各段运行表按虚拟簇号拼起来。`read` 读其他 MFT 记录。
fn collect(
    region: &Region,
    cluster_size: u64,
    number: u64,
    record: &Record,
    kind: u32,
    name: &[u16],
    read: &dyn Fn(u64) -> Result<Option<Record>, FsError>,
) -> Result<Option<Value>, FsError> {
    let broken = FsError::Invalid("属性列表指向的扩展记录对不上");
    let Some(list) = record.find(ATTRIBUTE_LIST, &[]) else {
        return record
            .attributes()
            .find(|attribute| attribute.kind == kind && attribute.name_is(name))
            .map(|attribute| owned_segment(&attribute))
            .transpose()?
            .map(|segment| segment.into_value(cluster_size))
            .transpose();
    };
    let list_bytes = match &list.body {
        Body::Resident { value, .. } => value.to_vec(),
        Body::NonResident(body) => {
            let extents =
                decode_runs(body.runs, 0).ok_or(FsError::Invalid("属性列表的运行表不对"))?;
            let size = body.size.min(MAX_ATTRIBUTE_LIST);
            let mut bytes = vec![0u8; size as usize];
            Stream::new(region.clone(), cluster_size, extents, size).read_at(0, &mut bytes)?;
            bytes
        }
    };
    let mut segments = Vec::new();
    let mut extension_reads = 0usize;
    let mut offset = 0usize;
    while let (Some(entry_kind), Some(length)) =
        (le32(&list_bytes, offset), le16(&list_bytes, offset + 4))
    {
        let length = usize::from(length);
        if length < 0x1A {
            break;
        }
        let entry = list_bytes
            .get(offset..offset + length)
            .ok_or(FsError::Invalid("属性列表的项越界"))?;
        offset += length;
        let name_length = usize::from(*entry.get(6).unwrap_or(&0));
        let name_offset = usize::from(*entry.get(7).unwrap_or(&0));
        let entry_name: Vec<u16> = entry
            .get(name_offset..name_offset + name_length * 2)
            .unwrap_or_default()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|unit| u16::from_le_bytes(*unit))
            .collect();
        if entry_kind != kind || entry_name != name {
            continue;
        }
        let (Some(reference), Some(id)) = (le64(entry, 0x10), le16(entry, 0x18)) else {
            return Err(broken);
        };
        let (holder, holder_sequence) = split_ref(reference);
        let find = |owner: &Record| {
            owner
                .attributes()
                .find(|attribute| {
                    attribute.kind == kind && attribute.id == id && attribute.name_is(name)
                })
                .map(|attribute| owned_segment(&attribute))
        };
        let segment = if holder == number {
            find(record)
        } else {
            extension_reads += 1;
            if extension_reads > MAX_EXTENSIONS {
                return Err(broken);
            }
            let extension =
                read(holder)?.ok_or(FsError::Invalid("属性列表指向的扩展记录读不出"))?;
            // 扩展记录要属于这条记录；删除时扩展记录一同释放，序号可能已加 1
            let belongs = split_ref(extension.base).0 == number
                && if record.in_use() {
                    extension.in_use() && extension.sequence == holder_sequence
                } else {
                    !extension.in_use()
                        && (extension.sequence == holder_sequence
                            || extension.sequence == holder_sequence.wrapping_add(1))
                };
            if !belongs {
                return Err(broken);
            }
            find(&extension)
        };
        segments.push(segment.ok_or(FsError::Invalid("扩展记录里没有属性列表说的属性"))??);
    }
    if segments.is_empty() {
        return Ok(None);
    }
    if segments.len() == 1 {
        return segments
            .pop()
            .map(|segment| segment.into_value(cluster_size))
            .transpose();
    }
    // 多段：必须都是非常驻的，按起始虚拟簇号接起来
    segments.sort_by_key(|segment| segment.lowest_vcn);
    let mut joined: Option<Segment> = None;
    for segment in segments {
        joined = Some(match joined {
            None if segment.lowest_vcn == 0 => segment,
            Some(mut head) if head.resident.is_none() && segment.resident.is_none() => {
                let next = head
                    .extents
                    .last()
                    .map_or(0, |extent| extent.vcn.saturating_add(extent.length));
                if segment.lowest_vcn != next {
                    return Err(FsError::Invalid("属性的各段运行表接不上"));
                }
                head.extents.extend(segment.extents);
                head
            }
            _ => return Err(FsError::Invalid("属性的各段运行表接不上")),
        });
    }
    joined
        .map(|segment| segment.into_value(cluster_size))
        .transpose()
}

/// 属性的一段，已经从记录里复制出来。
struct Segment {
    lowest_vcn: u64,
    /// 常驻的内容与它在记录里的偏移。
    resident: Option<(Vec<u8>, usize)>,
    extents: Vec<Extent>,
    size: u64,
    initialized: u64,
    compressed: bool,
    encrypted: bool,
}

fn owned_segment(attribute: &record::Attribute<'_>) -> Result<Segment, FsError> {
    Ok(match &attribute.body {
        Body::Resident { value, offset } => Segment {
            lowest_vcn: 0,
            resident: Some((value.to_vec(), *offset)),
            extents: Vec::new(),
            size: value.len() as u64,
            initialized: value.len() as u64,
            compressed: attribute.compressed(),
            encrypted: attribute.encrypted(),
        },
        Body::NonResident(body) => {
            let extents = decode_runs(body.runs, body.lowest_vcn)
                .ok_or(FsError::Invalid("运行表的格式不对"))?;
            // 这一段的运行表要正好覆盖它声明的虚拟簇范围（最后一段可以是 0 簇的空文件）
            let end = extents.last().map_or(body.lowest_vcn, |extent| {
                extent.vcn.saturating_add(extent.length)
            });
            if !extents.is_empty() && end != body.highest_vcn.saturating_add(1) {
                return Err(FsError::Invalid("运行表与属性声明的范围不符"));
            }
            Segment {
                lowest_vcn: body.lowest_vcn,
                resident: None,
                extents,
                size: body.size,
                initialized: body.initialized.min(body.size),
                compressed: attribute.compressed(),
                encrypted: attribute.encrypted(),
            }
        }
    })
}

impl Segment {
    fn into_value(self, cluster_size: u64) -> Result<Value, FsError> {
        if let Some((bytes, offset)) = self.resident {
            return Ok(Value::Resident {
                bytes,
                position: Some(offset as u64),
                compressed: self.compressed,
                encrypted: self.encrypted,
            });
        }
        if self.lowest_vcn != 0 {
            return Err(FsError::Invalid("属性缺少起始的一段"));
        }
        // 运行表覆盖不到声明的长度时，读到那里会报簇链中断；这里只拦住明显荒唐的长度
        let covered = self
            .extents
            .last()
            .map_or(0, |extent| extent.vcn.saturating_add(extent.length))
            .saturating_mul(cluster_size);
        if self.size > covered.saturating_add(cluster_size.saturating_mul(16)) && !self.compressed {
            return Err(FsError::Invalid("属性的长度超出运行表"));
        }
        Ok(Value::NonResident {
            extents: self.extents,
            size: self.size,
            initialized: self.initialized,
            compressed: self.compressed,
            encrypted: self.encrypted,
        })
    }
}

/// 记录里全部的 `$FILE_NAME`：DOS 名单独给出，供作短文件名。
pub(crate) fn file_names(record: &Record) -> Vec<record::FileName> {
    record
        .attributes()
        .filter(|attribute| attribute.kind == FILE_NAME)
        .filter_map(|attribute| attribute.resident_value().and_then(parse_file_name))
        .collect()
}
