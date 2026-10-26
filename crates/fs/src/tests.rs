//! 用按规范拼出的镜像检查目录读取、删除识别与文件读出。

use std::io::Read;
use std::sync::Arc;

use yisi_part::samples::{self as part_samples, MbrEntry};

use crate::samples::{
    ExfatImage, FatImage, FatType, Layout, MemoryDisk, Noise, SAMPLE_TIME, pattern,
};
use crate::{Entry, FileRef, FsKind, State, Volume, detect};

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
    assert!(!entry.file.contiguous);
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

    let mut ntfs = vec![0u8; 4096];
    ntfs[3..11].copy_from_slice(b"NTFS    ");
    assert_eq!(
        detect(&MemoryDisk::new(ntfs.clone(), 512), 0, 4096).unwrap(),
        FsKind::Ntfs
    );
    assert!(matches!(
        Volume::open(Arc::new(MemoryDisk::new(ntfs, 512)), 0, 4096),
        Err(crate::FsError::Unsupported(FsKind::Ntfs))
    ));
    assert_eq!(
        detect(&MemoryDisk::new(vec![0; 4096], 512), 0, 4096).unwrap(),
        FsKind::Unknown
    );
}

/// 递归列出并读出全部文件，限制深度与总数，防止测试本身陷进构造出来的环里。
fn walk(volume: &Volume, dir: &FileRef, depth: usize, budget: &mut usize) {
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

    let mut noise = Noise::new(11);
    for _ in 0..150 {
        for base in [&fat, &exfat] {
            let mut image = base.clone();
            // 改坏引导扇区、FAT 与开头的目录簇
            let hot = image.len().min(64 * 1024);
            let count = 1 + noise.below(24) as usize;
            noise.mutate(&mut image[..hot], count);
            let length = image.len() as u64;
            let Ok(volume) = Volume::open(Arc::new(MemoryDisk::new(image, 512)), 0, length) else {
                continue;
            };
            let mut budget = 2000;
            walk(&volume, &volume.root(), 0, &mut budget);
        }
    }
}
