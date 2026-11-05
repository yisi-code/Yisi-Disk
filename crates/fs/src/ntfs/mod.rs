//! NTFS：引导扇区、MFT 记录、常驻与非常驻属性（含属性列表）、`$I30` 目录索引；
//! 扫一遍 MFT 找出「未使用」的记录，按父目录引用挂回目录树，用 `$Bitmap` 判断它们的簇是否已被占用。
//! 依据公开的 NTFS 结构资料实现，只读。

mod audit;
mod huffman;
mod index;
pub(crate) mod lznt1;
mod lzx;
mod record;
mod scan;
mod stream;
pub(crate) mod wof;
mod xpress;

use std::sync::{Arc, Mutex, PoisonError};

use yisi_part::bytes::{le16, le16_units, le32, le64};

pub use self::audit::{
    AuditStage, Listed, NtfsAudit, SharedClusters, UnmarkedClusters, audit_ntfs,
};
use self::record::{
    ATTRIBUTE_LIST, Body, DATA, Extent, FILE_NAME, INDEX_ROOT, NAMESPACE_DOS, REPARSE_POINT,
    Record, STANDARD_INFORMATION, VOLUME_NAME, compression_unit_clusters, decode_runs,
    parse_file_name, split_ref, utf16,
};
use self::scan::Scan;
use self::stream::Stream;
use self::wof::{WOF_STREAM, WofAlgorithm, WofReader, wof_algorithm};
use crate::bpb::NtfsBpb;
use crate::entry::{Entry, FileAttributes, FileRef, Listing, Location, Stamp, State, Unsupported};
use crate::reader::{FileReader, Piece};
use crate::region::Region;
use crate::{FsError, MAX_ENTRIES, ScanSummary, bpb};

/// 系统文件的记录号。
const MFT_RECORD: u64 = 0;
const VOLUME_RECORD: u64 = 3;
const ROOT_RECORD: u64 = 5;
const BITMAP_RECORD: u64 = 6;
const BAD_CLUSTERS_RECORD: u64 = 8;
/// 0 到 15 号记录留给系统文件，不列出。
const FIRST_USER_RECORD: u64 = 16;

/// 文件属性（`$STANDARD_INFORMATION` 与 `$FILE_NAME` 里的）。
const FILE_REPARSE_POINT: u32 = 0x0400;
const FILE_ENCRYPTED: u32 = 0x4000;
/// `$FILE_NAME` 里表示目录的位。
const FILE_NAME_DIRECTORY: u32 = 0x1000_0000;
/// 重分析标记：Windows 的系统压缩（WOF）、目录联接、符号链接。
const TAG_WOF: u32 = 0x8000_0017;
const TAG_MOUNT_POINT: u32 = 0xA000_0003;
const TAG_SYMLINK: u32 = 0xA000_000C;

/// 一个文件最多列这么多个备用数据流。
const MAX_STREAMS: usize = 64;

/// NTFS 的备用数据流（有名字的 `$DATA`，例如浏览器下载时加的 `Zone.Identifier`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedStream {
    pub name: String,
    pub size: u64,
}

/// 属性列表最多读这么多字节、跟这么多条扩展记录。
const MAX_ATTRIBUTE_LIST: u64 = 1 << 20;
const MAX_EXTENSIONS: usize = 4096;

/// 解析引导扇区；有致命问题时返回 `None`（规则在 [`bpb`](crate::bpb)）。
pub(crate) fn parse_boot(sector: &[u8]) -> Option<NtfsBpb> {
    bpb::check_ntfs(sector).geometry
}

/// 一个属性的内容（数据流、索引等）。
pub(crate) enum Value {
    /// 常驻：内容就在 MFT 记录里，`position` 是它在分区里的偏移。常驻的内容不压缩（属性上的压缩标记
    /// 只表示文件是压缩属性），原样能读。
    Resident {
        bytes: Vec<u8>,
        position: Option<u64>,
        encrypted: bool,
    },
    NonResident {
        extents: Vec<Extent>,
        size: u64,
        initialized: u64,
        compressed: bool,
        encrypted: bool,
        /// 压缩单元（见 [`record::NonResident::compression_unit`]）。
        compression_unit: u8,
    },
}

impl Value {
    fn encrypted(&self) -> bool {
        match self {
            Self::Resident { encrypted, .. } | Self::NonResident { encrypted, .. } => *encrypted,
        }
    }

    /// 压缩了、又解不开：与 [`record::Attribute::undecodable_compression`] 同一条规则。
    fn undecodable_compression(&self, cluster_size: u64) -> bool {
        match self {
            Self::Resident { .. } => false,
            Self::NonResident {
                compressed,
                compression_unit,
                ..
            } => {
                *compressed && compression_unit_clusters(*compression_unit, cluster_size).is_none()
            }
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
    /// 系统压缩（WOF）且算法认识时的算法：内容要从 `WofCompressedData` 流里解开。
    pub wof: Option<WofAlgorithm>,
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

/// `$MFT` 的第一条记录（`$MFT` 自己）可用：签名、更新序列、记录头都对，在用，且是基本记录。
/// 打开卷、文件系统检查与丢失分区搜索都按这一条判断 `$MFT`（或 `$MFTMirr`）在不在。
pub fn mft_self_record_ok(bytes: &[u8]) -> bool {
    Record::parse(bytes.to_vec(), MFT_RECORD).is_some_and(|record| is_mft_self(&record))
}

/// 第 `number` 条 MFT 记录里，没名字的数据流第一段从哪个簇开始，以及记录头记的记录大小（字节）。
/// 重建引导扇区时，由 `$MFT`（第 0 条）、`$MFTMirr`（第 1 条）自己的记录推出它们在卷上的位置。
pub fn mft_record_layout(bytes: &[u8], number: u64) -> Option<(u64, u64)> {
    let record = Record::parse(bytes.to_vec(), number)?;
    let record_size = u64::from(le32(bytes, 0x1C)?);
    let lcn = record
        .attributes()
        .find_map(|attribute| match attribute.body {
            Body::NonResident(ref body) if attribute.kind == DATA && attribute.name_is(&[]) => {
                decode_runs(body.runs, body.lowest_vcn)?.first()?.lcn
            }
            _ => None,
        })?;
    Some((lcn, record_size))
}

/// 根目录（第 5 条记录）的索引根里记的索引块大小（字节）。
pub fn root_index_block_size(bytes: &[u8]) -> Option<u64> {
    let record = Record::parse(bytes.to_vec(), ROOT_RECORD)?;
    let value = record.find(INDEX_ROOT, &utf16("$I30"))?.resident_value()?;
    Some(u64::from(le32(value, 0x08)?))
}

/// `$BadClus`（第 8 条记录）里 `$Bad` 流的长度：这个稀疏流盖住整个卷，长度是卷的簇数乘簇大小。
/// 主备引导扇区都没了时，丢失分区搜索按它定卷有多长。
pub fn mft_bad_clusters_length(bytes: &[u8]) -> Option<u64> {
    let record = Record::parse(bytes.to_vec(), BAD_CLUSTERS_RECORD)?;
    if !record.in_use() {
        return None;
    }
    match record.find(DATA, &utf16("$Bad"))?.body {
        Body::NonResident(ref body) if body.lowest_vcn == 0 => Some(body.size),
        _ => None,
    }
}

/// 第 `number` 条 MFT 记录本身完整：签名 `FILE`、更新序列对得上、记录头合理、记的记录号（有的话）就是它。
/// 修复 `$MFT` 与 `$MFTMirr` 时逐条判断哪一份可信。
pub fn mft_record_ok(bytes: &[u8], number: u64) -> bool {
    Record::parse(bytes.to_vec(), number).is_some()
}

fn is_mft_self(record: &Record) -> bool {
    record.in_use() && record.is_base()
}

impl Ntfs {
    /// 按引导扇区 `boot`（主的或备份的）打开。
    pub fn open(region: Region, boot: &[u8]) -> Result<Self, FsError> {
        let geometry = parse_boot(boot).ok_or(FsError::Invalid(
            "新技术文件系统（NTFS）引导扇区的参数不自洽",
        ))?;
        let cluster_size = geometry.cluster_size;
        let cluster_count = geometry.total_bytes().min(region.length()) / cluster_size;
        let record_size = geometry.record_size;

        // $MFT 自己的记录：主表读不出时用 $MFTMirr 里的副本
        let read_first = |lcn: u64| -> Option<Record> {
            let mut bytes = vec![0u8; record_size as usize];
            region
                .read_exact(lcn.checked_mul(cluster_size)?, &mut bytes)
                .ok()?;
            Record::parse(bytes, MFT_RECORD).filter(is_mft_self)
        };
        let base = read_first(geometry.mft_lcn)
            .or_else(|| read_first(geometry.mirror_lcn))
            .ok_or(FsError::Invalid("主文件表（$MFT）的记录读不出"))?;
        // 先用基本记录里的第一段运行表读出扩展记录，再拼出完整的运行表（$MFT 碎片很多时才有扩展记录）
        let first = base
            .attributes()
            .find_map(|attribute| match attribute.body {
                Body::NonResident(ref body) if attribute.kind == DATA && body.lowest_vcn == 0 => {
                    Some((decode_runs(body.runs, 0)?, body.size))
                }
                _ => None,
            })
            .ok_or(FsError::Invalid("主文件表（$MFT）没有数据流"))?;
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
            return Err(FsError::Invalid("主文件表（$MFT）的数据流不是非常驻的"));
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
            .ok_or(FsError::Invalid("新技术文件系统（NTFS）根目录的记录读不出"))?;
        ntfs.root_sequence = root.sequence;
        if let Ok(Some(volume)) = ntfs.record(VOLUME_RECORD)
            && let Some(name) = volume
                .find(VOLUME_NAME, &[])
                .and_then(|attribute| attribute.resident_value())
        {
            let label = String::from_utf16_lossy(&le16_units(name));
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
            modified: None,
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
        let reparse = reparse_point(record);
        let wof = reparse.and_then(wof_algorithm).filter(|_| !record.is_dir());
        let unsupported = unsupported(
            data.as_ref()
                .is_some_and(|value| value.undecodable_compression(self.cluster_size)),
            data.as_ref().is_some_and(Value::encrypted) || attributes & FILE_ENCRYPTED != 0,
            reparse.and_then(|value| le32(value, 0)),
            wof.is_some(),
            data.as_ref().map_or(0, Value::size),
            record.is_dir(),
        );
        Ok(Described {
            data,
            unsupported,
            wof,
        })
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
                .ok_or(FsError::Invalid("目录的主文件表（MFT）记录读不出"))?;
            if !base.in_use() || !base.is_dir() || base.sequence != sequence {
                return Err(FsError::Invalid(
                    "目录已不在原处：它的主文件表（MFT）记录已被重用",
                ));
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
                    attributes: FileAttributes::default(),
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

    /// 文件的基本记录：定位信息要对得上（记录号、序号、是否删除），否则记录已被重用。
    fn base_record(&self, file: &FileRef) -> Result<(u64, Record), FsError> {
        let Location::Mft { record, sequence } = file.location else {
            return Err(FsError::BadRef);
        };
        if file.is_dir {
            return Err(FsError::BadRef);
        }
        let base = self
            .record(record)?
            .ok_or(FsError::Invalid("文件的主文件表（MFT）记录读不出"))?;
        if !base.is_base() || base.sequence != sequence || base.in_use() == file.deleted {
            return Err(FsError::Invalid(
                "主文件表（MFT）记录已被重用，内容已不是这个文件",
            ));
        }
        Ok((record, base))
    }

    pub fn open_file(&self, file: &FileRef) -> Result<FileReader, FsError> {
        let (record, base) = self.base_record(file)?;
        let described = self.describe(record, &base)?;
        if let Some(reason) = described.unsupported {
            return Err(FsError::Unreadable(reason));
        }
        if truncated_on_delete(&base, described.data.as_ref()) {
            return Err(FsError::Invalid(
                "删除时新技术文件系统（NTFS）先把这个文件截成了 0 字节，它的数据位置已不在主文件表（MFT）里",
            ));
        }
        let Some(algorithm) = described.wof else {
            return Ok(self.reader(described.data));
        };
        // 系统压缩：无名数据流只是占着原大小的稀疏流，内容在压缩流里
        let size = described.data.as_ref().map_or(0, Value::size);
        let compressed =
            self.value(record, &base, DATA, &utf16(WOF_STREAM))?
                .ok_or(FsError::Invalid(
                    "系统压缩（WOF）的文件没有压缩流（WofCompressedData）",
                ))?;
        if compressed.encrypted() || compressed.undecodable_compression(self.cluster_size) {
            return Err(FsError::Unreadable(Unsupported::SystemCompressed));
        }
        let pieces = self
            .reader(Some(compressed))
            .into_pieces()
            .ok_or(FsError::Invalid("系统压缩（WOF）的压缩流读不出"))?;
        Ok(FileReader::wof(WofReader::new(pieces, algorithm, size)?))
    }

    /// 文件的备用数据流：名字与长度，按在记录（或属性列表）里的顺序，最多 [`MAX_STREAMS`] 个。
    pub fn streams(&self, file: &FileRef) -> Result<Vec<NamedStream>, FsError> {
        let (record, base) = self.base_record(file)?;
        let mut names: Vec<Vec<u16>> = Vec::new();
        let mut add = |name: Vec<u16>| {
            if !name.is_empty() && !names.contains(&name) && names.len() < MAX_STREAMS {
                names.push(name);
            }
        };
        match base.find(ATTRIBUTE_LIST, &[]) {
            Some(list) => {
                for entry in list_entries(&attribute_list_bytes(
                    &self.region,
                    self.cluster_size,
                    &list,
                )?)? {
                    if entry.kind == DATA {
                        add(entry.name);
                    }
                }
            }
            None => {
                for attribute in base.attributes() {
                    if attribute.kind == DATA {
                        add(attribute.name_units());
                    }
                }
            }
        }
        names
            .into_iter()
            .map(|name| {
                let size = self
                    .value(record, &base, DATA, &name)?
                    .map_or(0, |value| value.size());
                Ok(NamedStream {
                    name: String::from_utf16_lossy(&name),
                    size,
                })
            })
            .collect()
    }

    /// 读一个备用数据流。加密或解不开的压缩照样按不支持拒绝。
    pub fn open_stream(&self, file: &FileRef, name: &str) -> Result<FileReader, FsError> {
        let (record, base) = self.base_record(file)?;
        let value = self
            .value(record, &base, DATA, &utf16(name))?
            .ok_or(FsError::Invalid("没有这个数据流"))?;
        if value.encrypted() {
            return Err(FsError::Unreadable(Unsupported::Encrypted));
        }
        if value.undecodable_compression(self.cluster_size) {
            return Err(FsError::Unreadable(Unsupported::Compressed));
        }
        Ok(self.reader(Some(value)))
    }

    /// 把一个属性的内容换成读取器；压缩的按压缩单元读时解开（调用方已拒绝解不开的）。
    fn reader(&self, value: Option<Value>) -> FileReader {
        match value {
            None => FileReader::from_pieces(self.region.clone(), Vec::new(), 0, 0),
            Some(Value::Resident { bytes, .. }) => {
                let size = bytes.len() as u64;
                FileReader::from_pieces(self.region.clone(), vec![Piece::Bytes(bytes)], size, size)
            }
            Some(Value::NonResident {
                extents,
                size,
                initialized,
                compressed,
                compression_unit,
                ..
            }) => {
                let stream = self.stream(extents, size);
                let pieces = match compression_unit_clusters(compression_unit, self.cluster_size) {
                    Some(unit) if compressed => stream.compressed_pieces(size, unit),
                    _ => stream.pieces(size),
                };
                FileReader::from_pieces(self.region.clone(), pieces, size, initialized)
            }
        }
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

/// 记录里的重分析点内容（常驻；开头 4 字节是标记）。
pub(crate) fn reparse_point(record: &Record) -> Option<&[u8]> {
    record
        .find(REPARSE_POINT, &[])
        .and_then(|attribute| attribute.resident_value())
}

/// 存放方式是否暂不支持读出。目录只看链接类的重分析点（目录本身带压缩、加密属性只表示新文件的默认方式）。
/// `undecodable_compression`：压缩了、又解不开（LZNT1 压缩能读出）；`wof_readable`：系统压缩的算法认识
/// （「单个文件」提供者，XPRESS 或 LZX），能从压缩流里解开。
fn unsupported(
    undecodable_compression: bool,
    encrypted: bool,
    reparse_tag: Option<u32>,
    wof_readable: bool,
    size: u64,
    is_dir: bool,
) -> Option<Unsupported> {
    match reparse_tag {
        Some(TAG_MOUNT_POINT | TAG_SYMLINK) => return Some(Unsupported::Reparse),
        Some(TAG_WOF) if !is_dir && !wof_readable => {
            return Some(Unsupported::SystemCompressed);
        }
        Some(TAG_WOF) => {}
        // 其他重分析点（云端文件等）：已下载到本地的，数据流里就是内容
        Some(_) if !is_dir && size == 0 => return Some(Unsupported::Reparse),
        _ => {}
    }
    if is_dir {
        None
    } else if encrypted {
        Some(Unsupported::Encrypted)
    } else if undecodable_compression {
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
        // 目录索引里只有「压缩」这个属性与重分析标记，看不出解不解得开：按解得开列出，读的时候以记录为准
        let mut unsupported = unsupported(
            false,
            name.flags & FILE_ENCRYPTED != 0,
            reparse,
            true,
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
            modified: Stamp::Nt(name.modified).text(),
            created: Stamp::Nt(name.created).text(),
            state: State::Live,
            position,
            unsupported,
            attributes: FileAttributes::from_bits(name.flags),
            file: FileRef {
                size,
                is_dir,
                deleted: false,
                location: Location::Mft {
                    record: key.record,
                    sequence: key.sequence,
                },
                modified: (name.modified != 0).then_some(Stamp::Nt(name.modified)),
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
    let mut segments = Vec::new();
    let mut extension_reads = 0usize;
    for entry in list_entries(&attribute_list_bytes(region, cluster_size, &list)?)? {
        if entry.kind != kind || entry.name != name {
            continue;
        }
        let (Some(reference), Some(id)) = (entry.reference, entry.id) else {
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

/// 属性列表的内容（常驻在记录里，或按运行表读出，最多 [`MAX_ATTRIBUTE_LIST`] 字节）。
fn attribute_list_bytes(
    region: &Region,
    cluster_size: u64,
    list: &record::Attribute<'_>,
) -> Result<Vec<u8>, FsError> {
    Ok(match &list.body {
        Body::Resident { value, .. } => value.to_vec(),
        Body::NonResident(body) => {
            let extents =
                decode_runs(body.runs, 0).ok_or(FsError::Invalid("属性列表的运行表不对"))?;
            let size = body.size.min(MAX_ATTRIBUTE_LIST);
            let mut bytes = vec![0u8; size as usize];
            Stream::new(region.clone(), cluster_size, extents, size).read_at(0, &mut bytes)?;
            bytes
        }
    })
}

/// 属性列表的一项：属性类型与名字、它在哪条记录（引用）与属性号；后两个读不出时为 `None`。
struct ListEntry {
    kind: u32,
    name: Vec<u16>,
    reference: Option<u64>,
    id: Option<u16>,
}

/// 依次解析属性列表的项；项长不足 0x1A 时结束，项越界时报错。
fn list_entries(bytes: &[u8]) -> Result<Vec<ListEntry>, FsError> {
    let mut entries = Vec::new();
    let mut offset = 0usize;
    while let (Some(kind), Some(length)) = (le32(bytes, offset), le16(bytes, offset + 4)) {
        let length = usize::from(length);
        if length < 0x1A {
            break;
        }
        let entry = bytes
            .get(offset..offset + length)
            .ok_or(FsError::Invalid("属性列表的项越界"))?;
        offset += length;
        let name_length = usize::from(*entry.get(6).unwrap_or(&0));
        let name_offset = usize::from(*entry.get(7).unwrap_or(&0));
        let name = le16_units(
            entry
                .get(name_offset..name_offset + name_length * 2)
                .unwrap_or_default(),
        );
        entries.push(ListEntry {
            kind,
            name,
            reference: le64(entry, 0x10),
            id: le16(entry, 0x18),
        });
    }
    Ok(entries)
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
    compression_unit: u8,
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
            compression_unit: 0,
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
                compression_unit: body.compression_unit,
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
            compression_unit: self.compression_unit,
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
