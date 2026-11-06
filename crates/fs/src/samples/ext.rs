//! ext 测试样本：按公开的 ext4 磁盘格式文档拼出的小卷。能建目录、写文件（连续、碎片、指定位置、带稀疏空洞、
//! inline data）、删除文件与空目录、在删除的文件原来的位置写新文件；新文件取最小的空闲 inode，删除后再建文件会重用它。
//!
//! 三种卷：
//! - [`ExtFlavor::Ext2`]：1 KiB 块、两个块组（1 号块组开头有备份超级块）、128 字节 inode、旧式块映射（直接、一级、
//!   二级间接块），删除时留着长度与块指针；
//! - [`ExtFlavor::Ext4`]：1 KiB 块、两个块组、256 字节 inode（纳秒时间与创建时间）、extent（段多时放进一个叶子块）、
//!   64 位描述符、metadata_csum（超级块与块组描述符的 CRC-32C，目录块末尾留校验项）、inline data；删除时清空长度与 extent；
//! - [`ExtFlavor::Ext4Large`]：4 KiB 块、一个块组（超级块在 0 号块里的第 1024 字节处），描述符用旧式的 CRC-16 校验，其余同 ext4。
//!
//! inode、目录块与位图本身的校验和不写（读的一方不看）。

use std::collections::BTreeMap;

use yisi_part::crc32::{crc16, crc32c};

use super::Layout;

/// 样本里的时间：2024-03-05 13:45:30 UTC，与其他样本相同。
pub const EXT_SAMPLE_SECONDS: i64 = 1_709_646_330;
/// ext4 样本的修改时间另记的纳秒。
pub const EXT_SAMPLE_NANOS: u32 = 123_456_789;
/// 根目录的 inode。
pub const EXT_ROOT: u32 = 2;

const LOST_FOUND: u32 = 11;
const FIRST_INODE: u32 = 11;
const EXTENT_MAGIC: u16 = 0xF30A;
const S_IFDIR: u16 = 0x4000;
const S_IFREG: u16 = 0x8000;
const S_IFLNK: u16 = 0xA000;
const FLAG_EXTENTS: u32 = 0x0008_0000;
const FLAG_INLINE_DATA: u32 = 0x1000_0000;
const COMPAT_HAS_JOURNAL: u32 = 0x0004;
const COMPAT_EXT_ATTR: u32 = 0x0008;
const INCOMPAT_FILETYPE: u32 = 0x0002;
const INCOMPAT_EXTENTS: u32 = 0x0040;
const INCOMPAT_64BIT: u32 = 0x0080;
const INCOMPAT_FLEX_BG: u32 = 0x0200;
const INCOMPAT_INLINE_DATA: u32 = 0x8000;
const RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
const RO_COMPAT_LARGE_FILE: u32 = 0x0002;
const RO_COMPAT_GDT_CSUM: u32 = 0x0010;
const RO_COMPAT_EXTRA_ISIZE: u32 = 0x0040;
const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
/// 256 字节 inode 的扩展部分长度，以及 inode 里扩展属性区的起点。
const EXTRA_ISIZE: usize = 32;
const XATTR_AT: usize = 128 + EXTRA_ISIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtFlavor {
    Ext2,
    Ext4,
    Ext4Large,
}

#[derive(Debug, Clone, Default)]
struct Node {
    mode: u16,
    links: u16,
    size: u64,
    flags: u32,
    generation: u32,
    dtime: u32,
    /// 修改时间：秒（可以为负，或超过 2038 年）与纳秒。
    modified: (i64, u32),
    /// `i_block` 的 60 字节（保留的 inode 为空）。
    i_block: Vec<u8>,
    /// 数据块，按逻辑块号；`None` 是空洞。
    data: Vec<Option<u32>>,
    /// 间接块与 extent 叶子块。
    meta: Vec<u32>,
    /// inline data 放不进 `i_block` 的部分。
    inline_rest: Option<Vec<u8>>,
}

pub struct ExtImage {
    flavor: ExtFlavor,
    image: Vec<u8>,
    block_size: usize,
    blocks: u32,
    first_data_block: u32,
    blocks_per_group: u32,
    groups: u32,
    inodes_per_group: u32,
    inode_size: usize,
    /// 已分配的块（按块号）。
    used: Vec<bool>,
    nodes: BTreeMap<u32, Node>,
    next_block: u32,
    next_generation: u32,
    label: String,
}

impl ExtImage {
    pub fn new(flavor: ExtFlavor) -> Self {
        let (block_size, blocks, blocks_per_group, inodes_per_group, inode_size) = match flavor {
            ExtFlavor::Ext2 => (1024usize, 16_384u32, 8192u32, 256u32, 128usize),
            ExtFlavor::Ext4 => (1024, 16_384, 8192, 256, 256),
            ExtFlavor::Ext4Large => (4096, 1024, 32_768, 128, 256),
        };
        let first_data_block = u32::from(block_size == 1024);
        let groups = (blocks - first_data_block).div_ceil(blocks_per_group);
        let mut sample = Self {
            flavor,
            image: vec![0u8; blocks as usize * block_size],
            block_size,
            blocks,
            first_data_block,
            blocks_per_group,
            groups,
            inodes_per_group,
            inode_size,
            used: vec![false; blocks as usize],
            nodes: BTreeMap::new(),
            next_block: 0,
            next_generation: 1000,
            label: "ext sample".to_owned(),
        };
        // 引导块（1 KiB 块时的 0 号块）与各块组的超级块、描述符表、位图、inode 表
        for block in 0..first_data_block {
            sample.used[block as usize] = true;
        }
        for group in 0..groups {
            let (start, end) = sample.group_metadata(group);
            for block in start..end {
                sample.used[block as usize] = true;
            }
        }
        sample.next_block = sample.group_metadata(0).1;
        // 1 到 10 号是保留的 inode；根目录与 lost+found
        for number in 1..FIRST_INODE {
            if number != EXT_ROOT {
                sample.nodes.insert(number, Node::default());
            }
        }
        sample.new_dir_node(EXT_ROOT, EXT_ROOT);
        sample.nodes.get_mut(&EXT_ROOT).unwrap().generation = 0;
        sample.new_dir_node(LOST_FOUND, EXT_ROOT);
        sample.add_entry(EXT_ROOT, LOST_FOUND, "lost+found", 2);
        sample.nodes.get_mut(&EXT_ROOT).unwrap().links += 1;
        sample
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn set_label(&mut self, label: &str) {
        self.label = label.to_owned();
    }

    fn extents(&self) -> bool {
        self.flavor != ExtFlavor::Ext2
    }

    fn metadata_csum(&self) -> bool {
        self.flavor == ExtFlavor::Ext4
    }

    fn desc_size(&self) -> usize {
        if self.extents() { 64 } else { 32 }
    }

    fn has_super(group: u32) -> bool {
        group <= 1
            || [3u32, 5, 7].iter().any(|&base| {
                let mut power = base;
                while power < group {
                    power *= base;
                }
                power == group
            })
    }

    fn group_start(&self, group: u32) -> u32 {
        self.first_data_block + group * self.blocks_per_group
    }

    /// 块组开头的元数据块：超级块与描述符表（有备份的块组）、块位图、inode 位图、inode 表。
    fn group_metadata(&self, group: u32) -> (u32, u32) {
        let start = self.group_start(group);
        let table_blocks =
            (self.inodes_per_group as usize * self.inode_size / self.block_size) as u32;
        let header = if Self::has_super(group) { 2 } else { 0 };
        (start, start + header + 2 + table_blocks)
    }

    fn block_bitmap(&self, group: u32) -> u32 {
        let start = self.group_start(group);
        start + if Self::has_super(group) { 2 } else { 0 }
    }

    fn inode_table(&self, group: u32) -> u32 {
        self.block_bitmap(group) + 2
    }

    fn block_mut(&mut self, block: u32) -> &mut [u8] {
        let at = block as usize * self.block_size;
        &mut self.image[at..at + self.block_size]
    }

    fn allocate(&mut self) -> u32 {
        while self.used[self.next_block as usize] {
            self.next_block += 1;
        }
        let block = self.next_block;
        self.used[block as usize] = true;
        block
    }

    fn allocate_at(&mut self, block: u32) -> u32 {
        assert!(!self.used[block as usize], "样本的第 {block} 块已占用");
        self.used[block as usize] = true;
        block
    }

    /// 取最小的空闲 inode（删除后再建文件会重用它）。
    fn new_inode(&mut self) -> u32 {
        let number = (FIRST_INODE..=self.groups * self.inodes_per_group)
            .find(|number| {
                self.nodes
                    .get(number)
                    .is_none_or(|node| node.mode == 0 || node.links == 0)
            })
            .expect("样本的 inode 用完了");
        self.next_generation += 1;
        self.nodes.insert(
            number,
            Node {
                generation: self.next_generation,
                modified: (EXT_SAMPLE_SECONDS, self.nanos()),
                i_block: vec![0; 60],
                ..Node::default()
            },
        );
        number
    }

    fn nanos(&self) -> u32 {
        if self.inode_size > 128 {
            EXT_SAMPLE_NANOS
        } else {
            0
        }
    }

    fn new_dir_node(&mut self, number: u32, parent: u32) {
        let block = self.allocate();
        let block_size = self.block_size;
        let limit = self.dir_limit();
        let tail = self.metadata_csum();
        let bytes = self.block_mut(block);
        bytes.fill(0);
        put_entry(bytes, 0, number, 12, b".", 2);
        put_entry(bytes, 12, parent, (limit - 12) as u16, b"..", 2);
        if tail {
            put_tail(bytes, block_size);
        }
        self.next_generation += 1;
        let node = Node {
            mode: S_IFDIR | 0o755,
            links: 2,
            size: block_size as u64,
            generation: self.next_generation,
            modified: (EXT_SAMPLE_SECONDS, self.nanos()),
            data: vec![Some(block)],
            ..Node::default()
        };
        self.nodes.insert(number, node);
        self.encode_dir(number);
    }

    /// 目录的块映射：样本里的目录不超过 12 块、4 段，不用另分配间接块或叶子块。
    fn encode_dir(&mut self, dir: u32) {
        let data = self.nodes[&dir].data.clone();
        let (i_block, meta, flags) = self.encode_blocks(&data);
        assert!(meta.is_empty(), "样本的目录太大");
        let node = self.nodes.get_mut(&dir).unwrap();
        node.i_block = i_block.to_vec();
        node.flags = flags;
    }

    /// 目录块里能放目录项的长度（metadata_csum 时末尾 12 字节是校验项）。
    fn dir_limit(&self) -> usize {
        self.block_size - if self.metadata_csum() { 12 } else { 0 }
    }

    /// 在目录 `dir` 里加一项：找一项空隙够的拆开，都不够时给目录加一块。
    fn add_entry(&mut self, dir: u32, inode: u32, name: &str, kind: u8) {
        let needed = entry_len(name.len());
        let limit = self.dir_limit();
        let blocks: Vec<u32> = self.nodes[&dir].data.iter().flatten().copied().collect();
        for block in blocks {
            let bytes = self.block_mut(block);
            let mut at = 0;
            while at < limit {
                let (number, length, name_len) = read_entry(bytes, at);
                let used = if number == 0 { 0 } else { entry_len(name_len) };
                if length - used >= needed {
                    if used == 0 {
                        put_entry(bytes, at, inode, length as u16, name.as_bytes(), kind);
                    } else {
                        bytes[at + 4..at + 6].copy_from_slice(&(used as u16).to_le_bytes());
                        put_entry(
                            bytes,
                            at + used,
                            inode,
                            (length - used) as u16,
                            name.as_bytes(),
                            kind,
                        );
                    }
                    return;
                }
                at += length;
            }
        }
        let block = self.allocate();
        let block_size = self.block_size;
        let tail = self.metadata_csum();
        let bytes = self.block_mut(block);
        bytes.fill(0);
        put_entry(bytes, 0, inode, limit as u16, name.as_bytes(), kind);
        if tail {
            put_tail(bytes, block_size);
        }
        let node = self.nodes.get_mut(&dir).unwrap();
        node.data.push(Some(block));
        node.size += block_size as u64;
        self.encode_dir(dir);
    }

    /// 从目录 `dir` 里删掉名字为 `name` 的项：并进前一项的长度里；是块里第一项时只把 inode 号清零。
    fn remove_entry(&mut self, dir: u32, name: &str) {
        let limit = self.dir_limit();
        let blocks: Vec<u32> = self.nodes[&dir].data.iter().flatten().copied().collect();
        for block in blocks {
            let bytes = self.block_mut(block);
            let mut at = 0;
            let mut previous: Option<usize> = None;
            while at < limit {
                let (number, length, name_len) = read_entry(bytes, at);
                if number != 0 && &bytes[at + 8..at + 8 + name_len] == name.as_bytes() {
                    match previous {
                        Some(before) => {
                            let (_, before_length, _) = read_entry(bytes, before);
                            let joined = (before_length + length) as u16;
                            bytes[before + 4..before + 6].copy_from_slice(&joined.to_le_bytes());
                        }
                        None => bytes[at..at + 4].fill(0),
                    }
                    return;
                }
                previous = Some(at);
                at += length;
            }
        }
        panic!("目录里没有 {name}");
    }

    pub fn mkdir(&mut self, parent: u32, name: &str) -> u32 {
        let number = self.new_inode();
        self.new_dir_node(number, parent);
        self.add_entry(parent, number, name, 2);
        self.nodes.get_mut(&parent).unwrap().links += 1;
        number
    }

    pub fn add_file(&mut self, parent: u32, name: &str, data: &[u8], layout: Layout) -> u32 {
        let count = data.len().div_ceil(self.block_size);
        let blocks: Vec<Option<u32>> = match layout {
            Layout::Contiguous => (0..count).map(|_| Some(self.allocate())).collect(),
            Layout::Fragmented => (0..count)
                .map(|_| {
                    let block = self.allocate();
                    // 空出下一块
                    self.next_block = block + 2;
                    Some(block)
                })
                .collect(),
            Layout::At(first) => (0..count as u32)
                .map(|step| Some(self.allocate_at(first + step)))
                .collect(),
        };
        self.write_file(parent, name, data, blocks)
    }

    /// 带空洞的文件：`holes` 里的逻辑块不分配（内容必须是 0），文件末尾的空洞也一样。
    pub fn add_sparse(&mut self, parent: u32, name: &str, data: &[u8], holes: &[usize]) -> u32 {
        let count = data.len().div_ceil(self.block_size);
        let blocks: Vec<Option<u32>> = (0..count)
            .map(|index| {
                if holes.contains(&index) {
                    let chunk = &data
                        [index * self.block_size..((index + 1) * self.block_size).min(data.len())];
                    assert!(chunk.iter().all(|&byte| byte == 0), "空洞的内容必须是 0");
                    None
                } else {
                    Some(self.allocate())
                }
            })
            .collect();
        self.write_file(parent, name, data, blocks)
    }

    fn write_file(
        &mut self,
        parent: u32,
        name: &str,
        data: &[u8],
        blocks: Vec<Option<u32>>,
    ) -> u32 {
        let block_size = self.block_size;
        for (index, block) in blocks.iter().enumerate() {
            if let Some(block) = block {
                let chunk = &data[index * block_size..((index + 1) * block_size).min(data.len())];
                let bytes = self.block_mut(*block);
                bytes.fill(0);
                bytes[..chunk.len()].copy_from_slice(chunk);
            }
        }
        // 末尾的空洞不算块
        let mut blocks = blocks;
        while blocks.last() == Some(&None) {
            blocks.pop();
        }
        let number = self.new_inode();
        let (i_block, meta, flags) = self.encode_blocks(&blocks);
        let node = self.nodes.get_mut(&number).unwrap();
        node.mode = S_IFREG | 0o644;
        node.links = 1;
        node.size = data.len() as u64;
        node.flags = flags;
        node.i_block = i_block.to_vec();
        node.data = blocks;
        node.meta = meta;
        self.add_entry(parent, number, name, 1);
        number
    }

    /// 内容放在 inode 里的小文件（ext4 的 inline data）：前 60 字节在 `i_block`，其余在扩展属性 `system.data`。
    pub fn add_inline(&mut self, parent: u32, name: &str, data: &[u8]) -> u32 {
        assert!(self.extents(), "ext2 样本没有 inline data");
        let number = self.new_inode();
        let node = self.nodes.get_mut(&number).unwrap();
        node.mode = S_IFREG | 0o644;
        node.links = 1;
        node.size = data.len() as u64;
        node.flags = FLAG_INLINE_DATA;
        let head = data.len().min(60);
        node.i_block[..head].copy_from_slice(&data[..head]);
        node.inline_rest = Some(data[head..].to_vec());
        self.add_entry(parent, number, name, 1);
        number
    }

    /// 目标直接放在 `i_block` 里的符号链接。
    pub fn add_symlink(&mut self, parent: u32, name: &str, target: &str) -> u32 {
        assert!(target.len() < 60);
        let number = self.new_inode();
        let node = self.nodes.get_mut(&number).unwrap();
        node.mode = S_IFLNK | 0o777;
        node.links = 1;
        node.size = target.len() as u64;
        node.i_block[..target.len()].copy_from_slice(target.as_bytes());
        self.add_entry(parent, number, name, 7);
        number
    }

    /// 改 inode 的修改时间（秒可以为负，或超过 2038 年；128 字节的 inode 只记得下有符号 32 位的秒数）。
    pub fn set_modified(&mut self, inode: u32, seconds: i64, nanos: u32) {
        self.nodes.get_mut(&inode).unwrap().modified = (seconds, nanos);
    }

    /// 把块映射写进 `i_block`：旧式映射用直接块与间接块（间接块现分配），extent 不超过 4 段放在 inode 里，
    /// 多了放进一个叶子块。返回 `i_block`、间接块或叶子块、inode 标志。
    fn encode_blocks(&mut self, blocks: &[Option<u32>]) -> ([u8; 60], Vec<u32>, u32) {
        let mut i_block = [0u8; 60];
        let mut meta = Vec::new();
        if !self.extents() {
            let per_block = self.block_size / 4;
            for (index, block) in blocks.iter().take(12).enumerate() {
                i_block[index * 4..index * 4 + 4]
                    .copy_from_slice(&block.unwrap_or(0).to_le_bytes());
            }
            let rest = blocks.get(12..).unwrap_or_default();
            if !rest.is_empty() {
                let single = self.allocate();
                meta.push(single);
                i_block[48..52].copy_from_slice(&single.to_le_bytes());
                self.write_pointers(single, &rest[..rest.len().min(per_block)]);
            }
            let rest = rest.get(per_block..).unwrap_or_default();
            if !rest.is_empty() {
                assert!(rest.len() <= per_block * per_block, "样本只做到二级间接块");
                let double = self.allocate();
                meta.push(double);
                i_block[52..56].copy_from_slice(&double.to_le_bytes());
                let mut pointers = Vec::new();
                for chunk in rest.chunks(per_block) {
                    let single = self.allocate();
                    meta.push(single);
                    self.write_pointers(single, chunk);
                    pointers.push(Some(single));
                }
                self.write_pointers(double, &pointers);
            }
            return (i_block, meta, 0);
        }
        let mut extents: Vec<(u32, u32, u16)> = Vec::new();
        for (logical, block) in blocks.iter().enumerate() {
            let Some(physical) = *block else { continue };
            match extents.last_mut() {
                Some((start, first, length))
                    if *start + u32::from(*length) == logical as u32
                        && *first + u32::from(*length) == physical =>
                {
                    *length += 1;
                }
                _ => extents.push((logical as u32, physical, 1)),
            }
        }
        if extents.len() <= 4 {
            put_extent_node(&mut i_block, 4, 0, &extents);
        } else {
            let leaf = self.allocate();
            meta.push(leaf);
            let max = ((self.block_size - 12) / 12) as u16;
            put_extent_node(self.block_mut(leaf), max, 0, &extents);
            put_extent_header(&mut i_block, 1, 4, 1);
            i_block[12..16].copy_from_slice(&0u32.to_le_bytes());
            i_block[16..20].copy_from_slice(&leaf.to_le_bytes());
        }
        (i_block, meta, FLAG_EXTENTS)
    }

    fn write_pointers(&mut self, block: u32, pointers: &[Option<u32>]) {
        let bytes = self.block_mut(block);
        bytes.fill(0);
        for (index, pointer) in pointers.iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&pointer.unwrap_or(0).to_le_bytes());
        }
    }

    /// 删除文件或空目录：目录项并进前一项，inode 与块在位图里释放，记下删除时间。ext2 留着长度与块指针；
    /// ext4 清空长度与 extent（只留空的 extent 头）。
    pub fn delete(&mut self, parent: u32, name: &str, inode: u32) {
        self.remove_entry(parent, name);
        let extents = self.extents();
        let node = self.nodes.get_mut(&inode).unwrap();
        let is_dir = node.mode & 0xF000 == S_IFDIR;
        node.links = 0;
        node.dtime = EXT_SAMPLE_SECONDS as u32;
        let owned: Vec<u32> = node
            .data
            .iter()
            .flatten()
            .copied()
            .chain(node.meta.iter().copied())
            .collect();
        if extents {
            node.size = 0;
            node.i_block = vec![0; 60];
            if node.flags & FLAG_EXTENTS != 0 {
                put_extent_header(&mut node.i_block, 0, 4, 0);
            }
        }
        for block in owned {
            self.used[block as usize] = false;
        }
        if is_dir {
            self.nodes.get_mut(&parent).unwrap().links -= 1;
        }
    }

    /// 文件第一个数据块的块号。
    pub fn first_block(&self, inode: u32) -> u32 {
        self.nodes[&inode]
            .data
            .iter()
            .flatten()
            .copied()
            .next()
            .expect("没有数据块")
    }

    /// inode 的版本号（定位信息里的 `g`）。
    pub fn generation(&self, inode: u32) -> u32 {
        self.nodes[&inode].generation
    }

    /// 主超级块在镜像里的位置，以及 1 号块组开头那份备份的位置（有的话）。
    pub fn superblock_offsets(&self) -> (usize, Option<usize>) {
        let backup = (self.groups > 1).then(|| self.group_start(1) as usize * self.block_size);
        (1024, backup)
    }

    /// 第 0 个块组描述符在镜像里的位置。
    pub fn descriptor_offset(&self) -> usize {
        (self.first_data_block as usize + 1) * self.block_size
    }

    pub fn finish(mut self) -> Vec<u8> {
        let numbers: Vec<u32> = self.nodes.keys().copied().collect();
        for number in numbers {
            self.write_inode(number);
        }
        let mut descriptors = Vec::new();
        let mut free_blocks_total = 0u64;
        let mut free_inodes_total = 0u32;
        for group in 0..self.groups {
            let start = self.group_start(group);
            let mut block_bitmap = vec![0u8; self.block_size];
            let mut free_blocks = 0u32;
            for bit in 0..self.blocks_per_group {
                let block = start + bit;
                let used = block >= self.blocks || self.used[block as usize];
                if used {
                    block_bitmap[bit as usize / 8] |= 1 << (bit % 8);
                } else {
                    free_blocks += 1;
                }
            }
            let mut inode_bitmap = vec![0u8; self.block_size];
            let mut free_inodes = 0u32;
            let mut dirs = 0u32;
            for bit in 0..self.inodes_per_group {
                let number = group * self.inodes_per_group + bit + 1;
                match self.nodes.get(&number) {
                    Some(node) if number < FIRST_INODE || node.links > 0 => {
                        inode_bitmap[bit as usize / 8] |= 1 << (bit % 8);
                        if node.mode & 0xF000 == S_IFDIR {
                            dirs += 1;
                        }
                    }
                    _ => free_inodes += 1,
                }
            }
            // inode 位图在一块里只用到每组 inode 数那么多位，其余置 1
            for bit in self.inodes_per_group as usize..self.block_size * 8 {
                inode_bitmap[bit / 8] |= 1 << (bit % 8);
            }
            let bitmap_block = self.block_bitmap(group);
            self.block_mut(bitmap_block).copy_from_slice(&block_bitmap);
            self.block_mut(bitmap_block + 1)
                .copy_from_slice(&inode_bitmap);
            free_blocks_total += u64::from(free_blocks);
            free_inodes_total += free_inodes;
            let mut descriptor = vec![0u8; self.desc_size()];
            let put32 = |bytes: &mut [u8], low: usize, high: usize, value: u64| {
                bytes[low..low + 4].copy_from_slice(&(value as u32).to_le_bytes());
                if bytes.len() >= 64 {
                    bytes[high..high + 4].copy_from_slice(&((value >> 32) as u32).to_le_bytes());
                }
            };
            put32(&mut descriptor, 0x00, 0x20, u64::from(bitmap_block));
            put32(&mut descriptor, 0x04, 0x24, u64::from(bitmap_block + 1));
            put32(
                &mut descriptor,
                0x08,
                0x28,
                u64::from(self.inode_table(group)),
            );
            descriptor[0x0C..0x0E].copy_from_slice(&(free_blocks as u16).to_le_bytes());
            descriptor[0x0E..0x10].copy_from_slice(&(free_inodes as u16).to_le_bytes());
            descriptor[0x10..0x12].copy_from_slice(&(dirs as u16).to_le_bytes());
            descriptors.push(descriptor);
        }
        let superblock = self.superblock(free_blocks_total, free_inodes_total);
        let uuid: [u8; 16] = superblock[0x68..0x78].try_into().unwrap();
        let seed = crc32c(!0, &[&uuid]);
        for (group, descriptor) in descriptors.iter_mut().enumerate() {
            let number = (group as u32).to_le_bytes();
            let checksum = if self.metadata_csum() {
                crc32c(
                    seed,
                    &[&number, &descriptor[..0x1E], &[0, 0], &descriptor[0x20..]],
                ) as u16
            } else if self.extents() {
                crc16(
                    0xFFFF,
                    &[&uuid, &number, &descriptor[..0x1E], &descriptor[0x20..]],
                )
            } else {
                0
            };
            descriptor[0x1E..0x20].copy_from_slice(&checksum.to_le_bytes());
        }
        let table: Vec<u8> = descriptors.concat();
        for group in 0..self.groups {
            if !Self::has_super(group) {
                continue;
            }
            let mut copy = superblock.clone();
            copy[0x5A..0x5C].copy_from_slice(&(group as u16).to_le_bytes());
            if self.metadata_csum() {
                let checksum = crc32c(!0, &[&copy[..0x3FC]]);
                copy[0x3FC..].copy_from_slice(&checksum.to_le_bytes());
            }
            let at = if group == 0 {
                1024
            } else {
                self.group_start(group) as usize * self.block_size
            };
            self.image[at..at + 1024].copy_from_slice(&copy);
            let table_at = (self.group_start(group) as usize + 1) * self.block_size;
            self.image[table_at..table_at + table.len()].copy_from_slice(&table);
        }
        self.image
    }

    fn superblock(&self, free_blocks: u64, free_inodes: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 1024];
        let mut put =
            |at: usize, value: u32| bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        put(0x00, self.groups * self.inodes_per_group);
        put(0x04, self.blocks);
        put(0x0C, free_blocks as u32);
        put(0x10, free_inodes);
        put(0x14, self.first_data_block);
        let log = (self.block_size / 1024).trailing_zeros();
        put(0x18, log);
        put(0x1C, log);
        put(0x20, self.blocks_per_group);
        put(0x24, self.blocks_per_group);
        put(0x28, self.inodes_per_group);
        put(0x2C, EXT_SAMPLE_SECONDS as u32);
        put(0x30, EXT_SAMPLE_SECONDS as u32);
        put(0x4C, 1);
        put(0x54, FIRST_INODE);
        let (compat, incompat, ro_compat) = match self.flavor {
            ExtFlavor::Ext2 => (
                COMPAT_EXT_ATTR,
                INCOMPAT_FILETYPE,
                RO_COMPAT_SPARSE_SUPER | RO_COMPAT_LARGE_FILE,
            ),
            ExtFlavor::Ext4 | ExtFlavor::Ext4Large => (
                COMPAT_HAS_JOURNAL | COMPAT_EXT_ATTR,
                INCOMPAT_FILETYPE
                    | INCOMPAT_EXTENTS
                    | INCOMPAT_64BIT
                    | INCOMPAT_FLEX_BG
                    | INCOMPAT_INLINE_DATA,
                RO_COMPAT_SPARSE_SUPER
                    | RO_COMPAT_LARGE_FILE
                    | RO_COMPAT_EXTRA_ISIZE
                    | if self.metadata_csum() {
                        RO_COMPAT_METADATA_CSUM
                    } else {
                        RO_COMPAT_GDT_CSUM
                    },
            ),
        };
        put(0x5C, compat);
        put(0x60, incompat);
        put(0x64, ro_compat);
        bytes[0x38..0x3A].copy_from_slice(&0xEF53u16.to_le_bytes());
        bytes[0x3A..0x3C].copy_from_slice(&1u16.to_le_bytes());
        bytes[0x58..0x5A].copy_from_slice(&(self.inode_size as u16).to_le_bytes());
        for (index, byte) in bytes[0x68..0x78].iter_mut().enumerate() {
            *byte = 0x40 + index as u8;
        }
        let label = self.label.as_bytes();
        bytes[0x78..0x78 + label.len().min(16)].copy_from_slice(&label[..label.len().min(16)]);
        if self.extents() {
            bytes[0xFE..0x100].copy_from_slice(&(self.desc_size() as u16).to_le_bytes());
            bytes[0x15C..0x15E].copy_from_slice(&(EXTRA_ISIZE as u16).to_le_bytes());
            bytes[0x15E..0x160].copy_from_slice(&(EXTRA_ISIZE as u16).to_le_bytes());
        }
        if self.metadata_csum() {
            bytes[0x175] = 1;
        }
        bytes
    }

    fn write_inode(&mut self, number: u32) {
        let node = self.nodes[&number].clone();
        let index = number - 1;
        let group = index / self.inodes_per_group;
        let at = self.inode_table(group) as usize * self.block_size
            + (index % self.inodes_per_group) as usize * self.inode_size;
        let block_size = self.block_size as u64;
        let inode_size = self.inode_size;
        let bytes = &mut self.image[at..at + inode_size];
        bytes.fill(0);
        let put = |bytes: &mut [u8], at: usize, value: u32| {
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes())
        };
        bytes[0..2].copy_from_slice(&node.mode.to_le_bytes());
        put(bytes, 0x04, node.size as u32);
        let (seconds, nanos) = node.modified;
        let low = seconds as u32;
        let epoch = ((seconds - i64::from(low as i32)) >> 32) as u32 & 3;
        for field in [0x08, 0x0C, 0x10] {
            put(bytes, field, low);
        }
        put(bytes, 0x14, node.dtime);
        bytes[0x1A..0x1C].copy_from_slice(&node.links.to_le_bytes());
        let owned = node.data.iter().flatten().count() + node.meta.len();
        put(bytes, 0x1C, (owned as u64 * block_size / 512) as u32);
        put(bytes, 0x20, node.flags);
        if node.i_block.len() == 60 {
            bytes[0x28..0x64].copy_from_slice(&node.i_block);
        }
        put(bytes, 0x64, node.generation);
        put(bytes, 0x6C, (node.size >> 32) as u32);
        if inode_size > 128 {
            bytes[0x80..0x82].copy_from_slice(&(EXTRA_ISIZE as u16).to_le_bytes());
            let extra = (nanos << 2) | epoch;
            for field in [0x84, 0x88, 0x8C, 0x94] {
                put(bytes, field, extra);
            }
            put(bytes, 0x90, low);
            if let Some(rest) = &node.inline_rest {
                // 扩展属性区：魔数，一项 system.data（名字前缀 7、名字 data），值放在项之后
                put(bytes, XATTR_AT, 0xEA02_0000);
                let entry = XATTR_AT + 4;
                let value_offset = 24usize;
                bytes[entry] = 4;
                bytes[entry + 1] = 7;
                bytes[entry + 2..entry + 4].copy_from_slice(&(value_offset as u16).to_le_bytes());
                put(bytes, entry + 8, rest.len() as u32);
                bytes[entry + 16..entry + 20].copy_from_slice(b"data");
                bytes[entry + value_offset..entry + value_offset + rest.len()]
                    .copy_from_slice(rest);
            }
        }
    }
}

fn entry_len(name_len: usize) -> usize {
    (8 + name_len + 3) & !3
}

fn read_entry(bytes: &[u8], at: usize) -> (u32, usize, usize) {
    let number = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let length = usize::from(u16::from_le_bytes(
        bytes[at + 4..at + 6].try_into().unwrap(),
    ));
    (number, length, usize::from(bytes[at + 6]))
}

fn put_entry(bytes: &mut [u8], at: usize, inode: u32, length: u16, name: &[u8], kind: u8) {
    bytes[at..at + 4].copy_from_slice(&inode.to_le_bytes());
    bytes[at + 4..at + 6].copy_from_slice(&length.to_le_bytes());
    bytes[at + 6] = name.len() as u8;
    bytes[at + 7] = kind;
    bytes[at + 8..at + 8 + name.len()].copy_from_slice(name);
}

/// metadata_csum 时目录块末尾的校验项（校验和不写）。
fn put_tail(bytes: &mut [u8], block_size: usize) {
    let at = block_size - 12;
    bytes[at..at + 12].fill(0);
    bytes[at + 4..at + 6].copy_from_slice(&12u16.to_le_bytes());
    bytes[at + 7] = 0xDE;
}

fn put_extent_header(bytes: &mut [u8], entries: u16, max: u16, depth: u16) {
    bytes[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
    bytes[2..4].copy_from_slice(&entries.to_le_bytes());
    bytes[4..6].copy_from_slice(&max.to_le_bytes());
    bytes[6..8].copy_from_slice(&depth.to_le_bytes());
}

fn put_extent_node(bytes: &mut [u8], max: u16, depth: u16, extents: &[(u32, u32, u16)]) {
    bytes.fill(0);
    put_extent_header(bytes, extents.len() as u16, max, depth);
    for (index, &(logical, physical, length)) in extents.iter().enumerate() {
        let at = 12 + index * 12;
        bytes[at..at + 4].copy_from_slice(&logical.to_le_bytes());
        bytes[at + 4..at + 6].copy_from_slice(&length.to_le_bytes());
        bytes[at + 8..at + 12].copy_from_slice(&physical.to_le_bytes());
    }
}
