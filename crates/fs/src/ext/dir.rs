//! 目录块：线性目录项（inode 号、项长、名字长度、文件类型、名字），以及删除后残留在空隙里的目录项。
//!
//! 删除一项时，它的项长并进前一项（是块里第一项时只把 inode 号清零），名字与 inode 号原样留在前一项的空隙里；
//! 新项插进空隙时从前一项名字之后开始放。所以每一项名字之后到项长末尾的空隙里，逐 4 字节找长得像目录项的残留。
//! htree 索引目录的索引块（第一块 `..` 之后、内部节点整块）不是目录项，不在里面找；metadata_csum 时块末尾
//! 12 字节的校验项跳过。

use yisi_part::bytes::{le16, le32};

/// 目录项头：inode 号 4、项长 2、名字长度 1、文件类型 1。
const HEADER: usize = 8;
/// metadata_csum 时块末尾的校验项：inode 号 0、项长 12、名字长度 0、类型 0xDE。
const TAIL_LENGTH: usize = 12;
const TAIL_TYPE: u8 = 0xDE;
/// 文件类型的最大值（7 是符号链接）。
const MAX_FILE_TYPE: u8 = 7;

/// 一个目录项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawEntry {
    pub inode: u32,
    pub name: Vec<u8>,
    /// 目录项里的文件类型（没有 filetype 特性时为 0）。
    pub file_type: u8,
    /// 在空隙里找到的残留。
    pub deleted: bool,
}

/// 解析目录块要知道的卷参数。
#[derive(Debug, Clone, Copy)]
pub(crate) struct DirFormat {
    /// 目录项带文件类型（filetype 特性）；没有时名字长度是 16 位。
    pub file_type: bool,
    /// 64 KiB 的块：项长按规范的写法换算。
    pub big_blocks: bool,
    /// 残留项的 inode 号要在 `first_inode..=inodes_count` 里（保留的 inode 不会被删）。
    pub first_inode: u32,
    pub inodes_count: u32,
    /// htree 索引目录：索引块不是目录项。
    pub indexed: bool,
}

/// 读一项的头：inode 号、项长、名字长度、文件类型。
fn header(bytes: &[u8], at: usize, format: &DirFormat) -> Option<(u32, usize, usize, u8)> {
    let inode = le32(bytes, at)?;
    let raw = le16(bytes, at + 4)?;
    let length = if format.big_blocks {
        match raw {
            0 | 0xFFFF => 1 << 16,
            _ => usize::from(raw & 0xFFFC) | usize::from(raw & 3) << 16,
        }
    } else {
        usize::from(raw)
    };
    let (name_length, file_type) = if format.file_type {
        (usize::from(*bytes.get(at + 6)?), *bytes.get(at + 7)?)
    } else {
        (usize::from(le16(bytes, at + 6)?), 0)
    };
    Some((inode, length, name_length, file_type))
}

/// 名字占的长度（头加名字，按 4 对齐）。
fn used(name_length: usize) -> usize {
    (HEADER + name_length + 3) & !3
}

/// 解析一块目录（或 inline 目录的一段），未删除的项与空隙里的残留按位置先后放进 `out`。`.` 与 `..` 不放。
pub(crate) fn parse_block(block: &[u8], format: &DirFormat, out: &mut Vec<RawEntry>) {
    let mut at = 0usize;
    let mut first = true;
    while at + HEADER <= block.len() {
        let Some((inode, length, name_length, file_type)) = header(block, at, format) else {
            return;
        };
        let end = at.saturating_add(length);
        if length < HEADER || length % 4 != 0 || end > block.len() || HEADER + name_length > length
        {
            // 这一块的结构坏了：后面找不到项的边界
            return;
        }
        if inode == 0 && name_length == 0 {
            // 校验项、htree 的内部节点（整块一项）；普通目录里整块空着的块照样看空隙
            let tail = length == TAIL_LENGTH && file_type == TAIL_TYPE;
            let index_node = first && format.indexed && length == block.len();
            if !tail && !index_node {
                scan_slack(block, at + HEADER, end, format, out);
            }
        } else {
            let name = block
                .get(at + HEADER..at + HEADER + name_length)
                .unwrap_or_default();
            let dot = name == b"." || name == b"..";
            if inode != 0 && !dot {
                out.push(RawEntry {
                    inode,
                    name: name.to_vec(),
                    file_type,
                    deleted: false,
                });
            }
            // htree 根块：`..` 的项长盖住了后面的索引
            let index_root = format.indexed && name == b"..";
            if !index_root {
                scan_slack(block, at + used(name_length), end, format, out);
            }
        }
        first = false;
        at = end;
    }
}

/// 在 `from..end` 的空隙里逐 4 字节找残留的目录项。
fn scan_slack(block: &[u8], from: usize, end: usize, format: &DirFormat, out: &mut Vec<RawEntry>) {
    let mut at = from;
    while at + HEADER <= end {
        match deleted_at(block, at, end, format) {
            Some((entry, skip)) => {
                out.push(entry);
                at += skip;
            }
            None => at += 4,
        }
    }
}

/// `at` 处是不是一个说得通的残留项：inode 号在用户 inode 的范围里、名字在空隙之内、项长按 4 对齐且不超出块、
/// 文件类型合法、名字是不含 `/` 与 NUL 的 UTF-8。是的话给出这一项和名字占的长度。
fn deleted_at(
    block: &[u8],
    at: usize,
    end: usize,
    format: &DirFormat,
) -> Option<(RawEntry, usize)> {
    let (inode, length, name_length, file_type) = header(block, at, format)?;
    let skip = used(name_length);
    let plausible = inode >= format.first_inode
        && inode <= format.inodes_count
        && (1..=255).contains(&name_length)
        && at + HEADER + name_length <= end
        && length >= skip
        && length % 4 == 0
        && at
            .checked_add(length)
            .is_some_and(|stop| stop <= block.len())
        && file_type <= MAX_FILE_TYPE;
    if !plausible {
        return None;
    }
    let name = block.get(at + HEADER..at + HEADER + name_length)?;
    let text = std::str::from_utf8(name).ok()?;
    if text.contains(['/', '\0']) || text == "." || text == ".." {
        return None;
    }
    Some((
        RawEntry {
            inode,
            name: name.to_vec(),
            file_type,
            deleted: true,
        },
        skip,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORMAT: DirFormat = DirFormat {
        file_type: true,
        big_blocks: false,
        first_inode: 11,
        inodes_count: 1000,
        indexed: false,
    };

    fn put(block: &mut [u8], at: usize, inode: u32, length: u16, name: &[u8], kind: u8) {
        block[at..at + 4].copy_from_slice(&inode.to_le_bytes());
        block[at + 4..at + 6].copy_from_slice(&length.to_le_bytes());
        block[at + 6] = name.len() as u8;
        block[at + 7] = kind;
        block[at + 8..at + 8 + name.len()].copy_from_slice(name);
    }

    fn names(entries: &[RawEntry]) -> Vec<(String, bool)> {
        entries
            .iter()
            .map(|entry| {
                (
                    String::from_utf8_lossy(&entry.name).into_owned(),
                    entry.deleted,
                )
            })
            .collect()
    }

    #[test]
    fn deleted_entries_are_found_in_the_slack() {
        let mut block = vec![0u8; 1024];
        put(&mut block, 0, 2, 12, b".", 2);
        put(&mut block, 12, 2, 12, b"..", 2);
        // a.txt 的项长盖住了删掉的 gone.bin 与 old，之后是活着的 keep
        put(&mut block, 24, 20, 52, b"a.txt", 1);
        put(&mut block, 40, 21, 36, b"gone.bin", 1);
        put(&mut block, 56, 22, 20, b"old", 2);
        put(&mut block, 76, 23, 948, b"keep", 1);
        let mut out = Vec::new();
        parse_block(&block, &FORMAT, &mut out);
        assert_eq!(
            names(&out),
            [
                ("a.txt".to_owned(), false),
                ("gone.bin".to_owned(), true),
                ("old".to_owned(), true),
                ("keep".to_owned(), false),
            ]
        );
    }

    #[test]
    fn index_blocks_and_checksum_tails_are_not_scanned() {
        let indexed = DirFormat {
            indexed: true,
            ..FORMAT
        };
        // htree 根：`..` 的项长到块末尾，后面是索引（这里放一段像目录项的字节，不该被认出来）
        let mut root = vec![0u8; 1024];
        put(&mut root, 0, 2, 12, b".", 2);
        put(&mut root, 12, 2, 1012, b"..", 2);
        put(&mut root, 40, 30, 16, b"fake", 1);
        let mut out = Vec::new();
        parse_block(&root, &indexed, &mut out);
        assert!(out.is_empty());
        // 内部节点：整块一个空项
        let mut node = vec![0u8; 1024];
        put(&mut node, 0, 0, 1024, b"", 0);
        put(&mut node, 16, 30, 16, b"fake", 1);
        parse_block(&node, &indexed, &mut out);
        assert!(out.is_empty());
        // 校验项
        let mut leaf = vec![0u8; 1024];
        put(&mut leaf, 0, 40, 1012, b"live", 1);
        put(&mut leaf, 1012, 0, 12, b"", TAIL_TYPE);
        parse_block(&leaf, &FORMAT, &mut out);
        assert_eq!(names(&out), [("live".to_owned(), false)]);
    }

    #[test]
    fn broken_blocks_stop_without_panicking() {
        let mut block = vec![0u8; 64];
        put(&mut block, 0, 40, 6, b"x", 1);
        let mut out = Vec::new();
        parse_block(&block, &FORMAT, &mut out);
        assert!(out.is_empty());
        put(&mut block, 0, 40, 0xFFFC, b"x", 1);
        parse_block(&block, &FORMAT, &mut out);
        let big = DirFormat {
            big_blocks: true,
            ..FORMAT
        };
        parse_block(&[0xFF; 9], &big, &mut out);
        parse_block(&[], &big, &mut out);
        assert!(out.is_empty());
    }
}
