//! 用按规范拼出的镜像检查目录读取、删除识别与文件读出。

use std::io::Read;
use std::sync::Arc;

use yisi_part::samples::{self as part_samples, MbrEntry};

use crate::samples::{
    ExfatImage, FatImage, FatType, Layout, MemoryDisk, NTFS_ROOT, Noise, NtfsImage, SAMPLE_TIME,
    TAG_WOF, pattern,
};
use crate::{Entry, FileRef, FsError, FsKind, ScanSummary, State, Unsupported, Volume, detect};

fn open(image: Vec<u8>) -> Volume {
    let length = image.len() as u64;
    Volume::open(Arc::new(MemoryDisk::new(image, 512)), 0, length).unwrap()
}

fn read_all(volume: &Volume, file: &FileRef) -> Vec<u8> {
    let mut data = Vec::new();
    volume
        .open_file(file)
        .unwrap()
        .read_to_end(&mut data)
        .unwrap();
    data
}

fn find<'a>(entries: &'a [Entry], name: &str) -> &'a Entry {
    entries
        .iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("找不到 {name}：{:?}", names(entries)))
}

fn names(entries: &[Entry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.name.as_str()).collect()
}

/// 三种 FAT 都要满足的情况：长文件名、Unicode、嵌套目录、碎片文件、删除的文件与目录、被覆盖的文件。
fn exercise_fat(kind: FatType) {
    let mut sample = FatImage::new(kind);
    let cs = sample.cluster_size();
    let root = sample.root();
    sample.set_label(b"MY DISK    ");
    let hello = pattern(1, cs * 3 + 17);
    sample.add_file(root, "hello world.txt", &hello, Layout::Contiguous);
    sample.add_short_file(root, "README.TXT", &pattern(2, 100));
    let photos = sample.mkdir(root, "Photos").as_dir();
    let photo = pattern(3, cs * 4 + 1);
    sample.add_file(photos, "照片 2024-03-05.jpg", &photo, Layout::Fragmented);

    let report = pattern(4, cs * 2 + 9);
    let placed = sample.add_file(root, "deleted report.docx", &report, Layout::Contiguous);
    sample.delete(&placed);
    let old = sample.add_short_file(root, "OLD.TXT", &pattern(5, 10));
    sample.delete(&old);
    let victim = sample.add_file(root, "victim.bin", &pattern(6, cs * 2), Layout::Contiguous);
    sample.delete(&victim);
    sample.add_file(
        root,
        "new.bin",
        &pattern(7, cs),
        Layout::At(victim.first_cluster()),
    );

    let trash = sample.mkdir(root, "Trash");
    let inner = pattern(8, cs + 5);
    let inner_placed =
        sample.add_file(trash.as_dir(), "inner file.dat", &inner, Layout::Contiguous);
    sample.delete(&inner_placed);
    sample.delete(&trash);

    let volume = open(sample.finish());
    let expected_kind = match kind {
        FatType::Fat12 => FsKind::Fat12,
        FatType::Fat16 => FsKind::Fat16,
        FatType::Fat32 => FsKind::Fat32,
    };
    assert_eq!(volume.kind(), expected_kind);
    // 根目录里的卷标优先于引导扇区里的
    assert_eq!(volume.label(), Some("MY DISK"));

    let listing = volume.list(&volume.root()).unwrap();
    assert!(!listing.truncated && !listing.overwritten);
    let entries = &listing.entries;

    let entry = find(entries, "hello world.txt");
    assert_eq!(
        (entry.state, entry.size, entry.is_dir()),
        (State::Live, hello.len() as u64, false)
    );
    assert_eq!(entry.modified.as_deref(), Some(SAMPLE_TIME));
    assert_eq!(entry.short_name.as_deref(), Some("HELLOW~1.TXT"));
    assert_eq!(read_all(&volume, &entry.file), hello);

    let readme = find(entries, "README.TXT");
    assert!(!readme.name_uncertain);
    assert_eq!(read_all(&volume, &readme.file), pattern(2, 100));

    // 碎片文件沿 FAT 链读出
    let photos = find(entries, "Photos");
    assert!(photos.is_dir() && photos.state == State::Live);
    let inside = volume.list(&photos.file).unwrap().entries;
    assert_eq!(names(&inside), ["照片 2024-03-05.jpg"]);
    assert_eq!(read_all(&volume, &inside[0].file), photo);

    // 删除的文件：长文件名完整，簇空闲，按连续存放读出
    let deleted = find(entries, "deleted report.docx");
    assert!(deleted.deleted());
    assert_eq!(deleted.state, State::Intact);
    assert_eq!(deleted.short_name.as_deref(), Some("DELETE~1.DOC"));
    assert_eq!(read_all(&volume, &deleted.file), report);

    // 只有短文件名的删除项丢了首字母
    let old = find(entries, "_LD.TXT");
    assert!(old.deleted() && old.name_uncertain);
    assert_eq!(read_all(&volume, &old.file), pattern(5, 10));

    // 新文件写进了已删除文件的位置。样本的 FAT32 簇数超过 16 位，首簇高位为 0，说不准（见下面的专门测试）
    let victim = find(entries, "victim.bin");
    assert!(victim.deleted());
    let expected = if kind == FatType::Fat32 {
        State::Unknown
    } else {
        State::Overwritten
    };
    assert_eq!(victim.state, expected);
    assert_eq!(find(entries, "new.bin").state, State::Live);

    // 删除的目录：开头仍是 `.` 与 `..`，里面的项按已删除列出
    let trash = find(entries, "Trash");
    assert!(trash.is_dir() && trash.deleted());
    assert_eq!(trash.state, State::Intact);
    let trash_listing = volume.list(&trash.file).unwrap();
    assert!(!trash_listing.overwritten);
    let inner_entry = find(&trash_listing.entries, "inner file.dat");
    assert!(inner_entry.deleted());
    assert_eq!(read_all(&volume, &inner_entry.file), inner);

    // 定位信息编成字符串再解回来，照样能读
    let token = deleted.file.to_token();
    assert_eq!(
        read_all(&volume, &FileRef::from_token(&token).unwrap()),
        report
    );
}

#[test]
fn fat12_volume_is_browsed() {
    exercise_fat(FatType::Fat12);
}

#[test]
fn fat16_volume_is_browsed() {
    exercise_fat(FatType::Fat16);
}

#[test]
fn fat32_volume_is_browsed() {
    exercise_fat(FatType::Fat32);
}

#[test]
fn deleted_fragmented_fat_file_is_read_as_contiguous() {
    // 已知的局限：FAT 删除后簇链清零，碎片文件只能按连续存放读，读出的内容不对。
    // 中间空着的簇都没被占用，所以仍判为「可能完整」——可信度只看占用情况，界面要说明。
    let mut sample = FatImage::new(FatType::Fat16);
    let cs = sample.cluster_size();
    let root = sample.root();
    let data = pattern(9, cs * 3);
    let placed = sample.add_file(root, "split.bin", &data, Layout::Fragmented);
    sample.delete(&placed);
    let volume = open(sample.finish());
    let entries = volume.list(&volume.root()).unwrap().entries;
    let entry = find(&entries, "split.bin");
    assert_eq!(entry.state, State::Intact);
    let read = read_all(&volume, &entry.file);
    assert_eq!(read[..cs], data[..cs]);
    assert_ne!(read, data);
}

#[test]
fn fat32_cleared_high_word_makes_overwritten_files_unknown() {
    let mut sample = FatImage::new(FatType::Fat32);
    let root = sample.root();
    let cs = sample.cluster_size();
    let victim = sample.add_file(root, "victim.bin", &pattern(1, cs), Layout::Contiguous);
    sample.delete(&victim);
    sample.add_file(
        root,
        "new.bin",
        &pattern(2, cs),
        Layout::At(victim.first_cluster()),
    );
    let volume = open(sample.finish());
    let entries = volume.list(&volume.root()).unwrap().entries;
    // 簇数超过 16 位，首簇高位为 0：被占用可能只是首簇找错了
    assert!(u64::from(FatImage::new(FatType::Fat32).cluster_count()) > 0xFFFF);
    assert_eq!(find(&entries, "victim.bin").state, State::Unknown);
}

#[test]
fn overwritten_deleted_directory_lists_nothing() {
    let mut sample = FatImage::new(FatType::Fat16);
    let root = sample.root();
    let gone = sample.mkdir(root, "Gone");
    sample.delete(&gone);
    sample.add_file(
        root,
        "filler.bin",
        &pattern(3, 100),
        Layout::At(gone.first_cluster()),
    );
    let volume = open(sample.finish());
    let entries = volume.list(&volume.root()).unwrap().entries;
    let gone = find(&entries, "Gone");
    assert_eq!(gone.state, State::Overwritten);
    let listing = volume.list(&gone.file).unwrap();
    assert!(listing.overwritten && listing.entries.is_empty());
}

#[test]
fn deleted_directory_never_written_to_disk_is_overwritten() {
    // 刚建就删的目录，数据可能从没离开写缓存：簇是空闲的，开头却不是 `.` 与 `..`
    let mut sample = FatImage::new(FatType::Fat16);
    let root = sample.root();
    let gone = sample.mkdir(root, "Never Written");
    sample.delete(&gone);
    sample.zero_cluster(gone.first_cluster());
    let volume = open(sample.finish());
    let entries = volume.list(&volume.root()).unwrap().entries;
    let gone = find(&entries, "Never Written");
    assert_eq!(gone.state, State::Overwritten);
    assert!(volume.list(&gone.file).unwrap().overwritten);
}

#[test]
fn exfat_volume_is_browsed() {
    let mut sample = ExfatImage::new();
    let cs = sample.cluster_size();
    let root = sample.root();
    sample.set_label("数据盘");
    let plain = pattern(1, cs * 2 + 3);
    sample.add_file(root, "plain.txt", &plain, Layout::Contiguous);
    let docs = sample.mkdir(root, "文档").as_dir();
    let chained = pattern(2, cs * 3 + 1);
    sample.add_file(
        docs,
        "a very long file name that needs three name entries.bin",
        &chained,
        Layout::Fragmented,
    );

    let gone = pattern(3, cs + 7);
    let placed = sample.add_file(root, "gone.jpg", &gone, Layout::Contiguous);
    sample.delete(&placed);
    // 走 FAT 链的文件删除后 FAT 表项还在，照链读出
    let gone_chain = pattern(4, cs * 2 + 1);
    let placed = sample.add_file(root, "gone chain.mp4", &gone_chain, Layout::Fragmented);
    sample.delete(&placed);
    let victim = sample.add_file(root, "victim.bin", &pattern(5, cs), Layout::Contiguous);
    sample.delete(&victim);
    sample.add_file(
        root,
        "new.bin",
        &pattern(6, cs),
        Layout::At(victim.first_cluster()),
    );
    let old_dir = sample.mkdir(root, "Old");
    let old_file = pattern(7, 300);
    let old_placed = sample.add_file(old_dir.as_dir(), "old.txt", &old_file, Layout::Contiguous);
    sample.delete(&old_placed);
    sample.delete(&old_dir);

    let volume = open(sample.finish());
    assert_eq!(volume.kind(), FsKind::Exfat);
    assert_eq!(volume.label(), Some("数据盘"));
    let entries = volume.list(&volume.root()).unwrap().entries;
    assert_eq!(
        names(&entries),
        [
            "plain.txt",
            "文档",
            "gone.jpg",
            "gone chain.mp4",
            "victim.bin",
            "new.bin",
            "Old"
        ]
    );

    let entry = find(&entries, "plain.txt");
    assert_eq!((entry.state, entry.size), (State::Live, plain.len() as u64));
    assert_eq!(entry.modified.as_deref(), Some(SAMPLE_TIME));
    assert_eq!(read_all(&volume, &entry.file), plain);

    let docs = find(&entries, "文档");
    let inside = volume.list(&docs.file).unwrap().entries;
    assert_eq!(read_all(&volume, &inside[0].file), chained);

    let entry = find(&entries, "gone.jpg");
    assert_eq!((entry.deleted(), entry.state), (true, State::Intact));
    assert_eq!(read_all(&volume, &entry.file), gone);

    let entry = find(&entries, "gone chain.mp4");
    assert_eq!((entry.deleted(), entry.state), (true, State::Intact));
    assert!(!entry.file.clusters().unwrap().contiguous);
    assert_eq!(read_all(&volume, &entry.file), gone_chain);

    assert_eq!(find(&entries, "victim.bin").state, State::Overwritten);

    let old = find(&entries, "Old");
    assert!(old.deleted() && old.is_dir());
    let inside = volume.list(&old.file).unwrap().entries;
    assert_eq!(names(&inside), ["old.txt"]);
    assert!(inside[0].deleted());
    assert_eq!(read_all(&volume, &inside[0].file), old_file);
}

#[test]
fn file_systems_are_detected_inside_partitions() {
    let fat = FatImage::new(FatType::Fat12).finish();
    let exfat = ExfatImage::new().finish();
    let fat_start = 2048u32;
    let exfat_start = fat_start + (fat.len() / 512) as u32;
    let total = exfat_start as usize * 512 + exfat.len() + 512;
    let mut image = part_samples::mbr_disk(
        total,
        &[
            MbrEntry::new(0x01, fat_start, (fat.len() / 512) as u32),
            MbrEntry::new(0x07, exfat_start, (exfat.len() / 512) as u32),
        ],
    );
    let fat_at = fat_start as usize * 512;
    image[fat_at..fat_at + fat.len()].copy_from_slice(&fat);
    let exfat_at = exfat_start as usize * 512;
    image[exfat_at..exfat_at + exfat.len()].copy_from_slice(&exfat);
    let disk: Arc<MemoryDisk> = Arc::new(MemoryDisk::new(image, 512));

    let table = yisi_part::read_table(disk.as_ref()).unwrap();
    let kinds: Vec<FsKind> = table
        .partitions
        .iter()
        .map(|partition| detect(disk.as_ref(), partition.offset, partition.length).unwrap())
        .collect();
    assert_eq!(kinds, [FsKind::Fat12, FsKind::Exfat]);
    let second = &table.partitions[1];
    let volume = Volume::open(disk.clone(), second.offset, second.length).unwrap();
    assert!(volume.list(&volume.root()).unwrap().entries.is_empty());

    // 只有 NTFS 标识、参数不自洽的引导扇区不算 NTFS
    let mut ntfs = vec![0u8; 4096];
    ntfs[3..11].copy_from_slice(b"NTFS    ");
    assert_eq!(
        detect(&MemoryDisk::new(ntfs.clone(), 512), 0, 4096).unwrap(),
        FsKind::Unknown
    );
    assert!(matches!(
        Volume::open(Arc::new(MemoryDisk::new(ntfs, 512)), 0, 4096),
        Err(crate::FsError::Unsupported(FsKind::Unknown))
    ));
    let ntfs = NtfsImage::new().finish();
    let length = ntfs.len() as u64;
    assert_eq!(
        detect(&MemoryDisk::new(ntfs, 512), 0, length).unwrap(),
        FsKind::Ntfs
    );
    assert_eq!(
        detect(&MemoryDisk::new(vec![0; 4096], 512), 0, 4096).unwrap(),
        FsKind::Unknown
    );
}

/// 递归列出并读出全部文件，限制深度与总数，防止测试本身陷进构造出来的环里。
fn walk(volume: &Volume, dir: &FileRef, depth: usize, budget: &mut usize) {
    if depth == 0 {
        let _ = volume.scan(&mut |_, _| true);
    }
    let Ok(listing) = volume.list(dir) else {
        return;
    };
    for entry in listing.entries {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        if entry.is_dir() {
            if depth < 4 {
                walk(volume, &entry.file, depth + 1, budget);
            }
        } else if let Ok(mut reader) = volume.open_file(&entry.file) {
            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            while total < 1 << 20 {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => total += read as u64,
                }
            }
        }
    }
}

#[test]
fn mutated_volumes_never_panic() {
    let mut fat = FatImage::new(FatType::Fat12);
    let root = fat.root();
    let dir = fat.mkdir(root, "dir").as_dir();
    fat.add_file(dir, "inside.bin", &pattern(1, 3000), Layout::Fragmented);
    let gone = fat.add_file(root, "gone.bin", &pattern(2, 2000), Layout::Contiguous);
    fat.delete(&gone);
    let fat = fat.finish();

    let mut exfat = ExfatImage::new();
    let root = exfat.root();
    let dir = exfat.mkdir(root, "dir").as_dir();
    exfat.add_file(dir, "inside.bin", &pattern(1, 9000), Layout::Fragmented);
    let gone = exfat.add_file(root, "gone.bin", &pattern(2, 5000), Layout::Contiguous);
    exfat.delete(&gone);
    let exfat = exfat.finish();

    let mut ntfs = NtfsImage::new();
    let dir = ntfs.mkdir(NTFS_ROOT, "dir");
    for index in 0..40 {
        ntfs.add_file(dir, &format!("file {index}.txt"), &pattern(index, 100));
    }
    ntfs.add_split(dir, "split.bin", &pattern(1, 20_000));
    ntfs.add_sparse(
        NTFS_ROOT,
        "sparse.bin",
        &pattern(2, 4096),
        2,
        &pattern(3, 10),
    );
    let gone = ntfs.add_split(NTFS_ROOT, "gone.bin", &pattern(4, 9000));
    ntfs.delete(gone);
    let old = ntfs.mkdir(NTFS_ROOT, "old");
    let inner = ntfs.add_file(old, "inner.txt", &pattern(5, 3000));
    ntfs.delete(inner);
    ntfs.delete(old);
    let ntfs = ntfs.finish();

    let mut noise = Noise::new(11);
    for _ in 0..150 {
        for (base, hot) in [
            // 改坏引导扇区、FAT 与开头的目录簇
            (&fat, 0..64 * 1024),
            (&exfat, 0..64 * 1024),
            // NTFS：引导扇区、MFT（4 号簇起）、索引块与开头的数据（64 号簇起）
            (&ntfs, 0..512),
            (&ntfs, 16 * 1024..160 * 1024),
            (&ntfs, 256 * 1024..320 * 1024),
        ] {
            let mut image = base.clone();
            let hot = hot.start..hot.end.min(image.len());
            let count = 1 + noise.below(24) as usize;
            noise.mutate(&mut image[hot], count);
            let length = image.len() as u64;
            let disk: Arc<MemoryDisk> = Arc::new(MemoryDisk::new(image, 512));
            for copy in [crate::BootCopy::Primary, crate::BootCopy::Backup] {
                let _ = crate::detect_with(disk.as_ref(), 0, length, copy);
                let Ok(volume) = Volume::open_with(disk.clone(), 0, length, copy) else {
                    continue;
                };
                let mut budget = 2000;
                walk(&volume, &volume.root(), 0, &mut budget);
            }
        }
    }
}

// ───────────────────────────── NTFS ─────────────────────────────

fn scan(volume: &Volume) -> ScanSummary {
    volume.scan(&mut |_, _| true).unwrap().unwrap()
}

#[test]
fn ntfs_volume_is_browsed_and_deleted_files_are_found() {
    let mut sample = NtfsImage::new();
    let cs = sample.cluster_size();
    sample.set_label("我的 NTFS");
    let hello = pattern(1, 100);
    sample.add_file(NTFS_ROOT, "hello.txt", &hello);
    let big = pattern(2, cs * 3 + 17);
    sample.add_file(NTFS_ROOT, "big.bin", &big);
    let photos = sample.mkdir(NTFS_ROOT, "Photos");
    let photo = pattern(3, cs * 4 + 1);
    sample.add_fragmented(photos, "照片 2024.jpg", &photo);
    let many = sample.mkdir(NTFS_ROOT, "many");
    for index in 0..60 {
        sample.add_file(many, &format!("f{index:03}.bin"), &pattern(index as u8, 50));
    }
    let named = sample.add_file(NTFS_ROOT, "a long file name.txt", &pattern(4, 30));
    sample.add_dos_name(named, "ALONGF~1.TXT");
    let linked = sample.add_file(NTFS_ROOT, "link-a.txt", &pattern(5, 40));
    sample.add_link(linked, photos, "link-b.txt");
    let split = pattern(6, cs * 5 + 3);
    sample.add_split(NTFS_ROOT, "split.bin", &split);
    let before = pattern(7, cs);
    let after = pattern(8, 100);
    sample.add_sparse(NTFS_ROOT, "sparse.bin", &before, 3, &after);
    let packed = sample.add_file(NTFS_ROOT, "packed.bin", &pattern(9, cs * 2));
    sample.mark_compressed(packed);
    let secret = sample.add_file(NTFS_ROOT, "secret.bin", &pattern(10, 300));
    sample.mark_encrypted(secret);
    let system = sample.add_file(NTFS_ROOT, "system.dll", &[]);
    sample.mark_reparse(system, TAG_WOF);

    let gone = pattern(11, cs * 2 + 5);
    let placed = sample.add_file(NTFS_ROOT, "gone.jpg", &gone);
    sample.delete(placed);
    let small = pattern(12, 200);
    let placed = sample.add_file(NTFS_ROOT, "gone small.txt", &small);
    sample.delete(placed);
    let gone_frag = pattern(13, cs * 3);
    let placed = sample.add_fragmented(NTFS_ROOT, "gone frag.bin", &gone_frag);
    sample.delete(placed);
    let gone_split = pattern(14, cs * 4 + 9);
    let placed = sample.add_split(NTFS_ROOT, "gone split.bin", &gone_split);
    sample.delete(placed);
    let placed = sample.add_split(NTFS_ROOT, "gone truncated.bin", &pattern(20, cs * 3));
    sample.delete_truncating(placed);
    let victim = sample.add_file(NTFS_ROOT, "victim.bin", &pattern(15, cs * 2));
    let at = sample.first_lcn(victim);
    sample.delete(victim);
    sample.add_file_at(NTFS_ROOT, "new.bin", &pattern(16, cs), at);
    let old = sample.mkdir(NTFS_ROOT, "Old");
    let old_file = pattern(17, 700);
    let inner = sample.add_file(old, "old.txt", &old_file);
    sample.delete(inner);
    sample.delete(old);
    // 父目录的记录被重用：里面删除的文件找不到原目录
    let temp = sample.mkdir(NTFS_ROOT, "Temp");
    let lost = pattern(18, 64);
    let placed = sample.add_file(temp, "lost.txt", &lost);
    sample.delete(placed);
    sample.delete(temp);
    sample.reuse(temp, NTFS_ROOT, "reused.txt", &pattern(19, 10));

    let volume = open(sample.finish());
    assert_eq!(volume.kind(), FsKind::Ntfs);
    assert_eq!(volume.label(), Some("我的 NTFS"));
    assert!(volume.times_utc());
    assert!(volume.needs_scan());

    // 扫描之前：只有现存的项，长度取自目录索引
    let entries = volume.list(&volume.root()).unwrap().entries;
    assert!(
        entries.iter().all(|entry| !entry.deleted()),
        "{:?}",
        names(&entries)
    );
    assert!(!names(&entries).contains(&"$Orphans"));
    let entry = find(&entries, "hello.txt");
    assert_eq!((entry.size, entry.state), (100, State::Live));
    assert_eq!(entry.modified.as_deref(), Some(SAMPLE_TIME));
    assert_eq!(entry.position, None);
    assert_eq!(read_all(&volume, &entry.file), hello);

    let summary = scan(&volume);
    assert!(!volume.needs_scan());
    assert_eq!(summary.orphans, 1);
    assert!(summary.deleted >= 7, "{summary:?}");
    // 再扫一次直接给出上次的结果
    assert_eq!(scan(&volume), summary);

    let entries = volume.list(&volume.root()).unwrap().entries;
    let entry = find(&entries, "big.bin");
    assert_eq!(entry.size, big.len() as u64);
    assert!(entry.position.is_some());
    assert_eq!(read_all(&volume, &entry.file), big);
    let entry = find(&entries, "a long file name.txt");
    assert_eq!(entry.short_name.as_deref(), Some("ALONGF~1.TXT"));
    assert!(!names(&entries).contains(&"ALONGF~1.TXT"));
    assert_eq!(read_all(&volume, &find(&entries, "split.bin").file), split);
    let mut sparse = before.clone();
    sparse.extend(vec![0u8; cs * 3]);
    sparse.extend(&after);
    assert_eq!(
        read_all(&volume, &find(&entries, "sparse.bin").file),
        sparse
    );

    for (name, reason) in [
        ("packed.bin", Unsupported::Compressed),
        ("secret.bin", Unsupported::Encrypted),
        ("system.dll", Unsupported::SystemCompressed),
    ] {
        let entry = find(&entries, name);
        assert_eq!(entry.unsupported, Some(reason), "{name}");
        assert!(matches!(
            volume.open_file(&entry.file),
            Err(FsError::Unreadable(found)) if found == reason
        ));
    }

    let photos = find(&entries, "Photos");
    let inside = volume.list(&photos.file).unwrap().entries;
    assert_eq!(names(&inside).len(), 2);
    assert_eq!(
        read_all(&volume, &find(&inside, "照片 2024.jpg").file),
        photo
    );
    assert_eq!(
        find(&inside, "link-b.txt").file,
        find(&entries, "link-a.txt").file
    );

    // 项多的目录用 INDEX_ALLOCATION；位图里没标在用的索引块里的旧项不列出
    let many = volume.list(&find(&entries, "many").file).unwrap();
    assert_eq!(many.entries.len(), 60);
    assert!(!names(&many.entries).contains(&"stale.txt"));

    for (name, content) in [
        ("gone.jpg", &gone),
        ("gone small.txt", &small),
        ("gone frag.bin", &gone_frag),
        ("gone split.bin", &gone_split),
    ] {
        let entry = find(&entries, name);
        assert_eq!(
            (entry.deleted(), entry.state),
            (true, State::Intact),
            "{name}"
        );
        assert_eq!(&read_all(&volume, &entry.file), content, "{name}");
        // 定位信息编成字符串再解回来，照样能读
        let token = FileRef::from_token(&entry.file.to_token()).unwrap();
        assert_eq!(&read_all(&volume, &token), content, "{name}");
    }
    assert_eq!(find(&entries, "victim.bin").state, State::Overwritten);

    let old = find(&entries, "Old");
    assert!(old.deleted() && old.is_dir());
    let inside = volume.list(&old.file).unwrap().entries;
    assert_eq!(names(&inside), ["old.txt"]);
    assert_eq!(read_all(&volume, &inside[0].file), old_file);

    // 删除时被截成 0 字节：位置已不在 MFT 里，不能报「可能完整」，也不能读出一个空文件
    let truncated = find(&entries, "gone truncated.bin");
    assert_eq!(truncated.state, State::Unknown);
    assert!(volume.open_file(&truncated.file).is_err());

    let orphans = find(&entries, "$Orphans");
    assert!(orphans.file.is_orphans() && !orphans.deleted());
    let inside = volume.list(&orphans.file).unwrap().entries;
    assert_eq!(names(&inside), ["lost.txt"]);
    assert_eq!(read_all(&volume, &inside[0].file), lost);
    // 被重用的记录现在是别的文件：拿着旧的（已删除目录的）定位信息去读，要被拒绝
    let reused = find(&entries, "reused.txt");
    let stale = FileRef {
        deleted: true,
        ..reused.file
    };
    assert!(volume.open_file(&stale).is_err());
}

#[test]
fn ntfs_mft_mirror_is_used_when_the_first_record_is_unreadable() {
    let mut sample = NtfsImage::new();
    sample.add_file(NTFS_ROOT, "a.txt", &pattern(1, 10));
    let mut image = sample.finish();
    // $MFT 在 4 号簇：把 0 号记录写坏
    let mft = 4 * 4096;
    image[mft..mft + 1024].fill(0xAB);
    let volume = open(image);
    let entries = volume.list(&volume.root()).unwrap().entries;
    assert_eq!(names(&entries), ["a.txt"]);
}

#[test]
fn ntfs_unreadable_records_keep_their_index_entries_after_a_scan() {
    let mut sample = NtfsImage::new();
    let torn = sample.add_file(NTFS_ROOT, "torn.txt", &pattern(1, 10));
    let gone = sample.add_file(NTFS_ROOT, "gone.txt", &pattern(2, 10));
    sample.delete(gone);
    let mut image = sample.finish();
    // 记录写到一半：第一个扇区末尾与更新序列号对不上，整条记录不用
    let at = 4 * 4096 + torn as usize * 1024 + 510;
    image[at] ^= 0xFF;
    let volume = open(image);
    scan(&volume);
    let entries = volume.list(&volume.root()).unwrap().entries;
    let entry = find(&entries, "torn.txt");
    assert_eq!((entry.state, entry.size), (State::Live, 10));
    assert!(volume.open_file(&entry.file).is_err());
    assert!(find(&entries, "gone.txt").deleted());
}

#[test]
fn ntfs_scan_can_be_cancelled_and_restarted() {
    let mut sample = NtfsImage::new();
    let placed = sample.add_file(NTFS_ROOT, "gone.txt", &pattern(1, 10));
    sample.delete(placed);
    let volume = open(sample.finish());
    let mut calls = 0;
    let cancelled = volume
        .scan(&mut |_, _| {
            calls += 1;
            false
        })
        .unwrap();
    assert_eq!((cancelled, calls), (None, 1));
    assert!(volume.needs_scan());
    let mut last = (0, 0);
    let summary = volume
        .scan(&mut |done, total| {
            last = (done, total);
            true
        })
        .unwrap()
        .unwrap();
    assert_eq!(last.0, last.1);
    assert_eq!(summary.deleted, 1);
}

// ───────────────────────────── 备份引导扇区 ─────────────────────────────

/// 主引导扇区清零后按备份打开：类型按备份识别，目录与文件内容和用主引导扇区时一样。
fn check_backup_open(image: Vec<u8>, kind: FsKind, name: &str, data: &[u8]) {
    let length = image.len() as u64;
    let intact = open(image.clone());
    let mut broken = image;
    broken[..512].fill(0);
    let disk: Arc<MemoryDisk> = Arc::new(MemoryDisk::new(broken, 512));
    assert_eq!(detect(disk.as_ref(), 0, length).unwrap(), FsKind::Unknown);
    assert!(Volume::open(disk.clone(), 0, length).is_err());
    assert_eq!(
        crate::detect_with(disk.as_ref(), 0, length, crate::BootCopy::Backup).unwrap(),
        kind
    );
    let volume = Volume::open_with(disk, 0, length, crate::BootCopy::Backup).unwrap();
    assert_eq!(volume.kind(), kind);
    assert_eq!(volume.label(), intact.label());
    let entries = volume.list(&volume.root()).unwrap().entries;
    assert_eq!(read_all(&volume, &find(&entries, name).file), data);
}

#[test]
fn volumes_open_from_the_backup_boot_sector() {
    let mut fat = FatImage::new(FatType::Fat32);
    let data = pattern(7, fat.cluster_size() * 3 + 5);
    let root = fat.root();
    fat.add_file(root, "backup.bin", &data, Layout::Fragmented);
    check_backup_open(fat.finish(), FsKind::Fat32, "backup.bin", &data);

    let mut exfat = ExfatImage::new();
    let data = pattern(8, exfat.cluster_size() * 2 + 9);
    let root = exfat.root();
    exfat.add_file(root, "backup.bin", &data, Layout::Contiguous);
    check_backup_open(exfat.finish(), FsKind::Exfat, "backup.bin", &data);

    let mut ntfs = NtfsImage::new();
    let data = pattern(9, 20_000);
    ntfs.add_file(NTFS_ROOT, "backup.bin", &data);
    check_backup_open(ntfs.finish(), FsKind::Ntfs, "backup.bin", &data);
}

#[test]
fn backup_boot_sector_is_not_invented() {
    // FAT12/16 没有备份引导扇区
    let fat = FatImage::new(FatType::Fat16).finish();
    let length = fat.len() as u64;
    let disk: Arc<MemoryDisk> = Arc::new(MemoryDisk::new(fat, 512));
    assert_eq!(
        crate::detect_with(disk.as_ref(), 0, length, crate::BootCopy::Backup).unwrap(),
        FsKind::Unknown
    );
    assert!(matches!(
        Volume::open_with(disk, 0, length, crate::BootCopy::Backup),
        Err(FsError::Invalid(_))
    ));
    // 备份扇区记的扇区大小与它所在的位置不符时不用
    let mut ntfs = NtfsImage::new().finish();
    let last = ntfs.len() - 512;
    ntfs[last + 0x0B..last + 0x0D].copy_from_slice(&4096u16.to_le_bytes());
    let length = ntfs.len() as u64;
    let disk: Arc<MemoryDisk> = Arc::new(MemoryDisk::new(ntfs, 512));
    assert_eq!(
        crate::detect_with(disk.as_ref(), 0, length, crate::BootCopy::Backup).unwrap(),
        FsKind::Unknown
    );
}

#[test]
fn backup_candidates_follow_the_primary_hints() {
    use crate::backup::{BackupFamily, backup_candidates};
    let ntfs = NtfsImage::new().finish();
    let length = ntfs.len() as u64;
    let candidates = backup_candidates(&ntfs[..512], 512, length);
    assert_eq!(candidates[0].family, BackupFamily::Ntfs);
    assert_eq!(candidates[0].offset, length - 512);
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.offset + 512 <= length)
    );
    let fat = FatImage::new(FatType::Fat32).finish();
    let candidates = backup_candidates(&fat[..512], 4096, fat.len() as u64);
    assert_eq!(
        (
            candidates[0].family,
            candidates[0].offset,
            candidates[0].sector_size
        ),
        (BackupFamily::Fat32, 6 * 512, 512)
    );
    // 没有线索时三种都试，设备的扇区大小在前
    let candidates = backup_candidates(&[0u8; 512], 4096, 64 << 20);
    assert_eq!(candidates[0].sector_size, 4096);
    for family in [BackupFamily::Fat32, BackupFamily::Exfat, BackupFamily::Ntfs] {
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.family == family)
        );
    }
}
