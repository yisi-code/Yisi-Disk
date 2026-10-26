//! 目录的 `$I30` 索引：`INDEX_ROOT` 里的项，加上 `INDEX_ALLOCATION` 里按 `$BITMAP` 标为在用的索引块里的项。
//! 只为列出目录，不需要按 B+ 树的次序走，逐块读出全部叶子与中间节点里的项即可。

use yisi_part::bytes::{le16, le32, le64};

use super::record::{
    BITMAP, FileName, INDEX_ALLOCATION, INDEX_ROOT, Record, apply_fixup, parse_file_name,
    split_ref, utf16,
};
use super::{Ntfs, Value};
use crate::{FsError, MAX_ENTRIES};

/// 一个目录的索引最多读这么多字节；超过时只列出读到的部分。
const MAX_INDEX_BYTES: u64 = 256 << 20;
/// 一次读这么多字节的索引块。
const BATCH: u64 = 1 << 20;
/// 索引项的标志：这是节点里的最后一项（没有键）。
const LAST_ENTRY: u16 = 0x02;

/// 索引里的一项：指向的记录，以及键（文件名属性的副本，NTFS 会同步更新其中的长度、时间与属性）。
pub(crate) struct Key {
    pub record: u64,
    pub sequence: u16,
    pub name: FileName,
}

/// 读出目录记录 `number` 的全部索引项；第二个值为 `true` 表示项太多、没有全部读出。
pub(crate) fn read(ntfs: &Ntfs, number: u64, record: &Record) -> Result<(Vec<Key>, bool), FsError> {
    let i30 = utf16("$I30");
    let root = match ntfs.value(number, record, INDEX_ROOT, &i30)? {
        Some(Value::Resident { bytes, .. }) => bytes,
        _ => return Err(FsError::Invalid("目录没有可用的 $I30 索引根")),
    };
    let mut keys = Vec::new();
    // 索引根的值：前 16 字节是索引的参数（其中第 8 字节起是索引块大小），之后是节点头
    let block_size = u64::from(le32(&root, 8).unwrap_or(0));
    if !node_entries(&root, 0x10, &mut keys) {
        return Ok((keys, true));
    }
    let Some(Value::NonResident { extents, size, .. }) =
        ntfs.value(number, record, INDEX_ALLOCATION, &i30)?
    else {
        return Ok((keys, false));
    };
    if !block_size.is_power_of_two() || !(512..=65536).contains(&block_size) {
        return Err(FsError::Invalid("目录索引块的大小不合法"));
    }
    let bitmap = match ntfs.value(number, record, BITMAP, &i30)? {
        Some(Value::Resident { bytes, .. }) => bytes,
        Some(Value::NonResident { extents, size, .. }) => {
            let stream = ntfs.stream(extents, size.min(MAX_INDEX_BYTES / block_size / 8 + 1));
            let mut bytes = vec![0u8; stream.size() as usize];
            stream.read_at(0, &mut bytes)?;
            bytes
        }
        None => return Err(FsError::Invalid("目录索引没有 $BITMAP")),
    };
    let truncated_size = size.min(MAX_INDEX_BYTES);
    let stream = ntfs.stream(extents, truncated_size);
    let blocks = truncated_size / block_size;
    let in_use = |block: u64| {
        bitmap
            .get((block / 8) as usize)
            .is_some_and(|byte| byte & (1 << (block % 8)) != 0)
    };
    let per_batch = (BATCH / block_size).max(1);
    let mut batch = Vec::new();
    let mut first = 0u64;
    while first < blocks {
        let count = per_batch.min(blocks - first);
        if (first..first + count).any(in_use) {
            batch.resize((count * block_size) as usize, 0);
            // 读不出（坏扇区、运行表不完整）时跳过这一批，能列多少列多少
            if stream.read_at(first * block_size, &mut batch).is_ok() {
                for (index, block) in batch.chunks_exact_mut(block_size as usize).enumerate() {
                    if !in_use(first + index as u64) {
                        continue;
                    }
                    if block.get(..4) != Some(b"INDX".as_slice()) || !apply_fixup(block) {
                        continue;
                    }
                    if !node_entries(block, 0x18, &mut keys) {
                        return Ok((keys, true));
                    }
                }
            }
        }
        first += count;
    }
    Ok((keys, size > MAX_INDEX_BYTES))
}

/// 读一个节点里的项：`header` 是节点头在 `node` 里的偏移，项的偏移都相对节点头。
/// 项数超过上限时返回 `false`。
fn node_entries(node: &[u8], header: usize, keys: &mut Vec<Key>) -> bool {
    let (Some(first), Some(total)) = (le32(node, header), le32(node, header + 4)) else {
        return true;
    };
    let end = (header + total as usize).min(node.len());
    let mut offset = header + first as usize;
    while offset < end {
        let (Some(reference), Some(length), Some(key_length), Some(flags)) = (
            le64(node, offset),
            le16(node, offset + 8),
            le16(node, offset + 10),
            le16(node, offset + 12),
        ) else {
            break;
        };
        let length = usize::from(length);
        if length < 0x10 || offset + length > end {
            break;
        }
        if flags & LAST_ENTRY != 0 {
            break;
        }
        let key = node.get(offset + 0x10..offset + 0x10 + usize::from(key_length));
        if let Some(name) = key.and_then(parse_file_name) {
            if keys.len() >= MAX_ENTRIES {
                return false;
            }
            let (record, sequence) = split_ref(reference);
            keys.push(Key {
                record,
                sequence,
                name,
            });
        }
        offset += length;
    }
    true
}
