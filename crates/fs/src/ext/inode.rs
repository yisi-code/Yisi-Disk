//! inode：类型与长度、时间（含大 inode 的纳秒与年份扩展）、标志；数据在哪里：extent 树、旧式直接 / 间接块、
//! 存放在 inode 里的 inline data（含 inode 里扩展属性 `system.data` 的续段）与快速符号链接。

use yisi_part::bytes::{le16, le32};

use crate::FsError;
use crate::entry::Stamp;

/// `i_block`：60 字节，放块指针、extent 树的根、inline data 或快速符号链接的目标。
pub(crate) const I_BLOCK_AT: usize = 0x28;
pub(crate) const I_BLOCK_SIZE: usize = 60;
/// 大 inode 的扩展部分从第 128 字节起，长度记在 `i_extra_isize`。
const EXTRA_AT: usize = 0x80;

/// 文件类型（`i_mode` 的高 4 位）。
const S_IFMT: u16 = 0xF000;
const S_IFDIR: u16 = 0x4000;
const S_IFREG: u16 = 0x8000;
const S_IFLNK: u16 = 0xA000;
const S_IFCHR: u16 = 0x2000;
const S_IFBLK: u16 = 0x6000;
const S_IFIFO: u16 = 0x1000;
const S_IFSOCK: u16 = 0xC000;

/// inode 标志：fscrypt 加密、htree 索引目录、用 extent、inline data。
pub(crate) const FLAG_ENCRYPT: u32 = 0x0000_0800;
pub(crate) const FLAG_INDEX: u32 = 0x0000_1000;
const FLAG_EXTENTS: u32 = 0x0008_0000;
const FLAG_INLINE_DATA: u32 = 0x1000_0000;

/// extent 树：节点头的魔数、最多几层（规范说不超过 5）。
const EXTENT_MAGIC: u16 = 0xF30A;
const MAX_EXTENT_DEPTH: u16 = 5;
/// 未初始化的 extent：长度字段大于这个值，减去它是真正的长度，内容读作 0。
const UNWRITTEN_FROM: u16 = 32768;
/// 旧式块映射：12 个直接块，之后是一级、二级、三级间接块。
const DIRECT_BLOCKS: u64 = 12;
/// 映射一个文件最多读这么多个间接块或 extent 索引块（4 KiB 块时够映射约 500 GiB 的旧式文件），挡住构造出来的结构。
const MAX_MAP_READS: usize = 1 << 17;
/// 一个文件最多这么多段。
const MAX_EXTENTS: usize = 1 << 20;
/// inode 里扩展属性区的魔数；inline data 的续段是 `system.data`（名字前缀编号 7、名字 `data`）。
const XATTR_MAGIC: u32 = 0xEA02_0000;
const XATTR_INDEX_SYSTEM: u8 = 7;

/// 一个 inode 里用得着的内容。
#[derive(Debug, Clone)]
pub(crate) struct Inode {
    pub mode: u16,
    pub links: u16,
    pub size: u64,
    pub flags: u32,
    pub generation: u32,
    pub modified: Option<Stamp>,
    pub created: Option<Stamp>,
    block: [u8; I_BLOCK_SIZE],
    /// inode 里扩展属性区（`i_extra_isize` 之后）的内容。
    xattrs: Vec<u8>,
}

/// 文件的一段：从第 `logical` 块起 `length` 块放在第 `physical` 块起；`unwritten` 的读作 0。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extent {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
    pub unwritten: bool,
}

/// 文件数据在哪些块里。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Mapping {
    /// 按逻辑块号排序、互不重叠。
    pub extents: Vec<Extent>,
    /// 间接块与 extent 树的索引块、叶子块（判断删除的文件是否被覆盖时一起看）。
    pub meta_blocks: Vec<u64>,
    /// 结构都对得上、块号都在卷里；为 `false` 时 `extents` 只到出问题之前。
    pub complete: bool,
}

/// 映射块时要知道的卷参数。
pub(crate) struct MapContext<'a> {
    pub block_size: u64,
    pub first_data_block: u64,
    pub blocks_count: u64,
    /// 读一整块（块号相对分区开头）。
    pub read_block: &'a dyn Fn(u64) -> Result<Vec<u8>, FsError>,
}

impl MapContext<'_> {
    fn in_volume(&self, block: u64, count: u64) -> bool {
        block >= self.first_data_block
            && block
                .checked_add(count)
                .is_some_and(|end| end <= self.blocks_count)
    }
}

impl Inode {
    /// 解析 inode。`large_dir`：目录的长度也用高 32 位（largedir 特性；否则那 4 字节是 ext2 的目录 ACL）。
    pub fn parse(bytes: &[u8], large_dir: bool) -> Option<Self> {
        let mode = le16(bytes, 0x00)?;
        let flags = le32(bytes, 0x20)?;
        let high = if mode & S_IFMT != S_IFDIR || large_dir {
            u64::from(le32(bytes, 0x6C)?)
        } else {
            0
        };
        let size = u64::from(le32(bytes, 0x04)?) | high << 32;
        // 扩展部分：长度要按 4 对齐、不超出 inode
        let extra = match le16(bytes, EXTRA_AT) {
            Some(extra) if bytes.len() > EXTRA_AT => usize::from(extra),
            _ => 0,
        };
        let extra = if extra % 4 == 0 && EXTRA_AT + extra <= bytes.len() {
            extra
        } else {
            0
        };
        // 扩展部分里第 `at` 字节起的 4 字节在不在
        let has = |at: usize| at + 4 <= EXTRA_AT + extra;
        let stamp = |seconds_at: usize, extra_at: usize| -> Option<Stamp> {
            let seconds = le32(bytes, seconds_at)?;
            let extra = if has(extra_at) {
                le32(bytes, extra_at)?
            } else {
                0
            };
            Some(Stamp::Ext { seconds, extra })
        };
        let modified = stamp(0x10, 0x88).filter(|stamp| stamp.text().is_some());
        let created = if has(0x90) {
            stamp(0x90, 0x94).filter(|stamp| stamp.text().is_some())
        } else {
            None
        };
        Some(Self {
            mode,
            links: le16(bytes, 0x1A)?,
            size,
            flags,
            generation: le32(bytes, 0x64)?,
            modified,
            created,
            block: bytes
                .get(I_BLOCK_AT..I_BLOCK_AT + I_BLOCK_SIZE)?
                .try_into()
                .ok()?,
            xattrs: bytes.get(EXTRA_AT + extra..).unwrap_or_default().to_vec(),
        })
    }

    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & S_IFMT == S_IFLNK
    }

    pub fn uses_extents(&self) -> bool {
        self.flags & FLAG_EXTENTS != 0
    }

    pub fn is_inline(&self) -> bool {
        self.flags & FLAG_INLINE_DATA != 0
    }

    /// 目标不到 60 字节、直接放在 `i_block` 里的符号链接。
    pub fn is_fast_symlink(&self) -> bool {
        self.is_symlink()
            && self.flags & (FLAG_EXTENTS | FLAG_INLINE_DATA) == 0
            && self.size < I_BLOCK_SIZE as u64
    }

    /// 目录项里的文件类型（1 普通文件、2 目录、3 字符设备、4 块设备、5 管道、6 套接字、7 符号链接）；不认得时为 0。
    pub fn file_type(&self) -> u8 {
        match self.mode & S_IFMT {
            S_IFREG => 1,
            S_IFDIR => 2,
            S_IFCHR => 3,
            S_IFBLK => 4,
            S_IFIFO => 5,
            S_IFSOCK => 6,
            S_IFLNK => 7,
            _ => 0,
        }
    }

    /// `i_block` 原样。
    pub fn block_area(&self) -> &[u8] {
        &self.block
    }

    /// inline data 的全部内容：`i_block` 里的 60 字节，接上 inode 里扩展属性 `system.data` 的值。不截到文件长度。
    pub fn inline_bytes(&self) -> Vec<u8> {
        let mut bytes = self.block.to_vec();
        if let Some(rest) = self.system_data() {
            bytes.extend_from_slice(rest);
        }
        bytes
    }

    /// inode 里扩展属性 `system.data` 的值（inline data 放不进 `i_block` 的部分）。
    pub fn system_data(&self) -> Option<&[u8]> {
        if le32(&self.xattrs, 0)? != XATTR_MAGIC {
            return None;
        }
        // 值的偏移相对第一项（魔数之后）
        let base = self.xattrs.get(4..)?;
        let mut at = 0usize;
        while let (Some(name_len), Some(index)) = (base.get(at).copied(), base.get(at + 1).copied())
        {
            if le32(base, at)? == 0 {
                break;
            }
            let value_at = usize::from(le16(base, at + 2)?);
            let value_size = usize::try_from(le32(base, at + 8)?).ok()?;
            let name = base.get(at + 16..at + 16 + usize::from(name_len))?;
            if index == XATTR_INDEX_SYSTEM && name == b"data" {
                return base.get(value_at..value_at.checked_add(value_size)?);
            }
            at = at.checked_add((16 + usize::from(name_len) + 3) & !3)?;
        }
        None
    }

    /// 数据第一块的块号，不读别的块：extent 树只有一层时取第一段，旧式映射取第一个直接块。拿不到时为 `None`。
    pub fn first_block(&self) -> Option<u64> {
        if self.is_inline() || self.is_fast_symlink() {
            return None;
        }
        let block = &self.block;
        let first = if self.flags & FLAG_EXTENTS != 0 {
            if le16(block, 0)? != EXTENT_MAGIC || le16(block, 2)? == 0 || le16(block, 6)? != 0 {
                return None;
            }
            u64::from(le32(block, 12 + 8)?) | u64::from(le16(block, 12 + 6)?) << 32
        } else {
            u64::from(le32(block, 0)?)
        };
        (first != 0).then_some(first)
    }

    /// 数据所在的块，最多找到能放下 `size` 字节为止（extent 树整棵读出）。
    pub fn mapping(&self, context: &MapContext<'_>) -> Mapping {
        let mut walk = Walk {
            context,
            mapping: Mapping {
                complete: true,
                ..Mapping::default()
            },
            reads: 0,
        };
        if self.is_inline() || self.is_fast_symlink() {
            return walk.mapping;
        }
        if self.flags & FLAG_EXTENTS != 0 {
            walk.extent_node(&self.block, None);
            walk.mapping.extents.sort_by_key(|extent| extent.logical);
            // 去掉与前一段重叠的段（结构坏了才会有）
            let mut end = 0u64;
            let mut overlapped = false;
            walk.mapping.extents.retain(|extent| {
                let keep = extent.logical >= end;
                if keep {
                    end = extent.logical.saturating_add(extent.length);
                } else {
                    overlapped = true;
                }
                keep
            });
            if overlapped {
                walk.mapping.complete = false;
            }
        } else {
            let needed = self.size.div_ceil(context.block_size);
            walk.indirect_root(&self.block, needed);
        }
        walk.mapping
    }
}

struct Walk<'a, 'b> {
    context: &'a MapContext<'b>,
    mapping: Mapping,
    reads: usize,
}

impl Walk<'_, '_> {
    fn broken(&mut self) {
        self.mapping.complete = false;
    }

    /// 读一个间接块或索引块；块号越界、读得太多或读不出时记为不完整。
    fn read(&mut self, block: u64) -> Option<Vec<u8>> {
        if !self.context.in_volume(block, 1) || self.reads >= MAX_MAP_READS {
            self.broken();
            return None;
        }
        self.reads += 1;
        self.mapping.meta_blocks.push(block);
        match (self.context.read_block)(block) {
            Ok(bytes) => Some(bytes),
            Err(_) => {
                self.broken();
                None
            }
        }
    }

    /// 记下一段；与上一段首尾相接（逻辑与物理都连着、都写过或都没写过）时并成一段。
    fn push(&mut self, extent: Extent) -> bool {
        if let Some(last) = self.mapping.extents.last_mut()
            && last.unwritten == extent.unwritten
            && last.logical.checked_add(last.length) == Some(extent.logical)
            && last.physical.checked_add(last.length) == Some(extent.physical)
        {
            last.length = last.length.saturating_add(extent.length);
            return true;
        }
        if self.mapping.extents.len() >= MAX_EXTENTS {
            self.broken();
            return false;
        }
        self.mapping.extents.push(extent);
        true
    }

    /// extent 树的一个节点（根在 `i_block` 里，其余各占一块）；`depth` 是父节点说的这一层的深度。
    fn extent_node(&mut self, node: &[u8], depth: Option<u16>) {
        let (Some(magic), Some(entries), Some(max), Some(level)) =
            (le16(node, 0), le16(node, 2), le16(node, 4), le16(node, 6))
        else {
            return self.broken();
        };
        let fits = 12 + usize::from(entries) * 12 <= node.len();
        if magic != EXTENT_MAGIC
            || entries > max
            || !fits
            || level > MAX_EXTENT_DEPTH
            || depth.is_some_and(|depth| depth != level)
        {
            return self.broken();
        }
        for index in 0..usize::from(entries) {
            if !self.mapping.complete {
                return;
            }
            let at = 12 + index * 12;
            if level == 0 {
                let (Some(logical), Some(length), Some(high), Some(low)) = (
                    le32(node, at),
                    le16(node, at + 4),
                    le16(node, at + 6),
                    le32(node, at + 8),
                ) else {
                    return self.broken();
                };
                let (length, unwritten) = if length > UNWRITTEN_FROM {
                    (length - UNWRITTEN_FROM, true)
                } else {
                    (length, false)
                };
                let physical = u64::from(low) | u64::from(high) << 32;
                if length == 0 || !self.context.in_volume(physical, u64::from(length)) {
                    return self.broken();
                }
                if !self.push(Extent {
                    logical: u64::from(logical),
                    physical,
                    length: u64::from(length),
                    unwritten,
                }) {
                    return;
                }
            } else {
                let (Some(low), Some(high)) = (le32(node, at + 4), le16(node, at + 8)) else {
                    return self.broken();
                };
                let child = u64::from(low) | u64::from(high) << 32;
                let Some(bytes) = self.read(child) else {
                    return;
                };
                self.extent_node(&bytes, Some(level - 1));
            }
        }
    }

    /// 旧式映射：12 个直接块，之后一级、二级、三级间接块；块号 0 是空洞。
    fn indirect_root(&mut self, block: &[u8], needed: u64) {
        let per_block = self.context.block_size / 4;
        let mut logical = 0u64;
        for slot in 0..DIRECT_BLOCKS as usize {
            if logical >= needed {
                return;
            }
            let pointer = u64::from(le32(block, slot * 4).unwrap_or(0));
            self.pointer(pointer, 0, logical, needed, per_block);
            if !self.mapping.complete {
                return;
            }
            logical += 1;
        }
        for (level, slot) in [(1u32, 12usize), (2, 13), (3, 14)] {
            if logical >= needed || !self.mapping.complete {
                return;
            }
            let pointer = u64::from(le32(block, slot * 4).unwrap_or(0));
            self.pointer(pointer, level, logical, needed, per_block);
            let Some(span) = per_block.checked_pow(level) else {
                return;
            };
            logical = logical.saturating_add(span);
        }
    }

    /// 块指针 `pointer`：`level` 为 0 时是数据块，否则是第 `level` 级间接块，从第 `base` 个逻辑块起。
    fn pointer(&mut self, pointer: u64, level: u32, base: u64, needed: u64, per_block: u64) {
        if pointer == 0 {
            return;
        }
        if level == 0 {
            if !self.context.in_volume(pointer, 1) {
                return self.broken();
            }
            self.push(Extent {
                logical: base,
                physical: pointer,
                length: 1,
                unwritten: false,
            });
            return;
        }
        let Some(bytes) = self.read(pointer) else {
            return;
        };
        let Some(span) = per_block.checked_pow(level - 1) else {
            return self.broken();
        };
        for index in 0..per_block {
            let Some(child_base) = index
                .checked_mul(span)
                .and_then(|offset| base.checked_add(offset))
            else {
                return;
            };
            if child_base >= needed || !self.mapping.complete {
                return;
            }
            let child = u64::from(le32(&bytes, (index * 4) as usize).unwrap_or(0));
            self.pointer(child, level - 1, child_base, needed, per_block);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(read: &dyn Fn(u64) -> Result<Vec<u8>, FsError>) -> MapContext<'_> {
        MapContext {
            block_size: 1024,
            first_data_block: 1,
            blocks_count: 10_000,
            read_block: read,
        }
    }

    fn inode_with(block: &[u8], flags: u32, size: u64) -> Inode {
        let mut bytes = vec![0u8; 128];
        bytes[0..2].copy_from_slice(&S_IFREG.to_le_bytes());
        bytes[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        bytes[0x20..0x24].copy_from_slice(&flags.to_le_bytes());
        bytes[I_BLOCK_AT..I_BLOCK_AT + block.len()].copy_from_slice(block);
        Inode::parse(&bytes, false).unwrap()
    }

    #[test]
    fn extent_trees_reject_loops_bad_depths_and_out_of_range_blocks() {
        // 根是一层索引，指向的叶子块又说自己是索引（深度对不上）
        let mut root = vec![0u8; 60];
        root[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
        root[2..4].copy_from_slice(&1u16.to_le_bytes());
        root[4..6].copy_from_slice(&4u16.to_le_bytes());
        root[6..8].copy_from_slice(&1u16.to_le_bytes());
        root[16..20].copy_from_slice(&100u32.to_le_bytes());
        let looping = |_: u64| Ok(root.clone());
        let mapping = inode_with(&root, FLAG_EXTENTS, 4096).mapping(&context(&looping));
        assert!(!mapping.complete);
        // 叶子里的段超出卷末尾
        let mut leaf = vec![0u8; 60];
        leaf[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
        leaf[2..4].copy_from_slice(&1u16.to_le_bytes());
        leaf[4..6].copy_from_slice(&4u16.to_le_bytes());
        leaf[16..18].copy_from_slice(&u16::MAX.to_le_bytes());
        leaf[18..20].copy_from_slice(&u16::MAX.to_le_bytes());
        leaf[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        let never = |_: u64| -> Result<Vec<u8>, FsError> { unreachable!() };
        let mapping = inode_with(&leaf, FLAG_EXTENTS, u64::MAX).mapping(&context(&never));
        assert!(!mapping.complete && mapping.extents.is_empty());
    }

    #[test]
    fn indirect_blocks_with_huge_sizes_stay_bounded() {
        // 每个间接块都指向自己：二级、三级间接块会一直读同一块，直到读满上限
        let mut block = vec![0u8; 60];
        for slot in 0..15 {
            block[slot * 4..slot * 4 + 4].copy_from_slice(&7u32.to_le_bytes());
        }
        let self_pointing = |_: u64| Ok(7u32.to_le_bytes().repeat(256));
        let mut bytes = vec![0u8; 128];
        bytes[0..2].copy_from_slice(&S_IFREG.to_le_bytes());
        bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[0x6C..0x70].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[I_BLOCK_AT..I_BLOCK_AT + 60].copy_from_slice(&block);
        let inode = Inode::parse(&bytes, false).unwrap();
        assert_eq!(inode.size, u64::MAX);
        let mapping = inode.mapping(&context(&self_pointing));
        assert!(!mapping.complete);
        assert!(mapping.meta_blocks.len() <= MAX_MAP_READS);
    }
}
