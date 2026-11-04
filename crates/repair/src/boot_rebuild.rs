//! 主、备引导扇区都坏了时，按文件系统自己留在分区里的结构推算出引导扇区的参数，重新写出来（FAT32、exFAT、NTFS）。
//!
//! 只推算、不写盘：给出要写的各段（主引导扇区与备份，FAT32 的 FSInfo 坏了时一起写），写入走修复引导扇区的
//! 写入计划（`boot_fix` 的 `Rebuild`），按推算的参数浏览走叠加视图（[`overlay_rebuilt`]）。
//!
//! - FAT32：在分区开头附近找 FAT 表头（第 0 项是介质描述符、其余位全为 1，第 1、2 项不为空），它的位置就是
//!   保留扇区数；在按各种簇大小算出的 FAT 大小附近找内容相同的第二份 FAT，得出 FAT 大小；每簇扇区数取让
//!   FAT 装得下全部簇的最小值，再用根目录里每个子目录首簇处的 `.` 目录项核对。根目录在 2 号簇。
//! - exFAT：在第 24 扇区之后找 FAT 表头（`F8 FF FF FF FF FF FF FF`），在它之后找根目录（含分配位图与大写表
//!   目录项）；对每种簇大小与根目录簇号解出簇区起点，大写表的校验和、分配位图里这几个簇的占用位、FAT 里根目录
//!   那一项都对得上的才算；簇数由卷长度与分配位图的长度核对，FAT 长度按 Windows 格式化时的写法（[`exfat_fat_length`]）。
//! - NTFS：在分区里找 `$MFT` 的第一条记录（与打开卷同一个判断），它和第二条记录的数据流起始簇号给出 `$MFT`、
//!   `$MFTMirr` 的位置；找到的位置除以其中一个簇号就是簇大小，另一份也要在算出的位置上。记录大小取记录头，
//!   索引块大小取根目录的索引根。
//!
//! 卷序列号推不出来，由调用方给一个新的。推算有不止一种结果时不给出，免得写进去的是错的。

use std::collections::HashMap;
use std::sync::Arc;

use yisi_fs::boot_write::{
    EXFAT_BOOT_REGION_SECTORS, ExfatBoot, FatBoot, NtfsBoot, exfat_boot_region, fat_boot,
    fat32_fs_info, ntfs_boot,
};
use yisi_fs::format::{
    EXFAT_TYPE_BITMAP, EXFAT_TYPE_LABEL, EXFAT_TYPE_UPCASE, FatWidth, exfat_label,
    exfat_table_checksum, fat_first_cluster, fat_media_ok,
};
use yisi_fs::{FsKind, bpb, mft_record_layout, mft_self_record_ok, root_index_block_size};
use yisi_io::{BlockDevice, Overlay, SectorWrite};
use yisi_part::Partition;
use yisi_part::bytes::{le32, le64};

use crate::boot::{Verdict, diagnose_partition};

/// FAT32 的保留扇区数是 16 位：FAT 表头最远在第 65535 扇区。
const MAX_RESERVED: u64 = 0xFFFF;
/// FAT32 的最少簇数（少于它就是 FAT16）与表项上限。
const FAT32_MIN_CLUSTERS: u64 = 65_525;
const FAT32_MAX_CLUSTERS: u64 = 0x0FFF_FFF5;
/// FSInfo 在第 1 扇区，备份引导扇区在第 6 扇区、它的 FSInfo 在第 7 扇区（Windows 的写法）。
const FS_INFO_SECTOR: u64 = 1;
const FAT32_BACKUP_SECTOR: u64 = 6;
/// 在 FAT 表头之后最多找这么远的 exFAT 根目录：FAT、分配位图、大写表都在它前面。
const EXFAT_ROOT_SEARCH: u64 = 512 << 20;
/// exFAT 的 FAT 最远从这里开始找。
const EXFAT_FAT_SEARCH: u64 = 64 << 20;
/// exFAT 簇最大 32 MiB。
const EXFAT_MAX_CLUSTER: u64 = 32 << 20;
/// 根目录簇号最多试到 2 号簇之后这么多个（根目录一般紧跟在分配位图与大写表后面）。
const EXFAT_ROOT_SLACK: u64 = 64;
/// 大写表最多读这么多字节来核对校验和。
const MAX_UPCASE: u64 = 1 << 20;
/// 在分区开头这么远之内找 `$MFT` 的记录（`$MFTMirr` 一般在 2 号簇，`$MFT` 多在 3 GiB 附近）。
const NTFS_SEARCH: u64 = 8 << 30;
/// 读分区时一次读这么多。
const CHUNK: u64 = 1 << 20;
const DEFAULT_INDEX_BLOCK: u64 = 4096;

/// 推算出来、要写进去的引导扇区。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebuilt {
    pub kind: FsKind,
    /// 要写的各段（设备上的偏移，按偏移排序）：主引导扇区（区）、备份；FAT32 的 FSInfo 坏了时含在前一段里。
    pub writes: Vec<SectorWrite>,
    /// 写出的主引导扇区（exFAT 是引导扇区本身），按 [`bpb::significant_fields`] 列出各项给人看。
    pub boot: Vec<u8>,
    pub label: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    #[error("读不出偏移 {0} 处")]
    Unreadable(u64),
    #[error("分区的位置或长度没有按设备的扇区对齐")]
    Unaligned,
    /// 这种文件系统不重建（FAT12/16 等）。
    #[error("不重建 {0:?} 的引导扇区")]
    Unsupported(FsKind),
    /// 找不到推算要用的结构（举出第一处缺的）。
    #[error("推算不出参数：{0}")]
    NotFound(&'static str),
    /// 有不止一种推算结果，或者结构之间对不上。
    #[error("推算的结果不唯一或对不上：{0}")]
    Ambiguous(&'static str),
}

/// 按分区里的结构推算 `kind` 的引导扇区（`kind` 为 `Unknown` 时三种都试，只有一种推得出才算）。`serial` 是新的卷序列号。
pub fn rebuild(
    device: &Arc<dyn BlockDevice>,
    partition: &Partition,
    kind: FsKind,
    serial: u64,
) -> Result<Rebuilt, RebuildError> {
    let source = Source::new(device, partition)?;
    match kind {
        FsKind::Fat32 => source.fat32(serial),
        FsKind::Exfat => source.exfat(serial),
        FsKind::Ntfs => source.ntfs(serial),
        FsKind::Unknown => {
            let found: Vec<Rebuilt> = [
                source.fat32(serial),
                source.exfat(serial),
                source.ntfs(serial),
            ]
            .into_iter()
            .filter_map(Result::ok)
            .collect();
            let mut found = found.into_iter();
            match (found.next(), found.next()) {
                (Some(rebuilt), None) => Ok(rebuilt),
                (None, _) => Err(RebuildError::NotFound("认不出是 FAT32、exFAT 还是 NTFS")),
                (Some(_), Some(_)) => Err(RebuildError::Ambiguous(
                    "FAT32、exFAT、NTFS 中不止一种推得出",
                )),
            }
        }
        other => Err(RebuildError::Unsupported(other)),
    }
}

/// 把主、备都坏了（或认不出）的分区换成推算出的引导扇区后的视图：浏览、复制「按推算的参数」时用，不写盘。
/// 推算不出的分区保持原样。
pub fn overlay_rebuilt(
    device: &Arc<dyn BlockDevice>,
    partitions: &[Partition],
    serial: u64,
) -> Arc<dyn BlockDevice> {
    let mut writes = Vec::new();
    for partition in partitions {
        let report = diagnose_partition(device, partition);
        if !matches!(report.verdict, Verdict::BothDamaged | Verdict::Unrecognized) {
            continue;
        }
        match rebuild(device, partition, report.filesystem, serial) {
            Ok(rebuilt) => writes.extend(rebuilt.writes),
            Err(error) => tracing::info!(partition = partition.number, %error, "推算不出引导扇区"),
        }
    }
    if writes.is_empty() {
        return Arc::clone(device);
    }
    writes.sort_by_key(|write| write.offset);
    Arc::new(Overlay::new(Arc::clone(device), &writes))
}

/// 一个分区：按分区里的偏移读，记着设备的扇区大小。
struct Source<'a> {
    device: &'a Arc<dyn BlockDevice>,
    partition: &'a Partition,
    sector: u64,
    sectors: u64,
}

impl<'a> Source<'a> {
    fn new(
        device: &'a Arc<dyn BlockDevice>,
        partition: &'a Partition,
    ) -> Result<Self, RebuildError> {
        let sector = u64::from(device.sector_size()).max(512);
        if !partition.offset.is_multiple_of(sector) || !partition.length.is_multiple_of(sector) {
            return Err(RebuildError::Unaligned);
        }
        Ok(Self {
            device,
            partition,
            sector,
            sectors: partition.length / sector,
        })
    }

    fn read(&self, at: u64, len: u64) -> Result<Vec<u8>, RebuildError> {
        let unreadable = || RebuildError::Unreadable(self.partition.offset.saturating_add(at));
        if at
            .checked_add(len)
            .is_none_or(|end| end > self.partition.length)
        {
            return Err(unreadable());
        }
        let mut buf = vec![0u8; usize::try_from(len).map_err(|_| unreadable())?];
        self.device
            .read_exact_at(self.partition.offset + at, &mut buf)
            .map_err(|_| unreadable())?;
        Ok(buf)
    }

    fn read_sector(&self, sector: u64) -> Result<Vec<u8>, RebuildError> {
        self.read(sector * self.sector, self.sector)
    }

    /// 从分区里 `start` 字节起、不超过 `limit` 字节的范围里，按 `step` 字节对齐逐处看，返回第一处让 `hit` 成立的位置。
    /// `hit` 拿到这一处起、读进来的剩余字节（至少 `need` 字节，读到范围末尾时可能不足）。
    fn scan(
        &self,
        start: u64,
        limit: u64,
        step: u64,
        need: u64,
        mut hit: impl FnMut(u64, &[u8]) -> bool,
    ) -> Result<Option<u64>, RebuildError> {
        let end = start.saturating_add(limit).min(self.partition.length);
        let mut at = start;
        while at < end {
            let len = (end - at).min(CHUNK + need);
            let bytes = self.read(at, len)?;
            let mut offset = 0u64;
            while offset < CHUNK.min(len) {
                let Some(slice) = bytes.get(offset as usize..) else {
                    break;
                };
                if hit(at + offset, slice) {
                    return Ok(Some(at + offset));
                }
                offset += step;
            }
            at += CHUNK;
        }
        Ok(None)
    }

    /// 写到分区里 `at` 字节处的一段：写前内容现读。
    fn write(&self, at: u64, after: Vec<u8>) -> Result<SectorWrite, RebuildError> {
        let before = self.read(at, after.len() as u64)?;
        Ok(SectorWrite {
            offset: self.partition.offset + at,
            before,
            after,
        })
    }

    fn hidden_sectors(&self) -> u32 {
        u32::try_from(self.partition.offset / self.sector).unwrap_or(u32::MAX)
    }

    fn fat32(&self, serial: u64) -> Result<Rebuilt, RebuildError> {
        let sector = self.sector;
        let bps = u16::try_from(sector).map_err(|_| RebuildError::Unsupported(FsKind::Fat32))?;
        // FAT 表头：第 0 项低 8 位是介质描述符、其余位全为 1，第 1 项除两个脏标记位外全为 1，第 2 项（根目录）不为空
        let head = |bytes: &[u8]| -> Option<u8> {
            let entry0 = le32(bytes, 0)?;
            let entry1 = le32(bytes, 4)?;
            let entry2 = le32(bytes, 8)? & 0x0FFF_FFFF;
            let media = (entry0 & 0xFF) as u8;
            (fat_media_ok(FatWidth::Fat32, media, entry0)
                && (media == 0xF0 || media >= 0xF8)
                && entry1 & 0x03FF_FFFF == 0x03FF_FFFF
                && entry2 != 0)
                .then_some(media)
        };
        let reserved_at = self
            .scan(sector, MAX_RESERVED * sector, sector, 12, |_, bytes| {
                head(bytes).is_some()
            })?
            .ok_or(RebuildError::NotFound("找不到 FAT 表头"))?;
        let reserved = reserved_at / sector;
        let first = self.read_sector(reserved)?;
        let media = head(&first).ok_or(RebuildError::NotFound("找不到 FAT 表头"))?;
        let total = self.sectors;
        let room = total
            .checked_sub(reserved)
            .ok_or(RebuildError::NotFound("分区太小"))?;

        // 第二份 FAT 的位置：每种簇大小算一个 FAT 大小的范围（最小够用到按整个数据区算），在里面找内容相同的扇区
        let entries_per_sector = sector / 4;
        let mut sizes = Vec::new();
        for shift in 0..=7u32 {
            let spc = 1u64 << shift;
            let low = (room + 2 * spc).div_ceil(spc * entries_per_sector + 2);
            let high = (room / spc + 2).div_ceil(entries_per_sector) + 1;
            if high <= low || reserved + 2 * low >= total {
                continue;
            }
            let window = self.read(
                (reserved + low) * sector,
                ((high - low + 1) * sector).min((total - reserved - low) * sector),
            )?;
            for (index, candidate) in window.chunks_exact(sector as usize).enumerate() {
                let size = low + index as u64;
                if candidate == first.as_slice() && !sizes.contains(&size) {
                    sizes.push(size);
                }
            }
        }
        let fat_sectors = match sizes.as_slice() {
            [] => return Err(RebuildError::NotFound("找不到与第一份内容相同的第二份 FAT")),
            [size] => *size,
            _ => return Err(RebuildError::Ambiguous("第二份 FAT 可能在不止一处")),
        };
        let data_start = reserved + 2 * fat_sectors;
        let capacity = fat_sectors * entries_per_sector;
        // 每簇扇区数：FAT 装得下全部簇的最小值（再小一档就装不下，再大一档 FAT 就大了一倍）
        let spc = (0..=7u32)
            .map(|shift| 1u64 << shift)
            .find(|&spc| (total - data_start) / spc + 2 <= capacity)
            .ok_or(RebuildError::Ambiguous("FAT 大小与分区大小对不上"))?;
        let clusters = (total - data_start) / spc;
        if !(FAT32_MIN_CLUSTERS..=FAT32_MAX_CLUSTERS).contains(&clusters) {
            return Err(RebuildError::NotFound("簇数不在 FAT32 的范围里"));
        }

        // 根目录在 2 号簇：核对每个子目录的首簇处是 `.` 目录项，顺便取卷标
        let cluster_bytes = spc * sector;
        let root = self.read(data_start * sector, cluster_bytes)?;
        let mut label = None;
        for entry in root.as_chunks::<32>().0 {
            let attributes = entry.get(11).copied().unwrap_or(0);
            match entry.first().copied().unwrap_or(0) {
                0 => break,
                0xE5 => continue,
                _ if attributes & 0x0F == 0x0F => continue,
                _ if attributes & 0x08 != 0 => {
                    label = entry
                        .get(..11)
                        .and_then(|name| <[u8; 11]>::try_from(name).ok());
                }
                _ if attributes & 0x10 != 0 => {
                    let cluster = u64::from(fat_first_cluster(entry, true));
                    if !(2..clusters + 2).contains(&cluster) {
                        return Err(RebuildError::Ambiguous("根目录里的子目录首簇不在卷内"));
                    }
                    let dir = self.read((data_start + (cluster - 2) * spc) * sector, 32)?;
                    if dir.get(..11) != Some(b".          ".as_slice())
                        || dir.get(11).is_none_or(|attributes| attributes & 0x10 == 0)
                    {
                        return Err(RebuildError::Ambiguous(
                            "子目录的首簇处不是目录（每簇扇区数对不上）",
                        ));
                    }
                }
                _ => {}
            }
        }

        let label_bytes: [u8; 11] = label.unwrap_or(*b"NO NAME    ");
        let boot = fat_boot(&FatBoot {
            kind: FsKind::Fat32,
            bytes_per_sector: bps,
            sectors_per_cluster: spc as u8,
            reserved: u16::try_from(reserved).unwrap_or(u16::MAX),
            fats: 2,
            root_entries: 0,
            total_sectors: u32::try_from(total)
                .map_err(|_| RebuildError::Unsupported(FsKind::Fat32))?,
            media,
            fat_sectors: u32::try_from(fat_sectors).unwrap_or(u32::MAX),
            hidden_sectors: self.hidden_sectors(),
            root_cluster: 2,
            fs_info: if reserved > FS_INFO_SECTOR {
                FS_INFO_SECTOR as u16
            } else {
                0
            },
            backup_boot: if reserved > FAT32_BACKUP_SECTOR + 1 {
                FAT32_BACKUP_SECTOR as u16
            } else {
                0
            },
            serial: serial as u32,
            label: label_bytes,
        });
        check_fatal(FsKind::Fat32, &boot)?;
        // 引导扇区连同 FSInfo（坏了的换成新的）一起写；保留区放得下时备份也写
        let fs_info = |at: u64| -> Result<Vec<u8>, RebuildError> {
            let current = self.read_sector(at)?;
            Ok(if fs_info_ok(&current) {
                current
            } else {
                fat32_fs_info(bps)
            })
        };
        let mut writes = Vec::new();
        let mut head_sectors = boot.clone();
        if reserved > FS_INFO_SECTOR {
            head_sectors.extend(fs_info(FS_INFO_SECTOR)?);
        }
        writes.push(self.write(0, head_sectors.clone())?);
        if reserved > FAT32_BACKUP_SECTOR + 1 {
            let mut backup = boot.clone();
            backup.extend(fs_info(FAT32_BACKUP_SECTOR + 1)?);
            writes.push(self.write(FAT32_BACKUP_SECTOR * sector, backup)?);
        }
        Ok(Rebuilt {
            kind: FsKind::Fat32,
            writes,
            boot,
            label: label.map(|bytes| String::from_utf8_lossy(&bytes).trim_end().to_owned()),
        })
    }

    fn exfat(&self, serial: u64) -> Result<Rebuilt, RebuildError> {
        let sector = self.sector;
        let sector_shift = sector.trailing_zeros();
        const FAT_HEAD: [u8; 8] = [0xF8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let region_bytes = 2 * EXFAT_BOOT_REGION_SECTORS * sector;
        let fat_at = self
            .scan(region_bytes, EXFAT_FAT_SEARCH, sector, 8, |_, bytes| {
                bytes.get(..8) == Some(FAT_HEAD.as_slice())
            })?
            .ok_or(RebuildError::NotFound("找不到 exFAT 的 FAT 表头"))?;
        let fat_offset = fat_at / sector;

        // 根目录：开头几项里有分配位图项与大写表项，之前只有卷标、卷 GUID 这类系统项
        let mut entries = (None, None, None);
        let root_at = self
            .scan(
                fat_at + sector,
                EXFAT_ROOT_SEARCH,
                sector,
                32 * 8,
                |_, bytes| match root_entries(bytes) {
                    Some(found) => {
                        entries = found;
                        true
                    }
                    None => false,
                },
            )?
            .ok_or(RebuildError::NotFound("找不到 exFAT 的根目录"))?;
        let (Some(bitmap), Some(upcase), label) = entries else {
            return Err(RebuildError::NotFound("找不到 exFAT 的根目录"));
        };
        let root_sector = root_at / sector;
        let bitmap_first = u64::from(le32(&bitmap, 20).unwrap_or(0));
        let bitmap_length = le64(&bitmap, 24).unwrap_or(0);
        let upcase_checksum = le32(&upcase, 4).unwrap_or(0);
        let upcase_first = u64::from(le32(&upcase, 20).unwrap_or(0));
        let upcase_length = le64(&upcase, 24).unwrap_or(0).min(MAX_UPCASE);
        if bitmap_first < 2 || upcase_first < 2 || bitmap_length == 0 || upcase_length == 0 {
            return Err(RebuildError::NotFound("分配位图或大写表的目录项不合法"));
        }

        // 对每种簇大小与根目录簇号解出簇区起点，大写表、分配位图、FAT 都要对得上
        let mut solutions = Vec::new();
        let mut fat_cache: HashMap<u64, Vec<u8>> = HashMap::new();
        for cluster_shift in 0..=(EXFAT_MAX_CLUSTER / sector).trailing_zeros() {
            let per_cluster = 1u64 << cluster_shift;
            let used = |length: u64| length.div_ceil(per_cluster * sector);
            let last_root = 2 + used(bitmap_length) + used(upcase_length) + EXFAT_ROOT_SLACK;
            for root_cluster in 2..=last_root {
                let Some(heap) = root_sector.checked_sub((root_cluster - 2) * per_cluster) else {
                    break;
                };
                if heap <= fat_offset {
                    break;
                }
                let at = |cluster: u64| (heap + (cluster - 2) * per_cluster) * sector;
                let Ok(table) = self.read(at(upcase_first), upcase_length) else {
                    continue;
                };
                if exfat_table_checksum(&table) != upcase_checksum {
                    continue;
                }
                let Ok(map) = self.read(at(bitmap_first), sector) else {
                    continue;
                };
                let allocated = |cluster: u64| {
                    let bit = cluster - 2;
                    map.get((bit / 8) as usize)
                        .is_some_and(|byte| byte & (1 << (bit % 8)) != 0)
                };
                if !(allocated(bitmap_first) && allocated(upcase_first) && allocated(root_cluster))
                {
                    continue;
                }
                // 根目录走 FAT 链：FAT 里它那一项不为 0
                let fat_sector = fat_offset + root_cluster * 4 / sector;
                let fat = match fat_cache.get(&fat_sector) {
                    Some(bytes) => bytes.clone(),
                    None => {
                        let bytes = self.read_sector(fat_sector)?;
                        fat_cache.insert(fat_sector, bytes.clone());
                        bytes
                    }
                };
                if le32(&fat, ((root_cluster * 4) % sector) as usize).unwrap_or(0) == 0 {
                    continue;
                }
                // 簇数要与分配位图的长度对得上，FAT 要放得进簇区之前
                let clusters = (self.sectors - heap) / per_cluster;
                let Some(fat_length) = exfat_fat_length(
                    self.sectors,
                    per_cluster,
                    sector,
                    clusters,
                    heap - fat_offset,
                ) else {
                    continue;
                };
                if clusters.div_ceil(8) != bitmap_length {
                    continue;
                }
                solutions.push((cluster_shift, root_cluster, heap, clusters, fat_length));
            }
        }
        let (cluster_shift, root_cluster, heap, clusters, fat_length) = match solutions.as_slice() {
            [] => return Err(RebuildError::NotFound("解不出簇区起点与簇大小")),
            [solution] => *solution,
            _ => return Err(RebuildError::Ambiguous("簇区起点与簇大小有不止一种解")),
        };
        let too_big = || RebuildError::Unsupported(FsKind::Exfat);
        let region = exfat_boot_region(&ExfatBoot {
            partition_offset: self.partition.offset / sector,
            volume_length: self.sectors,
            fat_offset: u32::try_from(fat_offset).map_err(|_| too_big())?,
            fat_length: u32::try_from(fat_length).map_err(|_| too_big())?,
            heap_offset: u32::try_from(heap).map_err(|_| too_big())?,
            cluster_count: u32::try_from(clusters).map_err(|_| too_big())?,
            root_cluster: u32::try_from(root_cluster).map_err(|_| too_big())?,
            serial: serial as u32,
            sector_shift: sector_shift as u8,
            cluster_shift: cluster_shift as u8,
        });
        let boot = region.get(..sector as usize).unwrap_or_default().to_vec();
        check_fatal(FsKind::Exfat, &boot)?;
        let writes = vec![
            self.write(0, region.clone())?,
            self.write(EXFAT_BOOT_REGION_SECTORS * sector, region)?,
        ];
        Ok(Rebuilt {
            kind: FsKind::Exfat,
            writes,
            boot,
            label: label.as_deref().and_then(exfat_label),
        })
    }

    fn ntfs(&self, serial: u64) -> Result<Rebuilt, RebuildError> {
        let sector = self.sector;
        let bps = u16::try_from(sector).map_err(|_| RebuildError::Unsupported(FsKind::Ntfs))?;
        // 记录按 512 字节对齐找（记录至少 1 KiB、从簇边界开始）。记录头给出记录大小，第 0 条记录给出 $MFT 的簇号，
        // 第 1 条（$MFTMirr）给出副本的簇号；第 1 条拿不到时接着找第 0 条记录的另一份，用两处的位置解
        let mut hits: Vec<(u64, u64, Option<u64>, u64)> = Vec::new();
        self.scan(sector, NTFS_SEARCH, 512, 8192, |at, bytes| {
            if bytes.get(..4) != Some(b"FILE".as_slice()) {
                return false;
            }
            // 只把这两条记录交给解析（每个 `FILE` 都整段复制会很慢）；解析要求正好一条记录的长度，按记录头记的切
            let Some(size) = le32(bytes, 0x1C)
                .map(|size| size as usize)
                .filter(|size| size.is_power_of_two() && (1024..=4096).contains(size))
            else {
                return false;
            };
            let first = bytes.get(..size).unwrap_or_default();
            if !mft_self_record_ok(first) {
                return false;
            }
            let Some((mft_lcn, record_size)) = mft_record_layout(first, 0) else {
                return false;
            };
            let mirror_lcn = usize::try_from(record_size)
                .ok()
                .and_then(|size| bytes.get(size..size.checked_mul(2)?))
                .and_then(|next| mft_record_layout(next, 1))
                .map(|(lcn, _)| lcn);
            hits.push((at, mft_lcn, mirror_lcn, record_size));
            mirror_lcn.is_some() || hits.len() >= 2
        })?;
        let Some(&(at, mft_lcn, mirror_hint, record_size)) = hits.first() else {
            return Err(RebuildError::NotFound("找不到 $MFT 的第一条记录"));
        };
        if !record_size.is_power_of_two() || !(1024..=4096).contains(&record_size) {
            return Err(RebuildError::NotFound("$MFT 记录头里的记录大小不合法"));
        }
        let plausible = |size: u64| size.is_power_of_two() && size >= sector && size <= 2 << 20;
        let copy_ok = |cluster_size: u64, lcn: u64| -> bool {
            lcn.checked_mul(cluster_size)
                .and_then(|offset| self.read(offset, record_size).ok())
                .is_some_and(|bytes| mft_self_record_ok(&bytes))
        };
        // 可能的（簇大小，副本簇号）：找到的是 $MFT 或 $MFTMirr 里的那一份，位置除以对应的簇号就是簇大小，
        // 两份都要在算出的位置上
        let mut solutions: Vec<(u64, u64)> = Vec::new();
        match mirror_hint {
            Some(mirror_lcn) => {
                for lcn in [mft_lcn, mirror_lcn] {
                    if lcn > 0 && at % lcn == 0 {
                        let size = at / lcn;
                        if plausible(size) && copy_ok(size, mft_lcn) && copy_ok(size, mirror_lcn) {
                            solutions.push((size, mirror_lcn));
                        }
                    }
                }
            }
            None => {
                let other = hits
                    .get(1)
                    .map(|hit| hit.0)
                    .ok_or(RebuildError::NotFound("找不到 $MFTMirr"))?;
                for (mft_at, mirror_at) in [(at, other), (other, at)] {
                    if mft_lcn == 0 || mft_at % mft_lcn != 0 {
                        continue;
                    }
                    let size = mft_at / mft_lcn;
                    if plausible(size) && mirror_at % size == 0 {
                        solutions.push((size, mirror_at / size));
                    }
                }
            }
        }
        solutions.dedup();
        let (cluster_size, mirror_lcn) = match solutions.as_slice() {
            [] => {
                return Err(RebuildError::NotFound(
                    "$MFT 与 $MFTMirr 的位置算不出簇大小",
                ));
            }
            [solution] => *solution,
            _ => return Err(RebuildError::Ambiguous("簇大小有不止一种解")),
        };
        let index_size = self
            .read(mft_lcn * cluster_size + 5 * record_size, record_size)
            .ok()
            .and_then(|bytes| root_index_block_size(&bytes))
            .filter(|size| size.is_power_of_two() && (512..=65536).contains(size))
            .unwrap_or(DEFAULT_INDEX_BLOCK);
        let total_sectors = self.sectors - 1;
        let boot = ntfs_boot(&NtfsBoot {
            bytes_per_sector: bps,
            cluster_size,
            hidden_sectors: self.hidden_sectors(),
            total_sectors,
            mft_lcn,
            mirror_lcn,
            record_size,
            index_size,
            serial,
        });
        check_fatal(FsKind::Ntfs, &boot)?;
        let writes = vec![
            self.write(0, boot.clone())?,
            self.write(total_sectors * sector, boot.clone())?,
        ];
        Ok(Rebuilt {
            kind: FsKind::Ntfs,
            writes,
            boot,
            label: None,
        })
    }
}

/// exFAT 根目录开头的系统项：遇到分配位图项与大写表项时给出它们（还有卷标项），中间夹着别的类型就不是根目录。
type RootEntries = (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);

fn root_entries(bytes: &[u8]) -> Option<RootEntries> {
    let entries = bytes.as_chunks::<32>().0;
    let (mut bitmap, mut upcase) = (None, None);
    for entry in entries.iter().take(8) {
        match entry.first().copied().unwrap_or(0) {
            EXFAT_TYPE_BITMAP if bitmap.is_none() => bitmap = Some(entry.to_vec()),
            EXFAT_TYPE_UPCASE if upcase.is_none() => upcase = Some(entry.to_vec()),
            // 卷标项、卷 GUID 项与未用的卷标项
            EXFAT_TYPE_LABEL | 0xA0 | 0x03 => {}
            _ => break,
        }
        if bitmap.is_some() && upcase.is_some() {
            // 卷标项不一定排在最前，在根目录开头这一段里找
            let label = entries
                .iter()
                .take(128)
                .take_while(|entry| entry.first() != Some(&0))
                .find(|entry| entry.first() == Some(&EXFAT_TYPE_LABEL))
                .map(|entry| entry.to_vec());
            return Some((bitmap, upcase, label));
        }
    }
    None
}

/// exFAT 的 FAT 长度（扇区）。FAT 区里够用之后的部分都是 0，原来的值读不出来，按 Windows 格式化时的写法算：
/// 按「卷长度 ÷ 每簇扇区数 + 2」个表项、每项 4 字节，按扇区向上取整，再按每簇扇区数向上取整；
/// 簇区之前放不下时取够用的最小值，最小值也放不下（`room` 是 FAT 起点到簇区起点的扇区数）时为 `None`。
fn exfat_fat_length(
    volume_sectors: u64,
    per_cluster: u64,
    sector: u64,
    clusters: u64,
    room: u64,
) -> Option<u64> {
    let windows = ((volume_sectors / per_cluster + 2) * 4)
        .div_ceil(sector)
        .next_multiple_of(per_cluster);
    let minimal = ((clusters + 2) * 4).div_ceil(sector);
    [windows, minimal]
        .into_iter()
        .find(|&length| length <= room)
}

/// FSInfo 扇区的三处签名都对。
fn fs_info_ok(sector: &[u8]) -> bool {
    le32(sector, 0) == Some(0x4161_5252)
        && le32(sector, 484) == Some(0x6141_7272)
        && le32(sector, 508) == Some(0xAA55_0000)
}

/// 写出来的引导扇区按打开卷的规则核对，不能有致命问题。
fn check_fatal(kind: FsKind, boot: &[u8]) -> Result<(), RebuildError> {
    let fatal = match kind {
        FsKind::Exfat => bpb::check_exfat(boot).problems,
        FsKind::Ntfs => bpb::check_ntfs(boot).problems,
        _ => bpb::check_fat(boot, Some(kind)).problems,
    }
    .iter()
    .any(|problem| problem.fatal);
    if fatal {
        return Err(RebuildError::Ambiguous("推算出的参数按规则核对不通过"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
