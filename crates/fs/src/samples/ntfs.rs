//! NTFS 测试样本：按公开的结构资料拼出的小卷（8 MiB、4 KiB 一簇、1 KiB 一条 MFT 记录）。
//! 能建目录、写常驻或非常驻的文件（连续、碎片、带稀疏空洞、指定位置）、硬链接、DOS 短名、
//! 数据流放进扩展记录（属性列表）、压缩 / 加密 / 系统压缩标记，删除文件与目录、重用记录。
//! 目录索引在 `finish` 时按在用的子项生成，项多时放进 INDEX_ALLOCATION。

use std::collections::BTreeMap;

const SECTOR: usize = 512;
const CLUSTER: usize = 4096;
const RECORD: usize = 1024;
const TOTAL_CLUSTERS: usize = 2048;
const MFT_LCN: usize = 4;
const MFT_RECORDS: usize = 128;
const MIRROR_LCN: usize = 40;
const BITMAP_LCN: usize = 44;
/// 文件数据从这个簇起分配。
const DATA_LCN: usize = 64;
const INDEX_BLOCK: usize = 4096;
/// 索引根里最多放这么多字节的项，再多就放进索引块。
const ROOT_INDEX_LIMIT: usize = 600;
/// 2024-03-05 13:45:30 UTC，与 FAT 样本的时间相同。
const SAMPLE_FILETIME: u64 = 116_444_736_000_000_000 + 1_709_646_330 * 10_000_000;

pub const ROOT: u64 = 5;
pub const TAG_WOF: u32 = 0x8000_0017;

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
    encrypted: bool,
    reparse_tag: Option<u32>,
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
}

impl Default for NtfsImage {
    fn default() -> Self {
        Self::new()
    }
}

impl NtfsImage {
    pub fn new() -> Self {
        let mut bitmap = vec![false; TOTAL_CLUSTERS];
        // 引导区、MFT、MFT 镜像、位图与预留区都算已占用
        for slot in bitmap.iter_mut().take(DATA_LCN) {
            *slot = true;
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
        }
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

    pub fn mark_compressed(&mut self, record: u64) {
        self.nodes.get_mut(&record).unwrap().compressed = true;
    }

    pub fn mark_encrypted(&mut self, record: u64) {
        self.nodes.get_mut(&record).unwrap().encrypted = true;
    }

    pub fn mark_reparse(&mut self, record: u64, tag: u32) {
        self.nodes.get_mut(&record).unwrap().reparse_tag = Some(tag);
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
        // 目录索引：按在用的子项生成（DOS 名单独一项）
        let mut children: BTreeMap<u64, Vec<(u64, Vec<u8>)>> = BTreeMap::new();
        for (&record, node) in &self.nodes {
            if !node.in_use {
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
        let mut records: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        let numbers: Vec<u64> = self.nodes.keys().copied().collect();
        for record in numbers {
            let node = &self.nodes[&record];
            if node.extension_of.is_some() {
                continue;
            }
            let index = if node.is_dir && node.in_use {
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
        records.insert(0, self.system_record(0, "$MFT"));
        records.insert(3, self.system_record(3, "$Volume"));
        records.insert(6, self.system_record(6, "$Bitmap"));
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
        self.image
    }

    fn write_boot(&mut self) {
        let boot = &mut self.image[..SECTOR];
        boot[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
        boot[3..11].copy_from_slice(b"NTFS    ");
        boot[0x0B..0x0D].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        boot[0x0D] = (CLUSTER / SECTOR) as u8;
        boot[0x15] = 0xF8;
        boot[0x28..0x30]
            .copy_from_slice(&((TOTAL_CLUSTERS * CLUSTER / SECTOR - 1) as u64).to_le_bytes());
        boot[0x30..0x38].copy_from_slice(&(MFT_LCN as u64).to_le_bytes());
        boot[0x38..0x40].copy_from_slice(&(MIRROR_LCN as u64).to_le_bytes());
        boot[0x40] = 0xF6; // 2^10 = 1024 字节一条记录
        boot[0x44] = 0x01; // 一个索引块一簇
        boot[0x48..0x50].copy_from_slice(&0x1234_5678_9ABC_DEF0u64.to_le_bytes());
        boot[510] = 0x55;
        boot[511] = 0xAA;
    }

    fn file_name_value(&self, record: u64, name: &Name) -> Vec<u8> {
        let node = &self.nodes[&record];
        let parent_sequence = name.parent_sequence;
        let size = match &node.data {
            Data::None => 0,
            Data::Resident(bytes) => bytes.len(),
            Data::NonResident { size, .. } => *size,
        } as u64;
        let mut flags = 0u32;
        if node.is_dir {
            flags |= 0x1000_0000;
        }
        if node.compressed {
            flags |= 0x0800;
        }
        if node.encrypted {
            flags |= 0x4000;
        }
        if node.reparse_tag.is_some() {
            flags |= 0x0400;
        }
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
        value[0x3C..0x40].copy_from_slice(&node.reparse_tag.unwrap_or(0).to_le_bytes());
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
        let mut attributes = 0x20u32;
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
                attributes.push(non_resident(
                    0x80, "", 10, data_flags, 0, runs, clusters, *size,
                ));
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
        if let Some(tag) = node.reparse_tag {
            let mut value = vec![0u8; 8];
            value[0..4].copy_from_slice(&tag.to_le_bytes());
            attributes.push(resident(0xC0, "", 12, &value));
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
            sequence: number.max(1) as u16,
            ..Node::default()
        };
        let file_name = self.file_name_value_raw(name, ROOT, 5);
        let mut attributes = vec![
            resident(0x10, "", 0, &self.standard_information(&node)),
            resident(0x30, "", 1, &file_name),
        ];
        match number {
            0 => attributes.push(non_resident(
                0x80,
                "",
                2,
                0,
                0,
                &[(Some(MFT_LCN), MFT_RECORDS * RECORD / CLUSTER)],
                MFT_RECORDS * RECORD / CLUSTER,
                MFT_RECORDS * RECORD,
            )),
            3 => {
                let label: Vec<u8> = self
                    .label
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect();
                attributes.push(resident(0x60, "", 2, &label));
            }
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
    apply_usa(&mut record, 0x30);
    record
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
