//! E01（EWF，Expert Witness 格式）镜像，只读：取证常用的分段压缩镜像。
//!
//! 镜像分成多个段文件（`.E01`、`.E02` … `.E99`、`.EAA` … `.ZZZ`），放在同一个目录里。每段开头 13 字节的文件头
//! （签名与段号），之后是一串节：每节 76 字节的描述（类型、下一节在本段里的偏移、本节长度、Adler-32），后面跟数据。
//! 读取要用的节：
//!
//! - `volume` / `disk`：块数、每块扇区数、每扇区字节数、总扇区数（第一段里）；
//! - `sectors`：块的数据；
//! - `table`：块表，每项 4 字节，最高位为 1 表示这一块是 zlib 压缩的，其余 31 位是块在本段里的偏移（加上表头记的基准偏移）；
//!   一块的长度到下一项为止，表里最后一块到它所在的 `sectors` 节结尾为止；`table2` 是同一张表的副本，`table` 坏了时用它；
//! - `next`（不是最后一段）/ `done`（最后一段）：本段到此为止。
//!
//! 其余的节（`header`、`header2`、`data`、`hash`、`digest`、`error2` 等）只是元数据，不影响读出的内容，跳过。
//! 压缩的块用 zlib 解开（带它自己的 Adler-32），未压缩的块后面跟 4 字节 Adler-32；校验不对、解不开时这一块
//! 按读错误报告（[`IoError::Read`]，与物理盘读不出一样），不给零。

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::adler32::adler32;
use crate::bytes::{le16, le32, le64};
use crate::device::validate_sector_size;
use crate::read::read_file_at;
use crate::{BlockDevice, IoError};

/// E01 段文件开头的签名（SMART 的 `.s01` 相同）。
pub(crate) const EVF_SIGNATURE: [u8; 8] = *b"EVF\x09\x0d\x0a\xff\x00";
/// 同一家族、这里还不支持的：逻辑证据文件（`.L01`）与第二版格式（`.Ex01`、`.Lx01`）。
pub(crate) const UNSUPPORTED_SIGNATURES: [([u8; 8], &str); 3] = [
    (*b"LVF\x09\x0d\x0a\xff\x00", "逻辑证据文件（L01）"),
    (*b"EVF2\x0d\x0a\x81\x00", "第二版 EWF（Ex01）"),
    (*b"LEF2\x0d\x0a\x81\x00", "第二版逻辑证据文件（Lx01）"),
];

const FILE_HEADER_LEN: usize = 13;
const DESCRIPTOR_LEN: usize = 76;
const DESCRIPTOR_LEN_U64: u64 = DESCRIPTOR_LEN as u64;
const TABLE_HEADER_LEN: usize = 24;
/// `volume` 节数据的两种长度：EnCase 与 FTK 写的 1052 字节，早期 EWF / SMART 的 94 字节。
const VOLUME_LONG_LEN: usize = 1052;
const VOLUME_SHORT_LEN: usize = 94;
/// `volume` 节最多读这么多。
const MAX_VOLUME_BYTES: u64 = 64 * 1024;
/// 一块（每块扇区数 × 每扇区字节数）最大这么多：EnCase 最多 32768 个 512 字节的扇区。
const MAX_CHUNK_BYTES: u64 = 16 * 1024 * 1024;
/// 一个块表最多这么多项（EnCase 6 以后实际不超过 65534）。
const MAX_TABLE_ENTRIES: u32 = 1 << 20;
/// 一段最多这么多节，节链成环时停下。
const MAX_SECTIONS: usize = 1 << 20;
/// 已解压的块最多缓存这么多字节（至少一块）。
const CACHE_BYTES: u64 = 8 * 1024 * 1024;
const OFFSET_MASK: u32 = 0x7fff_ffff;
const COMPRESSED_FLAG: u32 = 0x8000_0000;

/// 打开的 E01 镜像。
pub struct EwfImage {
    files: Vec<File>,
    paths: Vec<PathBuf>,
    tables: Vec<Table>,
    size: u64,
    sector_size: u32,
    chunk_size: u64,
    cache: Mutex<ChunkCache>,
}

/// 一张块表：在第几段、从第几块起、各项。
struct Table {
    segment: usize,
    first_chunk: u64,
    base: u64,
    entries: Vec<u32>,
    /// 从这一项起偏移超过了 31 位（个别版本把 2 GiB 之后的偏移写满 32 位），按完整的 32 位偏移、未压缩读。
    overflow_from: usize,
    /// 表里最后一块的数据到这里为止。
    data_end: u64,
}

/// 一块存在哪里。
struct ChunkPlace {
    segment: usize,
    offset: u64,
    stored: u64,
    compressed: bool,
}

impl Table {
    fn start(&self, index: usize) -> Option<(u64, bool)> {
        let entry = *self.entries.get(index)?;
        if index >= self.overflow_from {
            Some((self.base.checked_add(u64::from(entry))?, false))
        } else {
            let offset = self.base.checked_add(u64::from(entry & OFFSET_MASK))?;
            Some((offset, entry & COMPRESSED_FLAG != 0))
        }
    }

    fn place(&self, index: usize) -> Option<ChunkPlace> {
        let (offset, compressed) = self.start(index)?;
        let end = if index + 1 < self.entries.len() {
            self.start(index + 1)?.0
        } else {
            self.data_end
        };
        let stored = end.checked_sub(offset).filter(|&stored| stored > 0)?;
        Some(ChunkPlace {
            segment: self.segment,
            offset,
            stored,
            compressed,
        })
    }

    fn end_chunk(&self) -> u64 {
        self.first_chunk + self.entries.len() as u64
    }
}

impl EwfImage {
    /// 打开 E01 镜像：`path` 是第一段，后续段在同一目录、按扩展名的规则找（`.E01` 之后是 `.E02`，大小写随第一段）。
    /// 缺段时答复 [`IoError::EwfSegmentMissing`]，结构不对时答复 [`IoError::Ewf`]。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, IoError> {
        let first = path.as_ref();
        let mut builder = Builder::default();
        let mut number: u32 = 1;
        let mut path = first.to_path_buf();
        loop {
            let file = open_segment(&path, number)?;
            let ewf_error = |reason: String| IoError::Ewf {
                path: path.clone(),
                reason,
            };
            let len = file
                .metadata()
                .map_err(|source| IoError::Open {
                    path: path.clone(),
                    source,
                })?
                .len();
            let segment = builder.files.len();
            let end = builder
                .walk_segment(&file, segment, number, len)
                .map_err(ewf_error)?;
            builder.files.push(file);
            builder.paths.push(path.clone());
            if end == SegmentEnd::Done {
                break;
            }
            number += 1;
            path = segment_path(first, number).ok_or_else(|| IoError::Ewf {
                path: first.to_path_buf(),
                reason: format!(
                    "第一段的扩展名不是 .E01 这样的样式，找不到第 {number} 段（段文件要保持原来的名字）"
                ),
            })?;
        }
        builder.finish(first)
    }

    /// 组成这个镜像的段文件，按段号排列。
    pub fn segments(&self) -> &[PathBuf] {
        &self.paths
    }

    fn place(&self, chunk: u64) -> Option<ChunkPlace> {
        let after = self
            .tables
            .partition_point(|table| table.first_chunk <= chunk);
        let table = self.tables.get(after.checked_sub(1)?)?;
        let index = usize::try_from(chunk - table.first_chunk).ok()?;
        table.place(index)
    }

    fn chunk(&self, index: u64) -> Result<Arc<Vec<u8>>, IoError> {
        if let Some(data) = self.lock_cache().get(index) {
            return Ok(data);
        }
        let data = Arc::new(self.load(index)?);
        self.lock_cache().put(index, Arc::clone(&data));
        Ok(data)
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, ChunkCache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 读出第 `index` 块并核对：未压缩的核对 Adler-32，压缩的由 zlib 核对；长度要够这一块。
    fn load(&self, index: u64) -> Result<Vec<u8>, IoError> {
        let start = index * self.chunk_size;
        let expected = self.chunk_size.min(self.size - start);
        let expected_len = expected as usize;
        let fail = |kind: io::ErrorKind, reason: String| IoError::Read {
            offset: start,
            len: expected_len,
            source: io::Error::new(kind, reason),
        };
        let invalid = |reason: String| fail(io::ErrorKind::InvalidData, reason);
        let place = self
            .place(index)
            .ok_or_else(|| invalid(format!("E01 第 {index} 块在块表里的位置不对")))?;
        let limit = if place.compressed {
            // 压不小的块照规范存成未压缩的；个别写法总是压缩，压完可能比原来稍大
            self.chunk_size + self.chunk_size / 2 + 1024
        } else {
            expected + 4
        };
        if place.compressed && place.stored > limit {
            return Err(invalid(format!(
                "E01 第 {index} 块存了 {} 字节，超过一块应有的大小",
                place.stored
            )));
        }
        if !place.compressed && place.stored < limit {
            return Err(invalid(format!(
                "E01 第 {index} 块只存了 {} 字节，不够一块（{expected} 字节）加校验和",
                place.stored
            )));
        }
        let file = self
            .files
            .get(place.segment)
            .ok_or_else(|| invalid(format!("E01 第 {index} 块所在的段不对")))?;
        let mut stored = vec![0u8; place.stored.min(limit) as usize];
        read_exact_file(file, place.offset, &mut stored).map_err(|error| {
            let reason = format!(
                "E01 第 {index} 块读不出（第 {} 段偏移 {}）：{error}",
                place.segment + 1,
                place.offset
            );
            fail(error.kind(), reason)
        })?;
        if place.compressed {
            let mut data = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(
                &stored,
                self.chunk_size as usize,
            )
            .map_err(|error| invalid(format!("E01 第 {index} 块解压失败：{:?}", error.status)))?;
            if data.len() < expected_len {
                return Err(invalid(format!(
                    "E01 第 {index} 块解压后只有 {} 字节，应为 {expected} 字节",
                    data.len()
                )));
            }
            data.truncate(expected_len);
            Ok(data)
        } else {
            let recorded = le32(&stored, expected_len);
            stored.truncate(expected_len);
            if recorded != Some(adler32(&stored)) {
                return Err(invalid(format!("E01 第 {index} 块的 Adler-32 校验和不对")));
            }
            Ok(stored)
        }
    }
}

impl BlockDevice for EwfImage {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        let Some(remaining) = self.size.checked_sub(offset).filter(|&left| left > 0) else {
            return Ok(0);
        };
        let len = usize::try_from(remaining).map_or(buf.len(), |left| left.min(buf.len()));
        let mut done = 0;
        while done < len {
            let position = offset + done as u64;
            let index = position / self.chunk_size;
            let within = (position % self.chunk_size) as usize;
            let data = self.chunk(index)?;
            let available = data.get(within..).unwrap_or_default();
            let take = available.len().min(len - done);
            if take == 0 {
                break;
            }
            buf[done..done + take].copy_from_slice(&available[..take]);
            done += take;
        }
        Ok(done)
    }
}

/// 第 `number` 段文件：存在、签名对、段号对。
fn open_segment(path: &Path, number: u32) -> Result<File, IoError> {
    let file = File::open(path).map_err(|source| {
        if number > 1 && source.kind() == io::ErrorKind::NotFound {
            IoError::EwfSegmentMissing {
                number,
                path: path.to_path_buf(),
            }
        } else {
            IoError::Open {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    let ewf_error = |reason: String| IoError::Ewf {
        path: path.to_path_buf(),
        reason,
    };
    let mut header = [0u8; FILE_HEADER_LEN];
    read_exact_file(&file, 0, &mut header)
        .map_err(|error| ewf_error(format!("读不出文件头：{error}")))?;
    let signature = header.get(..8).unwrap_or_default();
    if let Some((_, name)) = UNSUPPORTED_SIGNATURES
        .iter()
        .find(|(known, _)| known.as_slice() == signature)
    {
        return Err(ewf_error(format!("这是{name}，暂不支持")));
    }
    if signature != EVF_SIGNATURE {
        return Err(ewf_error(format!(
            "不是 E01 段文件（第 {number} 段开头没有 EVF 签名）"
        )));
    }
    let recorded = le16(&header, 9).unwrap_or_default();
    if u32::from(recorded) != number {
        return Err(ewf_error(format!(
            "文件头记的是第 {recorded} 段，这里应是第 {number} 段"
        )));
    }
    Ok(file)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentEnd {
    Next,
    Done,
}

/// `volume` 节记的几何。
struct Volume {
    chunk_count: u32,
    sectors_per_chunk: u32,
    bytes_per_sector: u32,
    sector_count: u64,
}

/// 刚读过的 `table` 怎样：坏了时紧跟着的 `table2` 顶上。
enum TableState {
    None,
    Good,
    Bad(String),
}

#[derive(Default)]
struct Builder {
    files: Vec<File>,
    paths: Vec<PathBuf>,
    tables: Vec<Table>,
    volume: Option<Volume>,
    next_chunk: u64,
}

impl Builder {
    /// 沿节链读完一段，收下 `volume` 与块表。
    fn walk_segment(
        &mut self,
        file: &File,
        segment: usize,
        number: u32,
        len: u64,
    ) -> Result<SegmentEnd, String> {
        let mut offset = FILE_HEADER_LEN as u64;
        let mut sectors: Option<(u64, u64)> = None;
        let mut table_state = TableState::None;
        for _ in 0..MAX_SECTIONS {
            let section = read_descriptor(file, offset)
                .map_err(|reason| format!("第 {number} 段偏移 {offset} 处的节：{reason}"))?;
            let data_start = offset + DESCRIPTOR_LEN_U64;
            let data_len = section.size.saturating_sub(DESCRIPTOR_LEN_U64);
            if let TableState::Bad(reason) = &table_state
                && section.kind != "table2"
            {
                return Err(format!("第 {number} 段的块表坏了，也没有副本：{reason}"));
            }
            match section.kind.as_str() {
                "next" | "done" => {
                    return Ok(if section.kind == "done" {
                        SegmentEnd::Done
                    } else {
                        SegmentEnd::Next
                    });
                }
                "volume" | "disk" if self.volume.is_none() => {
                    let mut data = vec![0u8; data_len.min(MAX_VOLUME_BYTES) as usize];
                    read_exact_file(file, data_start, &mut data)
                        .map_err(|error| format!("读不出 volume 节：{error}"))?;
                    self.volume = Some(parse_volume(&data)?);
                }
                "sectors" => {
                    sectors = Some((data_start, offset.saturating_add(section.size)));
                }
                "table" | "table2" => {
                    let wanted =
                        section.kind == "table" || matches!(table_state, TableState::Bad(_));
                    if wanted {
                        if self.volume.is_none() {
                            return Err("块表出现在 volume 节之前".to_owned());
                        }
                        let parsed =
                            self.parse_table(file, segment, (offset, section.size), sectors);
                        table_state = match (parsed, section.kind.as_str()) {
                            (Ok(table), _) => {
                                self.next_chunk = table.end_chunk();
                                self.tables.push(table);
                                TableState::Good
                            }
                            (Err(reason), "table") => TableState::Bad(reason),
                            (Err(reason), _) => {
                                return Err(format!(
                                    "第 {number} 段的块表与它的副本都坏了：{reason}"
                                ));
                            }
                        };
                    } else {
                        table_state = TableState::None;
                    }
                }
                _ => {}
            }
            if section.kind == "table2" {
                table_state = TableState::None;
            }
            if section.next <= offset || section.next >= len {
                return Err(format!(
                    "第 {number} 段在偏移 {offset} 的 {} 节之后断了：没有以 next 或 done 节结尾（文件可能不完整）",
                    section.kind
                ));
            }
            offset = section.next;
        }
        Err(format!("第 {number} 段的节太多"))
    }

    /// 读一张块表：表头校验和、各项（有表尾校验和时核对），算出表里最后一块到哪里为止。
    fn parse_table(
        &self,
        file: &File,
        segment: usize,
        (section_start, section_size): (u64, u64),
        sectors: Option<(u64, u64)>,
    ) -> Result<Table, String> {
        let data_start = section_start + DESCRIPTOR_LEN_U64;
        let data_len = section_size.saturating_sub(DESCRIPTOR_LEN_U64);
        let mut header = [0u8; TABLE_HEADER_LEN];
        if data_len < TABLE_HEADER_LEN as u64 {
            return Err("块表太短".to_owned());
        }
        read_exact_file(file, data_start, &mut header)
            .map_err(|error| format!("读不出块表：{error}"))?;
        if le32(&header, 20) != Some(adler32(header.get(..20).unwrap_or_default())) {
            return Err("块表头的校验和不对".to_owned());
        }
        let count = le32(&header, 0).unwrap_or_default();
        if count > MAX_TABLE_ENTRIES {
            return Err(format!("块表有 {count} 项，太多"));
        }
        let entries_len = u64::from(count) * 4;
        let available = data_len - TABLE_HEADER_LEN as u64;
        if entries_len > available {
            return Err(format!("块表记了 {count} 项，节里放不下"));
        }
        let with_footer = available >= entries_len + 4;
        let mut raw = vec![0u8; (entries_len + if with_footer { 4 } else { 0 }) as usize];
        read_exact_file(file, data_start + TABLE_HEADER_LEN as u64, &mut raw)
            .map_err(|error| format!("读不出块表：{error}"))?;
        let (items, footer) = raw.split_at(entries_len as usize);
        if with_footer && le32(footer, 0) != Some(adler32(items)) {
            return Err("块表各项的校验和不对".to_owned());
        }
        let entries: Vec<u32> = items
            .as_chunks::<4>()
            .0
            .iter()
            .map(|entry| u32::from_le_bytes(*entry))
            .collect();
        let overflow_from = entries
            .windows(2)
            .position(|pair| pair[1] & OFFSET_MASK < pair[0] & OFFSET_MASK)
            .map_or(entries.len(), |before| before + 1);
        let base = le64(&header, 8).unwrap_or_default();
        let mut table = Table {
            segment,
            first_chunk: self.next_chunk,
            base,
            entries,
            overflow_from,
            data_end: 0,
        };
        // 最后一块到它所在的 sectors 节结尾；没有 sectors 节（早期的写法）时到块表之前，块在块表之后时到块表结尾
        let last = table
            .entries
            .len()
            .checked_sub(1)
            .and_then(|index| table.start(index));
        table.data_end = match (last, sectors) {
            (None, _) => 0,
            (Some((offset, _)), Some((start, end))) if (start..end).contains(&offset) => end,
            (Some((offset, _)), _) if offset < section_start => section_start,
            (Some(_), _) => section_start.saturating_add(section_size),
        };
        Ok(table)
    }

    fn finish(self, first: &Path) -> Result<EwfImage, IoError> {
        let ewf_error = |reason: String| IoError::Ewf {
            path: first.to_path_buf(),
            reason,
        };
        let volume = self
            .volume
            .ok_or_else(|| ewf_error("没有 volume 节，不知道镜像多大".to_owned()))?;
        let sector_size = validate_sector_size(volume.bytes_per_sector).map_err(|_| {
            ewf_error(format!(
                "记的每扇区 {} 字节不受支持",
                volume.bytes_per_sector
            ))
        })?;
        let chunk_size = u64::from(volume.sectors_per_chunk) * u64::from(sector_size);
        if chunk_size == 0 || chunk_size > MAX_CHUNK_BYTES {
            return Err(ewf_error(format!(
                "记的每块 {} 个扇区不受支持",
                volume.sectors_per_chunk
            )));
        }
        let size = if volume.sector_count > 0 {
            volume.sector_count.checked_mul(u64::from(sector_size))
        } else {
            u64::from(volume.chunk_count).checked_mul(chunk_size)
        }
        .ok_or_else(|| ewf_error(format!("记的扇区数 {} 不合理", volume.sector_count)))?;
        let needed = size.div_ceil(chunk_size);
        if self.next_chunk < needed {
            return Err(ewf_error(format!(
                "块表里只有 {} 块，镜像需要 {needed} 块（段文件可能不全）",
                self.next_chunk
            )));
        }
        let capacity = (CACHE_BYTES / chunk_size).max(1) as usize;
        Ok(EwfImage {
            files: self.files,
            paths: self.paths,
            tables: self.tables,
            size,
            sector_size,
            chunk_size,
            cache: Mutex::new(ChunkCache::new(capacity)),
        })
    }
}

fn parse_volume(data: &[u8]) -> Result<Volume, String> {
    let (checked, sector_count) = if data.len() >= VOLUME_LONG_LEN {
        (VOLUME_LONG_LEN - 4, le64(data, 16))
    } else if data.len() >= VOLUME_SHORT_LEN {
        (VOLUME_SHORT_LEN - 4, le32(data, 16).map(u64::from))
    } else {
        return Err(format!("volume 节只有 {} 字节", data.len()));
    };
    let body = data.get(..checked).unwrap_or_default();
    if le32(data, checked) != Some(adler32(body)) {
        return Err("volume 节的校验和不对".to_owned());
    }
    Ok(Volume {
        chunk_count: le32(data, 4).unwrap_or_default(),
        sectors_per_chunk: le32(data, 8).unwrap_or_default(),
        bytes_per_sector: le32(data, 12).unwrap_or_default(),
        sector_count: sector_count.unwrap_or_default(),
    })
}

/// 一节的描述。
struct Section {
    kind: String,
    next: u64,
    size: u64,
}

fn read_descriptor(file: &File, offset: u64) -> Result<Section, String> {
    let mut descriptor = [0u8; DESCRIPTOR_LEN];
    read_exact_file(file, offset, &mut descriptor).map_err(|error| format!("读不出：{error}"))?;
    if le32(&descriptor, 72) != Some(adler32(descriptor.get(..72).unwrap_or_default())) {
        return Err("描述的校验和不对".to_owned());
    }
    let name = descriptor.get(..16).unwrap_or_default();
    let end = name
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(name.len());
    Ok(Section {
        kind: String::from_utf8_lossy(name.get(..end).unwrap_or_default()).into_owned(),
        next: le64(&descriptor, 16).unwrap_or_default(),
        size: le64(&descriptor, 24).unwrap_or_default(),
    })
}

/// 读满 `buf`；文件在此之前结束时报 `UnexpectedEof`。
fn read_exact_file(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match read_file_at(file, offset + filled as u64, &mut buf[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "文件在这里结束了",
                ));
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// 第 `number` 段的扩展名：1 到 99 是 `E01` … `E99`，之后 `EAA` … `EAZ`、`EBA` … `EZZ`、`FAA` … `ZZZ`。
/// `letter` 是第一段扩展名的首字母（`E`；SMART 是 `S`），大小写随它。超出范围时为 `None`。
pub(crate) fn segment_extension(letter: char, number: u32) -> Option<String> {
    if !letter.is_ascii_alphabetic() || number == 0 {
        return None;
    }
    let upper = letter.to_ascii_uppercase() as u8;
    let extension = if number <= 99 {
        format!("{}{number:02}", upper as char)
    } else {
        let value = number - 100;
        let third = b'A' + (value % 26) as u8;
        let second = b'A' + (value / 26 % 26) as u8;
        let first = u32::from(upper) + value / 26 / 26;
        if first > u32::from(b'Z') {
            return None;
        }
        String::from_utf8(vec![first as u8, second, third]).ok()?
    };
    Some(if letter.is_ascii_lowercase() {
        extension.to_ascii_lowercase()
    } else {
        extension
    })
}

/// 与第一段 `first` 同目录、同名的第 `number` 段；第一段的扩展名不是 `E01` 这样的样式时为 `None`。
pub(crate) fn segment_path(first: &Path, number: u32) -> Option<PathBuf> {
    let extension = first.extension()?.to_str()?;
    let mut chars = extension.chars();
    let letter = chars.next()?;
    if chars.as_str() != "01" {
        return None;
    }
    Some(first.with_extension(segment_extension(letter, number)?))
}

/// 已解压的块，最近用过的在后面；满了丢掉最久没用的。
struct ChunkCache {
    capacity: usize,
    entries: VecDeque<(u64, Arc<Vec<u8>>)>,
}

impl ChunkCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: VecDeque::with_capacity(capacity),
        }
    }

    fn get(&mut self, index: u64) -> Option<Arc<Vec<u8>>> {
        let position = self
            .entries
            .iter()
            .position(|(cached, _)| *cached == index)?;
        let entry = self.entries.remove(position)?;
        let data = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(data)
    }

    fn put(&mut self, index: u64, data: Arc<Vec<u8>>) {
        if self.entries.iter().any(|(cached, _)| *cached == index) {
            return;
        }
        self.entries.push_back((index, data));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }
}

#[cfg(test)]
mod tests;
