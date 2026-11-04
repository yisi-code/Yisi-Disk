//! `$MFT` 与 `$MFTMirr` 互相修复：在搜索用的整盘样本的 NTFS 分区上改坏一边，生成计划、写进样本，再比较两边。
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;

use yisi_fs::samples::MemoryDisk;

use super::*;
use crate::samples::{SearchDisk, search_disk};

fn device(image: &[u8]) -> Arc<dyn BlockDevice> {
    Arc::new(MemoryDisk::new(image.to_vec(), 512))
}

/// NTFS 分区的起点、长度，`$MFT` 与 `$MFTMirr` 在设备上的起点，记录大小。
fn layout(disk: &SearchDisk) -> (u64, u64, usize, usize, usize) {
    let (offset, length) = disk.ntfs;
    let start = offset as usize;
    let geometry = bpb::check_ntfs(&disk.image[start..start + 512])
        .geometry
        .unwrap();
    let at = |lcn: u64| start + (lcn * geometry.cluster_size) as usize;
    (
        offset,
        length,
        at(geometry.mft_lcn),
        at(geometry.mirror_lcn),
        geometry.record_size as usize,
    )
}

fn apply(image: &mut [u8], writes: &[SectorWrite]) {
    for write in writes {
        let at = write.offset as usize;
        assert_eq!(&image[at..at + write.before.len()], write.before.as_slice());
        image[at..at + write.after.len()].copy_from_slice(&write.after);
    }
}

fn plan(image: &[u8], offset: u64, length: u64, fix: MftFix) -> Result<MftFixPlan, MftFixError> {
    plan_mft_fix(&device(image), offset, length, fix)
}

#[test]
fn a_healthy_volume_needs_nothing() {
    let disk = search_disk();
    let (offset, length, mft, mirror, record) = layout(&disk);
    assert_eq!(
        disk.image[mft..mft + 4 * record],
        disk.image[mirror..mirror + 4 * record]
    );
    for fix in [MftFix::MirrorFromMft, MftFix::MftFromMirror] {
        assert!(matches!(
            plan(&disk.image, offset, length, fix),
            Err(MftFixError::NotApplicable)
        ));
    }
}

#[test]
fn a_stale_mirror_record_is_rewritten_from_the_mft() {
    let mut disk = search_disk();
    let (offset, length, mft, mirror, record) = layout(&disk);
    // 镜像里的第 3 条记录（$Volume）与 $MFT 里的不同：改记录中间的一个字节（不碰更新序列所在的扇区末尾）。
    // 样本只造了第 0、3 条记录，第 1、2 条是空的：两边都不可信，不会拿来互相覆盖
    disk.image[mirror + 3 * record + 300] ^= 0xFF;
    let planned = plan(&disk.image, offset, length, MftFix::MirrorFromMft).unwrap();
    assert_eq!(planned.records, [3]);
    assert_eq!((planned.from, planned.to), (mft as u64, mirror as u64));
    apply(&mut disk.image, &planned.writes);
    assert_eq!(
        disk.image[mft..mft + 4 * record],
        disk.image[mirror..mirror + 4 * record]
    );
    // 反方向不适用：$MFT 里的记录都可信
    assert!(matches!(
        plan(&disk.image, offset, length, MftFix::MftFromMirror),
        Err(MftFixError::NotApplicable)
    ));
}

#[test]
fn a_damaged_mft_record_is_restored_from_the_mirror() {
    let mut disk = search_disk();
    let original = disk.image.clone();
    let (offset, length, mft, _, record) = layout(&disk);
    // $MFT 自己那条记录的签名被改坏
    disk.image[mft..mft + 4].copy_from_slice(b"BAAD");
    assert!(!mft_self_record_ok(&disk.image[mft..mft + record]));
    // $MFT 自己不可信：不能拿它去重写镜像
    assert!(matches!(
        plan(&disk.image, offset, length, MftFix::MirrorFromMft),
        Err(MftFixError::NotApplicable)
    ));
    let planned = plan(&disk.image, offset, length, MftFix::MftFromMirror).unwrap();
    assert_eq!(planned.records, [0]);
    apply(&mut disk.image, &planned.writes);
    assert!(disk.image == original);
}

#[test]
fn an_unusable_boot_sector_is_refused() {
    let mut disk = search_disk();
    let (offset, length, ..) = layout(&disk);
    disk.wipe_primary_boot(offset);
    assert!(matches!(
        plan(&disk.image, offset, length, MftFix::MirrorFromMft),
        Err(MftFixError::BootInvalid)
    ));
}
