//! 检查的单元测试：用 `yisi-fs` 的样本生成器造镜像，再按需改坏。
#![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;

use yisi_fs::samples::{ExfatImage, FatImage, FatType, Layout, MemoryDisk, NtfsImage, pattern};
use yisi_io::{BlockDevice, IoError};

use super::*;

/// 不暂停、不取消，记下最后一次进度。
#[derive(Default)]
struct Quiet {
    stages: Cell<u32>,
}

impl Control for Quiet {
    fn checkpoint(&self) -> bool {
        true
    }

    fn progress(&self, _stage: Stage, _done: u64, _total: u64) {
        self.stages.set(self.stages.get() + 1);
    }
}

fn run(image: &[u8]) -> Report {
    let disk = MemoryDisk::new(image.to_vec(), 512);
    check(&disk, 0, image.len() as u64, &Quiet::default()).unwrap()
}

fn kinds(report: &Report) -> Vec<IssueKind> {
    report.issues.iter().map(|issue| issue.kind).collect()
}

fn find(report: &Report, kind: IssueKind) -> &Issue {
    report
        .issues
        .iter()
        .find(|issue| issue.kind == kind)
        .unwrap_or_else(|| panic!("没有 {kind:?}：{:#?}", report.issues))
}

// ───────────────────────────── FAT ─────────────────────────────

/// 一个有子目录、长文件名、碎片文件、空文件、已删除文件的 FAT 样本。
struct FatSample {
    image: FatImage,
    a: yisi_fs::samples::Placed,
    b: yisi_fs::samples::Placed,
}

fn fat_sample(kind: FatType) -> FatSample {
    let mut image = FatImage::new(kind);
    image.set_label(b"CHECKED    ");
    let root = image.root();
    let docs = image.mkdir(root, "Documents").as_dir();
    let nested = image.mkdir(docs, "a much longer folder name").as_dir();
    let size = image.cluster_size() * 3 + 100;
    let a = image.add_file(
        docs,
        "report final.docx",
        &pattern(1, size),
        Layout::Fragmented,
    );
    let b = image.add_file(nested, "photo.jpg", &pattern(2, size), Layout::Contiguous);
    image.add_file(root, "empty.txt", b"", Layout::Contiguous);
    image.add_short_file(root, "README.TXT", b"hello");
    let gone = image.add_file(root, "deleted.bin", &pattern(3, 4000), Layout::Contiguous);
    image.delete(&gone);
    FatSample { image, a, b }
}

/// 引导扇区里 FAT 的位置：保留扇区数、每份 FAT 的扇区数、份数。
fn fat_geometry(image: &[u8]) -> (usize, usize, usize) {
    let reserved = u16::from_le_bytes([image[14], image[15]]) as usize;
    let fats = image[16] as usize;
    let fat16 = u16::from_le_bytes([image[22], image[23]]) as usize;
    let sectors = if fat16 != 0 {
        fat16
    } else {
        u32::from_le_bytes(image[36..40].try_into().unwrap()) as usize
    };
    (reserved, sectors, fats)
}

/// 改 FAT 表项；`copies` 为 `None` 时各份都改。
fn set_fat(image: &mut [u8], kind: FatType, cluster: u32, value: u32, copies: Option<usize>) {
    let (reserved, sectors, fats) = fat_geometry(image);
    let range = copies.map_or(0..fats, |copy| copy..copy + 1);
    for copy in range {
        let table = (reserved + copy * sectors) * 512;
        let cluster = cluster as usize;
        match kind {
            FatType::Fat12 => {
                let at = table + cluster + cluster / 2;
                let mut pair = u16::from_le_bytes([image[at], image[at + 1]]);
                let value = (value & 0x0FFF) as u16;
                pair = if cluster % 2 == 1 {
                    (pair & 0x000F) | (value << 4)
                } else {
                    (pair & 0xF000) | value
                };
                image[at..at + 2].copy_from_slice(&pair.to_le_bytes());
            }
            FatType::Fat16 => {
                let at = table + cluster * 2;
                image[at..at + 2].copy_from_slice(&(value as u16).to_le_bytes());
            }
            FatType::Fat32 => {
                let at = table + cluster * 4;
                image[at..at + 4].copy_from_slice(&value.to_le_bytes());
            }
        }
    }
}

fn end_marker(kind: FatType) -> u32 {
    match kind {
        FatType::Fat12 => 0xFFF,
        FatType::Fat16 => 0xFFFF,
        FatType::Fat32 => 0x0FFF_FFFF,
    }
}

#[test]
fn clean_fat_images_have_no_issues() {
    for kind in [FatType::Fat12, FatType::Fat16, FatType::Fat32] {
        let sample = fat_sample(kind);
        let report = run(&sample.image.finish());
        assert!(report.issues.is_empty(), "{kind:?}：{:#?}", report.issues);
        assert_eq!(report.totals, Totals::default());
        assert_eq!(report.files, 4, "{kind:?}");
        assert_eq!(report.directories, 2, "{kind:?}");
        assert!(report.used_clusters > 0);
        for item in [
            CheckItem::Boot,
            CheckItem::FatCopies,
            CheckItem::Chains,
            CheckItem::Entries,
            CheckItem::CrossLinks,
            CheckItem::LostClusters,
        ] {
            assert!(report.checked.contains(&item), "{kind:?} 没查 {item:?}");
        }
        assert!(report.skipped.is_empty(), "{kind:?}：{:?}", report.skipped);
    }
    assert_eq!(
        run(&fat_sample(FatType::Fat16).image.finish()).fs,
        FsType::Fat16
    );
}

#[test]
fn fat_loop_is_reported_once_without_length_noise() {
    for kind in [FatType::Fat12, FatType::Fat16, FatType::Fat32] {
        let sample = fat_sample(kind);
        let clusters = sample.a.clusters.clone();
        let mut image = sample.image.finish();
        // 最后一个簇指回第二个簇：成环
        set_fat(
            &mut image,
            kind,
            *clusters.last().unwrap(),
            clusters[1],
            None,
        );
        let report = run(&image);
        assert_eq!(kinds(&report), vec![IssueKind::ChainLoop], "{kind:?}");
        let issue = find(&report, IssueKind::ChainLoop);
        assert_eq!(issue.path.as_deref(), Some("/Documents/report final.docx"));
        assert_eq!(issue.cluster, Some(u64::from(clusters[1])));
        assert_eq!(issue.severity, Severity::DataLoss);
    }
}

#[test]
fn cross_link_and_lost_clusters_are_reported() {
    let kind = FatType::Fat16;
    let mut sample = fat_sample(kind);
    // photo.jpg 的目录项改成从 report final.docx 的首簇开始：两个文件共用簇，photo.jpg 原来的簇没人用了
    let first = sample.a.first_cluster();
    sample
        .image
        .patch_short_entry(&sample.b, 26, &(first as u16).to_le_bytes());
    let photo = sample.b.clusters.clone();
    let report = run(&sample.image.finish());
    let cross = find(&report, IssueKind::CrossLink);
    let paths = [cross.path.as_deref(), cross.other_path.as_deref()];
    assert!(
        paths.contains(&Some("/Documents/report final.docx")),
        "{cross:?}"
    );
    assert!(
        paths.contains(&Some("/Documents/a much longer folder name/photo.jpg")),
        "{cross:?}"
    );
    assert_eq!(cross.cluster, Some(u64::from(first)));
    let lost = find(&report, IssueKind::LostClusters);
    assert_eq!(lost.cluster, Some(u64::from(photo[0])));
    assert_eq!(lost.count, Some(photo.len() as u64));
}

#[test]
fn allocated_but_unreferenced_clusters_are_lost() {
    let kind = FatType::Fat32;
    let sample = fat_sample(kind);
    let mut image = sample.image.finish();
    // 远处两个相邻的空闲簇被标成一条链
    set_fat(&mut image, kind, 5000, 5001, None);
    set_fat(&mut image, kind, 5001, end_marker(kind), None);
    let report = run(&image);
    assert_eq!(kinds(&report), vec![IssueKind::LostClusters]);
    let lost = find(&report, IssueKind::LostClusters);
    assert_eq!((lost.cluster, lost.count), (Some(5000), Some(2)));
}

#[test]
fn many_lost_runs_are_summarised() {
    let kind = FatType::Fat16;
    let mut image = fat_sample(kind).image.finish();
    for index in 0..150u32 {
        set_fat(&mut image, kind, 10_000 + index * 2, end_marker(kind), None);
    }
    let report = run(&image);
    let listed = report
        .issues
        .iter()
        .filter(|issue| issue.kind == IssueKind::LostClusters)
        .count();
    assert_eq!(listed, 100);
    let more = find(&report, IssueKind::LostClustersMore);
    assert_eq!((more.count, more.actual), (Some(50), Some(50)));
}

#[test]
fn broken_chains_and_wrong_sizes_are_reported() {
    let kind = FatType::Fat16;
    // 链在中途指向空闲簇：之后的簇成了丢失簇
    let sample = fat_sample(kind);
    let clusters = sample.b.clusters.clone();
    let mut image = sample.image.finish();
    set_fat(&mut image, kind, clusters[1], 0, None);
    let report = run(&image);
    let free = find(&report, IssueKind::ChainFree);
    assert_eq!(free.cluster, Some(u64::from(clusters[1])));
    assert_eq!(find(&report, IssueKind::LostClusters).count, Some(2));

    // 指向簇区之外
    let sample = fat_sample(kind);
    let clusters = sample.a.clusters.clone();
    let mut image = sample.image.finish();
    set_fat(&mut image, kind, clusters[0], 0xFFF0, None);
    let report = run(&image);
    let out = find(&report, IssueKind::ChainOutOfRange);
    assert_eq!(
        (out.cluster, out.actual),
        (Some(u64::from(clusters[0])), Some(0xFFF0))
    );

    // 目录项里的大小比链长：链太短；比链短：链太长
    let mut sample = fat_sample(kind);
    let cluster_size = sample.image.cluster_size() as u32;
    sample
        .image
        .patch_short_entry(&sample.b, 28, &(cluster_size * 6).to_le_bytes());
    sample
        .image
        .patch_short_entry(&sample.a, 28, &10u32.to_le_bytes());
    let report = run(&sample.image.finish());
    let short = find(&report, IssueKind::ChainTooShort);
    assert_eq!((short.expected, short.actual), (Some(6), Some(4)));
    let long = find(&report, IssueKind::ChainTooLong);
    assert_eq!((long.expected, long.actual), (Some(1), Some(4)));
    assert_eq!(long.severity, Severity::Structure);
}

#[test]
fn fat_copies_boot_and_flags_are_checked() {
    let kind = FatType::Fat16;
    let mut image = fat_sample(kind).image.finish();
    // 只改第二份 FAT
    set_fat(&mut image, kind, 6000, end_marker(kind), Some(1));
    // 第 1 项：未正常卸下（第 15 位为 0）
    set_fat(&mut image, kind, 1, 0x7FFF, None);
    let report = run(&image);
    let differ = find(&report, IssueKind::FatCopiesDiffer);
    assert_eq!((differ.cluster, differ.count), (Some(6000), Some(1)));
    find(&report, IssueKind::VolumeDirty);
    assert!(!kinds(&report).contains(&IssueKind::LostClusters));

    // 引导扇区的参数不对：检查到此为止
    let mut image = fat_sample(kind).image.finish();
    image[13] = 3;
    let report = run(&image);
    assert_eq!(report.fs, FsType::Unknown);
    find(&report, IssueKind::UnknownFilesystem);
}

#[test]
fn long_name_checksum_and_dot_entries_are_checked() {
    let kind = FatType::Fat32;
    let mut sample = fat_sample(kind);
    // 改短文件名：长文件名的校验和就对不上了
    sample.image.patch_short_entry(&sample.a, 1, b"X");
    let report = run(&sample.image.finish());
    let lfn = find(&report, IssueKind::LfnChecksum);
    assert!(
        lfn.path.as_deref().unwrap().starts_with("/Documents/R"),
        "{lfn:?}"
    );

    // 子目录的 `.` 指向别处
    let mut image = FatImage::new(kind);
    let root = image.root();
    let folder = image.mkdir(root, "folder");
    let first = folder.first_cluster();
    let mut bytes = image.finish();
    let heap = {
        let (reserved, sectors, fats) = fat_geometry(&bytes);
        (reserved + fats * sectors) * 512
    };
    let at = heap + (first as usize - 2) * 512;
    bytes[at + 26] = bytes[at + 26].wrapping_add(1);
    let report = run(&bytes);
    let dots = find(&report, IssueKind::DotEntries);
    assert_eq!(dots.path.as_deref(), Some("/folder"));
}

// ───────────────────────────── exFAT ─────────────────────────────

struct ExfatSample {
    image: Vec<u8>,
    fragmented: yisi_fs::samples::Placed,
    contiguous: yisi_fs::samples::Placed,
}

fn exfat_sample() -> ExfatSample {
    let mut image = ExfatImage::new();
    image.set_label("CHECKED");
    let root = image.root();
    let photos = image.mkdir(root, "Photos").as_dir();
    let size = image.cluster_size() * 3 + 10;
    let fragmented = image.add_file(photos, "海边.jpg", &pattern(1, size), Layout::Fragmented);
    let contiguous = image.add_file(root, "notes.txt", &pattern(2, size), Layout::Contiguous);
    image.add_file(
        root,
        "a name that needs more than one name entry.txt",
        b"x",
        Layout::Contiguous,
    );
    image.add_file(root, "empty", b"", Layout::Contiguous);
    let gone = image.add_file(root, "gone.bin", &pattern(3, 9000), Layout::Fragmented);
    image.delete(&gone);
    ExfatSample {
        image: image.finish(),
        fragmented,
        contiguous,
    }
}

fn exfat_heap(image: &[u8]) -> usize {
    u32::from_le_bytes(image[88..92].try_into().unwrap()) as usize * 512
}

/// 分配位图在 2 号簇。
fn set_bitmap(image: &mut [u8], cluster: u32, allocated: bool) {
    let at = exfat_heap(image) + (cluster as usize - 2) / 8;
    let bit = 1u8 << ((cluster - 2) % 8);
    if allocated {
        image[at] |= bit;
    } else {
        image[at] &= !bit;
    }
}

/// 改完主引导区后重算它的校验和。
fn reseal_boot(image: &mut [u8]) {
    let mut checksum = 0u32;
    for (index, &byte) in image[..11 * 512].iter().enumerate() {
        if matches!(index, 106 | 107 | 112) {
            continue;
        }
        checksum = checksum.rotate_right(1).wrapping_add(u32::from(byte));
    }
    for word in image[11 * 512..12 * 512].as_chunks_mut::<4>().0 {
        word.copy_from_slice(&checksum.to_le_bytes());
    }
}

#[test]
fn clean_exfat_image_has_no_issues() {
    let report = run(&exfat_sample().image);
    assert_eq!(report.fs, FsType::Exfat);
    assert!(report.issues.is_empty(), "{:#?}", report.issues);
    assert_eq!((report.files, report.directories), (4, 1));
    for item in [
        CheckItem::Boot,
        CheckItem::BackupBoot,
        CheckItem::BootChecksum,
        CheckItem::Chains,
        CheckItem::Entries,
        CheckItem::Bitmap,
        CheckItem::Upcase,
        CheckItem::CrossLinks,
        CheckItem::LostClusters,
    ] {
        assert!(report.checked.contains(&item), "没查 {item:?}");
    }
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
}

#[test]
fn exfat_boot_checksum_is_verified() {
    let mut image = exfat_sample().image;
    // 卷序列号改了，校验和没跟着改
    image[100] ^= 0xFF;
    let report = run(&image);
    let checksum = find(&report, IssueKind::BootChecksum);
    assert_eq!(checksum.area, Some(Area::Boot));
    // 备份引导区还是旧的：参数也不一致
    find(&report, IssueKind::BackupBootMismatch);

    // 脏标记不计入校验和
    let mut image = exfat_sample().image;
    image[106] |= 0x02;
    assert_eq!(kinds(&run(&image)), vec![IssueKind::VolumeDirty]);
}

#[test]
fn volumes_larger_than_the_partition_are_reported() {
    // exFAT：卷长度写成两倍，重算校验和；备份引导区还是旧的
    let mut image = exfat_sample().image;
    let length = u64::from_le_bytes(image[72..80].try_into().unwrap());
    image[72..80].copy_from_slice(&(length * 2).to_le_bytes());
    reseal_boot(&mut image);
    let report = run(&image);
    let truncated = find(&report, IssueKind::VolumeTruncated);
    assert_eq!(truncated.expected, Some(length * 2 * 512));
    assert_eq!(truncated.actual, Some(image.len() as u64));
    find(&report, IssueKind::BackupBootMismatch);
    assert!(!kinds(&report).contains(&IssueKind::BootChecksum));

    // FAT：分区比卷短一半，后半的簇读不到
    let image = fat_sample(FatType::Fat16).image.finish();
    let disk = MemoryDisk::new(image.clone(), 512);
    let half = image.len() as u64 / 2;
    let report = check(&disk, 0, half, &Quiet::default()).unwrap();
    let truncated = find(&report, IssueKind::VolumeTruncated);
    assert_eq!(truncated.actual, Some(half));
}

#[test]
fn exfat_falls_back_to_the_backup_boot_region() {
    let mut image = exfat_sample().image;
    image[..512].fill(0);
    let report = run(&image);
    assert_eq!(report.fs, FsType::Exfat);
    assert_eq!(kinds(&report), vec![IssueKind::BootUnrecognized]);
    assert_eq!(report.files, 4);
}

#[test]
fn exfat_entry_set_checksum_is_verified() {
    let sample = exfat_sample();
    let mut image = sample.image;
    // 根目录在 4 号簇：找到 notes.txt 的文件名项，改掉一个字符
    let root = exfat_heap(&image) + 2 * 4096;
    let name: Vec<u8> = "notes".encode_utf16().flat_map(u16::to_le_bytes).collect();
    let at = (root..root + 4096 - name.len())
        .find(|&at| image[at..at + name.len()] == name[..])
        .unwrap();
    image[at] = b'N';
    let report = run(&image);
    let issue = find(&report, IssueKind::EntrySetChecksum);
    assert_eq!(issue.path.as_deref(), Some("/Notes.txt"));
    assert_eq!(issue.severity, Severity::Structure);
    // 校验和不对的集合照样用来认领簇，不会多出丢失簇
    assert_eq!(kinds(&report), vec![IssueKind::EntrySetChecksum]);
    assert_eq!(sample.contiguous.clusters.len(), 4);
}

#[test]
fn exfat_entry_count_and_name_length_are_verified() {
    let mut image = exfat_sample().image;
    let root = exfat_heap(&image) + 2 * 4096;
    // 找到长名字那一组：文件项的次项个数少写一项，再重算校验和
    let name: Vec<u8> = "a name".encode_utf16().flat_map(u16::to_le_bytes).collect();
    let name_at = (root..root + 4096)
        .find(|&at| image[at..at + name.len()] == name[..])
        .unwrap();
    let primary = name_at - 2 - 64;
    assert_eq!(image[primary], 0x85);
    let declared = image[primary + 1];
    image[primary + 1] = declared + 1;
    let report = run(&image);
    let count = find(&report, IssueKind::EntrySetCount);
    assert_eq!(
        (count.expected, count.actual),
        (Some(u64::from(declared) + 1), Some(u64::from(declared)))
    );

    // 文件名长度写成 3：一项文件名就够，实际有 4 项
    let mut image = exfat_sample().image;
    let stream = primary + 32;
    image[stream + 3] = 3;
    let mut checksum = 0u16;
    let entries = usize::from(image[primary + 1]) + 1;
    for (index, &byte) in image[primary..primary + entries * 32].iter().enumerate() {
        if index == 2 || index == 3 {
            continue;
        }
        checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
    }
    image[primary + 2..primary + 4].copy_from_slice(&checksum.to_le_bytes());
    let report = run(&image);
    let length = find(&report, IssueKind::EntryNameLength);
    assert_eq!((length.expected, length.actual), (Some(1), Some(4)));
    assert_eq!(length.path.as_deref(), Some("/a n"));
}

#[test]
fn exfat_bitmap_must_cover_every_used_cluster() {
    let sample = exfat_sample();
    let mut image = sample.image;
    let clusters = sample.fragmented.clusters.clone();
    set_bitmap(&mut image, clusters[1], false);
    set_bitmap(&mut image, clusters[2], false);
    // 另一个没人用的簇在位图里却是已分配
    set_bitmap(&mut image, 1500, true);
    let report = run(&image);
    let missing = find(&report, IssueKind::NotInBitmap);
    assert_eq!(missing.path.as_deref(), Some("/Photos/海边.jpg"));
    assert_eq!(
        (missing.cluster, missing.count),
        (Some(u64::from(clusters[1])), Some(2))
    );
    let lost = find(&report, IssueKind::LostClusters);
    assert_eq!((lost.cluster, lost.count), (Some(1500), Some(1)));
    assert_eq!(report.issues.len(), 2, "{:#?}", report.issues);
}

#[test]
fn exfat_fat_chain_loop_and_cross_link() {
    let sample = exfat_sample();
    let mut image = sample.image;
    let fat = u32::from_le_bytes(image[80..84].try_into().unwrap()) as usize * 512;
    let clusters = sample.fragmented.clusters.clone();
    let last = *clusters.last().unwrap() as usize;
    image[fat + last * 4..fat + last * 4 + 4].copy_from_slice(&clusters[0].to_le_bytes());
    let report = run(&image);
    assert_eq!(kinds(&report), vec![IssueKind::ChainLoop]);

    // 连续存放的 notes.txt 改成从照片的第二个簇开始：交叉链接
    let sample = exfat_sample();
    let mut image = sample.image;
    let root = exfat_heap(&image) + 2 * 4096;
    let first = sample.contiguous.first_cluster().to_le_bytes();
    let at = (root..root + 4096)
        .step_by(32)
        .find(|&at| image[at] == 0xC0 && image[at + 20..at + 24] == first)
        .unwrap();
    image[at + 20..at + 24].copy_from_slice(&sample.fragmented.clusters[1].to_le_bytes());
    let primary = at - 32;
    let entries = usize::from(image[primary + 1]) + 1;
    let mut checksum = 0u16;
    for (index, &byte) in image[primary..primary + entries * 32].iter().enumerate() {
        if index == 2 || index == 3 {
            continue;
        }
        checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
    }
    image[primary + 2..primary + 4].copy_from_slice(&checksum.to_le_bytes());
    let report = run(&image);
    let cross = find(&report, IssueKind::CrossLink);
    let mut paths = [cross.path.as_deref(), cross.other_path.as_deref()];
    paths.sort();
    assert_eq!(paths, [Some("/Photos/海边.jpg"), Some("/notes.txt")]);
    find(&report, IssueKind::LostClusters);
}

#[test]
fn exfat_upcase_and_bitmap_entries_are_required() {
    let mut image = exfat_sample().image;
    let root = exfat_heap(&image) + 2 * 4096;
    // 大写表项被标成已删除
    assert_eq!(image[root + 32], 0x82);
    image[root + 32] = 0x02;
    let report = run(&image);
    find(&report, IssueKind::UpcaseMissing);
    assert!(report.skipped.contains(&CheckItem::Upcase));
    // 大写表的 3 号簇没人引用了
    find(&report, IssueKind::LostClusters);

    let mut image = exfat_sample().image;
    // 大写表项第 4 到 7 字节是表的校验和
    image[root + 32 + 4] ^= 0x01;
    find(&run(&image), IssueKind::UpcaseChecksum);

    // 没有分配位图：位图、丢失簇、交叉链接都查不了
    let mut image = exfat_sample().image;
    assert_eq!(image[root], 0x81);
    image[root] = 0x01;
    let report = run(&image);
    find(&report, IssueKind::BitmapInvalid);
    assert!(report.skipped.contains(&CheckItem::Bitmap));
    assert!(report.skipped.contains(&CheckItem::LostClusters));
}

// ───────────────────────────── 其他 ─────────────────────────────

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

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        let end = offset + buf.len() as u64;
        if offset < self.bad.1 && self.bad.0 < end {
            return Err(IoError::Read {
                offset,
                len: buf.len(),
                source: std::io::Error::other("坏扇区"),
            });
        }
        self.inner.read_at(offset, buf)
    }
}

#[test]
fn unreadable_sectors_are_recorded_and_the_check_goes_on() {
    let kind = FatType::Fat16;
    let sample = fat_sample(kind);
    let image = sample.image.finish();
    // Documents 目录所在的簇读不出
    let (reserved, sectors, fats) = fat_geometry(&image);
    let root_sectors = 512 * 32 / 512;
    let heap = (reserved + fats * sectors + root_sectors) * 512;
    let docs_cluster = 2u64;
    let at = heap as u64 + (docs_cluster - 2) * 1024;
    let disk = FlakyDisk {
        inner: MemoryDisk::new(image.clone(), 512),
        bad: (at + 512, at + 1024),
    };
    let report = check(&disk, 0, image.len() as u64, &Quiet::default()).unwrap();
    let unreadable = find(&report, IssueKind::Unreadable);
    assert_eq!(unreadable.path.as_deref(), Some("/Documents"));
    assert_eq!(
        (unreadable.offset, unreadable.count),
        (Some(at + 512), Some(512))
    );
    assert_eq!(report.unreadable_bytes, 512);
    // 读得出的那半个簇里的子目录、文件照常检查
    assert!(report.files >= 3, "{report:#?}");
}

#[test]
fn cancel_stops_the_check() {
    struct Cancelled;
    impl Control for Cancelled {
        fn checkpoint(&self) -> bool {
            false
        }
        fn progress(&self, _: Stage, _: u64, _: u64) {}
    }
    let image = fat_sample(FatType::Fat32).image.finish();
    let disk = MemoryDisk::new(image.clone(), 512);
    assert!(check(&disk, 0, image.len() as u64, &Cancelled).is_none());
}

#[test]
fn ntfs_boot_and_mft_are_checked() {
    let mut image = NtfsImage::new();
    image.add_file(yisi_fs::samples::NTFS_ROOT, "a.txt", b"hello");
    let mut image = image.finish();
    // 样本没写备份引导扇区：补上之后应当没有问题
    let report = run(&image);
    assert_eq!(kinds(&report), vec![IssueKind::BackupBootInvalid]);
    let last = image.len() - 512;
    let boot = image[..512].to_vec();
    image[last..].copy_from_slice(&boot);
    let report = run(&image);
    assert_eq!(report.fs, FsType::Ntfs);
    assert!(report.issues.is_empty(), "{:#?}", report.issues);
    assert!(report.checked.contains(&CheckItem::MftMirror));

    // $MFT 第一条记录坏了：镜像也对不上
    let mft = u64::from_le_bytes(image[0x30..0x38].try_into().unwrap()) as usize * 4096;
    image[mft] = b'X';
    let report = run(&image);
    find(&report, IssueKind::MftInvalid);
    assert_eq!(find(&report, IssueKind::MftMirrorMismatch).count, Some(1));
}

#[test]
fn unknown_volumes_are_reported() {
    let report = run(&vec![0u8; 1 << 20]);
    assert_eq!(report.fs, FsType::Unknown);
    assert_eq!(kinds(&report), vec![IssueKind::UnknownFilesystem]);
}

#[test]
fn issues_beyond_the_limit_are_counted() {
    let kind = FatType::Fat16;
    let mut image = FatImage::new(kind);
    let root = image.root();
    let many = image.mkdir(root, "many").as_dir();
    let mut placed = Vec::new();
    for index in 0..1100 {
        placed.push(image.add_file(
            many,
            &format!("f{index}.bin"),
            &pattern(1, 10),
            Layout::Contiguous,
        ));
    }
    for file in &placed {
        image.patch_short_entry(file, 28, &5000u32.to_le_bytes());
    }
    let report = run(&image.finish());
    assert_eq!(report.issues.len(), MAX_ISSUES);
    assert_eq!(report.omitted, 100);
    assert_eq!(report.totals.data_loss, 1100);
}

#[test]
fn progress_is_reported() {
    let image = exfat_sample().image;
    let disk = MemoryDisk::new(image.clone(), 512);
    let quiet = Quiet::default();
    check(&disk, 0, image.len() as u64, &quiet).unwrap();
    assert!(quiet.stages.get() >= 4);
}
