//! 从 `$LogFile` 找回：样本按 Windows 建文件、写内容、删除时记日志的样子生成日志，再把 MFT 记录重用或清掉。

use std::sync::Arc;

use yisi_io::BlockDevice;

use crate::ntfs::Ntfs;
use crate::samples::{
    MemoryDisk, NTFS_LOG_FIRST_RECORD_PAGE, NTFS_LOG_LCN, NTFS_LOG_PAGE, NTFS_ROOT, NtfsImage,
    pattern,
};
use crate::tests::{find, names, open, read_all};
use crate::{Entry, FsError, Region, ScanStage, ScanSummary, State, Volume};

fn scan(volume: &Volume) -> ScanSummary {
    volume.scan(&mut |_, _| true).unwrap().unwrap()
}

fn list(volume: &Volume, dir: &Entry) -> Vec<Entry> {
    volume.list(&dir.file).unwrap().entries
}

fn root(volume: &Volume) -> Vec<Entry> {
    volume.list(&volume.root()).unwrap().entries
}

/// 建、写、删都记进日志，再删除：返回记录号。
fn logged_delete(sample: &mut NtfsImage, record: u64) -> u64 {
    sample.log_history(record);
    sample.log_deletion(record);
    sample.delete(record);
    record
}

#[test]
fn reused_and_cleared_records_are_found_in_the_log_and_read() {
    let mut sample = NtfsImage::new();
    sample.enable_log();
    let docs = sample.mkdir(NTFS_ROOT, "docs");
    let report = pattern(1, 20_000);
    let reused = sample.add_file(docs, "report.bin", &report);
    logged_delete(&mut sample, reused);
    // 记录被新文件重用（序号再加 1）
    sample.reuse(reused, NTFS_ROOT, "new.txt", &pattern(2, 100));
    let fragmented = pattern(3, 5 * 4096 + 123);
    let cleared = sample.add_fragmented(NTFS_ROOT, "frag.bin", &fragmented);
    logged_delete(&mut sample, cleared);
    sample.wipe(cleared);
    let volume = open(sample.finish());

    let summary = scan(&volume);
    assert!(summary.log_records >= 14, "{summary:?}");
    assert_eq!(summary.log_names, 2, "{summary:?}");
    // MFT 里已找不到它们
    assert_eq!(summary.deleted_names, 0, "{summary:?}");

    let entries = root(&volume);
    assert!(!find(&entries, "new.txt").deleted());
    let frag = find(&entries, "frag.bin");
    assert!(frag.file.from_log() && frag.deleted());
    assert_eq!(
        (frag.size, frag.state),
        (fragmented.len() as u64, State::Intact)
    );
    assert_eq!(read_all(&volume, &frag.file), fragmented);

    let docs = list(&volume, find(&entries, "docs"));
    let found = find(&docs, "report.bin");
    assert!(found.file.from_log());
    assert_eq!(found.file.record(), Some(reused));
    assert_eq!(found.state, State::Intact);
    assert!(found.position.is_some());
    assert_eq!(read_all(&volume, &found.file), report);
    // 定位信息编成字符串再解回来照样读得出
    let token = crate::FileRef::from_token(&found.file.to_token()).unwrap();
    assert_eq!(read_all(&volume, &token), report);
}

#[test]
fn records_still_in_the_mft_are_not_listed_twice() {
    let mut sample = NtfsImage::new();
    sample.enable_log();
    let gone = sample.add_file(NTFS_ROOT, "gone.bin", &pattern(4, 9000));
    logged_delete(&mut sample, gone);
    let volume = open(sample.finish());
    let summary = scan(&volume);
    assert_eq!((summary.deleted_names, summary.log_names), (1, 0));
    let entries = root(&volume);
    let matching: Vec<_> = entries
        .iter()
        .filter(|entry| entry.name == "gone.bin")
        .collect();
    assert_eq!(matching.len(), 1, "{:?}", names(&entries));
    assert!(!matching[0].file.from_log());
}

#[test]
fn name_only_entries_are_listed_but_not_readable() {
    let mut sample = NtfsImage::new();
    sample.enable_log();
    // 建得比日志还早：日志里只有删除
    let old = sample.add_file(NTFS_ROOT, "old.bin", &pattern(5, 9000));
    sample.log_deletion(old);
    sample.delete(old);
    sample.wipe(old);
    // 常驻的小文件：内容的写入只记了长度
    let small = sample.add_file(NTFS_ROOT, "small.txt", &pattern(6, 300));
    logged_delete(&mut sample, small);
    sample.wipe(small);
    let volume = open(sample.finish());
    assert_eq!(scan(&volume).log_names, 2);
    let entries = root(&volume);
    for (name, size) in [("old.bin", 9000), ("small.txt", 300)] {
        let entry = find(&entries, name);
        assert!(entry.file.from_log(), "{name}");
        assert_eq!((entry.state, entry.size), (State::Cleared, size), "{name}");
        assert_eq!(entry.position, None, "{name}");
        let error = volume.open_file(&entry.file).err().unwrap();
        assert!(
            matches!(error, FsError::Invalid(message) if message.contains("只留下了这个文件的名字")),
            "{name}"
        );
    }
}

#[test]
fn deleted_folders_from_the_log_hold_their_files() {
    let mut sample = NtfsImage::new();
    sample.enable_log();
    let old = sample.mkdir(NTFS_ROOT, "old");
    let inner_data = pattern(7, 12_000);
    let inner = sample.add_file(old, "inner.bin", &inner_data);
    sample.log_history(old);
    sample.log_history(inner);
    sample.log_deletion(inner);
    sample.log_deletion(old);
    sample.delete(inner);
    sample.delete(old);
    sample.wipe(inner);
    sample.wipe(old);
    // 上级目录日志里也找不到的：列进「找不到原目录的文件」
    let lost_dir = sample.mkdir(NTFS_ROOT, "lost");
    let stray_data = pattern(8, 7000);
    let stray = sample.add_file(lost_dir, "stray.bin", &stray_data);
    logged_delete(&mut sample, stray);
    sample.wipe(stray);
    sample.delete(lost_dir);
    sample.wipe(lost_dir);
    let volume = open(sample.finish());
    let summary = scan(&volume);
    assert_eq!(summary.log_names, 3, "{summary:?}");
    assert_eq!(summary.orphans, 1, "{summary:?}");

    let entries = root(&volume);
    let folder = find(&entries, "old");
    assert!(folder.is_dir() && folder.file.from_log());
    let inside = list(&volume, folder);
    assert_eq!(names(&inside), ["inner.bin"]);
    assert_eq!(read_all(&volume, &inside[0].file), inner_data);

    let orphans = list(&volume, find(&entries, "$Orphans"));
    let stray = find(&orphans, "stray.bin");
    assert!(stray.file.from_log());
    assert_eq!(read_all(&volume, &stray.file), stray_data);
}

#[test]
fn torn_log_pages_are_skipped() {
    let mut sample = NtfsImage::new();
    sample.enable_log();
    let first = sample.add_file(NTFS_ROOT, "first.bin", &pattern(9, 9000));
    sample.log_history(first);
    // 把后面的记录挤到下一页
    sample.log_padding(4000);
    let second_data = pattern(10, 9000);
    let second = sample.add_file(NTFS_ROOT, "second.bin", &second_data);
    sample.log_history(second);
    sample.log_deletion(first);
    sample.log_deletion(second);
    sample.delete(first);
    sample.delete(second);
    sample.wipe(first);
    sample.wipe(second);
    let mut image = sample.finish();
    // 第一页日志记录写到一半：第一个扇区末尾不是更新序列号（缓冲页里的副本完好，也不该被拿来用）
    let page = NTFS_LOG_LCN * 4096 + NTFS_LOG_FIRST_RECORD_PAGE * NTFS_LOG_PAGE;
    image[page + 510] ^= 0xFF;
    // 直接读日志：坏页跳过一页，副本里的记录不算
    let length = image.len() as u64;
    let device: Arc<dyn BlockDevice> = Arc::new(MemoryDisk::new(image.clone(), 512));
    let ntfs = Ntfs::open(Region::new(device, 0, length), &image[..512]).unwrap();
    let log = super::read(&ntfs, &mut |_, _| true).unwrap().unwrap();
    assert_eq!(log.torn_pages, 1);
    let volume = open(image);
    let summary = scan(&volume);
    // 坏页之后的记录：second.bin 的 6 条、两个文件的删除各 2 条
    assert_eq!(summary.log_records, log.records);
    assert_eq!(log.records, 10, "{summary:?}");
    assert_eq!(summary.log_names, 2, "{summary:?}");
    let entries = root(&volume);
    // first.bin 的建立在坏掉的那页里：只剩删除时的名字
    let first = find(&entries, "first.bin");
    assert_eq!(first.state, State::Cleared);
    let second = find(&entries, "second.bin");
    assert_eq!(second.state, State::Intact);
    assert_eq!(read_all(&volume, &second.file), second_data);
}

#[test]
fn reading_the_log_is_a_separate_scan_stage_and_can_be_cancelled() {
    let mut sample = NtfsImage::new();
    sample.enable_log();
    let gone = sample.add_file(NTFS_ROOT, "gone.bin", &pattern(11, 9000));
    logged_delete(&mut sample, gone);
    sample.wipe(gone);
    let volume = open(sample.finish());
    let cancelled = volume
        .scan(&mut |_, _| volume.scan_stage() != ScanStage::Log)
        .unwrap();
    assert_eq!(cancelled, None);
    assert!(volume.needs_scan());
    let mut stages = Vec::new();
    let summary = volume
        .scan(&mut |done, total| {
            stages.push((volume.scan_stage(), done, total));
            true
        })
        .unwrap()
        .unwrap();
    assert_eq!(summary.log_names, 1);
    let log: Vec<_> = stages
        .iter()
        .filter(|(stage, ..)| *stage == ScanStage::Log)
        .collect();
    assert!(
        stages
            .first()
            .is_some_and(|(stage, ..)| *stage == ScanStage::Mft)
    );
    assert!(
        log.last()
            .is_some_and(|(_, done, total)| done == total && *total == 65_536)
    );
}

#[test]
fn volumes_without_a_log_scan_as_before() {
    let mut sample = NtfsImage::new();
    let gone = sample.add_file(NTFS_ROOT, "gone.bin", &pattern(12, 100));
    sample.delete(gone);
    let volume = open(sample.finish());
    let summary = scan(&volume);
    assert_eq!((summary.log_records, summary.log_names), (0, 0));
    assert_eq!(summary.deleted_names, 1);
}

/// 日志区里任意改坏若干字节：扫描照样完成，不 panic。
#[test]
fn mutated_logs_never_panic() {
    let mut sample = NtfsImage::new();
    sample.enable_log();
    let dir = sample.mkdir(NTFS_ROOT, "dir");
    sample.log_history(dir);
    for index in 0..6 {
        let record =
            sample.add_fragmented(dir, &format!("file {index}.bin"), &pattern(index, 9000));
        logged_delete(&mut sample, record);
        if index % 2 == 0 {
            sample.wipe(record);
        }
    }
    let image = sample.finish();
    let start = NTFS_LOG_LCN * 4096;
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..200 {
        let mut mutated = image.clone();
        for _ in 0..1 + next() % 16 {
            let at = start + (next() % (16 * 4096)) as usize;
            mutated[at] = next() as u8;
        }
        let volume = open(mutated);
        if let Ok(Some(_)) = volume.scan(&mut |_, _| true) {
            let entries = root(&volume);
            for entry in entries.iter().filter(|entry| entry.file.from_log()) {
                if entry.is_dir() {
                    let _ = volume.list(&entry.file);
                } else if let Ok(mut reader) = volume.open_file(&entry.file) {
                    let _ = std::io::copy(&mut reader, &mut std::io::sink());
                }
            }
        }
    }
}
