//! ext2 / ext3 / ext4：超级块与块组描述符（含 64 位描述符、meta_bg、bigalloc）、inode（含大 inode 的纳秒时间与创建时间）、
//! extent 树与旧式直接 / 间接块、目录（线性目录项；htree 索引目录按线性读出全部叶子块）、inline data 的小文件与目录。
//! 依据公开的 ext4 磁盘格式文档实现，只读；有日志时不回放日志，读的是写在原处的内容。
//!
//! 已删除的项从目录块的空隙里找（[`dir`]）：删除时目录项的名字与 inode 号还在。之后看 inode：
//! - ext2 删除时 inode 里的长度与块指针还在：按块位图判断这些块有没有分给别的文件（可能完整 / 已被覆盖 / 无法判断）；
//! - ext3 / ext4 删除时清空了长度与块指针或 extent：判为 [`State::Cleared`]，只列出名字，读不出内容；
//! - inode 已经又分给了别的文件（inode 位图里在用，或类型与目录项记的不同）：判为已被覆盖。
//!
//! 删除项的 inode 号之外没有别的线索，目录项被新项覆盖、或删的是块里第一项（inode 号被清零）的找不到。

mod dir;
mod inode;
mod superblock;
#[cfg(test)]
mod tests;

use std::io::Read;
use std::sync::Arc;

use yisi_io::BlockDevice;
use yisi_part::bytes::{le16, le32};

use self::dir::{DirFormat, RawEntry, parse_block};
use self::inode::{FLAG_ENCRYPT, FLAG_INDEX, I_BLOCK_AT, I_BLOCK_SIZE, Inode, MapContext, Mapping};
pub use self::superblock::{ExtCopy, ext_superblock_copy, has_magic};
use self::superblock::{
    Group, INCOMPAT_FILETYPE, INCOMPAT_LARGEDIR, SUPERBLOCK_OFFSET, SUPERBLOCK_SIZE, Superblock,
};
use crate::entry::{Entry, FileAttributes, FileRef, Listing, Location, State, Unsupported};
use crate::reader::{FileReader, Piece};
use crate::region::{Chunks, Region};
use crate::{FsError, MAX_DIR_BYTES, MAX_ENTRIES};

/// 根目录的 inode 号。
const ROOT_INODE: u32 = 2;
/// 主超级块不能用时看这几个块组开头的备份（sparse_super 时 1、3、5、7、9 号都有）。
const BACKUP_GROUPS: [u32; 5] = [1, 3, 5, 7, 9];
/// 找备份时假设的块大小；每组的块数按 mke2fs 的默认取块大小的 8 倍（一块块位图管得了的数目）。
const BACKUP_BLOCK_SIZES: [u64; 4] = [1024, 2048, 4096, 65536];
/// 判断一个删除的文件有没有被覆盖时最多查这么多块，超过时判为无法判断。
const MAX_STATE_BLOCKS: u64 = 1 << 24;
/// 删除时被清空的文件与目录读不出的原因。
const CLEARED: &str = "删除时文件系统清空了它的长度与数据位置（ext3 / ext4 的做法），按 inode 找不到内容在哪里；可以试试「按文件类型深度恢复」";
const REUSED: &str = "这个 inode 已被重用，内容已不是这个文件";

/// 解析超级块；参数不自洽时为 `None`。识别文件系统与打开卷用同一条规则。
pub(crate) fn superblock_ok(bytes: &[u8]) -> bool {
    Superblock::parse(bytes).is_some()
}

/// 主超级块不能用时找一份备份：按常见的块大小推算 1、3、5、7、9 号块组开头的位置，那里的超级块要自洽、
/// 记的块组号就是这一组、按它自己的参数算出的位置正好在这里。读不出的位置跳过。
pub(crate) fn find_backup(device: &dyn BlockDevice, offset: u64, length: u64) -> Option<Vec<u8>> {
    for group in BACKUP_GROUPS {
        for block_size in BACKUP_BLOCK_SIZES {
            let first_block = u64::from(block_size == 1024);
            let Some(at) = block_size
                .checked_mul(8 * u64::from(group))
                .and_then(|block| block.checked_add(first_block))
                .and_then(|block| block.checked_mul(block_size))
            else {
                continue;
            };
            if at.saturating_add(SUPERBLOCK_SIZE as u64) > length {
                continue;
            }
            let Some(absolute) = offset.checked_add(at) else {
                continue;
            };
            let mut bytes = vec![0u8; SUPERBLOCK_SIZE];
            if device.read_exact_at(absolute, &mut bytes).is_err() {
                continue;
            }
            if let Some(superblock) = Superblock::parse(&bytes)
                && superblock.group_number == group
                && superblock.copy_offset() == Some(at)
            {
                return Some(bytes);
            }
        }
    }
    None
}

/// 丢失分区搜索的核对：按 `superblock`（在 `start` 起的分区里找到的那一份，主的或备份）打开得了卷，
/// 并且带校验和的卷上超级块与第 0 个块组描述符的校验和都对得上。打开卷的规则见 [`Ext::open`]。
pub fn ext_copy_confirmed(
    device: Arc<dyn BlockDevice>,
    start: u64,
    length: u64,
    superblock: &[u8],
) -> bool {
    Ext::open(Region::new(device, start, length), superblock).is_ok_and(|ext| ext.confirmed())
}

pub(crate) struct Ext {
    region: Region,
    superblock: Superblock,
    /// 用的这份块组描述符表的第一块：主超级块时紧跟在它后面，按备份打开时用那个块组里的备份表。
    table_block: u64,
    /// 块组描述符、位图、inode 表、间接块与 extent 索引块的分块缓存。
    meta: Chunks,
    root_generation: u32,
}

impl Ext {
    /// 按超级块 `bytes`（主的或备份）打开。第 0 个块组描述符要在卷里、根目录的 inode 要是目录。
    pub fn open(region: Region, bytes: &[u8]) -> Result<Self, FsError> {
        let superblock = Superblock::parse(bytes).ok_or(FsError::Invalid(
            "Linux 扩展文件系统（ext）超级块的参数不自洽",
        ))?;
        let table_block = superblock.descriptor_table_block().ok_or(FsError::Invalid(
            "Linux 扩展文件系统（ext）超级块记的块组号不对",
        ))?;
        let mut ext = Self {
            meta: Chunks::new(region.length()),
            region,
            superblock,
            table_block,
            root_generation: 0,
        };
        if ext.group(0)?.is_none() {
            return Err(FsError::Invalid(
                "Linux 扩展文件系统（ext）的块组描述符读不出或不对",
            ));
        }
        let root = ext
            .inode(ROOT_INODE)?
            .filter(Inode::is_dir)
            .ok_or(FsError::Invalid(
                "Linux 扩展文件系统（ext）根目录的 inode 读不出或不是目录",
            ))?;
        ext.root_generation = root.generation;
        Ok(ext)
    }

    /// 带校验和的卷上，超级块与第 0 个块组描述符的校验和都对得上（没有校验和时为 `true`）。
    fn confirmed(&self) -> bool {
        if !self.superblock.checksum_ok {
            return false;
        }
        let mut raw = vec![0u8; self.superblock.desc_size as usize];
        self.superblock
            .descriptor_offset(0, self.table_block)
            .is_some_and(|at| self.read_meta(at, &mut raw).is_ok())
            && self.superblock.descriptor_checksum_ok(0, &raw)
    }

    pub fn label(&self) -> Option<&str> {
        self.superblock.label.as_deref()
    }

    pub fn block_size(&self) -> u64 {
        self.superblock.block_size
    }

    pub fn root(&self) -> FileRef {
        FileRef {
            size: 0,
            is_dir: true,
            deleted: false,
            location: Location::Inode {
                inode: ROOT_INODE,
                generation: self.root_generation,
            },
            modified: None,
        }
    }

    fn read_meta(&self, at: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let inside = self
            .meta
            .read(at, buf, |begin, data| self.region.read_exact(begin, data))?;
        if inside {
            Ok(())
        } else {
            Err(FsError::OutOfRange { offset: at })
        }
    }

    /// 第 `number` 个块组的描述符；超出块组数、位置在分区之外或内容不对时为 `None`。
    fn group(&self, number: u32) -> Result<Option<Group>, FsError> {
        if number >= self.superblock.group_count {
            return Ok(None);
        }
        let Some(at) = self.superblock.descriptor_offset(number, self.table_block) else {
            return Ok(None);
        };
        let mut raw = vec![0u8; self.superblock.desc_size as usize];
        match self.read_meta(at, &mut raw) {
            Ok(()) => Ok(self.superblock.parse_group(&raw)),
            Err(FsError::OutOfRange { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// inode 所在的块组与它在块组里的序号。
    fn inode_slot(&self, number: u32) -> Option<(u32, u32)> {
        if number == 0 || number > self.superblock.inodes_count {
            return None;
        }
        let index = number - 1;
        let per_group = self.superblock.inodes_per_group;
        Some((index / per_group, index % per_group))
    }

    /// inode 在分区里的字节偏移。
    fn inode_offset(&self, number: u32) -> Result<Option<u64>, FsError> {
        let Some((group, index)) = self.inode_slot(number) else {
            return Ok(None);
        };
        let Some(group) = self.group(group)? else {
            return Ok(None);
        };
        if self.superblock.inodes_uninit(&group) {
            return Ok(None);
        }
        Ok(group
            .inode_table
            .checked_mul(self.superblock.block_size)
            .and_then(|table| {
                table.checked_add(u64::from(index) * u64::from(self.superblock.inode_size))
            }))
    }

    /// 读第 `number` 个 inode；号不合法、所在块组没有初始化、位置在分区之外时为 `None`。
    fn inode(&self, number: u32) -> Result<Option<Inode>, FsError> {
        let Some(at) = self.inode_offset(number)? else {
            return Ok(None);
        };
        let mut bytes = vec![0u8; self.superblock.inode_size as usize];
        match self.read_meta(at, &mut bytes) {
            Ok(()) => Ok(Inode::parse(
                &bytes,
                self.superblock.incompat & INCOMPAT_LARGEDIR != 0,
            )),
            Err(FsError::OutOfRange { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// 位图（从 `bitmap` 号块起）里第 `bit` 位是否为 1。
    fn bit(&self, bitmap: u64, bit: u64) -> Result<bool, FsError> {
        let at = bitmap
            .checked_mul(self.superblock.block_size)
            .and_then(|start| start.checked_add(bit / 8))
            .ok_or(FsError::OutOfRange { offset: bitmap })?;
        let mut byte = [0u8; 1];
        self.read_meta(at, &mut byte)?;
        let [byte] = byte;
        Ok(byte & (1 << (bit % 8)) != 0)
    }

    /// inode 在 inode 位图里是否在用；所在块组没有初始化时为空闲。
    fn inode_in_use(&self, number: u32) -> Result<bool, FsError> {
        let (group, index) = self.inode_slot(number).ok_or(FsError::BadRef)?;
        let group = self
            .group(group)?
            .ok_or(FsError::Invalid("块组描述符读不出"))?;
        if self.superblock.inodes_uninit(&group) {
            return Ok(false);
        }
        self.bit(group.inode_bitmap, u64::from(index))
    }

    /// 块在块位图里是否已分配；所在块组的块位图没有初始化时为空闲。
    fn block_in_use(&self, block: u64) -> Result<bool, FsError> {
        let (group, bit) = self
            .superblock
            .block_bit(block)
            .ok_or(FsError::OutOfRange { offset: block })?;
        let group = self
            .group(group)?
            .ok_or(FsError::Invalid("块组描述符读不出"))?;
        if self.superblock.blocks_uninit(&group) {
            return Ok(false);
        }
        self.bit(group.block_bitmap, bit)
    }

    fn mapping(&self, inode: &Inode) -> Mapping {
        let block_size = self.superblock.block_size;
        let read_block = |block: u64| -> Result<Vec<u8>, FsError> {
            let mut bytes = vec![0u8; block_size as usize];
            let at = block
                .checked_mul(block_size)
                .ok_or(FsError::OutOfRange { offset: block })?;
            self.read_meta(at, &mut bytes)?;
            Ok(bytes)
        };
        inode.mapping(&MapContext {
            block_size,
            first_data_block: u64::from(self.superblock.first_data_block),
            blocks_count: self.superblock.blocks_count,
            read_block: &read_block,
        })
    }

    /// 文件的内容按段：块之间的空洞与未初始化的段读作 0；映射完整时文件末尾没有块的部分也读作 0。
    /// inline data 与快速符号链接的内容在 inode 里。
    fn pieces(&self, inode: &Inode, mapping: &Mapping) -> Vec<Piece> {
        let size = inode.size;
        if inode.is_inline() || inode.is_fast_symlink() {
            let mut bytes = if inode.is_inline() {
                inode.inline_bytes()
            } else {
                inode.block_area().to_vec()
            };
            bytes.truncate(usize::try_from(size).unwrap_or(usize::MAX));
            return vec![Piece::Bytes(bytes)];
        }
        let block_size = self.superblock.block_size;
        let mut pieces = Vec::with_capacity(mapping.extents.len());
        let mut covered = 0u64;
        for extent in &mapping.extents {
            let Some(start) = extent.logical.checked_mul(block_size) else {
                break;
            };
            if start >= size {
                break;
            }
            if start > covered {
                pieces.push(Piece::Zeros(start - covered));
            }
            let length = extent.length.saturating_mul(block_size).min(size - start);
            if extent.unwritten {
                pieces.push(Piece::Zeros(length));
            } else {
                let Some(offset) = extent.physical.checked_mul(block_size) else {
                    break;
                };
                pieces.push(Piece::Disk { offset, length });
            }
            covered = start + length;
        }
        if mapping.complete && covered < size && size <= self.max_file_size(inode) {
            pieces.push(Piece::Zeros(size - covered));
        }
        pieces
    }

    /// 这种映射方式能表示的最大长度：extent 是 2^32 块，旧式映射是直接块加三级间接块。超过时末尾不补 0。
    fn max_file_size(&self, inode: &Inode) -> u64 {
        let block_size = self.superblock.block_size;
        let blocks = if inode.uses_extents() {
            1u64 << 32
        } else {
            // 12 个直接块，加一级、二级、三级间接块各管的块数
            let per_block = block_size / 4;
            [1u32, 2, 3]
                .iter()
                .map(|&level| per_block.saturating_pow(level))
                .fold(12u64, u64::saturating_add)
        };
        blocks.saturating_mul(block_size)
    }

    /// 删除时被清空了：不在 inode 里存放内容，块指针或 extent 一个都没有（结构本身完好）。
    fn cleared(inode: &Inode, mapping: &Mapping) -> bool {
        !inode.is_inline()
            && !inode.is_fast_symlink()
            && mapping.complete
            && mapping.extents.is_empty()
            && mapping.meta_blocks.is_empty()
    }

    /// 定位信息对应的 inode：版本号要对得上；未删除的项要还有链接，已删除的项的 inode 要还空着。
    fn checked(&self, file: &FileRef) -> Result<(u32, Inode), FsError> {
        let Location::Inode {
            inode: number,
            generation,
        } = file.location
        else {
            return Err(FsError::BadRef);
        };
        let inode = self
            .inode(number)?
            .ok_or(FsError::Invalid("inode 读不出"))?;
        let reused = if file.deleted {
            self.inode_in_use(number)?
        } else {
            inode.links == 0
        };
        if inode.generation != generation || reused {
            return Err(FsError::Invalid(REUSED));
        }
        Ok((number, inode))
    }

    pub fn list(&self, dir: &FileRef) -> Result<Listing, FsError> {
        if !dir.is_dir {
            return Err(FsError::BadRef);
        }
        let (number, inode) = self.checked(dir)?;
        if !inode.is_dir() {
            return Err(FsError::Invalid(REUSED));
        }
        let format = DirFormat {
            file_type: self.superblock.incompat & INCOMPAT_FILETYPE != 0,
            big_blocks: self.superblock.block_size >= 1 << 16,
            first_inode: self.superblock.first_inode,
            inodes_count: self.superblock.inodes_count,
            indexed: inode.flags & FLAG_INDEX != 0,
        };
        let overwritten = Listing {
            entries: Vec::new(),
            truncated: false,
            overwritten: true,
        };
        let mut raw = Vec::new();
        if inode.is_inline() {
            // 开头 4 字节是上级目录的 inode 号，没有 `.` 与 `..`
            parse_block(
                inode.block_area().get(4..I_BLOCK_SIZE).unwrap_or_default(),
                &format,
                &mut raw,
            );
            if let Some(rest) = inode.system_data() {
                parse_block(rest, &format, &mut raw);
            }
        } else {
            let mapping = self.mapping(&inode);
            if dir.deleted {
                if Self::cleared(&inode, &mapping) {
                    return Err(FsError::Invalid(CLEARED));
                }
                let first = mapping.extents.first().map(|extent| extent.physical);
                if first.is_none_or(|block| self.block_in_use(block).unwrap_or(true)) {
                    return Ok(overwritten);
                }
            }
            let bytes = self.dir_bytes(&inode, &mapping);
            if dir.deleted && !starts_with_dot(&bytes, number) {
                return Ok(overwritten);
            }
            let block_size = self.superblock.block_size as usize;
            for block in bytes.chunks(block_size) {
                parse_block(block, &format, &mut raw);
            }
        }
        let mut entries = Vec::new();
        let mut truncated = false;
        for item in raw {
            if entries.len() >= MAX_ENTRIES {
                truncated = true;
                break;
            }
            entries.push(self.entry(item, dir.deleted));
        }
        Ok(Listing {
            entries,
            truncated,
            overwritten: false,
        })
    }

    /// 目录的内容，最多 [`MAX_DIR_BYTES`]；读到读不出的地方为止。
    fn dir_bytes(&self, inode: &Inode, mapping: &Mapping) -> Vec<u8> {
        let size = inode.size.min(MAX_DIR_BYTES);
        let pieces = self.pieces(inode, mapping);
        let mut reader = FileReader::from_pieces(self.region.clone(), pieces, size, size);
        let mut bytes = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(read) => bytes.extend_from_slice(buf.get(..read).unwrap_or_default()),
            }
        }
        bytes
    }

    /// 目录项换成列表项：类型、长度、时间取 inode；删除的项判断还能不能找回。
    fn entry(&self, raw: RawEntry, parent_deleted: bool) -> Entry {
        let name = String::from_utf8_lossy(&raw.name).into_owned();
        let deleted = raw.deleted || parent_deleted;
        let mut inode = self.inode(raw.inode).ok().flatten();
        let (state, mapping) = if deleted {
            self.deleted_state(raw.inode, inode.as_ref(), raw.file_type)
        } else {
            (State::Live, None)
        };
        // inode 已经给了别的文件：长度、时间都不是这个文件的
        if state == State::Overwritten && mapping.is_none() {
            inode = None;
        }
        let is_dir = match &inode {
            Some(inode) if inode.mode != 0 => inode.is_dir(),
            _ => raw.file_type == 2,
        };
        let block_size = self.superblock.block_size;
        let position = match (&inode, &mapping) {
            (Some(inode), _) if inode.is_inline() || inode.is_fast_symlink() => self
                .inode_offset(raw.inode)
                .ok()
                .flatten()
                .and_then(|at| at.checked_add(I_BLOCK_AT as u64)),
            (_, Some(mapping)) => mapping
                .extents
                .iter()
                .find(|extent| !extent.unwritten)
                .and_then(|extent| extent.physical.checked_mul(block_size)),
            (Some(inode), None) => inode
                .first_block()
                .and_then(|block| block.checked_mul(block_size)),
            (None, None) => None,
        };
        let size = inode.as_ref().map_or(0, |inode| inode.size);
        let file = FileRef {
            size,
            is_dir,
            deleted,
            location: Location::Inode {
                inode: raw.inode,
                generation: inode.as_ref().map_or(0, |inode| inode.generation),
            },
            modified: inode.as_ref().and_then(|inode| inode.modified),
        };
        Entry {
            short_name: None,
            name_uncertain: false,
            size: if is_dir { 0 } else { size },
            modified: file.modified.and_then(|stamp| stamp.text()),
            created: inode
                .as_ref()
                .and_then(|inode| inode.created)
                .and_then(|stamp| stamp.text()),
            state,
            position: position.filter(|_| is_dir || size > 0),
            unsupported: inode.as_ref().and_then(unsupported),
            attributes: FileAttributes::from_unix_name(&name),
            name,
            file,
        }
    }

    /// 已删除的项还能不能找回，以及算出的块映射（inode 已被重用时没有）。
    fn deleted_state(
        &self,
        number: u32,
        inode: Option<&Inode>,
        file_type: u8,
    ) -> (State, Option<Mapping>) {
        let Some(inode) = inode else {
            return (State::Unknown, None);
        };
        match self.inode_in_use(number) {
            Ok(true) => return (State::Overwritten, None),
            Err(_) => return (State::Unknown, None),
            Ok(false) => {}
        }
        // inode 记的类型与目录项的不同：这个 inode 后来给过别的文件
        if file_type != 0 && inode.file_type() != 0 && inode.file_type() != file_type {
            return (State::Overwritten, None);
        }
        let mapping = self.mapping(inode);
        if inode.is_inline() || inode.is_fast_symlink() {
            let state = if inode.size > 0 {
                State::Intact
            } else {
                State::Cleared
            };
            return (state, Some(mapping));
        }
        if Self::cleared(inode, &mapping) {
            return (State::Cleared, Some(mapping));
        }
        if !mapping.complete {
            return (State::Unknown, Some(mapping));
        }
        let mut checked = 0u64;
        let data = mapping
            .extents
            .iter()
            .map(|extent| (extent.physical, extent.length));
        let meta = mapping.meta_blocks.iter().map(|&block| (block, 1));
        for (first, count) in data.chain(meta) {
            for block in (0..count).map(|step| first.saturating_add(step)) {
                checked += 1;
                if checked > MAX_STATE_BLOCKS {
                    return (State::Unknown, Some(mapping));
                }
                match self.block_in_use(block) {
                    Ok(false) => {}
                    Ok(true) => return (State::Overwritten, Some(mapping)),
                    Err(_) => return (State::Unknown, Some(mapping)),
                }
            }
        }
        (State::Intact, Some(mapping))
    }

    pub fn open_file(&self, file: &FileRef) -> Result<FileReader, FsError> {
        if file.is_dir {
            return Err(FsError::BadRef);
        }
        let (_, inode) = self.checked(file)?;
        if inode.is_dir() {
            return Err(FsError::BadRef);
        }
        if let Some(reason) = unsupported(&inode) {
            return Err(FsError::Unreadable(reason));
        }
        let mapping = self.mapping(&inode);
        if file.deleted && Self::cleared(&inode, &mapping) {
            return Err(FsError::Invalid(CLEARED));
        }
        let pieces = self.pieces(&inode, &mapping);
        Ok(FileReader::from_pieces(
            self.region.clone(),
            pieces,
            inode.size,
            inode.size,
        ))
    }
}

/// 读不出原样内容的：fscrypt 加密的文件是密文；符号链接的内容是它指向的路径，不是文件本身。
fn unsupported(inode: &Inode) -> Option<Unsupported> {
    if inode.is_symlink() {
        Some(Unsupported::Reparse)
    } else if inode.flags & FLAG_ENCRYPT != 0 && !inode.is_dir() {
        Some(Unsupported::Encrypted)
    } else {
        None
    }
}

/// 目录内容开头还是指向自己的 `.`：删除的目录的块没被别的内容写过。
fn starts_with_dot(bytes: &[u8], number: u32) -> bool {
    le32(bytes, 0) == Some(number)
        && le16(bytes, 4).is_some_and(|length| length >= 12)
        && bytes.get(6) == Some(&1)
        && bytes.get(8) == Some(&b'.')
}

/// 读主超级块（分区第 1024 字节起的 1024 字节）。
pub(crate) fn read_primary(region: &Region) -> Result<Vec<u8>, FsError> {
    let mut bytes = vec![0u8; SUPERBLOCK_SIZE];
    region.read_exact(SUPERBLOCK_OFFSET, &mut bytes)?;
    Ok(bytes)
}
