//! `$LogFile`（NTFS 日志）：找回 MFT 记录已被重用或清掉的已删除文件。只在内存里跟记录的变化，不写盘、不回放到卷上。
//!
//! 日志文件开头两页是重启区（`RSTR`），记着 LSN 里多少位是序号、日志页多大、页里的记录从哪里开始；之后是日志记录页
//! （`RCRD`）。两种页都有与 MFT 记录相同的多扇区保护（更新序列），修正不了的页（写到一半）整页跳过。日志记录头里
//! 记着自己的 LSN，LSN 去掉高位的序号再乘 8 就是它在日志文件里的位置：只认位置对得上的记录头，这样不必先找出哪一页
//! 最新，缓冲页里的副本也不会重复算。一条记录在本页放不下时接在下一页的数据区里。
//!
//! NTFS 的日志记录给出对 MFT 记录的每一步改动：建记录时整条记录的映像（InitializeFileRecordSegment），之后加删属性、
//! 改常驻属性的值、改运行表、改长度，删除时释放记录（DeallocateFileRecordSegment，撤销数据只有记录头的前 24 字节）。
//! 这里按 LSN 的先后把每条记录的映像跟到释放之前，得到删除前的样子；记录建得比日志里最老的一条还早时没有映像。
//! 名字与上级目录另从目录索引项的增删里取（删掉一个索引项时，撤销数据是整个索引项，键就是 `$FILE_NAME`）。
//!
//! 用户数据不进日志：常驻在记录里的小文件，内容的改动只记长度、不带数据，内容找不回。

use std::collections::{BTreeMap, HashMap};

use yisi_part::bytes::{le16, le32, le64};

use super::index::entry_key;
use super::record::{
    ATTRIBUTE_LIST, DATA, FileName, NAMESPACE_DOS, Record, apply_fixup, decode_runs,
};
use super::{FILE_NAME_DIRECTORY, FIRST_USER_RECORD, Ntfs, Value, file_names};
use crate::FsError;

/// `$LogFile` 的记录号。
const LOG_RECORD: u64 = 2;
/// 日志超过这么大时不读（Windows 建的一般是几 MiB 到 64 MiB）。
const MAX_LOG_BYTES: u64 = 1 << 30;
/// 留下的日志操作（大头是记录映像）最多这么多字节，再多的不再收集。
const MAX_KEPT_BYTES: usize = 256 << 20;
/// 一次读这么多字节的日志。
const BLOCK: u64 = 1 << 20;
/// 日志记录头与 NTFS 自己的记录头的长度。
const RECORD_HEADER: usize = 0x30;
const CLIENT_HEADER: usize = 0x20;
/// 一条日志记录的数据最多这么长（重做、撤销的长度都是 16 位的）。
const MAX_CLIENT_DATA: usize = 1 << 18;
/// 日志记录的类型：NTFS 的操作、检查点。
const CLIENT_RECORD: u32 = 1;
const RESTART_RECORD: u32 = 2;
/// 簇内偏移以 512 字节为单位。
const BLOCK_UNIT: u64 = 512;

/// 操作码。
const INITIALIZE_FILE_RECORD: u16 = 0x02;
const DEALLOCATE_FILE_RECORD: u16 = 0x03;
const CREATE_ATTRIBUTE: u16 = 0x05;
const DELETE_ATTRIBUTE: u16 = 0x06;
const UPDATE_RESIDENT_VALUE: u16 = 0x07;
const UPDATE_MAPPING_PAIRS: u16 = 0x09;
const SET_NEW_ATTRIBUTE_SIZES: u16 = 0x0B;
const ADD_INDEX_ENTRY_ROOT: u16 = 0x0C;
const ADD_INDEX_ENTRY_ALLOCATION: u16 = 0x0E;
const ZERO_END_OF_FILE_RECORD: u16 = 0x25;
/// 认得的最大操作码。
const LAST_OPERATION: u16 = 0x25;

/// 不改记录内容的操作：空操作、补偿记录、事务的结束与提交、打开属性、各种表的转储。
fn bookkeeping(operation: u16) -> bool {
    matches!(operation, 0x00 | 0x01 | 0x18..=0x20)
}

/// 跟着在内存里改记录映像的操作。其余落在 MFT 记录上的操作（目录索引根的增删等）不跟，那条记录的映像就此作废。
fn followed(operation: u16) -> bool {
    matches!(
        operation,
        INITIALIZE_FILE_RECORD
            | DEALLOCATE_FILE_RECORD
            | CREATE_ATTRIBUTE
            | DELETE_ATTRIBUTE
            | UPDATE_RESIDENT_VALUE
            | UPDATE_MAPPING_PAIRS
            | SET_NEW_ATTRIBUTE_SIZES
            | ZERO_END_OF_FILE_RECORD
    )
}

/// 日志里找到的一个文件或目录。
pub(crate) struct Logged {
    pub record: u64,
    pub sequence: u16,
    /// 名字（不含单独的 DOS 名）：有映像时取映像里的 `$FILE_NAME`，否则取目录索引里最后见到的那个。
    pub names: Vec<FileName>,
    pub short_name: Option<String>,
    pub is_dir: bool,
    /// 删除前的记录映像（基本记录）；记录建得比日志还早、或跟丢了时为 `None`。
    pub image: Option<Vec<u8>>,
    /// 映像里常驻数据流的内容不在日志里（只记了长度）。
    pub resident_unknown: bool,
}

/// 读日志的结果。
#[derive(Default)]
pub(crate) struct LogFile {
    /// 认出的日志记录数（位置与 LSN 对得上、内容完整的）。
    pub records: u64,
    /// 多扇区保护修正不了、跳过的页数。
    pub torn_pages: u64,
    pub found: Vec<Logged>,
}

/// 读 `$LogFile`，找出日志里的文件。`progress(已读字节, 日志字节)` 返回 `false` 时停下，结果为 `None`。
/// 没有 `$LogFile`、重启区读不出时返回错误（扫描照常完成，只是没有从日志找回的项）。
pub(crate) fn read(
    ntfs: &Ntfs,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Option<LogFile>, FsError> {
    let record = ntfs
        .record(LOG_RECORD)?
        .filter(Record::in_use)
        .ok_or(FsError::Invalid("$LogFile 的记录读不出"))?;
    let Some(Value::NonResident { extents, size, .. }) =
        ntfs.value(LOG_RECORD, &record, DATA, &[])?
    else {
        return Err(FsError::Invalid("$LogFile 没有非常驻的数据流"));
    };
    if size > MAX_LOG_BYTES {
        return Err(FsError::Invalid("$LogFile 超过 1 GiB，不读"));
    }
    let stream = ntfs.stream(extents, size);
    let restart = Restart::read(&stream, size)?;
    let mut collector = Collector {
        ntfs,
        restart,
        pending: None,
        log: LogFile::default(),
        ops: Vec::new(),
        kept: 0,
    };
    let page_size = restart.page_size as u64;
    let first = restart.system_page_size.saturating_mul(2) as u64;
    let pages = size.saturating_sub(first) / page_size;
    let per_block = (BLOCK / page_size).max(1);
    let mut buffer = Vec::new();
    let mut page = 0u64;
    while page < pages {
        let count = per_block.min(pages - page);
        let offset = first + page * page_size;
        buffer.resize((count * page_size) as usize, 0);
        if stream.read_at(offset, &mut buffer).is_ok() {
            for (index, bytes) in buffer.chunks_exact_mut(restart.page_size).enumerate() {
                collector.page(offset + index as u64 * page_size, bytes);
            }
        } else {
            // 读不出的一段：没完的记录接不上了
            collector.pending = None;
        }
        page += count;
        if !progress(offset + count * page_size, size) {
            return Ok(None);
        }
        if collector.kept > MAX_KEPT_BYTES {
            break;
        }
    }
    let Collector { mut log, ops, .. } = collector;
    log.found = follow(ntfs, ops);
    Ok(Some(log))
}

/// 重启区里要用的参数。
#[derive(Clone, Copy)]
struct Restart {
    system_page_size: usize,
    page_size: usize,
    /// 日志记录页里第一条记录的偏移。
    data_offset: usize,
    /// LSN 里表示位置的低位数（其余是序号）。
    file_bits: u32,
}

impl Restart {
    /// 两份重启区取 LSN 新的那份；修正不了、参数不合理的不用。
    fn read(stream: &super::stream::Stream, size: u64) -> Result<Self, FsError> {
        let mut head = [0u8; 0x20];
        stream.read_at(0, &mut head)?;
        let system_page_size = le32(&head, 0x10)
            .filter(|value| value.is_power_of_two() && (512..=65536).contains(value))
            .ok_or(FsError::Invalid("$LogFile 的重启区不对"))?
            as usize;
        let mut best: Option<(u64, Self)> = None;
        for copy in 0..2u64 {
            let offset = copy * system_page_size as u64;
            if offset + system_page_size as u64 > size {
                continue;
            }
            let mut page = vec![0u8; system_page_size];
            if stream.read_at(offset, &mut page).is_err() {
                continue;
            }
            if let Some((lsn, restart)) = Self::parse(&mut page, size)
                && best.is_none_or(|(newest, _)| lsn > newest)
            {
                best = Some((lsn, restart));
            }
        }
        best.map(|(_, restart)| restart)
            .ok_or(FsError::Invalid("$LogFile 的两份重启区都读不出"))
    }

    fn parse(page: &mut [u8], size: u64) -> Option<(u64, Self)> {
        if !matches!(page.get(..4), Some(b"RSTR" | b"CHKD")) || !apply_fixup(page) {
            return None;
        }
        let system_page_size = le32(page, 0x10)? as usize;
        let page_size = le32(page, 0x14)?;
        let area = usize::from(le16(page, 0x18)?);
        let current_lsn = le64(page, area)?;
        let sequence_bits = le32(page, area + 0x10)?;
        let header_length = usize::from(le16(page, area + 0x24)?);
        let data_offset = usize::from(le16(page, area + 0x26)?);
        let page_size = page_size as usize;
        let valid = system_page_size == page.len()
            && page_size.is_power_of_two()
            && (512..=65536).contains(&page_size)
            && (1..=60).contains(&sequence_bits)
            && header_length == RECORD_HEADER
            && data_offset.is_multiple_of(8)
            && data_offset >= 0x28
            && data_offset + RECORD_HEADER <= page_size
            // 位置的位数要够得着整个日志文件
            && (1u64 << (64 - sequence_bits)).saturating_mul(8) >= size;
        valid.then_some((
            current_lsn,
            Self {
                system_page_size,
                page_size,
                data_offset,
                file_bits: 64 - sequence_bits,
            },
        ))
    }

    /// LSN 指的是日志文件里的哪个字节。
    fn position(&self, lsn: u64) -> Option<u64> {
        (lsn & ((1u64 << self.file_bits) - 1)).checked_mul(8)
    }
}

/// 一条 NTFS 日志记录里要用的部分。
struct Op {
    lsn: u64,
    redo: u16,
    undo: u16,
    /// 落在哪条 MFT 记录上（簇号核对过）；不是 MFT 记录时为 `None`。
    record: Option<u64>,
    record_offset: usize,
    attribute_offset: usize,
    /// 重做数据声明的长度。日志里带着的数据可能比它短：用户数据只记长度、不带内容。
    redo_length: usize,
    redo_data: Vec<u8>,
    /// 撤销数据：只留释放记录的记录头、删掉的目录索引项。
    undo_data: Vec<u8>,
}

impl Op {
    fn redo_complete(&self) -> bool {
        self.redo_data.len() == self.redo_length
    }

    /// 带着的目录索引项：加索引项的重做数据、删索引项的撤销数据（撤销删除就是加回去）。
    fn index_entry(&self) -> Option<&[u8]> {
        if adds_index_entry(self.redo) {
            Some(&self.redo_data)
        } else if adds_index_entry(self.undo) {
            Some(&self.undo_data)
        } else {
            None
        }
    }
}

/// 往目录索引里加一项（索引根里或索引块里）。
fn adds_index_entry(operation: u16) -> bool {
    matches!(operation, ADD_INDEX_ENTRY_ROOT | ADD_INDEX_ENTRY_ALLOCATION)
}

/// 还没接完的一条日志记录。
struct Pending {
    lsn: u64,
    record_type: u32,
    length: usize,
    body: Vec<u8>,
}

struct Collector<'a> {
    ntfs: &'a Ntfs,
    restart: Restart,
    pending: Option<Pending>,
    log: LogFile,
    ops: Vec<Op>,
    kept: usize,
}

impl Collector<'_> {
    /// 处理日志文件里 `offset` 处的一页。
    fn page(&mut self, offset: u64, page: &mut [u8]) {
        let signed = page.get(..4) == Some(b"RCRD".as_slice());
        if !signed || !apply_fixup(page) {
            if signed {
                self.log.torn_pages += 1;
            }
            self.pending = None;
            return;
        }
        let data_offset = self.restart.data_offset;
        let mut start = data_offset;
        // 上一页没完的记录接在这一页的数据区开头；这一页要比那条记录新（不是上一轮留下的旧页）
        if let Some(mut pending) = self.pending.take() {
            let newer = |at| le64(page, at).is_some_and(|lsn| lsn >= pending.lsn);
            if newer(0x08) || newer(0x20) {
                let take = (pending.length - pending.body.len()).min(page.len() - data_offset);
                if let Some(bytes) = page.get(data_offset..data_offset + take) {
                    pending.body.extend_from_slice(bytes);
                    start = (data_offset + take).next_multiple_of(8);
                    if pending.body.len() == pending.length {
                        self.finish(pending);
                    } else {
                        self.pending = Some(pending);
                        return;
                    }
                }
            }
        }
        let mut at = start;
        while at + RECORD_HEADER <= page.len() {
            match self.record_at(offset, page, at) {
                Some(length) => {
                    let end = at + RECORD_HEADER + length;
                    if end > page.len() {
                        break;
                    }
                    at = end.next_multiple_of(8);
                }
                None => at += 8,
            }
        }
    }

    /// `at` 处是一条记录的开头时收下它（放不下的接到下一页），返回数据长度。
    fn record_at(&mut self, offset: u64, page: &[u8], at: usize) -> Option<usize> {
        let lsn = le64(page, at).filter(|&lsn| lsn != 0)?;
        if self.restart.position(lsn)? != offset.checked_add(at as u64)? {
            return None;
        }
        let previous = le64(page, at + 0x08)?;
        let undo_next = le64(page, at + 0x10)?;
        let length = le32(page, at + 0x18)? as usize;
        let record_type = le32(page, at + 0x20)?;
        if !matches!(record_type, CLIENT_RECORD | RESTART_RECORD)
            || length > MAX_CLIENT_DATA
            || previous >= lsn
            || undo_next >= lsn
        {
            return None;
        }
        let body_start = at + RECORD_HEADER;
        let here = (page.len() - body_start).min(length);
        let pending = Pending {
            lsn,
            record_type,
            length,
            body: page.get(body_start..body_start + here)?.to_vec(),
        };
        if here == length {
            self.finish(pending);
        } else {
            self.pending = Some(pending);
        }
        Some(length)
    }

    fn finish(&mut self, pending: Pending) {
        self.log.records += 1;
        if pending.record_type != CLIENT_RECORD {
            return;
        }
        if let Some(op) = self.client_op(pending.lsn, &pending.body) {
            self.kept = self
                .kept
                .saturating_add(op.redo_data.len() + op.undo_data.len() + 64);
            self.ops.push(op);
        }
    }

    /// NTFS 的记录头：重做与撤销的操作码、数据的位置，目标（记录里的偏移、簇号）。不用的操作不留。
    fn client_op(&self, lsn: u64, body: &[u8]) -> Option<Op> {
        let redo = le16(body, 0x00)?;
        let undo = le16(body, 0x02)?;
        if redo > LAST_OPERATION || undo > LAST_OPERATION || body.len() < CLIENT_HEADER {
            return None;
        }
        let redo_offset = usize::from(le16(body, 0x04)?);
        let redo_length = usize::from(le16(body, 0x06)?);
        let undo_offset = usize::from(le16(body, 0x08)?);
        let undo_length = usize::from(le16(body, 0x0A)?);
        let lcns = le16(body, 0x0E)?;
        let record_offset = usize::from(le16(body, 0x10)?);
        let attribute_offset = usize::from(le16(body, 0x12)?);
        let block = le16(body, 0x14)?;
        let vcn = le64(body, 0x18)?;
        let record = if lcns > 0 {
            le64(body, CLIENT_HEADER).and_then(|lcn| self.mft_record(vcn, block, lcn))
        } else {
            None
        };
        let data = |offset: usize, length: usize| {
            body.get(offset..offset.saturating_add(length))
                .map(<[u8]>::to_vec)
                .unwrap_or_default()
        };
        let mut op = Op {
            lsn,
            redo,
            undo,
            record,
            record_offset,
            attribute_offset,
            redo_length,
            redo_data: Vec::new(),
            undo_data: Vec::new(),
        };
        // 留下的：带目录索引项的（取名字），以及落在 MFT 记录上、会改记录的
        let names = adds_index_entry(redo) || adds_index_entry(undo);
        if !names && (record.is_none() || bookkeeping(redo)) {
            return None;
        }
        if followed(redo) || adds_index_entry(redo) {
            op.redo_data = data(redo_offset, redo_length);
        }
        if adds_index_entry(undo) {
            op.undo_data = data(undo_offset, undo_length);
        }
        Some(op)
    }

    /// 目标是不是 MFT 里的一条记录：簇内偏移落在记录的开头，日志里记的簇号正是 MFT 这一段所在的簇。
    fn mft_record(&self, vcn: u64, block: u16, lcn: u64) -> Option<u64> {
        let ntfs = self.ntfs;
        let start = vcn.checked_mul(ntfs.cluster_size)?;
        let byte = start.checked_add(u64::from(block).checked_mul(BLOCK_UNIT)?)?;
        if !byte.is_multiple_of(ntfs.record_size) {
            return None;
        }
        let number = byte / ntfs.record_size;
        let on_disk = ntfs.mft.disk_offset(start)?;
        (number < ntfs.record_count && on_disk == lcn.checked_mul(ntfs.cluster_size)?)
            .then_some(number)
    }
}

/// 一条记录在内存里的映像。
#[derive(Clone)]
struct Image {
    bytes: Vec<u8>,
    resident_unknown: bool,
}

impl Image {
    fn used(&self) -> Option<usize> {
        le32(&self.bytes, 0x18)
            .map(|used| used as usize)
            .filter(|&used| used <= self.bytes.len())
    }

    fn set_used(&mut self, used: usize) -> Option<()> {
        self.write(0x18, &u32::try_from(used).ok()?.to_le_bytes())
    }

    fn write(&mut self, at: usize, data: &[u8]) -> Option<()> {
        self.bytes
            .get_mut(at..at.checked_add(data.len())?)?
            .copy_from_slice(data);
        Some(())
    }

    /// 在 `at` 处插入，之后的内容往后移，记录的已用长度跟着变。
    fn insert(&mut self, at: usize, data: &[u8]) -> Option<()> {
        let used = self.used()?;
        let grown = used.checked_add(data.len())?;
        if at > used || grown > self.bytes.len() {
            return None;
        }
        self.bytes.copy_within(at..used, at + data.len());
        self.write(at, data)?;
        self.set_used(grown)
    }

    /// 去掉 `at` 起的 `length` 字节，之后的内容往前移。
    fn remove(&mut self, at: usize, length: usize) -> Option<()> {
        let used = self.used()?;
        let end = at.checked_add(length).filter(|&end| end <= used)?;
        self.bytes.copy_within(end..used, at);
        self.bytes.get_mut(used - length..used)?.fill(0);
        self.set_used(used - length)
    }

    /// `at` 处的属性（类型与长度看着合理时）的长度。
    fn attribute_length(&self, at: usize) -> Option<usize> {
        let used = self.used()?;
        let length = le32(&self.bytes, at.checked_add(4)?)? as usize;
        (length >= 0x18 && length.is_multiple_of(8) && at.checked_add(length)? <= used)
            .then_some(length)
    }

    /// 把 `at` 处属性的长度改成 `length`，之后的属性跟着移。
    fn resize_attribute(&mut self, at: usize, length: usize) -> Option<()> {
        let old = self.attribute_length(at)?;
        if length > old {
            self.insert(at + old, &vec![0u8; length - old])?;
        } else if length < old {
            self.remove(at + length, old - length)?;
        }
        self.write(at + 4, &u32::try_from(length).ok()?.to_le_bytes())
    }

    /// 照一条操作改映像；改不了（越界、属性对不上、数据不全）时返回 `None`，这份映像就不再可信。
    fn apply(&mut self, op: &Op) -> Option<()> {
        let at = op.record_offset;
        match op.redo {
            CREATE_ATTRIBUTE => {
                if !op.redo_complete() || op.redo_data.len() < 0x18 {
                    return None;
                }
                self.insert(at, &op.redo_data)
            }
            DELETE_ATTRIBUTE => {
                let length = self.attribute_length(at)?;
                self.remove(at, length)
            }
            UPDATE_RESIDENT_VALUE => self.update_resident(op),
            UPDATE_MAPPING_PAIRS => self.update_mapping_pairs(op),
            SET_NEW_ATTRIBUTE_SIZES => {
                if !op.redo_complete() || *self.bytes.get(at.checked_add(8)?)? == 0 {
                    return None;
                }
                self.attribute_length(at)?;
                // 分配长度、数据长度、有效长度，压缩的属性还有压缩后的长度；不越过运行表
                let runs_offset = usize::from(le16(&self.bytes, at + 0x20)?);
                let room = runs_offset.checked_sub(0x28)?;
                let take = op.redo_data.len().min(room);
                self.write(at + 0x28, op.redo_data.get(..take)?)
            }
            ZERO_END_OF_FILE_RECORD => {
                let end = at.checked_add(op.redo_length)?.min(self.bytes.len());
                self.bytes.get_mut(at..end)?.fill(0);
                Some(())
            }
            _ => None,
        }
    }

    /// 改常驻属性的值：写到值的末尾之外时值跟着变长。数据不在日志里（用户数据）时，数据流的内容记为不知道。
    fn update_resident(&mut self, op: &Op) -> Option<()> {
        let at = op.record_offset;
        self.attribute_length(at)?;
        if *self.bytes.get(at + 8)? != 0 {
            return None;
        }
        let kind = le32(&self.bytes, at)?;
        let value_offset = usize::from(le16(&self.bytes, at + 0x14)?);
        let value_length = le32(&self.bytes, at + 0x10)? as usize;
        let start = op.attribute_offset;
        if start < value_offset {
            return None;
        }
        let end = start.checked_add(op.redo_length)?;
        if end > value_offset + value_length {
            self.resize_attribute(at, end.next_multiple_of(8))?;
            self.write(
                at + 0x10,
                &u32::try_from(end - value_offset).ok()?.to_le_bytes(),
            )?;
        }
        if op.redo_complete() {
            self.write(at + start, &op.redo_data)
        } else if kind == DATA {
            self.bytes.get_mut(at + start..at + end)?.fill(0);
            self.resident_unknown = true;
            Some(())
        } else {
            None
        }
    }

    /// 改运行表：写到属性末尾之外时属性跟着变长，最后一个虚拟簇号按新的运行表重算。
    fn update_mapping_pairs(&mut self, op: &Op) -> Option<()> {
        let at = op.record_offset;
        let old = self.attribute_length(at)?;
        if !op.redo_complete() || *self.bytes.get(at + 8)? == 0 {
            return None;
        }
        let runs_offset = usize::from(le16(&self.bytes, at + 0x20)?);
        if op.attribute_offset < runs_offset {
            return None;
        }
        let end = op.attribute_offset.checked_add(op.redo_data.len())?;
        if end > old {
            self.resize_attribute(at, end.next_multiple_of(8))?;
        }
        self.write(at + op.attribute_offset, &op.redo_data)?;
        let length = self.attribute_length(at)?;
        let lowest = le64(&self.bytes, at + 0x10)?;
        if let Some(extents) = self
            .bytes
            .get(at + runs_offset..at + length)
            .and_then(|runs| decode_runs(runs, lowest))
        {
            let next = extents
                .last()
                .map_or(lowest, |extent| extent.vcn.saturating_add(extent.length));
            self.write(at + 0x18, &next.wrapping_sub(1).to_le_bytes())?;
        }
        Some(())
    }
}

/// 名字的来源：索引项或记录映像里的 `$FILE_NAME`，以及见到它时的 LSN。
struct Named {
    lsn: u64,
    name: FileName,
}

/// 按 LSN 的先后跟每条 MFT 记录的映像，收集名字，整理出日志里的文件。
fn follow(ntfs: &Ntfs, mut ops: Vec<Op>) -> Vec<Logged> {
    ops.sort_by_key(|op| op.lsn);
    let record_size = ntfs.record_size as usize;
    let mut images: HashMap<u64, Image> = HashMap::new();
    // 释放之前的映像：每个（记录号，序号）留最后一份
    let mut released: HashMap<(u64, u16), Image> = HashMap::new();
    let mut names: HashMap<(u64, u16), Vec<Named>> = HashMap::new();
    for op in &ops {
        if let Some(key) = op.index_entry().and_then(logged_key) {
            names
                .entry((key.record, key.sequence))
                .or_default()
                .push(Named {
                    lsn: op.lsn,
                    name: key.name,
                });
        }
        let Some(number) = op.record else {
            continue;
        };
        match op.redo {
            INITIALIZE_FILE_RECORD => {
                images.remove(&number);
                if !op.redo_complete() || op.record_offset != 0 || op.redo_length > record_size {
                    continue;
                }
                let mut bytes = vec![0u8; record_size];
                let Some(slot) = bytes.get_mut(..op.redo_data.len()) else {
                    continue;
                };
                slot.copy_from_slice(&op.redo_data);
                let Some(record) = Record::parse_image(bytes.clone(), number) else {
                    continue;
                };
                for name in file_names(&record) {
                    names
                        .entry((number, record.sequence))
                        .or_default()
                        .push(Named { lsn: op.lsn, name });
                }
                images.insert(
                    number,
                    Image {
                        bytes,
                        resident_unknown: false,
                    },
                );
            }
            DEALLOCATE_FILE_RECORD => {
                if let Some(image) = images.remove(&number)
                    && let Some(sequence) = le16(&image.bytes, 0x10)
                {
                    released.insert((number, sequence), image);
                }
            }
            redo if followed(redo) => {
                let applied = images
                    .get_mut(&number)
                    .map(|image| image.apply(op).is_some());
                if applied == Some(false) {
                    images.remove(&number);
                }
            }
            redo if bookkeeping(redo) => {}
            _ => {
                images.remove(&number);
            }
        }
    }

    let mut keys: Vec<(u64, u16)> = released.keys().chain(names.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();
    let mut found = Vec::new();
    for key in keys {
        let (record, sequence) = key;
        if record < FIRST_USER_RECORD {
            continue;
        }
        let image = released.remove(&key);
        let parsed = image
            .as_ref()
            .and_then(|image| Record::parse_image(image.bytes.clone(), record));
        // 扩展记录不是文件，它的数据流归基本记录
        if parsed.as_ref().is_some_and(|parsed| !parsed.is_base()) {
            continue;
        }
        let evidence = names.remove(&key).unwrap_or_default();
        let (chosen, short_name) = match &parsed {
            Some(parsed) => split_names(file_names(parsed)),
            None => latest_names(evidence),
        };
        if chosen.is_empty() {
            continue;
        }
        let is_dir = parsed.as_ref().is_some_and(Record::is_dir)
            || chosen
                .iter()
                .any(|name| name.flags & FILE_NAME_DIRECTORY != 0);
        // 有属性列表的文件，数据流的其余部分在扩展记录里，日志里拼不全：只给名字
        let usable = parsed
            .as_ref()
            .is_some_and(|parsed| parsed.find(ATTRIBUTE_LIST, &[]).is_none());
        let (image, resident_unknown) = match image {
            Some(image) if usable => (Some(image.bytes), image.resident_unknown),
            _ => (None, false),
        };
        found.push(Logged {
            record,
            sequence,
            names: chosen,
            short_name,
            is_dir,
            image,
            resident_unknown,
        });
    }
    found
}

/// 目录索引项里指着的文件（系统文件不算）：键要正好是一个 `$FILE_NAME`（别的索引的键长短不同）。
fn logged_key(entry: &[u8]) -> Option<super::index::Key> {
    let key = entry_key(entry)?;
    let key_length = usize::from(le16(entry, 10)?);
    let name_length = usize::from(*entry.get(0x10 + 0x40)?);
    let exact = key_length == 0x42 + name_length * 2 && key.name.namespace <= 3;
    (exact && key.record >= FIRST_USER_RECORD).then_some(key)
}

/// 记录映像里的名字：DOS 名单独作短文件名；只有 DOS 名时就用它。
fn split_names(names: Vec<FileName>) -> (Vec<FileName>, Option<String>) {
    let short = names
        .iter()
        .find(|name| name.namespace == NAMESPACE_DOS)
        .map(|name| name.name.clone());
    let long: Vec<FileName> = names
        .iter()
        .filter(|name| name.namespace != NAMESPACE_DOS)
        .cloned()
        .collect();
    if long.is_empty() {
        (names, None)
    } else {
        let short = short.filter(|short| long.iter().all(|name| name.name != *short));
        (long, short)
    }
}

/// 没有映像时：取最后见到的长名字（改过名的文件以后来的为准），DOS 名取最后见到的那个。
fn latest_names(mut evidence: Vec<Named>) -> (Vec<FileName>, Option<String>) {
    evidence.sort_by_key(|named| named.lsn);
    let mut latest: BTreeMap<bool, FileName> = BTreeMap::new();
    for named in evidence {
        latest.insert(named.name.namespace == NAMESPACE_DOS, named.name);
    }
    let short = latest.remove(&true).map(|name| name.name);
    match latest.remove(&false) {
        Some(long) => {
            let short = short.filter(|short| *short != long.name);
            (vec![long], short)
        }
        None => (Vec::new(), None),
    }
}

#[cfg(test)]
mod tests;
