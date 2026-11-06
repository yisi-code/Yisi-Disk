//! 超级块与块组描述符。主超级块在分区第 1024 字节处；各块组开头另有备份（sparse_super 时只在 0、1 号与 3、5、7 的幂号
//! 块组，sparse_super2 时只在超级块记的两个块组），备份所在块组号记在备份自己的 `s_block_group_nr` 里。
//! 打开卷、识别、按备份打开与丢失分区搜索都按这里的规则判断超级块能不能用。

use yisi_part::bytes::{le16, le32};
use yisi_part::crc32::{crc16, crc32c};

/// 主超级块在分区里的位置与超级块的长度。
pub(crate) const SUPERBLOCK_OFFSET: u64 = 1024;
pub(crate) const SUPERBLOCK_SIZE: usize = 1024;
/// 魔数 0xEF53 在超级块第 0x38 字节。
const MAGIC_AT: usize = 0x38;
const MAGIC: u16 = 0xEF53;
/// 超级块自己的校验和在最后 4 字节，覆盖之前的全部内容。
const CHECKSUM_AT: usize = 0x3FC;
/// 规范给出的块大小上限 64 KiB（`s_log_block_size` 最大 6）。
const MAX_LOG_BLOCK: u32 = 6;
/// rev 0 没有记 inode 大小与第一个可用 inode。
const GOOD_OLD_INODE_SIZE: u32 = 128;
const GOOD_OLD_FIRST_INODE: u32 = 11;

const COMPAT_SPARSE_SUPER2: u32 = 0x0200;
pub(crate) const INCOMPAT_FILETYPE: u32 = 0x0002;
/// 外部日志设备：只放日志，不是文件系统。
const INCOMPAT_JOURNAL_DEV: u32 = 0x0008;
const INCOMPAT_META_BG: u32 = 0x0010;
const INCOMPAT_64BIT: u32 = 0x0080;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
pub(crate) const INCOMPAT_LARGEDIR: u32 = 0x4000;
const RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
const RO_COMPAT_GDT_CSUM: u32 = 0x0010;
const RO_COMPAT_BIGALLOC: u32 = 0x0200;
const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;

/// 块组描述符的标志：inode 表与 inode 位图没有初始化（全部空闲）、块位图没有初始化（只有元数据占用）。
const BG_INODE_UNINIT: u16 = 0x0001;
const BG_BLOCK_UNINIT: u16 = 0x0002;
/// 描述符里校验和的位置。
const DESCRIPTOR_CHECKSUM_AT: usize = 0x1E;

/// 超级块里打开卷要用的参数，都已核对过范围。
#[derive(Debug, Clone)]
pub(crate) struct Superblock {
    pub inodes_count: u32,
    pub blocks_count: u64,
    pub first_data_block: u32,
    pub block_size: u64,
    /// 一簇几块的以 2 为底的对数（bigalloc 时大于 0）：块位图一位管一簇。
    pub cluster_bits: u32,
    pub blocks_per_group: u32,
    pub clusters_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: u32,
    pub first_inode: u32,
    /// 这份超级块所在的块组（主超级块为 0）。
    pub group_number: u32,
    pub compat: u32,
    pub incompat: u32,
    pub ro_compat: u32,
    pub uuid: [u8; 16],
    pub label: Option<String>,
    pub desc_size: u32,
    pub first_meta_bg: u32,
    pub backup_groups: [u32; 2],
    pub checksum_seed: u32,
    pub group_count: u32,
    /// 带 metadata_csum 时超级块自己的校验和对得上；没有这个特性时为 `true`。
    pub checksum_ok: bool,
}

/// 一个块组描述符里用得着的几项。块号都相对分区开头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Group {
    pub block_bitmap: u64,
    pub inode_bitmap: u64,
    pub inode_table: u64,
    pub flags: u16,
}

/// 丢失分区搜索用：一份超级块（主的或备份）说明的分区。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtCopy {
    /// 这份超级块在分区里的字节偏移：主超级块是 1024，备份是它所在块组的开头。
    pub offset: u64,
    /// 卷的字节数（块数乘块大小）。
    pub volume_bytes: u64,
    /// 所在的块组；为 0 时是主超级块。
    pub group: u32,
}

/// `bytes`（超级块的 1024 字节）是自洽的 ext2/3/4 超级块时，给出它在分区里的位置与卷的长度。
/// 丢失分区搜索按它推算分区起点，规则与打开卷相同（[`Superblock::parse`]）。
pub fn ext_superblock_copy(bytes: &[u8]) -> Option<ExtCopy> {
    let superblock = Superblock::parse(bytes)?;
    Some(ExtCopy {
        offset: superblock.copy_offset()?,
        volume_bytes: superblock.volume_bytes()?,
        group: superblock.group_number,
    })
}

/// 超级块开头（或它所在的第一个 512 字节扇区）里有 ext 的魔数：丢失分区搜索先按它筛一遍，再交给 [`ext_superblock_copy`]。
pub fn has_magic(bytes: &[u8]) -> bool {
    le16(bytes, MAGIC_AT) == Some(MAGIC)
}

impl Superblock {
    /// 解析超级块；魔数不对、参数不自洽（块大小、每组块数与 inode 数、inode 大小、块组数与 inode 总数对不上等）、
    /// 或是外部日志设备时为 `None`。校验和不对不拒绝（`checksum_ok` 记下），留给调用方决定。
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let bytes = bytes.get(..SUPERBLOCK_SIZE)?;
        if !has_magic(bytes) {
            return None;
        }
        let incompat = le32(bytes, 0x60)?;
        if incompat & INCOMPAT_JOURNAL_DEV != 0 {
            return None;
        }
        let compat = le32(bytes, 0x5C)?;
        let ro_compat = le32(bytes, 0x64)?;
        let log_block = le32(bytes, 0x18)?;
        if log_block > MAX_LOG_BLOCK {
            return None;
        }
        let block_size = 1024u64 << log_block;
        let (inode_size, first_inode) = match le32(bytes, 0x4C)? {
            0 => (GOOD_OLD_INODE_SIZE, GOOD_OLD_FIRST_INODE),
            _ => (u32::from(le16(bytes, 0x58)?), le32(bytes, 0x54)?),
        };
        if !inode_size.is_power_of_two()
            || inode_size < GOOD_OLD_INODE_SIZE
            || u64::from(inode_size) > block_size
        {
            return None;
        }
        let blocks_per_group = le32(bytes, 0x20)?;
        let (cluster_bits, clusters_per_group) = if ro_compat & RO_COMPAT_BIGALLOC != 0 {
            let log_cluster = le32(bytes, 0x1C)?;
            let bits = log_cluster.checked_sub(log_block)?;
            (bits, le32(bytes, 0x24)?)
        } else {
            (0, blocks_per_group)
        };
        let bits_per_bitmap = block_size.checked_mul(8)?;
        if cluster_bits > 16
            || clusters_per_group == 0
            || u64::from(clusters_per_group) > bits_per_bitmap
            || u64::from(clusters_per_group) << cluster_bits != u64::from(blocks_per_group)
            || !blocks_per_group.is_multiple_of(8)
        {
            return None;
        }
        let inodes_per_group = le32(bytes, 0x28)?;
        let inodes_per_block = block_size / u64::from(inode_size);
        if u64::from(inodes_per_group) < inodes_per_block
            || u64::from(inodes_per_group) > bits_per_bitmap
        {
            return None;
        }
        let is_64bit = incompat & INCOMPAT_64BIT != 0;
        let blocks_count = u64::from(le32(bytes, 0x04)?)
            | if is_64bit {
                u64::from(le32(bytes, 0x150)?) << 32
            } else {
                0
            };
        // 1 KiB 块时 0 号块是引导块，超级块在 1 号块；更大的块（与 bigalloc）时超级块在 0 号块里
        let first_data_block = le32(bytes, 0x14)?;
        let expected_first = u32::from(block_size == 1024 && cluster_bits == 0);
        if first_data_block != expected_first || blocks_count <= u64::from(first_data_block) {
            return None;
        }
        let group_count =
            (blocks_count - u64::from(first_data_block)).div_ceil(u64::from(blocks_per_group));
        let group_count = u32::try_from(group_count).ok()?;
        let inodes_count = le32(bytes, 0x00)?;
        if u64::from(group_count) * u64::from(inodes_per_group) != u64::from(inodes_count)
            || first_inode < GOOD_OLD_FIRST_INODE
            || first_inode >= inodes_count
        {
            return None;
        }
        let desc_size = if is_64bit {
            let size = u32::from(le16(bytes, 0xFE)?);
            if !size.is_power_of_two() || !(64..=1024).contains(&size) {
                return None;
            }
            size
        } else {
            32
        };
        let uuid: [u8; 16] = bytes.get(0x68..0x78)?.try_into().ok()?;
        let label = bytes.get(0x78..0x88).and_then(|raw| {
            let end = raw.iter().position(|&byte| byte == 0).unwrap_or(raw.len());
            let text = String::from_utf8_lossy(raw.get(..end)?).trim().to_owned();
            (!text.is_empty()).then_some(text)
        });
        let checksum_seed = if incompat & INCOMPAT_CSUM_SEED != 0 {
            le32(bytes, 0x270)?
        } else {
            crc32c(!0, &[&uuid])
        };
        let checksum_ok = ro_compat & RO_COMPAT_METADATA_CSUM == 0
            || le32(bytes, CHECKSUM_AT)? == crc32c(!0, &[bytes.get(..CHECKSUM_AT)?]);
        Some(Self {
            inodes_count,
            blocks_count,
            first_data_block,
            block_size,
            cluster_bits,
            blocks_per_group,
            clusters_per_group,
            inodes_per_group,
            inode_size,
            first_inode,
            group_number: u32::from(le16(bytes, 0x5A)?),
            compat,
            incompat,
            ro_compat,
            uuid,
            label,
            desc_size,
            first_meta_bg: le32(bytes, 0x104)?,
            backup_groups: [le32(bytes, 0x24C)?, le32(bytes, 0x250)?],
            checksum_seed,
            group_count,
            checksum_ok,
        })
    }

    /// 块组描述符带校验和（gdt_csum 或 metadata_csum）：这时「未初始化」的标志才算数。
    fn has_group_checksums(&self) -> bool {
        self.ro_compat & (RO_COMPAT_GDT_CSUM | RO_COMPAT_METADATA_CSUM) != 0
    }

    pub fn volume_bytes(&self) -> Option<u64> {
        self.blocks_count.checked_mul(self.block_size)
    }

    /// 第 `group` 个块组的第一块。
    pub fn group_first_block(&self, group: u32) -> Option<u64> {
        u64::from(group)
            .checked_mul(u64::from(self.blocks_per_group))?
            .checked_add(u64::from(self.first_data_block))
    }

    /// 这份超级块在分区里的字节偏移：主超级块在 1024 字节处，备份在所在块组的第一块开头。
    pub fn copy_offset(&self) -> Option<u64> {
        if self.group_number == 0 {
            return Some(SUPERBLOCK_OFFSET);
        }
        if self.group_number >= self.group_count {
            return None;
        }
        self.group_first_block(self.group_number)?
            .checked_mul(self.block_size)
    }

    /// 这份超级块后面紧跟的块组描述符表的第一块。
    pub fn descriptor_table_block(&self) -> Option<u64> {
        let block = if self.group_number == 0 {
            u64::from(self.first_data_block)
        } else {
            self.group_first_block(self.group_number)?
        };
        block.checked_add(1)
    }

    /// 第 `group` 个块组开头有没有超级块的备份（0 号块组是主超级块）。
    pub fn has_super(&self, group: u32) -> bool {
        if group == 0 {
            return true;
        }
        if self.compat & COMPAT_SPARSE_SUPER2 != 0 {
            return self.backup_groups.contains(&group);
        }
        if self.ro_compat & RO_COMPAT_SPARSE_SUPER == 0 || group == 1 {
            return true;
        }
        [3u32, 5, 7].iter().any(|&base| {
            let mut power = base;
            while power < group {
                match power.checked_mul(base) {
                    Some(next) => power = next,
                    None => return false,
                }
            }
            power == group
        })
    }

    /// 第 `group` 个描述符在分区里的字节偏移；`table_block` 是这份描述符表的第一块（[`descriptor_table_block`]）。
    /// meta_bg 时，第 `first_meta_bg` 块以后的描述符块放在各自那一组块组（元块组）的第一个块组里。
    ///
    /// [`descriptor_table_block`]: Self::descriptor_table_block
    pub fn descriptor_offset(&self, group: u32, table_block: u64) -> Option<u64> {
        let per_block = u32::try_from(self.block_size / u64::from(self.desc_size)).ok()?;
        let index = group / per_block;
        let block = if self.incompat & INCOMPAT_META_BG == 0 || index < self.first_meta_bg {
            table_block.checked_add(u64::from(index))?
        } else {
            let first = index.checked_mul(per_block)?;
            self.group_first_block(first)?
                .checked_add(u64::from(self.has_super(first)))?
        };
        block
            .checked_mul(self.block_size)?
            .checked_add(u64::from(group % per_block) * u64::from(self.desc_size))
    }

    /// 解析一个块组描述符；位图与 inode 表不在卷里时为 `None`。
    pub fn parse_group(&self, raw: &[u8]) -> Option<Group> {
        let wide = self.desc_size >= 64;
        let join = |low_at: usize, high_at: usize| -> Option<u64> {
            let low = u64::from(le32(raw, low_at)?);
            let high = if wide {
                u64::from(le32(raw, high_at)?)
            } else {
                0
            };
            Some(low | high << 32)
        };
        let group = Group {
            block_bitmap: join(0x00, 0x20)?,
            inode_bitmap: join(0x04, 0x24)?,
            inode_table: join(0x08, 0x28)?,
            flags: le16(raw, 0x12)?,
        };
        let table_blocks = (u64::from(self.inodes_per_group) * u64::from(self.inode_size))
            .div_ceil(self.block_size);
        let inside = |block: u64, count: u64| {
            block >= u64::from(self.first_data_block)
                && block
                    .checked_add(count)
                    .is_some_and(|end| end <= self.blocks_count)
        };
        (inside(group.block_bitmap, 1)
            && inside(group.inode_bitmap, 1)
            && inside(group.inode_table, table_blocks))
        .then_some(group)
    }

    /// 描述符的校验和对得上（没有 gdt_csum、metadata_csum 时一律算对）。
    pub fn descriptor_checksum_ok(&self, group: u32, raw: &[u8]) -> bool {
        let Some(raw) = raw.get(..self.desc_size as usize) else {
            return false;
        };
        let (Some(before), Some(after), Some(stored)) = (
            raw.get(..DESCRIPTOR_CHECKSUM_AT),
            raw.get(DESCRIPTOR_CHECKSUM_AT + 2..),
            le16(raw, DESCRIPTOR_CHECKSUM_AT),
        ) else {
            return false;
        };
        let number = group.to_le_bytes();
        if self.ro_compat & RO_COMPAT_METADATA_CSUM != 0 {
            let crc = crc32c(self.checksum_seed, &[&number, before, &[0, 0], after]);
            stored == crc as u16
        } else if self.ro_compat & RO_COMPAT_GDT_CSUM != 0 {
            stored == crc16(0xFFFF, &[&self.uuid, &number, before, after])
        } else {
            true
        }
    }

    /// 块组的 inode 表与 inode 位图没有初始化（全部空闲）。
    pub fn inodes_uninit(&self, group: &Group) -> bool {
        self.has_group_checksums() && group.flags & BG_INODE_UNINIT != 0
    }

    /// 块组的块位图没有初始化（除了元数据都空闲）。
    pub fn blocks_uninit(&self, group: &Group) -> bool {
        self.has_group_checksums() && group.flags & BG_BLOCK_UNINIT != 0
    }

    /// 块所在的块组与它在块位图里是第几位（bigalloc 时一位管一簇）。
    pub fn block_bit(&self, block: u64) -> Option<(u32, u64)> {
        let cluster = block.checked_sub(u64::from(self.first_data_block))? >> self.cluster_bits;
        let group = u32::try_from(cluster / u64::from(self.clusters_per_group)).ok()?;
        (group < self.group_count).then_some((group, cluster % u64::from(self.clusters_per_group)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_super_groups_are_powers_of_three_five_and_seven() {
        let mut bytes = vec![0u8; SUPERBLOCK_SIZE];
        bytes[0x38..0x3A].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[0x00..0x04].copy_from_slice(&(64u32 * 128).to_le_bytes());
        bytes[0x04..0x08].copy_from_slice(&(64u32 * 8192 + 1).to_le_bytes());
        bytes[0x14..0x18].copy_from_slice(&1u32.to_le_bytes());
        bytes[0x20..0x24].copy_from_slice(&8192u32.to_le_bytes());
        bytes[0x28..0x2C].copy_from_slice(&128u32.to_le_bytes());
        bytes[0x64..0x68].copy_from_slice(&RO_COMPAT_SPARSE_SUPER.to_le_bytes());
        let superblock = Superblock::parse(&bytes).unwrap();
        let groups: Vec<u32> = (0..64)
            .filter(|&group| superblock.has_super(group))
            .collect();
        assert_eq!(groups, [0, 1, 3, 5, 7, 9, 25, 27, 49]);
        // inode 总数与块组数对不上、首个数据块不对、块大小过大都不认
        for (at, value) in [(0x00, 1u32), (0x14, 0), (0x18, 7)] {
            let mut bad = bytes.clone();
            bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
            assert!(Superblock::parse(&bad).is_none(), "{at:#x}");
        }
    }

    #[test]
    fn extreme_values_are_rejected_without_overflow() {
        // 64 位块数：块数巨大、64 KiB 块，块组数超过 32 位
        let mut bytes = vec![0u8; SUPERBLOCK_SIZE];
        bytes[0x38..0x3A].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[0x18..0x1C].copy_from_slice(&6u32.to_le_bytes());
        bytes[0x20..0x24].copy_from_slice(&8u32.to_le_bytes());
        bytes[0x28..0x2C].copy_from_slice(&(1u32 << 19).to_le_bytes());
        bytes[0x04..0x08].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[0x150..0x154].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[0x60..0x64].copy_from_slice(&INCOMPAT_64BIT.to_le_bytes());
        bytes[0xFE..0x100].copy_from_slice(&64u16.to_le_bytes());
        assert!(Superblock::parse(&bytes).is_none());
        assert!(ext_superblock_copy(&bytes).is_none());
        // 全 0xFF（只留魔数）与 bigalloc 的簇比块还小
        let mut ones = vec![0xFFu8; SUPERBLOCK_SIZE];
        ones[0x38..0x3A].copy_from_slice(&MAGIC.to_le_bytes());
        assert!(Superblock::parse(&ones).is_none());
        ones[0x18..0x1C].copy_from_slice(&2u32.to_le_bytes());
        ones[0x1C..0x20].copy_from_slice(&1u32.to_le_bytes());
        assert!(Superblock::parse(&ones).is_none());
        assert!(Superblock::parse(&ones[..100]).is_none());
    }

    #[test]
    fn copies_far_out_have_no_position() {
        let mut bytes = vec![0u8; SUPERBLOCK_SIZE];
        bytes[0x38..0x3A].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[0x00..0x04].copy_from_slice(&(2u32 * 128).to_le_bytes());
        bytes[0x04..0x08].copy_from_slice(&(2u32 * 8192 + 1).to_le_bytes());
        bytes[0x14..0x18].copy_from_slice(&1u32.to_le_bytes());
        bytes[0x20..0x24].copy_from_slice(&8192u32.to_le_bytes());
        bytes[0x28..0x2C].copy_from_slice(&128u32.to_le_bytes());
        // 记的块组号超出块组数：不是这个卷的备份
        bytes[0x5A..0x5C].copy_from_slice(&u16::MAX.to_le_bytes());
        let superblock = Superblock::parse(&bytes).unwrap();
        assert_eq!(superblock.copy_offset(), None);
        assert!(ext_superblock_copy(&bytes).is_none());
        assert_eq!(superblock.block_bit(0), None);
        assert_eq!(superblock.block_bit(u64::MAX), None);
        assert!(superblock.descriptor_offset(u32::MAX, u64::MAX).is_none());
    }
}
