//! 引导扇区与 MBR 共用的最底层规则：结尾标志、跳转指令、扇区大小、FAT 的介质描述符。
//! 认分区表（这里）、核对 BPB（`yisi_fs::bpb`）、丢失分区搜索、宿主核对写入计划都按这一份。

use crate::bytes::le16;

/// 结尾标志 `55 AA`（按小端读是 0xAA55）与它的位置。
pub const BOOT_SIGNATURE: u16 = 0xAA55;
pub const SIGNATURE_OFFSET: usize = 510;
/// MBR（与 EBR）里分区表项从这里开始，前面是引导代码。
pub const MBR_TABLE_OFFSET: usize = 446;
/// BPB 记的每扇区字节数只能是这几种；按常见程度排，找备份时依次试。
pub const BYTES_PER_SECTOR: [u64; 4] = [512, 4096, 1024, 2048];

/// 结尾是 `55 AA`。
pub fn has_signature(sector: &[u8]) -> bool {
    le16(sector, SIGNATURE_OFFSET) == Some(BOOT_SIGNATURE)
}

/// 以跳转指令开头（`EB` 短跳转或 `E9` 近跳转）。核对 BPB 时另要求短跳转后面跟 `90`（`yisi_fs::bpb`），这里只看第一个字节。
pub fn starts_with_jump(sector: &[u8]) -> bool {
    matches!(sector.first(), Some(0xEB | 0xE9))
}

pub fn bytes_per_sector_ok(bytes: u64) -> bool {
    BYTES_PER_SECTOR.contains(&bytes)
}

/// FAT 引导扇区里的介质描述符只能是 `F0`（可移动介质）或 `F8` 以上。
pub fn fat_media_byte_ok(media: u8) -> bool {
    media == 0xF0 || media >= 0xF8
}
