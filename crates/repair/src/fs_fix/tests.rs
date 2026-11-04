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
    plan_fs_fix(&device(image), 0, image.len() as u64, fixes)
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
