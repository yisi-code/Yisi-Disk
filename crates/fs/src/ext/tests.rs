//! 用按规范拼出的 ext2 / ext4 镜像检查目录读取、删除项的判断、文件读出、按备份超级块打开与改坏后的表现。

use std::sync::Arc;

use crate::samples::{
    EXT_ROOT, EXT_SAMPLE_NANOS, EXT_SAMPLE_SECONDS, ExtFlavor, ExtImage, Layout, MemoryDisk, Noise,
    SAMPLE_TIME, pattern,
};
use crate::tests::{find, names, open, read_all, walk};
use crate::{
    BootCopy, FileRef, FsError, FsKind, Moment, Stamp, State, Unsupported, Volume, detect,
    detect_with, ext_copy_confirmed,
};

fn disk(image: Vec<u8>) -> (Arc<MemoryDisk>, u64) {
    let length = image.len() as u64;
    (Arc::new(MemoryDisk::new(image, 512)), length)
}

/// 1960-01-01 00:00:00 UTC：1970 年之前，秒数为负。
const BEFORE_1970: i64 = -315_619_200;

#[test]
fn ext2_volume_is_browsed_and_deleted_files_keep_their_blocks() {
    let mut sample = ExtImage::new(ExtFlavor::Ext2);
    let bs = sample.block_size();
    sample.set_label("我的 ext2");
    let hello = pattern(1, bs * 3 + 17);
    sample.add_file(EXT_ROOT, "hello world.txt", &hello, Layout::Contiguous);
    // 300 块、隔一块放一块：用到一级与二级间接块
    let big = pattern(2, bs * 300 + 5);
    sample.add_file(EXT_ROOT, "big.bin", &big, Layout::Fragmented);
    let photos = sample.mkdir(EXT_ROOT, "Photos");
    let photo = pattern(3, bs * 4 + 1);
    sample.add_file(photos, "照片 2024-03-05.jpg", &photo, Layout::Fragmented);
    let mut sparse = pattern(4, bs * 10);
    for hole in [2usize, 3, 8, 9] {
        sparse[hole * bs..(hole + 1) * bs].fill(0);
    }
    sample.add_sparse(EXT_ROOT, "sparse.bin", &sparse, &[2, 3, 8, 9]);
    sample.add_file(EXT_ROOT, ".hidden", &pattern(5, 10), Layout::Contiguous);
    let old = sample.add_file(EXT_ROOT, "1960.txt", &pattern(6, 10), Layout::Contiguous);
    sample.set_modified(old, BEFORE_1970, 0);
    sample.add_symlink(EXT_ROOT, "link", "hello world.txt");

    // 名字短的 s 先删：之后新建的文件取最小的空闲 inode，正好重用它
    let spacer = sample.add_file(EXT_ROOT, "s", &pattern(7, 100), Layout::Contiguous);
    let report = pattern(8, bs * 2 + 9);
    let report_inode =
        sample.add_file(EXT_ROOT, "deleted report.docx", &report, Layout::Contiguous);
    let victim = sample.add_file(
        EXT_ROOT,
        "victim.bin",
        &pattern(9, bs * 2),
        Layout::Contiguous,
    );
    let trash = sample.mkdir(EXT_ROOT, "Trash");
    let inner = pattern(10, bs + 5);
    let inner_inode = sample.add_file(trash, "inner file.dat", &inner, Layout::Contiguous);

    let victim_block = sample.first_block(victim);
    sample.delete(EXT_ROOT, "s", spacer);
    sample.delete(EXT_ROOT, "victim.bin", victim);
    // 名字长，放不进删掉的项留下的空隙，接在目录末尾
    let taker = sample.add_file(
        EXT_ROOT,
        "the new file that took the blocks.bin",
        &pattern(11, bs),
        Layout::At(victim_block),
    );
    assert_eq!(taker, spacer, "新文件重用了 s 的 inode");
    sample.delete(EXT_ROOT, "deleted report.docx", report_inode);
    sample.delete(trash, "inner file.dat", inner_inode);
    sample.delete(EXT_ROOT, "Trash", trash);

    let volume = open(sample.finish());
    assert_eq!(volume.kind(), FsKind::Ext);
    assert_eq!(volume.label(), Some("我的 ext2"));
    assert_eq!(volume.cluster_size(), 1024);
    assert!(volume.times_utc() && !volume.needs_scan());

    let listing = volume.list(&volume.root()).unwrap();
    assert!(!listing.truncated && !listing.overwritten);
    let entries = &listing.entries;
    assert!(find(entries, "lost+found").is_dir());

    let entry = find(entries, "hello world.txt");
    assert_eq!(
        (entry.state, entry.size, entry.is_dir()),
        (State::Live, hello.len() as u64, false)
    );
    assert_eq!(entry.modified.as_deref(), Some(SAMPLE_TIME));
    // 128 字节的 inode 没有创建时间
    assert_eq!(entry.created, None);
    assert!(entry.position.is_some());
    assert_eq!(read_all(&volume, &entry.file), hello);
    assert_eq!(read_all(&volume, &find(entries, "big.bin").file), big);
    assert_eq!(read_all(&volume, &find(entries, "sparse.bin").file), sparse);

    let photos = find(entries, "Photos");
    assert!(photos.is_dir() && photos.state == State::Live && photos.size == 0);
    let inside = volume.list(&photos.file).unwrap().entries;
    assert_eq!(names(&inside), ["照片 2024-03-05.jpg"]);
    assert_eq!(read_all(&volume, &inside[0].file), photo);

    // Linux 的惯例：以 . 开头的名字算隐藏
    assert!(find(entries, ".hidden").attributes.hidden);
    assert!(!find(entries, "big.bin").attributes.hidden);
    let old = find(entries, "1960.txt");
    assert_eq!(old.modified.as_deref(), Some("1960-01-01 00:00:00"));
    assert_eq!(
        old.file.modified.and_then(|stamp| stamp.moment()),
        Some(Moment::Utc {
            seconds: BEFORE_1970,
            nanos: 0
        })
    );
    // 符号链接的内容是它指向的路径，不当文件读出
    let link = find(entries, "link");
    assert_eq!(link.unsupported, Some(Unsupported::Reparse));
    assert!(matches!(
        volume.open_file(&link.file),
        Err(FsError::Unreadable(Unsupported::Reparse))
    ));

    // 删除的文件：块指针还在、块都空闲，原样读出
    let deleted = find(entries, "deleted report.docx");
    assert!(deleted.deleted());
    assert_eq!(
        (deleted.state, deleted.size),
        (State::Intact, report.len() as u64)
    );
    assert_eq!(read_all(&volume, &deleted.file), report);
    // 块已分给新文件
    let victim = find(entries, "victim.bin");
    assert_eq!((victim.deleted(), victim.state), (true, State::Overwritten));
    // inode 已分给新文件：长度、时间都不是它的，读不了
    let spacer = find(entries, "s");
    assert_eq!((spacer.deleted(), spacer.state), (true, State::Overwritten));
    assert_eq!((spacer.size, spacer.modified.as_deref()), (0, None));
    assert!(volume.open_file(&spacer.file).is_err());
    assert_eq!(
        find(entries, "the new file that took the blocks.bin").state,
        State::Live
    );

    // 删除的目录：块空闲、开头还是 `.`，里面的项按已删除列出
    let trash = find(entries, "Trash");
    assert!(trash.is_dir() && trash.deleted());
    assert_eq!(trash.state, State::Intact);
    let trash_listing = volume.list(&trash.file).unwrap();
    assert!(!trash_listing.overwritten);
    let inner_entry = find(&trash_listing.entries, "inner file.dat");
    assert_eq!(
        (inner_entry.deleted(), inner_entry.state),
        (true, State::Intact)
    );
    assert_eq!(read_all(&volume, &inner_entry.file), inner);

    // 定位信息编成字符串再解回来，照样能读
    let token = deleted.file.to_token();
    assert!(token.starts_with("e1-d0-x1-i"), "{token}");
    assert_eq!(
        read_all(&volume, &FileRef::from_token(&token).unwrap()),
        report
    );
    // 未删除的项被当成已删除的（或反过来）传回时读不了
    let forged = FileRef {
        deleted: true,
        ..find(entries, "hello world.txt").file
    };
    assert!(volume.open_file(&forged).is_err());
}

#[test]
fn ext4_volume_is_browsed_and_deleted_files_are_cleared() {
    let mut sample = ExtImage::new(ExtFlavor::Ext4);
    let bs = sample.block_size();
    let hello = pattern(1, bs * 3 + 17);
    sample.add_file(EXT_ROOT, "hello.txt", &hello, Layout::Contiguous);
    // 6 段：放不进 inode，extent 进一个叶子块
    let fragmented = pattern(2, bs * 6 + 100);
    sample.add_file(EXT_ROOT, "fragmented.bin", &fragmented, Layout::Fragmented);
    let mut sparse = pattern(3, bs * 8);
    for hole in [0usize, 4, 6, 7] {
        sparse[hole * bs..(hole + 1) * bs].fill(0);
    }
    sample.add_sparse(EXT_ROOT, "sparse.bin", &sparse, &[0, 4, 6, 7]);
    // inline data：60 字节在 i_block，其余在扩展属性 system.data
    let small = pattern(4, 100);
    sample.add_inline(EXT_ROOT, "small.txt", &small);
    let tiny = pattern(5, 20);
    sample.add_inline(EXT_ROOT, "tiny.txt", &tiny);
    // 2038 年之后（扩展字段的年份位）与 1970 年之前带纳秒
    let later = sample.add_file(EXT_ROOT, "2038.txt", &pattern(6, 10), Layout::Contiguous);
    sample.set_modified(later, (1 << 31) + 100, 5);
    let earlier = sample.add_file(EXT_ROOT, "1969.txt", &pattern(7, 10), Layout::Contiguous);
    sample.set_modified(earlier, -1, 500_000_000);
    let docs = sample.mkdir(EXT_ROOT, "docs");
    for index in 0..80 {
        sample.add_file(
            docs,
            &format!("note {index:02}.txt"),
            &pattern(index, 30),
            Layout::Contiguous,
        );
    }

    let gone = sample.add_file(
        EXT_ROOT,
        "gone.bin",
        &pattern(8, bs * 2),
        Layout::Contiguous,
    );
    let old = sample.mkdir(EXT_ROOT, "old");
    let old_inner = sample.add_file(old, "inner.txt", &pattern(9, 50), Layout::Contiguous);
    sample.delete(EXT_ROOT, "gone.bin", gone);
    sample.delete(old, "inner.txt", old_inner);
    sample.delete(EXT_ROOT, "old", old);

    let volume = open(sample.finish());
    assert_eq!(volume.kind(), FsKind::Ext);
    assert_eq!(volume.label(), Some("ext sample"));
    let entries = volume.list(&volume.root()).unwrap().entries;

    let entry = find(&entries, "hello.txt");
    assert_eq!(read_all(&volume, &entry.file), hello);
    // 大 inode：纳秒与创建时间
    assert_eq!(entry.created.as_deref(), Some(SAMPLE_TIME));
    assert_eq!(
        entry.file.modified.and_then(|stamp| stamp.moment()),
        Some(Moment::Utc {
            seconds: EXT_SAMPLE_SECONDS,
            nanos: EXT_SAMPLE_NANOS
        })
    );
    assert_eq!(
        read_all(&volume, &find(&entries, "fragmented.bin").file),
        fragmented
    );
    let sparse_entry = find(&entries, "sparse.bin");
    assert_eq!(read_all(&volume, &sparse_entry.file), sparse);
    assert!(sparse_entry.position.is_some());
    let small_entry = find(&entries, "small.txt");
    assert_eq!(small_entry.size, 100);
    assert!(small_entry.position.is_some());
    assert_eq!(read_all(&volume, &small_entry.file), small);
    assert_eq!(read_all(&volume, &find(&entries, "tiny.txt").file), tiny);
    assert_eq!(
        find(&entries, "2038.txt").modified.as_deref(),
        Some("2038-01-19 03:15:48")
    );
    let earlier = find(&entries, "1969.txt");
    assert_eq!(earlier.modified.as_deref(), Some("1969-12-31 23:59:59"));
    assert_eq!(
        earlier.file.modified.and_then(|stamp| stamp.moment()),
        Some(Moment::Utc {
            seconds: -1,
            nanos: 500_000_000
        })
    );
    // 目录占了几块，块末尾的校验项不当成删除的项
    let docs = volume.list(&find(&entries, "docs").file).unwrap().entries;
    assert_eq!(docs.len(), 80);
    assert!(docs.iter().all(|entry| !entry.deleted()));
    assert_eq!(
        read_all(&volume, &find(&docs, "note 07.txt").file),
        pattern(7, 30)
    );

    // ext4 删除时清空了长度与 extent：只列名字，读不出
    let gone = find(&entries, "gone.bin");
    assert_eq!(
        (gone.deleted(), gone.state, gone.size),
        (true, State::Cleared, 0)
    );
    assert!(matches!(
        volume.open_file(&gone.file),
        Err(FsError::Invalid(_))
    ));
    let old = find(&entries, "old");
    assert_eq!(
        (old.is_dir(), old.deleted(), old.state),
        (true, true, State::Cleared)
    );
    assert!(matches!(volume.list(&old.file), Err(FsError::Invalid(_))));
}

#[test]
fn ext4_with_4_kib_blocks_and_crc16_descriptors_is_browsed() {
    let mut sample = ExtImage::new(ExtFlavor::Ext4Large);
    let data = pattern(1, sample.block_size() * 5 + 3);
    sample.add_file(EXT_ROOT, "data.bin", &data, Layout::Fragmented);
    let image = sample.finish();
    let (device, length) = disk(image.clone());
    assert_eq!(detect(device.as_ref(), 0, length).unwrap(), FsKind::Ext);
    let superblock = &image[1024..2048];
    assert!(ext_copy_confirmed(device.clone(), 0, length, superblock));
    let volume = open(image.clone());
    assert_eq!(volume.cluster_size(), 4096);
    let entries = volume.list(&volume.root()).unwrap().entries;
    assert_eq!(read_all(&volume, &find(&entries, "data.bin").file), data);

    // 块组描述符改了一个字节：CRC-16 对不上，搜索不认；打开卷不看校验和，照样能浏览
    let mut broken = image;
    broken[4096 + 0x0C] ^= 1;
    let (device, length) = disk(broken.clone());
    assert!(!ext_copy_confirmed(device, 0, length, &broken[1024..2048]));
    assert!(open(broken).list(&volume.root()).is_ok());
}

/// ext2 样本（两个块组，1 号块组开头有备份超级块）。
fn ext2_with_file(data: &[u8]) -> ExtImage {
    let mut sample = ExtImage::new(ExtFlavor::Ext2);
    sample.add_file(EXT_ROOT, "kept.bin", data, Layout::Fragmented);
    sample
}

#[test]
fn ext_volumes_open_from_a_backup_superblock() {
    let data = pattern(3, 5000);
    let sample = ext2_with_file(&data);
    let (primary, backup) = sample.superblock_offsets();
    let backup = backup.unwrap();
    let image = sample.finish();
    assert_eq!(image[backup + 0x38..backup + 0x3A], [0x53, 0xEF]);
    let mut broken = image;
    broken[primary..primary + 1024].fill(0);
    let (device, length) = disk(broken);
    assert_eq!(detect(device.as_ref(), 0, length).unwrap(), FsKind::Unknown);
    assert!(Volume::open(device.clone(), 0, length).is_err());
    assert_eq!(
        detect_with(device.as_ref(), 0, length, BootCopy::Backup).unwrap(),
        FsKind::Ext
    );
    let volume = Volume::open_with(device, 0, length, BootCopy::Backup).unwrap();
    assert_eq!(volume.kind(), FsKind::Ext);
    let entries = volume.list(&volume.root()).unwrap().entries;
    assert_eq!(read_all(&volume, &find(&entries, "kept.bin").file), data);
}

#[test]
fn damaged_superblocks_and_descriptors_are_refused() {
    let sample = ext2_with_file(&pattern(4, 5000));
    let descriptor = sample.descriptor_offset();
    let image = sample.finish();
    // 魔数对、参数不自洽：不认作 ext
    let mut bad = image.clone();
    bad[1024..1028].copy_from_slice(&1u32.to_le_bytes());
    let (device, length) = disk(bad);
    assert_eq!(detect(device.as_ref(), 0, length).unwrap(), FsKind::Unknown);
    assert!(matches!(
        Volume::open(device, 0, length),
        Err(FsError::Unsupported(FsKind::Unknown))
    ));
    // 块组描述符清零：位图与 inode 表不在卷里，打不开、不崩
    let mut bad = image.clone();
    bad[descriptor..descriptor + 32].fill(0);
    let (device, length) = disk(bad);
    assert!(matches!(
        Volume::open(device, 0, length),
        Err(FsError::Invalid(_))
    ));
    // inode 表清零：根目录不是目录
    let mut bad = image.clone();
    bad[5 * 1024..37 * 1024].fill(0);
    let (device, length) = disk(bad);
    assert!(Volume::open(device, 0, length).is_err());
    // 分区被截短（文件的第 3 块之后在分区外）：超出的部分读不出，不越界
    let (device, _) = disk(image[..42 * 1024].to_vec());
    let volume = Volume::open(device, 0, 42 * 1024).unwrap();
    let entries = volume.list(&volume.root()).unwrap().entries;
    let mut reader = volume.open_file(&find(&entries, "kept.bin").file).unwrap();
    let mut data = Vec::new();
    assert!(std::io::Read::read_to_end(&mut reader, &mut data).is_err());

    // metadata_csum：超级块改了一个字节，校验和对不上，搜索不认
    let ext4 = ExtImage::new(ExtFlavor::Ext4).finish();
    let (device, length) = disk(ext4.clone());
    assert!(ext_copy_confirmed(device, 0, length, &ext4[1024..2048]));
    let mut bad = ext4;
    bad[1024 + 0x78] ^= 1;
    let (device, length) = disk(bad.clone());
    assert!(!ext_copy_confirmed(
        device.clone(),
        0,
        length,
        &bad[1024..2048]
    ));
    assert!(Volume::open(device, 0, length).is_ok());
}

#[test]
fn ext_stamps_and_tokens_round_trip() {
    let stamp = Stamp::Ext {
        seconds: 0x8000_0000,
        extra: 1 | (999_999_999 << 2),
    };
    // 有符号读是 1901 年，加上扩展字段的 2^32 秒到 2038 年
    assert_eq!(stamp.text().as_deref(), Some("2038-01-19 03:14:08"));
    let file = FileRef {
        size: 7,
        is_dir: false,
        deleted: true,
        location: crate::Location::Inode {
            inode: u32::MAX,
            generation: 42,
        },
        modified: Some(stamp),
    };
    assert_eq!(FileRef::from_token(&file.to_token()), Some(file));
    // 纳秒超过 10 亿、两项都没记、inode 号溢出
    assert_eq!(
        Stamp::Ext {
            seconds: 1,
            extra: 1_000_000_000 << 2
        }
        .text(),
        None
    );
    assert_eq!(
        Stamp::Ext {
            seconds: 0,
            extra: 0
        }
        .moment(),
        None
    );
    for bad in [
        "e1-d0-x0-i4294967296-g0-s0",
        "e1-d0-x0-i1-g1",
        "e1-d0-x0-i1-g0-s0-te1",
        "e1-d0-x0-i1-g0-s0-tex1",
    ] {
        assert_eq!(FileRef::from_token(bad), None, "{bad}");
    }
}

#[test]
fn mutated_ext_volumes_never_panic() {
    let mut ext2 = ExtImage::new(ExtFlavor::Ext2);
    let dir = ext2.mkdir(EXT_ROOT, "dir");
    ext2.add_file(
        dir,
        "inside.bin",
        &pattern(1, 300 * 1024),
        Layout::Fragmented,
    );
    let gone = ext2.add_file(EXT_ROOT, "gone.bin", &pattern(2, 5000), Layout::Contiguous);
    ext2.delete(EXT_ROOT, "gone.bin", gone);
    let ext2 = ext2.finish();

    let mut ext4 = ExtImage::new(ExtFlavor::Ext4);
    let dir = ext4.mkdir(EXT_ROOT, "dir");
    ext4.add_file(dir, "inside.bin", &pattern(1, 9000), Layout::Fragmented);
    ext4.add_inline(dir, "inline.txt", &pattern(3, 90));
    let gone = ext4.add_file(EXT_ROOT, "gone.bin", &pattern(2, 5000), Layout::Contiguous);
    ext4.delete(EXT_ROOT, "gone.bin", gone);
    let ext4 = ext4.finish();

    let mut large = ExtImage::new(ExtFlavor::Ext4Large);
    large.add_file(
        EXT_ROOT,
        "inside.bin",
        &pattern(1, 30_000),
        Layout::Fragmented,
    );
    let large = large.finish();

    let mut noise = Noise::new(23);
    for _ in 0..200 {
        for (base, hot) in [
            // 超级块、描述符、位图、inode 表与开头的目录和数据
            (&ext2, 1024..96 * 1024),
            // 1 号块组开头的备份超级块
            (&ext2, 8193 * 1024..8195 * 1024),
            (&ext4, 1024..160 * 1024),
            (&large, 0..128 * 1024),
        ] {
            let mut image = base.clone();
            let count = 1 + noise.below(24) as usize;
            noise.mutate(&mut image[hot.clone()], count);
            let superblock = image[1024..2048].to_vec();
            let (device, length) = disk(image);
            let _ = ext_copy_confirmed(device.clone(), 0, length, &superblock);
            for copy in [BootCopy::Primary, BootCopy::Backup] {
                let _ = detect_with(device.as_ref(), 0, length, copy);
                let Ok(volume) = Volume::open_with(device.clone(), 0, length, copy) else {
                    continue;
                };
                let mut budget = 2000;
                walk(&volume, &volume.root(), 0, &mut budget);
            }
        }
    }
}
