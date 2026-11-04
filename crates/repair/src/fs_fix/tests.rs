//! 文件系统修复的计划：在样本上造出各类问题，生成计划、写进样本，再检查一遍应当没有这些问题、也没冒出别的。
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use std::io::Read;

use yisi_fs::samples::{ExfatImage, FatImage, FatType, Layout, MemoryDisk, pattern};
use yisi_fs::{BootCopy, Volume};

use super::*;
use crate::check::check;
use crate::samples::{end_marker, fat_entry, reseal_exfat_boot, set_bitmap, set_fat};

fn device(image: &[u8]) -> Arc<dyn BlockDevice> {
    Arc::new(MemoryDisk::new(image.to_vec(), 512))
}

fn plan(image: &[u8], fixes: &[FsFix]) -> Result<FsFixPlan, FsFixError> {
    plan_fs_fix(&device(image), 0, image.len() as u64, fixes, &[])
}

fn apply(image: &mut [u8], plan: &FsFixPlan) {
    for write in &plan.writes {
        let at = write.offset as usize;
        assert_eq!(&image[at..at + write.before.len()], write.before.as_slice());
        image[at..at + write.after.len()].copy_from_slice(&write.after);
    }
}

fn kinds(image: &[u8]) -> Vec<IssueKind> {
    let disk = MemoryDisk::new(image.to_vec(), 512);
    check(&disk, 0, image.len() as u64, &Quiet)
        .unwrap()
        .issues
        .iter()
        .map(|issue| issue.kind)
        .collect()
}

fn read_file(image: &[u8], path: &[&str]) -> Vec<u8> {
    let volume =
        Volume::open_with(device(image), 0, image.len() as u64, BootCopy::Primary).unwrap();
    let mut dir = volume.root();
    let mut found = None;
    for (index, name) in path.iter().enumerate() {
        let listing = volume.list(&dir).unwrap();
        let entry = listing
            .entries
            .into_iter()
            .find(|entry| entry.name == *name)
            .unwrap();
        if index + 1 < path.len() {
            dir = entry.file;
        }
        found = Some(entry);
    }
    let mut data = Vec::new();
    volume
        .open_file(&found.unwrap().file)
        .unwrap()
        .read_to_end(&mut data)
        .unwrap();
    data
}

/// 根目录下一个文件、一个子目录里一个文件。
fn fat_image(kind: FatType) -> (FatImage, Vec<u32>) {
    let mut image = FatImage::new(kind);
    let root = image.root();
    let docs = image.mkdir(root, "Docs").as_dir();
    let size = image.cluster_size() * 4;
    let placed = image.add_file(docs, "long.bin", &pattern(1, size), Layout::Contiguous);
    image.add_file(root, "keep.txt", &pattern(2, 3000), Layout::Contiguous);
    (image, placed.clusters)
}

#[test]
fn lost_clusters_are_freed_in_every_fat() {
    for kind in [FatType::Fat16, FatType::Fat32] {
        let mut image = fat_image(kind).0.finish();
        set_fat(&mut image, kind, 5000, 5001, None);
        set_fat(&mut image, kind, 5001, end_marker(kind), None);
        assert_eq!(kinds(&image), [IssueKind::LostClusters], "{kind:?}");
        let fixed = plan(&image, &[FsFix::Lost]).unwrap();
        assert_eq!(
            fixed.counts,
            [FixCount {
                fix: FsFix::Lost,
                items: 1,
                clusters: 2
            }]
        );
        // 两份 FAT 各改一个扇区
        assert_eq!(fixed.writes.len(), 2, "{kind:?}");
        apply(&mut image, &fixed);
        assert!(kinds(&image).is_empty(), "{kind:?}");
        for copy in 0..2 {
            assert_eq!(fat_entry(&image, kind, 5000, copy), 0);
            assert_eq!(fat_entry(&image, kind, 5001, copy), 0);
        }
        // 没有可修的了
        assert!(matches!(
            plan(&image, &[FsFix::Lost]),
            Err(FsFixError::NothingToFix(FsFix::Lost))
        ));
    }
}

#[test]
fn a_chain_longer_than_the_file_is_cut() {
    let kind = FatType::Fat16;
    let (mut sample, clusters) = fat_image(kind);
    let cluster_size = sample.cluster_size();
    // 目录项里的大小改成一个多簇：链有 4 簇，应只留 2 簇
    let placed = {
        let root = sample.root();
        sample.add_file(
            root,
            "short.bin",
            &pattern(3, cluster_size * 3),
            Layout::Contiguous,
        )
    };
    sample.patch_short_entry(&placed, 28, &((cluster_size + 10) as u32).to_le_bytes());
    let mut image = sample.finish();
    assert_eq!(kinds(&image), [IssueKind::ChainTooLong]);
    let fixed = plan(&image, &[FsFix::TooLong]).unwrap();
    assert_eq!(fixed.counts[0].clusters, 1);
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty());
    assert_eq!(
        fat_entry(&image, kind, placed.clusters[1], 0),
        end_marker(kind)
    );
    assert_eq!(fat_entry(&image, kind, placed.clusters[2], 1), 0);
    assert_eq!(
        read_file(&image, &["short.bin"]),
        pattern(3, cluster_size + 10)
    );
    // 别的文件不受影响
    assert_eq!(
        read_file(&image, &["Docs", "long.bin"]).len(),
        cluster_size * 4
    );
    assert_eq!(clusters.len(), 4);
}

#[test]
fn the_dirty_flag_is_cleared_only_when_nothing_serious_is_left() {
    let kind = FatType::Fat32;
    let mut image = fat_image(kind).0.finish();
    // 第 1 项：未正常卸下（第 27 位为 0），另有丢失簇
    set_fat(&mut image, kind, 1, 0x07FF_FFFF, None);
    set_fat(&mut image, kind, 6000, end_marker(kind), None);
    assert!(matches!(
        plan(&image, &[FsFix::Dirty]),
        Err(FsFixError::DirtyWithProblems)
    ));
    // 连同丢失簇一起修就可以
    let fixed = plan(&image, &[FsFix::Dirty, FsFix::Lost]).unwrap();
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty());
    assert_eq!(fat_entry(&image, kind, 1, 1) & 0x0800_0000, 0x0800_0000);
}

#[test]
fn a_broken_fs_info_is_replaced() {
    let kind = FatType::Fat32;
    let mut image = fat_image(kind).0.finish();
    image[512] ^= 0xFF;
    assert_eq!(kinds(&image), [IssueKind::FsInfoInvalid]);
    let fixed = plan(&image, &[FsFix::FsInfo]).unwrap();
    assert_eq!(fixed.writes.len(), 1);
    assert_eq!(fixed.writes[0].offset, 512);
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty());
}

#[test]
fn fat_is_left_alone_when_the_copies_differ() {
    let kind = FatType::Fat16;
    let mut image = fat_image(kind).0.finish();
    set_fat(&mut image, kind, 7000, end_marker(kind), Some(1));
    set_fat(&mut image, kind, 5000, end_marker(kind), None);
    assert!(matches!(
        plan(&image, &[FsFix::Lost]),
        Err(FsFixError::FatCopiesDiffer)
    ));
}

fn exfat_image() -> (Vec<u8>, Vec<u32>) {
    let mut image = ExfatImage::new();
    let root = image.root();
    let size = image.cluster_size() * 3;
    let placed = image.add_file(root, "photo.jpg", &pattern(4, size), Layout::Fragmented);
    (image.finish(), placed.clusters)
}

#[test]
fn exfat_bitmap_lost_clusters_and_dirty_flag() {
    // 文件的一个簇在位图里是空闲的：标回已占用
    let (mut image, clusters) = exfat_image();
    set_bitmap(&mut image, clusters[1], false);
    assert_eq!(kinds(&image), [IssueKind::NotInBitmap]);
    let fixed = plan(&image, &[FsFix::Bitmap]).unwrap();
    assert_eq!(fixed.counts[0].clusters, 1);
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty());

    // 位图里占用、没人认领的簇：清掉
    let (mut image, _) = exfat_image();
    set_bitmap(&mut image, 900, true);
    set_bitmap(&mut image, 901, true);
    assert_eq!(kinds(&image), [IssueKind::LostClusters]);
    let fixed = plan(&image, &[FsFix::Lost]).unwrap();
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty());

    // 卷标志里的未正常卸下：清掉，校验和不受影响（卷标志不参与校验和）
    let (mut image, _) = exfat_image();
    image[106] |= 0x02;
    reseal_exfat_boot(&mut image);
    assert_eq!(kinds(&image), [IssueKind::VolumeDirty]);
    let fixed = plan(&image, &[FsFix::Dirty]).unwrap();
    assert_eq!((fixed.writes.len(), fixed.writes[0].offset), (1, 0));
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty());
    assert_eq!(read_file(&image, &["photo.jpg"]).len(), 4096 * 3);
}

// ───────────────────────────── 第二批 ─────────────────────────────

/// 改样本里一个 FAT 文件的短文件名项：起始簇（FAT32 另写高 16 位）。
fn set_first_cluster(image: &mut FatImage, placed: &yisi_fs::samples::Placed, cluster: u32) {
    image.patch_short_entry(placed, 26, &(cluster as u16).to_le_bytes());
    image.patch_short_entry(placed, 20, &((cluster >> 16) as u16).to_le_bytes());
}

#[test]
fn fat_copies_are_unified_from_the_chosen_copy() {
    let kind = FatType::Fat16;
    let mut image = fat_image(kind).0.finish();
    // 只有第二份 FAT 里多了一条链：以第一份为准修完就干净，以第二份为准会多出丢失簇
    set_fat(&mut image, kind, 7000, end_marker(kind), Some(1));
    assert_eq!(kinds(&image), [IssueKind::FatCopiesDiffer]);
    let ratings = rate_fat_copies(&device(&image), 0, image.len() as u64).unwrap();
    assert_eq!(ratings[0].fix, FsFix::FatFromFirst);
    assert_eq!(ratings[0].totals, Totals::default());
    assert_eq!(ratings[1].totals.structure, 1);
    assert_eq!(ratings[0].sectors, 1);
    assert!(matches!(
        plan(&image, &[FsFix::FatFromFirst, FsFix::Lost]),
        Err(FsFixError::FatCopiesAlone)
    ));
    let fixed = plan(&image, &[FsFix::FatFromFirst]).unwrap();
    assert_eq!(fixed.writes.len(), 1);
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty());
    assert_eq!(fat_entry(&image, kind, 7000, 1), 0);
}

#[test]
fn broken_chains_are_cut_and_sizes_follow() {
    for kind in [FatType::Fat16, FatType::Fat32] {
        let bad = match kind {
            FatType::Fat16 => 0xFFF7,
            _ => 0x0FFF_FFF7,
        };
        // 链在第 2 个簇处指向空闲：留下前 2 个簇，后面的成了丢失簇
        let (image, clusters) = fat_image(kind);
        let size = image.cluster_size();
        let mut image = image.finish();
        set_fat(&mut image, kind, clusters[1], 0, None);
        assert!(kinds(&image).contains(&IssueKind::ChainFree));
        let fixed = plan(&image, &[FsFix::Chains, FsFix::Lost]).unwrap();
        apply(&mut image, &fixed);
        assert!(kinds(&image).is_empty(), "{kind:?}: {:?}", kinds(&image));
        assert_eq!(
            read_file(&image, &["Docs", "long.bin"]),
            pattern(1, size * 4)[..size * 2]
        );

        // 最后一个簇指回第 2 个簇：在最后一个簇处结束，内容不变
        let (image, clusters) = fat_image(kind);
        let mut image = image.finish();
        set_fat(&mut image, kind, clusters[3], clusters[1], None);
        assert_eq!(kinds(&image), [IssueKind::ChainLoop]);
        let fixed = plan(&image, &[FsFix::Chains]).unwrap();
        apply(&mut image, &fixed);
        assert!(kinds(&image).is_empty());
        assert_eq!(
            read_file(&image, &["Docs", "long.bin"]),
            pattern(1, size * 4)
        );

        // 走进标成坏簇的簇：不要它，之后的簇成了丢失簇
        let (image, clusters) = fat_image(kind);
        let mut image = image.finish();
        set_fat(&mut image, kind, clusters[2], bad, None);
        assert!(kinds(&image).contains(&IssueKind::ChainBadCluster));
        let fixed = plan(&image, &[FsFix::Chains, FsFix::Lost]).unwrap();
        apply(&mut image, &fixed);
        assert_eq!(kinds(&image), [IssueKind::BadClustersMarked], "{kind:?}");
        assert_eq!(read_file(&image, &["Docs", "long.bin"]).len(), size * 2);
    }

    // 起始簇不合法：改为空文件；目录项记的大小比链长：大小改成链的长度
    let kind = FatType::Fat16;
    let mut image = FatImage::new(kind);
    let root = image.root();
    let size = image.cluster_size();
    let a = image.add_file(root, "a.bin", &pattern(5, size * 2), Layout::Contiguous);
    let b = image.add_file(root, "b.bin", &pattern(6, size * 2), Layout::Contiguous);
    set_first_cluster(&mut image, &a, 0xFFF0);
    image.patch_short_entry(&b, 28, &((size * 5) as u32).to_le_bytes());
    let mut image = image.finish();
    let found = kinds(&image);
    assert!(found.contains(&IssueKind::FirstClusterInvalid), "{found:?}");
    assert!(found.contains(&IssueKind::ChainTooShort), "{found:?}");
    let fixed = plan(&image, &[FsFix::Chains, FsFix::Lost]).unwrap();
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty(), "{:?}", kinds(&image));
    assert!(read_file(&image, &["a.bin"]).is_empty());
    assert_eq!(read_file(&image, &["b.bin"]), pattern(6, size * 2));
}

#[test]
fn cross_linked_files_get_their_own_copy() {
    for kind in [FatType::Fat16, FatType::Fat32] {
        let mut image = FatImage::new(kind);
        let root = image.root();
        let size = image.cluster_size();
        let long = image.add_file(root, "long.bin", &pattern(7, size * 4), Layout::Contiguous);
        let small = image.add_file(root, "small.bin", &pattern(8, size * 2), Layout::Contiguous);
        // small.bin 改成从 long.bin 的第 3 个簇开始：两个文件共用最后两个簇，small.bin 原来的簇丢了
        set_first_cluster(&mut image, &small, long.clusters[2]);
        let mut image = image.finish();
        assert!(kinds(&image).contains(&IssueKind::CrossLink));
        let fixed = plan(&image, &[FsFix::CrossLinks, FsFix::Lost]).unwrap();
        let cross = fixed
            .counts
            .iter()
            .find(|count| count.fix == FsFix::CrossLinks)
            .unwrap();
        assert_eq!((cross.items, cross.clusters), (1, 2));
        apply(&mut image, &fixed);
        assert!(kinds(&image).is_empty(), "{kind:?}: {:?}", kinds(&image));
        // 两个文件都还在：long.bin 原样，small.bin 是共用时读到的那两个簇的副本
        assert_eq!(read_file(&image, &["long.bin"]), pattern(7, size * 4));
        assert_eq!(
            read_file(&image, &["small.bin"]),
            pattern(7, size * 4)[size * 2..]
        );
    }
}

#[test]
fn fat_long_names_and_dot_entries_are_repaired() {
    let kind = FatType::Fat32;
    // 改短文件名的一个字母：长文件名的校验和对不上，长文件名项标为已删除，文件留下短文件名
    let (mut image, _) = fat_image(kind);
    let root = image.root();
    let placed = image.add_file(
        root,
        "a long file name.txt",
        &pattern(9, 100),
        Layout::Contiguous,
    );
    image.patch_short_entry(&placed, 1, b"X");
    let mut image = image.finish();
    assert_eq!(kinds(&image), [IssueKind::LfnChecksum]);
    let fixed = plan(&image, &[FsFix::Entries]).unwrap();
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty());

    // 子目录的 `.` 指向别处：重写 `.` 与 `..`
    let mut image = FatImage::new(kind);
    let root = image.root();
    let folder = image.mkdir(root, "folder");
    let first = folder.first_cluster();
    image.add_file(
        folder.as_dir(),
        "inside.txt",
        &pattern(10, 50),
        Layout::Contiguous,
    );
    let mut bytes = image.finish();
    let (reserved, sectors, fats) = crate::samples::fat_geometry(&bytes);
    let at = (reserved + fats * sectors) * 512 + (first as usize - 2) * 512;
    bytes[at + 26] = bytes[at + 26].wrapping_add(1);
    assert_eq!(kinds(&bytes), [IssueKind::DotEntries]);
    let fixed = plan(&bytes, &[FsFix::Entries]).unwrap();
    apply(&mut bytes, &fixed);
    assert!(kinds(&bytes).is_empty());
    assert_eq!(
        read_file(&bytes, &["folder", "inside.txt"]),
        pattern(10, 50)
    );
}

/// 样本 exFAT 根目录（4 号簇）里名字以 `name` 开头的那组项：文件项的位置。
fn exfat_primary(image: &[u8], name: &str) -> usize {
    let root = crate::samples::exfat_heap(image) + 2 * 4096;
    let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let at = (root..root + 4096 - units.len())
        .find(|&at| image[at..at + units.len()] == units[..])
        .unwrap();
    let primary = at - 2 - 64;
    assert_eq!(image[primary] | 0x80, 0x85);
    primary
}

/// 按规范重算样本里一组 exFAT 目录项的校验和。
fn reseal_set(image: &mut [u8], primary: usize) {
    let entries = usize::from(image[primary + 1]) + 1;
    let raws: Vec<[u8; 32]> = image[primary..primary + entries * 32]
        .as_chunks::<32>()
        .0
        .to_vec();
    let checksum = exfat_set_checksum(&raws);
    image[primary + 2..primary + 4].copy_from_slice(&checksum.to_le_bytes());
}

fn exfat_named() -> Vec<u8> {
    let mut image = ExfatImage::new();
    let root = image.root();
    image.add_file(root, "photo.jpg", &pattern(11, 5000), Layout::Contiguous);
    image.add_file(
        root,
        "a name that needs more than one name entry.txt",
        &pattern(12, 300),
        Layout::Contiguous,
    );
    image.finish()
}

#[test]
fn exfat_entry_sets_are_repaired() {
    // 校验和坏了：重算
    let mut image = exfat_named();
    let primary = exfat_primary(&image, "photo");
    image[primary + 2] ^= 0xFF;
    assert_eq!(kinds(&image), [IssueKind::EntrySetChecksum]);
    let fixed = plan(&image, &[FsFix::Entries]).unwrap();
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty());

    // 文件名长度写成 3（校验和照着改对）：按文件名项里的字符数改回
    let mut image = exfat_named();
    let primary = exfat_primary(&image, "a name");
    image[primary + 32 + 3] = 3;
    reseal_set(&mut image, primary);
    assert_eq!(kinds(&image), [IssueKind::EntryNameLength]);
    let fixed = plan(&image, &[FsFix::Entries]).unwrap();
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty());
    assert_eq!(
        read_file(&image, &["a name that needs more than one name entry.txt"]),
        pattern(12, 300)
    );

    // 次项个数多写一项：按后面连着的次项改回
    let mut image = exfat_named();
    let primary = exfat_primary(&image, "a name");
    image[primary + 1] += 1;
    assert!(kinds(&image).contains(&IssueKind::EntrySetCount));
    let fixed = plan(&image, &[FsFix::Entries]).unwrap();
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty(), "{:?}", kinds(&image));

    // 有效长度比数据长度大：改成一样
    let mut image = exfat_named();
    let primary = exfat_primary(&image, "photo");
    image[primary + 32 + 8..primary + 32 + 16].copy_from_slice(&9000u64.to_le_bytes());
    reseal_set(&mut image, primary);
    assert_eq!(kinds(&image), [IssueKind::SizeInvalid]);
    let fixed = plan(&image, &[FsFix::Entries]).unwrap();
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty());
    assert_eq!(read_file(&image, &["photo.jpg"]), pattern(11, 5000));

    // 文件项被删、次项还在：次项标为已删除，文件的簇成了丢失簇，一起释放
    let mut image = exfat_named();
    let primary = exfat_primary(&image, "photo");
    image[primary] &= 0x7F;
    let found = kinds(&image);
    assert!(found.contains(&IssueKind::OrphanEntries), "{found:?}");
    let fixed = plan(&image, &[FsFix::Entries, FsFix::Lost]).unwrap();
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty(), "{:?}", kinds(&image));
    assert_eq!(image[primary + 32] & 0x80, 0);
}

/// 样本 exFAT 的 FAT 里簇 `cluster` 的表项改成 `value`。
fn set_exfat_fat(image: &mut [u8], cluster: u32, value: u32) {
    let fat = u32::from_le_bytes(image[80..84].try_into().unwrap()) as usize * 512;
    let at = fat + cluster as usize * 4;
    image[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[test]
fn exfat_broken_chains_and_cross_links_are_repaired() {
    // 链在第 2 个簇处指向空闲：截短，大小与有效长度跟着改，校验和重算
    let (mut image, clusters) = exfat_image();
    set_exfat_fat(&mut image, clusters[1], 0);
    let found = kinds(&image);
    assert!(found.contains(&IssueKind::ChainFree), "{found:?}");
    let fixed = plan(&image, &[FsFix::Chains, FsFix::Lost]).unwrap();
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty(), "{:?}", kinds(&image));
    assert_eq!(
        read_file(&image, &["photo.jpg"]),
        pattern(4, 4096 * 3)[..4096 * 2]
    );

    // 第二个文件从 photo.jpg 的第 2 个簇开始（走 FAT 链）：复制一份，位图里标上新簇
    let mut sample = ExfatImage::new();
    let root = sample.root();
    let photo = sample.add_file(root, "photo.jpg", &pattern(4, 4096 * 3), Layout::Fragmented);
    sample.add_file(root, "copy.bin", &pattern(5, 4096 * 2), Layout::Fragmented);
    let mut image = sample.finish();
    let primary = exfat_primary(&image, "copy");
    image[primary + 32 + 20..primary + 32 + 24].copy_from_slice(&photo.clusters[1].to_le_bytes());
    reseal_set(&mut image, primary);
    let found = kinds(&image);
    assert!(found.contains(&IssueKind::CrossLink), "{found:?}");
    let fixed = plan(&image, &[FsFix::CrossLinks, FsFix::Lost]).unwrap();
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty(), "{:?}", kinds(&image));
    assert_eq!(read_file(&image, &["photo.jpg"]), pattern(4, 4096 * 3));
    assert_eq!(
        read_file(&image, &["copy.bin"]),
        pattern(4, 4096 * 3)[4096..]
    );
}

/// 落在坏区里的读取报错。
struct FlakyDisk {
    inner: MemoryDisk,
    bad: (u64, u64),
}

impl BlockDevice for FlakyDisk {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn sector_size(&self) -> u32 {
        512
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, yisi_io::IoError> {
        let end = offset + buf.len() as u64;
        if offset < self.bad.1 && self.bad.0 < end {
            return Err(yisi_io::IoError::Read {
                offset,
                len: buf.len(),
                source: std::io::Error::other("坏扇区"),
            });
        }
        self.inner.read_at(offset, buf)
    }
}

#[test]
fn unreadable_free_clusters_are_marked_bad() {
    // FAT16：簇 6000 读不出；簇 6001 读得出，给了也不标
    let kind = FatType::Fat16;
    let (image, _) = fat_image(kind);
    let size = image.cluster_size() as u64;
    let mut image = image.finish();
    // FAT16 的簇区在根目录区之后
    let (reserved, sectors, fats) = crate::samples::fat_geometry(&image);
    let root_entries = u16::from_le_bytes([image[17], image[18]]) as usize;
    let heap = ((reserved + fats * sectors) * 512 + root_entries * 32) as u64;
    let bad_at = heap + (6000 - 2) * size;
    let flaky: Arc<dyn BlockDevice> = Arc::new(FlakyDisk {
        inner: MemoryDisk::new(image.clone(), 512),
        bad: (bad_at, bad_at + size),
    });
    let candidates = [Run {
        first: 6000,
        count: 2,
    }];
    let fixed = plan_fs_fix(
        &flaky,
        0,
        image.len() as u64,
        &[FsFix::BadClusters],
        &candidates,
    )
    .unwrap();
    assert_eq!((fixed.counts[0].items, fixed.counts[0].clusters), (1, 1));
    apply(&mut image, &fixed);
    assert_eq!(fat_entry(&image, kind, 6000, 0), 0xFFF7);
    assert_eq!(fat_entry(&image, kind, 6000, 1), 0xFFF7);
    assert_eq!(fat_entry(&image, kind, 6001, 0), 0);
    assert_eq!(kinds(&image), [IssueKind::BadClustersMarked]);
    // 都读得出：没有要标的
    assert!(matches!(
        plan_fs_fix(
            &device(&image),
            0,
            image.len() as u64,
            &[FsFix::BadClusters],
            &[Run {
                first: 6001,
                count: 1
            }]
        ),
        Err(FsFixError::NothingToFix(FsFix::BadClusters))
    ));

    // exFAT：FAT 里标坏簇、位图里标占用；检查不把它当丢失簇
    let (mut image, _) = exfat_image();
    let heap = crate::samples::exfat_heap(&image) as u64;
    let bad_at = heap + (900 - 2) * 4096;
    let flaky: Arc<dyn BlockDevice> = Arc::new(FlakyDisk {
        inner: MemoryDisk::new(image.clone(), 512),
        bad: (bad_at, bad_at + 4096),
    });
    let candidates = [Run {
        first: 900,
        count: 1,
    }];
    let fixed = plan_fs_fix(
        &flaky,
        0,
        image.len() as u64,
        &[FsFix::BadClusters],
        &candidates,
    )
    .unwrap();
    apply(&mut image, &fixed);
    assert!(kinds(&image).is_empty(), "{:?}", kinds(&image));
    let fat = u32::from_le_bytes(image[80..84].try_into().unwrap()) as usize * 512;
    assert_eq!(
        image[fat + 900 * 4..fat + 900 * 4 + 4],
        0xFFFF_FFF7u32.to_le_bytes()
    );
}

#[test]
fn available_lists_what_the_check_found() {
    let kind = FatType::Fat16;
    let (image, clusters) = fat_image(kind);
    let mut image = image.finish();
    set_fat(&mut image, kind, clusters[3], clusters[1], None);
    set_fat(&mut image, kind, 6000, end_marker(kind), None);
    let disk = MemoryDisk::new(image.clone(), 512);
    let (_, findings) = check_for_repair(&disk, 0, image.len() as u64, &Quiet).unwrap();
    let fixes: Vec<FsFix> = available(&findings).iter().map(|count| count.fix).collect();
    assert_eq!(fixes, [FsFix::Lost, FsFix::Chains]);
}
