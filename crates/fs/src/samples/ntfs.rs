//! NTFS 测试样本：按公开的结构资料拼出的小卷（8 MiB、4 KiB 一簇、1 KiB 一条 MFT 记录）。
//! 能建目录、写常驻或非常驻的文件（连续、碎片、带稀疏空洞、指定位置）、硬链接、DOS 短名、
//! 数据流放进扩展记录（属性列表）、压缩 / 加密 / 系统压缩标记，删除文件与目录、重用或清掉记录。
//! 目录索引在 `finish` 时按在用的子项生成，项多时放进 INDEX_ALLOCATION。
//! 可以带 `$LogFile`：按 Windows 建文件、写内容、删除时记日志的样子记下操作，`finish` 时写成重启区与日志记录页。

use std::collections::BTreeMap;

use crate::boot_write::{NtfsBoot, ntfs_boot};

const SECTOR: usize = 512;
const CLUSTER: usize = 4096;
const RECORD: usize = 1024;
const TOTAL_CLUSTERS: usize = 2048;
const MFT_LCN: usize = 4;
const MFT_RECORDS: usize = 128;
const MIRROR_LCN: usize = 40;
/// `$Bitmap` 所在的簇（测试按它改坏位图）。
pub const BITMAP_LCN: usize = 44;
/// 文件数据从这个簇起分配。
const DATA_LCN: usize = 64;
const INDEX_BLOCK: usize = 4096;
/// 索引根里最多放这么多字节的项，再多就放进索引块。
const ROOT_INDEX_LIMIT: usize = 600;
/// 2024-03-05 13:45:30 UTC，与 FAT 样本的时间相同。
const SAMPLE_FILETIME: u64 = 116_444_736_000_000_000 + 1_709_646_330 * 10_000_000;

pub const ROOT: u64 = 5;
pub const TAG_WOF: u32 = 0x8000_0017;

/// 引导区（`$Boot`，8 KiB）占两个簇；MFT 镜像只放前 4 条记录，一个簇。
const BOOT_CLUSTERS: usize = 2;
const MIRROR_CLUSTERS: usize = 1;
/// 系统文件：记录号与名字。都在根目录里，序号等于记录号（0 号为 1）。
const SYSTEM_FILES: [(u64, &str); 6] = [
    (0, "$MFT"),
    (1, "$MFTMirr"),
    (3, "$Volume"),
    (6, "$Bitmap"),
    (7, "$Boot"),
    (8, "$BadClus"),
];
/// 卷的簇数：最后一个扇区留给备份引导扇区，最后一个簇不完整、不算。
const VOLUME_CLUSTERS: usize = TOTAL_CLUSTERS - 1;
/// 系统文件占的簇：起点与簇数。
const SYSTEM_CLUSTERS: [(usize, usize); 4] = [
    (0, BOOT_CLUSTERS),
    (MFT_LCN, MFT_RECORDS * RECORD / CLUSTER),
    (MIRROR_LCN, MIRROR_CLUSTERS),
    (BITMAP_LCN, 1),
];

#[derive(Debug, Clone, Default)]
enum Data {
    #[default]
    None,
    Resident(Vec<u8>),
    /// 运行表（簇号为空表示稀疏）与长度。
    NonResident {
        runs: Vec<(Option<usize>, usize)>,
        size: usize,
    },
}

/// [`NtfsImage::add_compressed`] 的一个压缩单元（16 个簇，64 KiB）。
pub enum CompressedUnit<'a> {
    /// 原样存放：16 个簇全分配，内容必须正好 64 KiB。
    Stored(&'a [u8]),
    /// LZNT1 压缩的内容，分配够放下它的簇、其余稀疏；`length` 是解开后的长度（只有最后一个单元可以不足 64 KiB）。
    Packed { packed: &'a [u8], length: usize },
    /// 整个单元稀疏，读作 0。
    Hole,
}

/// 压缩单元：2^4 = 16 个簇。
pub const COMPRESSION_UNIT_CLUSTERS: usize = 16;

#[derive(Debug, Clone)]
struct Name {
    parent: u64,
    /// 写下这个名字时父目录记录的序号。
    parent_sequence: u16,
    name: String,
    namespace: u8,
}

#[derive(Debug, Clone, Default)]
struct Node {
    in_use: bool,
    is_dir: bool,
    sequence: u16,
    names: Vec<Name>,
    data: Data,
    compressed: bool,
    /// 压缩的数据流记的压缩单元（[`NtfsImage::add_compressed`] 为 4；只 `mark_compressed` 时为 0，读不出）。
    compression_unit: u8,
    encrypted: bool,
    /// 另加的文件属性位（隐藏 0x02、系统 0x04 等），写进 `$STANDARD_INFORMATION` 与 `$FILE_NAME`。
    attributes: u32,
    /// 不写进上级目录的索引（造「文件不在上级目录索引里」）。
    unindexed: bool,
    /// 目录不写 `$I30` 索引（造「目录索引读不出」）。
    without_index: bool,
    /// 重分析点的内容（开头 4 字节是标记）。
    reparse: Option<Vec<u8>>,
    /// 备用数据流（有名字的 `$DATA`），放在基本记录里、无名数据流之后。
    streams: Vec<(String, Data)>,
    /// 数据流的第二段放进这条扩展记录，基本记录里加属性列表。
    extension: Option<u64>,
    /// 这是哪条记录的扩展记录。
    extension_of: Option<u64>,
}

pub struct NtfsImage {
    image: Vec<u8>,
    nodes: BTreeMap<u64, Node>,
    bitmap: Vec<bool>,
    next_cluster: usize,
    next_record: u64,
    label: String,
    /// `$MFT` 的记录位图（一位一条记录），`finish` 时按在用的记录生成。
    record_bitmap: Vec<u8>,
    /// 有 `$LogFile` 时，按先后写进日志的操作（[`NtfsImage::enable_log`]）。
    log: Option<Vec<LogOp>>,
}

/// `$LogFile` 从这个簇起，占 [`LOG_CLUSTERS`] 个簇（64 KiB）：两页重启区、两页缓冲页、12 页日志记录。
pub const LOG_LCN: usize = 48;
const LOG_CLUSTERS: usize = 16;
/// 日志页的大小；日志记录页从第 4 页起（第 2 页放第 4 页的副本，模拟缓冲页）。
pub const LOG_PAGE: usize = 4096;
pub const LOG_FIRST_RECORD_PAGE: usize = 4;
/// 页里第一条日志记录的偏移。
const LOG_DATA_OFFSET: usize = 0x40;
/// LSN 里序号占的位数（与 Windows 建的一样），其余位是位置除以 8。
const LOG_SEQUENCE_BITS: u32 = 45;

/// 写进样本 `$LogFile` 的一条 NTFS 日志记录。
#[derive(Debug, Clone)]
struct LogOp {
    redo: u16,
    undo: u16,
    /// 落在哪条 MFT 记录上；`None` 时落在别处（目录的索引块）。
    record: Option<u64>,
    record_offset: usize,
    attribute_offset: usize,
    redo_data: Vec<u8>,
    /// 声明的重做长度；`None` 时就是数据的长度。用户数据只记长度、不带数据时比数据长。
    redo_length: Option<usize>,
    undo_data: Vec<u8>,
}

impl LogOp {
    fn new(redo: u16, undo: u16, record: Option<u64>, record_offset: usize) -> Self {
        Self {
            redo,
            undo,
            record,
            record_offset,
            attribute_offset: 0,
            redo_data: Vec::new(),
            redo_length: None,
            undo_data: Vec::new(),
        }
    }

    /// NTFS 的记录头（含一个簇号）加重做、撤销数据。
    fn client_bytes(&self) -> Vec<u8> {
        let redo_offset = 0x28usize;
        let undo_offset = (redo_offset + self.redo_data.len()).next_multiple_of(8);
        let mut body = vec![0u8; undo_offset + self.undo_data.len()];
        let (vcn, block, lcn) = match self.record {
            Some(record) => {
                let byte = record as usize * RECORD;
                let vcn = byte / CLUSTER;
                (vcn, (byte % CLUSTER) / SECTOR, MFT_LCN + vcn)
            }
            None => (0, 0, 0),
        };
        let fields: [(usize, u16); 12] = [
            (0x00, self.redo),
            (0x02, self.undo),
            (0x04, redo_offset as u16),
            (
                0x06,
                self.redo_length.unwrap_or(self.redo_data.len()) as u16,
            ),
            (0x08, undo_offset as u16),
            (0x0A, self.undo_data.len() as u16),
            (0x0C, 24),
            (0x0E, 1),
            (0x10, self.record_offset as u16),
            (0x12, self.attribute_offset as u16),
            (0x14, block as u16),
            (0x16, 0),
        ];
        for (at, value) in fields {
            body[at..at + 2].copy_from_slice(&value.to_le_bytes());
        }
        body[0x18..0x20].copy_from_slice(&(vcn as u64).to_le_bytes());
        body[0x20..0x28].copy_from_slice(&(lcn as u64).to_le_bytes());
        body[redo_offset..redo_offset + self.redo_data.len()].copy_from_slice(&self.redo_data);
        body[undo_offset..].copy_from_slice(&self.undo_data);
        body
    }
}

/// 日志操作码（样本用到的）。
const OP_NOOP: u16 = 0x00;
const OP_INITIALIZE: u16 = 0x02;
const OP_DEALLOCATE: u16 = 0x03;
const OP_CREATE_ATTRIBUTE: u16 = 0x05;
const OP_DELETE_ATTRIBUTE: u16 = 0x06;
const OP_UPDATE_RESIDENT: u16 = 0x07;
const OP_UPDATE_MAPPING: u16 = 0x09;
const OP_SET_SIZES: u16 = 0x0B;
const OP_ADD_INDEX_ROOT: u16 = 0x0C;
const OP_DELETE_INDEX_ROOT: u16 = 0x0D;
const OP_ADD_INDEX_ALLOCATION: u16 = 0x0E;
const OP_DELETE_INDEX_ALLOCATION: u16 = 0x0F;

impl Default for NtfsImage {
    fn default() -> Self {
        Self::new()
    }
}

impl NtfsImage {
    pub fn new() -> Self {
        let mut bitmap = vec![false; TOTAL_CLUSTERS];
        // 系统文件的簇（引导区、MFT、MFT 镜像、位图）已占用，与它们记录里的运行表一致；
        // 其余到 DATA_LCN 为止的簇空着不用
        for (lcn, count) in SYSTEM_CLUSTERS {
            for slot in &mut bitmap[lcn..lcn + count] {
                *slot = true;
            }
        }
        let mut nodes = BTreeMap::new();
        nodes.insert(
            ROOT,
            Node {
                in_use: true,
                is_dir: true,
                sequence: 5,
                names: vec![Name {
                    parent: ROOT,
                    parent_sequence: 5,
                    name: ".".to_owned(),
                    namespace: 3,
                }],
                ..Node::default()
            },
        );
        Self {
            image: vec![0u8; TOTAL_CLUSTERS * CLUSTER],
            nodes,
            bitmap,
            next_cluster: DATA_LCN,
            next_record: 16,
            label: "NTFS SAMPLE".to_owned(),
            record_bitmap: Vec::new(),
            log: None,
        }
    }

    /// 加上 `$LogFile`（第 2 条记录，[`LOG_LCN`] 起 64 KiB）；之后 `log_*` 记下的操作在 `finish` 时写进日志。
    pub fn enable_log(&mut self) {
        for slot in &mut self.bitmap[LOG_LCN..LOG_LCN + LOG_CLUSTERS] {
            *slot = true;
        }
        self.log = Some(Vec::new());
    }

    fn push_log(&mut self, op: LogOp) {
        self.log.as_mut().expect("先 enable_log").push(op);
    }

    /// 像 Windows 建文件、写内容那样记日志（在 `delete` 之前调）：建记录的整条映像（只有 `$STANDARD_INFORMATION`、
    /// `$FILE_NAME` 与空的常驻数据流），往上级目录的索引根里加项；非常驻的文件再删掉常驻数据流、建非常驻的、
    /// 写运行表、定长度；常驻的文件只记写入的长度（用户数据不进日志）。
    pub fn log_history(&mut self, record: u64) {
        let node = self.nodes[&record].clone();
        let mut attributes = vec![resident(0x10, "", 0, &self.standard_information(&node))];
        for (id, name) in node.names.iter().enumerate() {
            attributes.push(resident(
                0x30,
                "",
                1 + id as u16,
                &self.file_name_value(record, name),
            ));
        }
        let data_offset = 0x38 + attributes.iter().map(Vec::len).sum::<usize>();
        let empty_data = resident(0x80, "", 10, &[]);
        if !node.is_dir {
            attributes.push(empty_data.clone());
        }
        let image = record_image(record, node.sequence, true, node.is_dir, 0, &attributes);
        let used = u32::from_le_bytes(image[0x18..0x1C].try_into().unwrap()) as usize;
        let mut init = LogOp::new(OP_INITIALIZE, OP_NOOP, Some(record), 0);
        init.redo_data = image[..used].to_vec();
        self.push_log(init);
        for name in &node.names {
            let mut add = LogOp::new(
                OP_ADD_INDEX_ROOT,
                OP_DELETE_INDEX_ROOT,
                Some(name.parent),
                0x98,
            );
            add.redo_data = self.index_entry_for(record, name);
            self.push_log(add);
        }
        match &node.data {
            Data::NonResident { runs, size } => {
                let mut delete = LogOp::new(
                    OP_DELETE_ATTRIBUTE,
                    OP_CREATE_ATTRIBUTE,
                    Some(record),
                    data_offset,
                );
                delete.undo_data = empty_data;
                self.push_log(delete);
                let mut create = LogOp::new(
                    OP_CREATE_ATTRIBUTE,
                    OP_DELETE_ATTRIBUTE,
                    Some(record),
                    data_offset,
                );
                create.redo_data = non_resident(0x80, "", 10, 0, 0, &[], 0, 0);
                self.push_log(create);
                let mut mapping = LogOp::new(
                    OP_UPDATE_MAPPING,
                    OP_UPDATE_MAPPING,
                    Some(record),
                    data_offset,
                );
                mapping.attribute_offset = 0x40;
                mapping.redo_data = encode_runs(runs);
                self.push_log(mapping);
                let clusters: usize = runs.iter().map(|run| run.1).sum();
                let mut sizes = LogOp::new(OP_SET_SIZES, OP_SET_SIZES, Some(record), data_offset);
                for value in [clusters * CLUSTER, *size, *size] {
                    sizes.redo_data.extend((value as u64).to_le_bytes());
                }
                self.push_log(sizes);
            }
            Data::Resident(bytes) => {
                let mut write = LogOp::new(
                    OP_UPDATE_RESIDENT,
                    OP_UPDATE_RESIDENT,
                    Some(record),
                    data_offset,
                );
                write.attribute_offset = 0x18;
                write.redo_length = Some(bytes.len());
                self.push_log(write);
            }
            Data::None => {}
        }
    }

    /// 像 Windows 删除那样记日志（在 `delete` 之前调）：从上级目录的索引块里删掉各个名字（撤销数据是整个索引项），
    /// 再释放记录（撤销数据是记录头的前 24 字节）。只调这个、不调 [`log_history`](Self::log_history) 时，
    /// 日志里就只有名字（文件建得比日志里最老的一条还早）。
    pub fn log_deletion(&mut self, record: u64) {
        let node = self.nodes[&record].clone();
        for name in &node.names {
            let mut remove =
                LogOp::new(OP_DELETE_INDEX_ALLOCATION, OP_ADD_INDEX_ALLOCATION, None, 0);
            remove.attribute_offset = 0x40;
            remove.undo_data = self.index_entry_for(record, name);
            self.push_log(remove);
        }
        let header = record_image(record, node.sequence, true, node.is_dir, 0, &[]);
        let mut release = LogOp::new(OP_DEALLOCATE, OP_INITIALIZE, Some(record), 0);
        release.undo_data = header[..24].to_vec();
        self.push_log(release);
    }

    /// 记一条不改记录的日志（空操作），带 `length` 字节的数据：把后面的记录挤到下一页去。
    pub fn log_padding(&mut self, length: usize) {
        let mut noop = LogOp::new(OP_NOOP, OP_NOOP, None, 0);
        noop.redo_data = vec![0x5A; length];
        self.push_log(noop);
    }

    /// 清掉一条记录（`finish` 时那里全是 0），模拟记录被清掉。
    pub fn wipe(&mut self, record: u64) {
        self.nodes.remove(&record);
    }

    fn index_entry_for(&self, record: u64, name: &Name) -> Vec<u8> {
        let reference = record | u64::from(self.nodes[&record].sequence) << 48;
        index_entry(reference, &self.file_name_value(record, name), 0)
    }

    pub fn cluster_size(&self) -> usize {
        CLUSTER
    }

    pub fn set_label(&mut self, label: &str) {
        self.label = label.to_owned();
    }

    pub fn sequence(&self, record: u64) -> u16 {
        self.nodes[&record].sequence
    }

    fn new_node(&mut self, parent: u64, name: &str, is_dir: bool) -> u64 {
        let record = self.next_record;
        self.next_record += 1;
        assert!((record as usize) < MFT_RECORDS, "样本的 MFT 记录用完了");
        let parent_sequence = self.nodes[&parent].sequence;
        self.nodes.insert(
            record,
            Node {
                in_use: true,
                is_dir,
                sequence: 1,
                names: vec![Name {
                    parent,
                    parent_sequence,
                    name: name.to_owned(),
                    namespace: 1,
                }],
                ..Node::default()
            },
        );
        record
    }

    pub fn mkdir(&mut self, parent: u64, name: &str) -> u64 {
        self.new_node(parent, name, true)
    }

    /// 连续存放的文件；不超过 600 字节时内容常驻在记录里。
    pub fn add_file(&mut self, parent: u64, name: &str, data: &[u8]) -> u64 {
        let record = self.new_node(parent, name, false);
        self.set_data(record, data, false);
        record
    }

    /// 每用一个簇就空出一个簇的碎片文件（总是非常驻）。
    pub fn add_fragmented(&mut self, parent: u64, name: &str, data: &[u8]) -> u64 {
        let record = self.new_node(parent, name, false);
        self.set_data(record, data, true);
        record
    }

    /// 从簇 `lcn` 起连续存放（模拟新文件写进了已删除文件的位置）。
    pub fn add_file_at(&mut self, parent: u64, name: &str, data: &[u8], lcn: usize) -> u64 {
        let record = self.new_node(parent, name, false);
        let count = data.len().div_ceil(CLUSTER);
        self.write_clusters(lcn, data);
        for slot in &mut self.bitmap[lcn..lcn + count] {
            *slot = true;
        }
        self.nodes.get_mut(&record).unwrap().data = Data::NonResident {
            runs: vec![(Some(lcn), count)],
            size: data.len(),
        };
        record
    }

    /// 真正压缩存放的文件：按压缩单元给出各段，压缩单元记为 4（16 个簇）。
    pub fn add_compressed(&mut self, parent: u64, name: &str, units: &[CompressedUnit<'_>]) -> u64 {
        let record = self.new_node(parent, name, false);
        let mut runs: Vec<(Option<usize>, usize)> = Vec::new();
        let mut size = 0;
        for unit in units {
            let (allocated, length) = match unit {
                CompressedUnit::Stored(data) => {
                    assert_eq!(data.len(), COMPRESSION_UNIT_CLUSTERS * CLUSTER);
                    let placed = self.allocate(COMPRESSION_UNIT_CLUSTERS, false);
                    self.write_runs(&placed, data);
                    (placed, data.len())
                }
                CompressedUnit::Packed { packed, length } => {
                    let count = packed.len().div_ceil(CLUSTER);
                    assert!(count < COMPRESSION_UNIT_CLUSTERS);
                    let placed = self.allocate(count, false);
                    self.write_runs(&placed, packed);
                    (placed, *length)
                }
                CompressedUnit::Hole => (Vec::new(), COMPRESSION_UNIT_CLUSTERS * CLUSTER),
            };
            let used: usize = allocated.iter().map(|run| run.1).sum();
            runs.extend(allocated);
            let hole = COMPRESSION_UNIT_CLUSTERS - used;
            if hole > 0 {
                match runs.last_mut() {
                    Some((None, count)) => *count += hole,
                    _ => runs.push((None, hole)),
                }
            }
            size += length;
        }
        let node = self.nodes.get_mut(&record).unwrap();
        node.data = Data::NonResident { runs, size };
        node.compressed = true;
        node.compression_unit = 4;
        record
    }

    /// 稀疏文件：`before`、`hole` 个簇的空洞、`after`；`before` 必须按簇对齐。
    pub fn add_sparse(
        &mut self,
        parent: u64,
        name: &str,
        before: &[u8],
        hole: usize,
        after: &[u8],
    ) -> u64 {
        assert_eq!(before.len() % CLUSTER, 0);
        let record = self.new_node(parent, name, false);
        let first = self.allocate(before.len() / CLUSTER, false);
        self.write_runs(&first, before);
        let second = self.allocate(after.len().div_ceil(CLUSTER), false);
        self.write_runs(&second, after);
        let mut runs = first;
        runs.push((None, hole));
        runs.extend(second);
        self.nodes.get_mut(&record).unwrap().data = Data::NonResident {
            runs,
            size: before.len() + hole * CLUSTER + after.len(),
        };
        record
    }

    /// 数据流分两段，第二段放进扩展记录（基本记录里有属性列表）。至少要占 2 个簇。
    pub fn add_split(&mut self, parent: u64, name: &str, data: &[u8]) -> u64 {
        assert!(data.len() > CLUSTER);
        let record = self.add_fragmented(parent, name, data);
        let extension = self.next_record;
        self.next_record += 1;
        self.nodes.insert(
            extension,
            Node {
                in_use: true,
                sequence: 1,
                extension_of: Some(record),
                ..Node::default()
            },
        );
        self.nodes.get_mut(&record).unwrap().extension = Some(extension);
        record
    }

    /// 给文件加一个 DOS 8.3 短名（长名改为 Win32 命名空间，短名是 DOS 命名空间）。
    pub fn add_dos_name(&mut self, record: u64, short: &str) {
        let node = self.nodes.get_mut(&record).unwrap();
        let first = node.names[0].clone();
        node.names.push(Name {
            name: short.to_owned(),
            namespace: 2,
            ..first
        });
    }

    /// 硬链接：同一条记录在另一个目录里的另一个名字。
    pub fn add_link(&mut self, record: u64, parent: u64, name: &str) {
        let parent_sequence = self.nodes[&parent].sequence;
        self.nodes.get_mut(&record).unwrap().names.push(Name {
            parent,
            parent_sequence,
            name: name.to_owned(),
            namespace: 1,
        });
    }

    /// 给文件加一个备用数据流：不超过 600 字节时常驻，否则连续存放。
    pub fn add_stream(&mut self, record: u64, name: &str, data: &[u8]) {
        let value = if data.len() <= 600 {
            Data::Resident(data.to_vec())
        } else {
            let runs = self.allocate(data.len().div_ceil(CLUSTER), false);
            self.write_runs(&runs, data);
            Data::NonResident {
                runs,
                size: data.len(),
            }
        };
        let node = self.nodes.get_mut(&record).unwrap();
        assert!(node.extension.is_none(), "带扩展记录的文件不加备用数据流");
        node.streams.push((name.to_owned(), value));
    }

    pub fn mark_compressed(&mut self, record: u64) {
        self.nodes.get_mut(&record).unwrap().compressed = true;
    }

    pub fn mark_encrypted(&mut self, record: u64) {
        self.nodes.get_mut(&record).unwrap().encrypted = true;
    }

    /// 加上文件属性位（隐藏 0x02、系统 0x04 等）。
    pub fn add_attributes(&mut self, record: u64, bits: u32) {
        self.nodes.get_mut(&record).unwrap().attributes |= bits;
    }

    /// 这个文件不写进上级目录的索引：记录还在用，浏览时却看不到。
    pub fn leave_out_of_index(&mut self, record: u64) {
        self.nodes.get_mut(&record).unwrap().unindexed = true;
    }

    /// 这个目录不写 `$I30` 索引：目录读不出。
    pub fn drop_index(&mut self, dir: u64) {
        self.nodes.get_mut(&dir).unwrap().without_index = true;
    }

    /// 把文件的运行表改成指到卷外（`$Bitmap` 不动）。
    pub fn point_runs_outside(&mut self, record: u64) {
        let node = self.nodes.get_mut(&record).unwrap();
        if let Data::NonResident { runs, .. } = &mut node.data {
            *runs = vec![(Some(TOTAL_CLUSTERS + 100), 1)];
        }
    }

    /// 只带标记的重分析点（内容为空）。
    pub fn mark_reparse(&mut self, record: u64, tag: u32) {
        let mut value = vec![0u8; 8];
        value[0..4].copy_from_slice(&tag.to_le_bytes());
        self.nodes.get_mut(&record).unwrap().reparse = Some(value);
    }

    /// 系统压缩（WOF）的文件：无名数据流是 `size` 长的稀疏流，`packed` 放进 `WofCompressedData` 流，
    /// 重分析点记 WOF 外部信息（提供者 2 = 单个文件）与算法（0 XPRESS4K、1 LZX、2 XPRESS8K、3 XPRESS16K）。
    pub fn add_wof(
        &mut self,
        parent: u64,
        name: &str,
        size: usize,
        algorithm: u32,
        packed: &[u8],
    ) -> u64 {
        let record = self.new_node(parent, name, false);
        self.nodes.get_mut(&record).unwrap().data = Data::NonResident {
            runs: vec![(None, size.div_ceil(CLUSTER))],
            size,
        };
        self.add_stream(record, "WofCompressedData", packed);
        let mut value = vec![0u8; 24];
        for (at, field) in [(0, TAG_WOF), (8, 1), (12, 2), (16, 1), (20, algorithm)] {
            value[at..at + 4].copy_from_slice(&field.to_le_bytes());
        }
        value[4..6].copy_from_slice(&16u16.to_le_bytes());
        self.nodes.get_mut(&record).unwrap().reparse = Some(value);
        record
    }

    /// 删除：记录标为未使用、序号加 1，簇在位图里释放；目录索引在 `finish` 时不再列出它。
    pub fn delete(&mut self, record: u64) {
        let node = self.nodes.get_mut(&record).unwrap();
        node.in_use = false;
        node.sequence = node.sequence.wrapping_add(1);
        let runs = match &node.data {
            Data::NonResident { runs, .. } => runs.clone(),
            _ => Vec::new(),
        };
        if let Some(extension) = node.extension {
            let extension = self.nodes.get_mut(&extension).unwrap();
            extension.in_use = false;
            extension.sequence = extension.sequence.wrapping_add(1);
        }
        for (lcn, count) in runs {
            if let Some(lcn) = lcn {
                for slot in &mut self.bitmap[lcn..lcn + count] {
                    *slot = false;
                }
            }
        }
    }

    /// 像 Windows 删除碎片多到要用属性列表的文件那样：先截成 0 字节（运行表清空、簇释放），再删除。
    pub fn delete_truncating(&mut self, record: u64) {
        self.delete(record);
        self.nodes.get_mut(&record).unwrap().data = Data::NonResident {
            runs: Vec::new(),
            size: 0,
        };
    }

    /// 把已删除的记录 `record` 重用为一个新文件（序号再加 1）。
    pub fn reuse(&mut self, record: u64, parent: u64, name: &str, data: &[u8]) {
        let sequence = self.nodes[&record].sequence.wrapping_add(1);
        let parent_sequence = self.nodes[&parent].sequence;
        self.nodes.insert(
            record,
            Node {
                in_use: true,
                sequence,
                names: vec![Name {
                    parent,
                    parent_sequence,
                    name: name.to_owned(),
                    namespace: 1,
                }],
                ..Node::default()
            },
        );
        self.set_data(record, data, false);
    }

    /// 文件第一个数据簇的簇号。
    pub fn first_lcn(&self, record: u64) -> usize {
        match &self.nodes[&record].data {
            Data::NonResident { runs, .. } => runs[0].0.unwrap(),
            _ => panic!("不是非常驻的文件"),
        }
    }

    fn set_data(&mut self, record: u64, data: &[u8], fragmented: bool) {
        let value = if data.len() <= 600 && !fragmented {
            Data::Resident(data.to_vec())
        } else {
            let runs = self.allocate(data.len().div_ceil(CLUSTER).max(1), fragmented);
            self.write_runs(&runs, data);
            Data::NonResident {
                runs,
                size: data.len(),
            }
        };
        self.nodes.get_mut(&record).unwrap().data = value;
    }

    fn allocate(&mut self, count: usize, fragmented: bool) -> Vec<(Option<usize>, usize)> {
        let mut runs: Vec<(Option<usize>, usize)> = Vec::new();
        for _ in 0..count {
            while self.bitmap[self.next_cluster] {
                self.next_cluster += 1;
            }
            let lcn = self.next_cluster;
            self.bitmap[lcn] = true;
            self.next_cluster += if fragmented { 2 } else { 1 };
            match runs.last_mut() {
                Some((Some(start), length)) if *start + *length == lcn => *length += 1,
                _ => runs.push((Some(lcn), 1)),
            }
        }
        runs
    }

    fn write_runs(&mut self, runs: &[(Option<usize>, usize)], data: &[u8]) {
        let mut written = 0;
        for &(lcn, count) in runs {
            let take = (count * CLUSTER).min(data.len() - written);
            if let Some(lcn) = lcn {
                self.write_clusters(lcn, &data[written..written + take]);
            }
            written += take;
        }
    }

    fn write_clusters(&mut self, lcn: usize, data: &[u8]) {
        self.image[lcn * CLUSTER..lcn * CLUSTER + data.len()].copy_from_slice(data);
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.write_boot();
        // 有日志时多一个系统文件 $LogFile（第 2 条记录）
        let system: Vec<(u64, &str)> = SYSTEM_FILES
            .iter()
            .copied()
            .chain(self.log.is_some().then_some((2, "$LogFile")))
            .collect();
        if let Some(log) = self.log.as_deref().map(log_file) {
            self.write_clusters(LOG_LCN, &log);
        }
        // 目录索引：按在用的子项生成（DOS 名单独一项）
        let mut children: BTreeMap<u64, Vec<(u64, Vec<u8>)>> = BTreeMap::new();
        for (&record, node) in &self.nodes {
            if !node.in_use || node.unindexed {
                continue;
            }
            for name in &node.names {
                if record == ROOT {
                    continue;
                }
                let key = self.file_name_value(record, name);
                let reference = record | u64::from(node.sequence) << 48;
                children
                    .entry(name.parent)
                    .or_default()
                    .push((reference, key));
            }
        }
        for &(number, name) in &system {
            let reference = number | u64::from(system_sequence(number)) << 48;
            let key = self.file_name_value_raw(name, ROOT, 5);
            children.entry(ROOT).or_default().push((reference, key));
        }
        let mut records: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        let numbers: Vec<u64> = self.nodes.keys().copied().collect();
        for record in numbers {
            let node = &self.nodes[&record];
            if node.extension_of.is_some() {
                continue;
            }
            let index = if node.is_dir && node.in_use && !node.without_index {
                let entries = children.remove(&record).unwrap_or_default();
                Some(self.index_attributes(entries))
            } else {
                None
            };
            let (base, extension) = self.record_bytes(record, index);
            records.insert(record, base);
            if let Some((number, bytes)) = extension {
                records.insert(number, bytes);
            }
        }
        let mut record_bitmap = vec![0u8; MFT_RECORDS / 8];
        let in_use = self
            .nodes
            .iter()
            .filter(|(_, node)| node.in_use)
            .map(|(&number, _)| number)
            .chain(system.iter().map(|&(number, _)| number));
        for number in in_use {
            record_bitmap[number as usize / 8] |= 1 << (number % 8);
        }
        self.record_bitmap = record_bitmap;
        for &(number, name) in &system {
            records.insert(number, self.system_record(number, name));
        }
        for (number, bytes) in &records {
            let at = MFT_LCN * CLUSTER + *number as usize * RECORD;
            self.image[at..at + RECORD].copy_from_slice(bytes);
            if *number < 4 {
                let mirror = MIRROR_LCN * CLUSTER + *number as usize * RECORD;
                self.image[mirror..mirror + RECORD].copy_from_slice(bytes);
            }
        }
        let mut bitmap = vec![0u8; TOTAL_CLUSTERS / 8];
        for (cluster, &used) in self.bitmap.iter().enumerate() {
            if used {
                bitmap[cluster / 8] |= 1 << (cluster % 8);
            }
        }
        self.write_clusters(BITMAP_LCN, &bitmap);
        // 备份引导扇区在卷的最后一个扇区（卷长度记的是总扇区数减 1）
        let backup = self.image.len() - SECTOR;
        self.image.copy_within(..SECTOR, backup);
        self.image
    }

    fn write_boot(&mut self) {
        let boot = ntfs_boot(&NtfsBoot {
            bytes_per_sector: SECTOR as u16,
            cluster_size: CLUSTER as u64,
            hidden_sectors: 0,
            total_sectors: (TOTAL_CLUSTERS * CLUSTER / SECTOR - 1) as u64,
            mft_lcn: MFT_LCN as u64,
            mirror_lcn: MIRROR_LCN as u64,
            record_size: RECORD as u64,
            index_size: INDEX_BLOCK as u64,
            serial: 0x1234_5678_9ABC_DEF0,
        });
        self.image[..SECTOR].copy_from_slice(&boot);
    }

    fn file_name_value(&self, record: u64, name: &Name) -> Vec<u8> {
        let node = &self.nodes[&record];
        let parent_sequence = name.parent_sequence;
        let size = match &node.data {
            Data::None => 0,
            Data::Resident(bytes) => bytes.len(),
            Data::NonResident { size, .. } => *size,
        } as u64;
        let mut flags = node.attributes;
        if node.is_dir {
            flags |= 0x1000_0000;
        }
        if node.compressed {
            flags |= 0x0800;
        }
        if node.encrypted {
            flags |= 0x4000;
        }
        if node.reparse.is_some() {
            flags |= 0x0400;
        }
        let reparse_tag = node.reparse.as_ref().map_or(0, |value| {
            u32::from_le_bytes(value[0..4].try_into().unwrap())
        });
        let units: Vec<u16> = name.name.encode_utf16().collect();
        let mut value = vec![0u8; 0x42 + units.len() * 2];
        value[0..8]
            .copy_from_slice(&(name.parent | u64::from(parent_sequence) << 48).to_le_bytes());
        for at in [0x08, 0x10, 0x18, 0x20] {
            value[at..at + 8].copy_from_slice(&SAMPLE_FILETIME.to_le_bytes());
        }
        value[0x28..0x30].copy_from_slice(&size.next_multiple_of(CLUSTER as u64).to_le_bytes());
        value[0x30..0x38].copy_from_slice(&size.to_le_bytes());
        value[0x38..0x3C].copy_from_slice(&flags.to_le_bytes());
        value[0x3C..0x40].copy_from_slice(&reparse_tag.to_le_bytes());
        value[0x40] = units.len() as u8;
        value[0x41] = name.namespace;
        for (index, unit) in units.iter().enumerate() {
            value[0x42 + index * 2..0x44 + index * 2].copy_from_slice(&unit.to_le_bytes());
        }
        value
    }

    fn standard_information(&self, node: &Node) -> Vec<u8> {
        let mut value = vec![0u8; 0x48];
        for at in [0x00, 0x08, 0x10, 0x18] {
            value[at..at + 8].copy_from_slice(&SAMPLE_FILETIME.to_le_bytes());
        }
        let mut attributes = 0x20u32 | node.attributes;
        if node.encrypted {
            attributes |= 0x4000;
        }
        value[0x20..0x24].copy_from_slice(&attributes.to_le_bytes());
        value
    }

    /// 基本记录（以及数据流第二段所在的扩展记录）。
    fn record_bytes(
        &self,
        record: u64,
        index: Option<Vec<Vec<u8>>>,
    ) -> (Vec<u8>, Option<(u64, Vec<u8>)>) {
        let node = &self.nodes[&record];
        let mut attributes = vec![resident(0x10, "", 0, &self.standard_information(node))];
        for (id, name) in node.names.iter().enumerate() {
            attributes.push(resident(
                0x30,
                "",
                1 + id as u16,
                &self.file_name_value(record, name),
            ));
        }
        let data_flags: u16 = if node.compressed {
            0x0001
        } else if node.encrypted {
            0x4000
        } else {
            0
        };
        let mut extension = None;
        match (&node.data, node.extension) {
            (Data::None, _) => {}
            (Data::Resident(bytes), _) => {
                let mut attribute = resident(0x80, "", 10, bytes);
                attribute[0x0C..0x0E].copy_from_slice(&data_flags.to_le_bytes());
                attributes.push(attribute);
            }
            (Data::NonResident { runs, size }, None) => {
                let clusters: usize = runs.iter().map(|run| run.1).sum();
                let mut attribute =
                    non_resident(0x80, "", 10, data_flags, 0, runs, clusters, *size);
                attribute[0x22] = node.compression_unit;
                attributes.push(attribute);
            }
            (Data::NonResident { runs, .. }, Some(number)) if runs.is_empty() => {
                // 删除时被截成 0 字节：扩展记录里只剩一个空的数据流，属性列表只指向它
                let empty = non_resident(0x80, "", 11, data_flags, 0, &[], 0, 0);
                let extension_node = &self.nodes[&number];
                let own = record | u64::from(sequence_when_written(node)) << 48;
                let ext_bytes = record_with(
                    number,
                    extension_node.sequence,
                    extension_node.in_use,
                    false,
                    own,
                    &[empty],
                );
                let ext_ref = number | u64::from(sequence_when_written(extension_node)) << 48;
                let mut list = Vec::new();
                list.extend(list_entry(0x10, 0, own, 0));
                for id in 0..node.names.len() {
                    list.extend(list_entry(0x30, 0, own, 1 + id as u16));
                }
                list.extend(list_entry(0x80, 0, ext_ref, 11));
                attributes.insert(1, resident(0x20, "", 20, &list));
                extension = Some((number, ext_bytes));
            }
            (Data::NonResident { runs, size }, Some(number)) => {
                // 第一段留在基本记录，其余放进扩展记录
                let clusters: usize = runs.iter().map(|run| run.1).sum();
                let (head, tail) = runs.split_at(1);
                let head_clusters = head[0].1;
                let mut first = non_resident(0x80, "", 10, data_flags, 0, head, clusters, *size);
                first[0x18..0x20].copy_from_slice(&((head_clusters - 1) as u64).to_le_bytes());
                attributes.push(first);
                let mut second = non_resident(
                    0x80,
                    "",
                    11,
                    data_flags,
                    head_clusters,
                    tail,
                    clusters,
                    *size,
                );
                second[0x10..0x18].copy_from_slice(&(head_clusters as u64).to_le_bytes());
                second[0x18..0x20].copy_from_slice(&((clusters - 1) as u64).to_le_bytes());
                let extension_node = &self.nodes[&number];
                let ext_bytes = record_with(
                    number,
                    extension_node.sequence,
                    extension_node.in_use,
                    false,
                    record | u64::from(sequence_when_written(node)) << 48,
                    &[second],
                );
                // 属性列表：$STANDARD_INFORMATION、$FILE_NAME、$DATA 第一段在基本记录，第二段在扩展记录
                let own = record | u64::from(sequence_when_written(node)) << 48;
                let ext_ref = number | u64::from(sequence_when_written(extension_node)) << 48;
                let mut list = Vec::new();
                list.extend(list_entry(0x10, 0, own, 0));
                for id in 0..node.names.len() {
                    list.extend(list_entry(0x30, 0, own, 1 + id as u16));
                }
                list.extend(list_entry(0x80, 0, own, 10));
                list.extend(list_entry(0x80, head_clusters as u64, ext_ref, 11));
                attributes.insert(1, resident(0x20, "", 20, &list));
                extension = Some((number, ext_bytes));
            }
        }
        for (index, (name, data)) in node.streams.iter().enumerate() {
            let id = 30 + index as u16;
            attributes.push(match data {
                Data::Resident(bytes) => resident(0x80, name, id, bytes),
                Data::NonResident { runs, size } => {
                    let clusters: usize = runs.iter().map(|run| run.1).sum();
                    non_resident(0x80, name, id, 0, 0, runs, clusters, *size)
                }
                Data::None => continue,
            });
        }
        if let Some(value) = &node.reparse {
            attributes.push(resident(0xC0, "", 12, value));
        }
        if let Some(index) = index {
            attributes.extend(index);
        }
        let base = record_with(
            record,
            node.sequence,
            node.in_use,
            node.is_dir,
            0,
            &attributes,
        );
        (base, extension)
    }

    fn index_attributes(&mut self, entries: Vec<(u64, Vec<u8>)>) -> Vec<Vec<u8>> {
        let encoded: Vec<Vec<u8>> = entries
            .iter()
            .map(|(reference, key)| index_entry(*reference, key, 0))
            .collect();
        let total: usize = encoded.iter().map(Vec::len).sum();
        let mut root = vec![0u8; 0x10];
        root[0..4].copy_from_slice(&0x30u32.to_le_bytes());
        root[4..8].copy_from_slice(&1u32.to_le_bytes());
        root[8..12].copy_from_slice(&(INDEX_BLOCK as u32).to_le_bytes());
        root[12] = 1;
        if total <= ROOT_INDEX_LIMIT {
            let mut node = encoded.concat();
            node.extend(index_entry(0, &[], 0x02));
            root.extend(node_header(0x10, node.len(), 0));
            root.extend(node);
            return vec![resident(0x90, "$I30", 13, &root)];
        }
        // 项放进索引块，每块装满为止；索引根里只有一个指向 0 号块的结束项
        let mut blocks: Vec<Vec<u8>> = vec![Vec::new()];
        for entry in encoded {
            if blocks.last().unwrap().len() + entry.len() + 0x40 + 0x20 > INDEX_BLOCK {
                blocks.push(Vec::new());
            }
            blocks.last_mut().unwrap().extend(entry);
        }
        // 再加一个在位图里标为未用的块，里面的旧项不该被列出
        let mut stale = Vec::new();
        stale.extend(index_entry(999 | 1 << 48, &stale_key(), 0));
        blocks.push(stale);
        let block_count = blocks.len();
        let runs = self.allocate(block_count, false);
        for (number, entries) in blocks.into_iter().enumerate() {
            let mut node = entries;
            node.extend(index_entry(0, &[], 0x02));
            let mut block = vec![0u8; INDEX_BLOCK];
            block[0..4].copy_from_slice(b"INDX");
            block[4..6].copy_from_slice(&0x28u16.to_le_bytes());
            block[6..8].copy_from_slice(&((INDEX_BLOCK / SECTOR + 1) as u16).to_le_bytes());
            block[0x10..0x18].copy_from_slice(&(number as u64).to_le_bytes());
            let header = node_header(0x28, node.len(), INDEX_BLOCK - 0x18);
            block[0x18..0x28].copy_from_slice(&header);
            block[0x40..0x40 + node.len()].copy_from_slice(&node);
            apply_usa(&mut block, 0x28);
            let mut offset = 0;
            let mut target = 0;
            for &(lcn, count) in &runs {
                if number < offset + count {
                    target = lcn.unwrap() + (number - offset);
                    break;
                }
                offset += count;
            }
            self.write_clusters(target, &block);
        }
        // 结束项带子节点时长 0x18：前 8 字节之后、末尾 8 字节是子节点的块号
        let mut last = vec![0u8; 0x18];
        last[8..10].copy_from_slice(&0x18u16.to_le_bytes());
        last[12..14].copy_from_slice(&0x03u16.to_le_bytes());
        root.extend(node_header(0x10, last.len(), last.len()));
        root[0x1C] = 1;
        root.extend(last);
        let mut bitmap = vec![0u8; 8];
        for number in 0..block_count - 1 {
            bitmap[number / 8] |= 1 << (number % 8);
        }
        vec![
            resident(0x90, "$I30", 13, &root),
            non_resident(
                0xA0,
                "$I30",
                14,
                0,
                0,
                &runs,
                block_count,
                block_count * INDEX_BLOCK,
            ),
            resident(0xB0, "$I30", 15, &bitmap),
        ]
    }

    fn system_record(&self, number: u64, name: &str) -> Vec<u8> {
        let node = Node {
            in_use: true,
            sequence: system_sequence(number),
            ..Node::default()
        };
        let file_name = self.file_name_value_raw(name, ROOT, 5);
        let mut attributes = vec![
            resident(0x10, "", 0, &self.standard_information(&node)),
            resident(0x30, "", 1, &file_name),
        ];
        match number {
            0 => {
                attributes.push(non_resident(
                    0x80,
                    "",
                    2,
                    0,
                    0,
                    &[(Some(MFT_LCN), MFT_RECORDS * RECORD / CLUSTER)],
                    MFT_RECORDS * RECORD / CLUSTER,
                    MFT_RECORDS * RECORD,
                ));
                attributes.push(resident(0xB0, "", 3, &self.record_bitmap));
            }
            3 => {
                let label: Vec<u8> = self
                    .label
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect();
                attributes.push(resident(0x60, "", 2, &label));
            }
            1 => attributes.push(non_resident(
                0x80,
                "",
                2,
                0,
                0,
                &[(Some(MIRROR_LCN), MIRROR_CLUSTERS)],
                MIRROR_CLUSTERS,
                4 * RECORD,
            )),
            2 => attributes.push(non_resident(
                0x80,
                "",
                2,
                0,
                0,
                &[(Some(LOG_LCN), LOG_CLUSTERS)],
                LOG_CLUSTERS,
                LOG_CLUSTERS * CLUSTER,
            )),
            6 => attributes.push(non_resident(
                0x80,
                "",
                2,
                0,
                0,
                &[(Some(BITMAP_LCN), 1)],
                1,
                TOTAL_CLUSTERS / 8,
            )),
            7 => attributes.push(non_resident(
                0x80,
                "",
                2,
                0,
                0,
                &[(Some(0), BOOT_CLUSTERS)],
                BOOT_CLUSTERS,
                BOOT_CLUSTERS * CLUSTER,
            )),
            // 没有坏簇：`$Bad` 是盖住整个卷的稀疏流
            8 => attributes.push(non_resident(
                0x80,
                "$Bad",
                2,
                0,
                0,
                &[(None, VOLUME_CLUSTERS)],
                VOLUME_CLUSTERS,
                VOLUME_CLUSTERS * CLUSTER,
            )),
            _ => {}
        }
        record_with(number, node.sequence, true, false, 0, &attributes)
    }

    fn file_name_value_raw(&self, name: &str, parent: u64, parent_sequence: u16) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut value = vec![0u8; 0x42 + units.len() * 2];
        value[0..8].copy_from_slice(&(parent | u64::from(parent_sequence) << 48).to_le_bytes());
        value[0x40] = units.len() as u8;
        value[0x41] = 3;
        for (index, unit) in units.iter().enumerate() {
            value[0x42 + index * 2..0x44 + index * 2].copy_from_slice(&unit.to_le_bytes());
        }
        value
    }
}

fn system_sequence(number: u64) -> u16 {
    number.max(1) as u16
}

/// 写进扩展记录、属性列表里的序号：删除后序号已加 1，记的是删除前的值。
fn sequence_when_written(node: &Node) -> u16 {
    if node.in_use {
        node.sequence
    } else {
        node.sequence.wrapping_sub(1)
    }
}

fn stale_key() -> Vec<u8> {
    let units: Vec<u16> = "stale.txt".encode_utf16().collect();
    let mut value = vec![0u8; 0x42 + units.len() * 2];
    value[0..8].copy_from_slice(&(ROOT | 5 << 48).to_le_bytes());
    value[0x40] = units.len() as u8;
    value[0x41] = 1;
    for (index, unit) in units.iter().enumerate() {
        value[0x42 + index * 2..0x44 + index * 2].copy_from_slice(&unit.to_le_bytes());
    }
    value
}

fn list_entry(kind: u32, vcn: u64, reference: u64, id: u16) -> Vec<u8> {
    let mut entry = vec![0u8; 0x20];
    entry[0..4].copy_from_slice(&kind.to_le_bytes());
    entry[4..6].copy_from_slice(&0x20u16.to_le_bytes());
    entry[7] = 0x1A;
    entry[8..16].copy_from_slice(&vcn.to_le_bytes());
    entry[16..24].copy_from_slice(&reference.to_le_bytes());
    entry[24..26].copy_from_slice(&id.to_le_bytes());
    entry
}

fn index_entry(reference: u64, key: &[u8], flags: u16) -> Vec<u8> {
    let length = (0x10 + key.len()).next_multiple_of(8);
    let mut entry = vec![0u8; length];
    entry[0..8].copy_from_slice(&reference.to_le_bytes());
    entry[8..10].copy_from_slice(&(length as u16).to_le_bytes());
    entry[10..12].copy_from_slice(&(key.len() as u16).to_le_bytes());
    entry[12..14].copy_from_slice(&flags.to_le_bytes());
    entry[0x10..0x10 + key.len()].copy_from_slice(key);
    entry
}

fn node_header(first: usize, entries: usize, allocated: usize) -> Vec<u8> {
    let mut header = vec![0u8; 0x10];
    header[0..4].copy_from_slice(&(first as u32).to_le_bytes());
    header[4..8].copy_from_slice(&((first + entries) as u32).to_le_bytes());
    header[8..12].copy_from_slice(&((first + entries).max(allocated) as u32).to_le_bytes());
    header
}

fn resident(kind: u32, name: &str, id: u16, value: &[u8]) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let value_offset = (0x18 + units.len() * 2).next_multiple_of(8);
    let length = (value_offset + value.len()).next_multiple_of(8);
    let mut attribute = vec![0u8; length];
    attribute[0..4].copy_from_slice(&kind.to_le_bytes());
    attribute[4..8].copy_from_slice(&(length as u32).to_le_bytes());
    attribute[9] = units.len() as u8;
    attribute[0x0A..0x0C].copy_from_slice(&0x18u16.to_le_bytes());
    attribute[0x0E..0x10].copy_from_slice(&id.to_le_bytes());
    attribute[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
    attribute[0x14..0x16].copy_from_slice(&(value_offset as u16).to_le_bytes());
    for (index, unit) in units.iter().enumerate() {
        attribute[0x18 + index * 2..0x1A + index * 2].copy_from_slice(&unit.to_le_bytes());
    }
    attribute[value_offset..value_offset + value.len()].copy_from_slice(value);
    attribute
}

#[allow(clippy::too_many_arguments)]
fn non_resident(
    kind: u32,
    name: &str,
    id: u16,
    flags: u16,
    first_vcn: usize,
    runs: &[(Option<usize>, usize)],
    total_clusters: usize,
    size: usize,
) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let runs_offset = (0x40 + units.len() * 2).next_multiple_of(8);
    let encoded = encode_runs(runs);
    let length = (runs_offset + encoded.len()).next_multiple_of(8);
    let mut attribute = vec![0u8; length];
    attribute[0..4].copy_from_slice(&kind.to_le_bytes());
    attribute[4..8].copy_from_slice(&(length as u32).to_le_bytes());
    attribute[8] = 1;
    attribute[9] = units.len() as u8;
    attribute[0x0A..0x0C].copy_from_slice(&0x40u16.to_le_bytes());
    attribute[0x0C..0x0E].copy_from_slice(&flags.to_le_bytes());
    attribute[0x0E..0x10].copy_from_slice(&id.to_le_bytes());
    let clusters: usize = runs.iter().map(|run| run.1).sum();
    attribute[0x10..0x18].copy_from_slice(&(first_vcn as u64).to_le_bytes());
    attribute[0x18..0x20].copy_from_slice(
        &((first_vcn + clusters) as u64)
            .wrapping_sub(1)
            .to_le_bytes(),
    );
    attribute[0x20..0x22].copy_from_slice(&(runs_offset as u16).to_le_bytes());
    attribute[0x28..0x30].copy_from_slice(&((total_clusters * CLUSTER) as u64).to_le_bytes());
    attribute[0x30..0x38].copy_from_slice(&(size as u64).to_le_bytes());
    attribute[0x38..0x40].copy_from_slice(&(size as u64).to_le_bytes());
    for (index, unit) in units.iter().enumerate() {
        attribute[0x40 + index * 2..0x42 + index * 2].copy_from_slice(&unit.to_le_bytes());
    }
    attribute[runs_offset..runs_offset + encoded.len()].copy_from_slice(&encoded);
    attribute
}

fn encode_runs(runs: &[(Option<usize>, usize)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut previous = 0i64;
    for &(lcn, count) in runs {
        let length = minimal_unsigned(count as u64);
        match lcn {
            Some(lcn) => {
                let delta = lcn as i64 - previous;
                previous = lcn as i64;
                let offset = minimal_signed(delta);
                out.push((offset.len() as u8) << 4 | length.len() as u8);
                out.extend(length);
                out.extend(offset);
            }
            None => {
                out.push(length.len() as u8);
                out.extend(length);
            }
        }
    }
    out.push(0);
    out
}

fn minimal_unsigned(value: u64) -> Vec<u8> {
    let bytes = value.to_le_bytes();
    let used = 8 - value.leading_zeros() as usize / 8;
    bytes[..used.max(1)].to_vec()
}

fn minimal_signed(value: i64) -> Vec<u8> {
    let bytes = value.to_le_bytes();
    for used in 1..=8 {
        let shift = 64 - used * 8;
        if (value << shift) >> shift == value {
            return bytes[..used].to_vec();
        }
    }
    bytes.to_vec()
}

/// 拼出一条记录并做更新序列处理。
fn record_with(
    number: u64,
    sequence: u16,
    in_use: bool,
    is_dir: bool,
    base: u64,
    attributes: &[Vec<u8>],
) -> Vec<u8> {
    let mut record = record_image(number, sequence, in_use, is_dir, base, attributes);
    apply_usa(&mut record, 0x30);
    record
}

/// 拼出一条记录在内存里的样子（不做更新序列处理，`$LogFile` 里记的就是这样）。
fn record_image(
    number: u64,
    sequence: u16,
    in_use: bool,
    is_dir: bool,
    base: u64,
    attributes: &[Vec<u8>],
) -> Vec<u8> {
    let mut record = vec![0u8; RECORD];
    record[0..4].copy_from_slice(b"FILE");
    record[4..6].copy_from_slice(&0x30u16.to_le_bytes());
    record[6..8].copy_from_slice(&((RECORD / SECTOR + 1) as u16).to_le_bytes());
    record[0x10..0x12].copy_from_slice(&sequence.to_le_bytes());
    record[0x12..0x14].copy_from_slice(&1u16.to_le_bytes());
    record[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
    let flags = u16::from(in_use) | if is_dir { 0x02 } else { 0 };
    record[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
    record[0x1C..0x20].copy_from_slice(&(RECORD as u32).to_le_bytes());
    record[0x20..0x28].copy_from_slice(&base.to_le_bytes());
    record[0x2C..0x30].copy_from_slice(&(number as u32).to_le_bytes());
    let mut offset = 0x38;
    for attribute in attributes {
        record[offset..offset + attribute.len()].copy_from_slice(attribute);
        offset += attribute.len();
    }
    record[offset..offset + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    offset += 8;
    record[0x18..0x1C].copy_from_slice(&(offset as u32).to_le_bytes());
    record
}

/// 按记下的操作写出整个 `$LogFile`：两页重启区，第 4 页起依次放日志记录（放不下的接到下一页的数据区），
/// 第 2 页放第 4 页的副本（缓冲页，LSN 指的位置不是它，不该被算进去）。
fn log_file(ops: &[LogOp]) -> Vec<u8> {
    let mut log = vec![0u8; LOG_CLUSTERS * CLUSTER];
    let lsn_of = |offset: usize| (1u64 << (64 - LOG_SEQUENCE_BITS)) | (offset as u64 / 8);
    // 每一页：最后一条开始或经过这一页的记录、最后一条在这一页结束的记录、下一条记录的偏移
    let mut pages: BTreeMap<usize, (u64, u64, usize)> = BTreeMap::new();
    let mut position = LOG_FIRST_RECORD_PAGE * LOG_PAGE + LOG_DATA_OFFSET;
    let mut previous = 0u64;
    for op in ops {
        if position % LOG_PAGE + 0x30 > LOG_PAGE {
            position = position.next_multiple_of(LOG_PAGE) + LOG_DATA_OFFSET;
        }
        let lsn = lsn_of(position);
        let body = op.client_bytes();
        let mut header = vec![0u8; 0x30];
        header[0x00..0x08].copy_from_slice(&lsn.to_le_bytes());
        header[0x08..0x10].copy_from_slice(&previous.to_le_bytes());
        header[0x18..0x1C].copy_from_slice(&(body.len() as u32).to_le_bytes());
        header[0x20..0x24].copy_from_slice(&1u32.to_le_bytes());
        header[0x24..0x28].copy_from_slice(&1u32.to_le_bytes());
        let crosses = position % LOG_PAGE + 0x30 + body.len() > LOG_PAGE;
        header[0x28..0x2A].copy_from_slice(&u16::from(crosses).to_le_bytes());
        log[position..position + 0x30].copy_from_slice(&header);
        let mut at = position + 0x30;
        let mut written = 0;
        loop {
            let page = at / LOG_PAGE;
            pages.entry(page).or_insert((0, 0, LOG_DATA_OFFSET)).0 = lsn;
            let take = (body.len() - written).min((page + 1) * LOG_PAGE - at);
            log[at..at + take].copy_from_slice(&body[written..written + take]);
            written += take;
            at += take;
            if written == body.len() {
                let meta = pages.get_mut(&page).unwrap();
                meta.1 = lsn;
                meta.2 = (at % LOG_PAGE).next_multiple_of(8);
                break;
            }
            at = (page + 1) * LOG_PAGE + LOG_DATA_OFFSET;
            assert!(at < log.len(), "样本的日志写满了");
        }
        position = at.next_multiple_of(8);
        previous = lsn;
    }
    for (&page, &(last, last_end, next)) in &pages {
        let block = &mut log[page * LOG_PAGE..(page + 1) * LOG_PAGE];
        block[0..4].copy_from_slice(b"RCRD");
        block[4..6].copy_from_slice(&0x28u16.to_le_bytes());
        block[6..8].copy_from_slice(&((LOG_PAGE / SECTOR + 1) as u16).to_le_bytes());
        block[0x08..0x10].copy_from_slice(&last.to_le_bytes());
        block[0x14..0x16].copy_from_slice(&1u16.to_le_bytes());
        block[0x16..0x18].copy_from_slice(&1u16.to_le_bytes());
        block[0x18..0x1A].copy_from_slice(&(next as u16).to_le_bytes());
        block[0x20..0x28].copy_from_slice(&last_end.to_le_bytes());
        apply_usa(block, 0x28);
    }
    // 缓冲页：第一页日志记录的副本
    let first = LOG_FIRST_RECORD_PAGE * LOG_PAGE;
    log.copy_within(first..first + LOG_PAGE, 2 * LOG_PAGE);
    for copy in 0..2 {
        let page = &mut log[copy * LOG_PAGE..(copy + 1) * LOG_PAGE];
        page[0..4].copy_from_slice(b"RSTR");
        page[4..6].copy_from_slice(&0x1Eu16.to_le_bytes());
        page[6..8].copy_from_slice(&((LOG_PAGE / SECTOR + 1) as u16).to_le_bytes());
        page[0x10..0x14].copy_from_slice(&(LOG_PAGE as u32).to_le_bytes());
        page[0x14..0x18].copy_from_slice(&(LOG_PAGE as u32).to_le_bytes());
        page[0x18..0x1A].copy_from_slice(&0x30u16.to_le_bytes());
        page[0x1A..0x1C].copy_from_slice(&1u16.to_le_bytes());
        page[0x1C..0x1E].copy_from_slice(&1u16.to_le_bytes());
        let area = 0x30;
        page[area..area + 8].copy_from_slice(&previous.to_le_bytes());
        page[area + 0x08..area + 0x0A].copy_from_slice(&1u16.to_le_bytes());
        page[area + 0x0A..area + 0x0C].copy_from_slice(&0xFFFFu16.to_le_bytes());
        page[area + 0x10..area + 0x14].copy_from_slice(&LOG_SEQUENCE_BITS.to_le_bytes());
        page[area + 0x14..area + 0x16].copy_from_slice(&0xE0u16.to_le_bytes());
        page[area + 0x16..area + 0x18].copy_from_slice(&0x40u16.to_le_bytes());
        page[area + 0x18..area + 0x20]
            .copy_from_slice(&((LOG_CLUSTERS * CLUSTER) as u64).to_le_bytes());
        page[area + 0x24..area + 0x26].copy_from_slice(&0x30u16.to_le_bytes());
        page[area + 0x26..area + 0x28].copy_from_slice(&(LOG_DATA_OFFSET as u16).to_le_bytes());
        apply_usa(page, 0x1E);
    }
    log
}

/// 写盘前的更新序列处理：每个扇区最后两个字节存进数组，换成更新序列号。
fn apply_usa(block: &mut [u8], offset: usize) {
    let usn = 0x0001u16;
    block[offset..offset + 2].copy_from_slice(&usn.to_le_bytes());
    for index in 0..block.len() / SECTOR {
        let tail = (index + 1) * SECTOR - 2;
        let original = [block[tail], block[tail + 1]];
        block[offset + 2 + index * 2..offset + 4 + index * 2].copy_from_slice(&original);
        block[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
    }
}
