use std::fs;

use super::*;
use crate::samples::{EwfLayout, EwfSample, ewf_sample};
use crate::{ImageFormat, image_format, open_image};

/// 前一半是重复的文字（压得很小），后一半是伪随机字节（压不小）。
fn sample_data(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_u32;
    (0..len)
        .map(|index| {
            if index < len / 2 {
                b"yisi E01 sample text "[index % 21]
            } else {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 24) as u8
            }
        })
        .collect()
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yisi-io-ewf-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 100 个扇区、每块 8 个扇区：13 块，最后一块只有 4 个扇区；每段 4 块、每张表 3 块：4 段，每段两组表。
fn layout() -> EwfLayout {
    EwfLayout {
        sectors_per_chunk: 8,
        bytes_per_sector: 512,
        chunks_per_segment: 4,
        chunks_per_table: 3,
        stored: |index| index % 3 == 1,
    }
}

fn write_sample(dir: &Path, data: &[u8], layout: &EwfLayout) -> (PathBuf, EwfSample) {
    let sample = ewf_sample(data, layout);
    let first = dir.join("evidence.E01");
    sample.write(&first);
    (first, sample)
}

#[test]
fn every_byte_matches_across_chunks_tables_and_segments() {
    let dir = test_dir("bytes");
    let data = sample_data(100 * 512);
    let (first, sample) = write_sample(&dir, &data, &layout());
    assert_eq!(sample.segments.len(), 4);
    assert!(sample.chunks.iter().any(|chunk| chunk.compressed));
    assert!(sample.chunks.iter().any(|chunk| !chunk.compressed));

    let image = EwfImage::open(&first).unwrap();
    assert_eq!(image.size(), data.len() as u64);
    assert_eq!(image.sector_size(), 512);
    let names: Vec<_> = image
        .segments()
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "evidence.E01",
            "evidence.E02",
            "evidence.E03",
            "evidence.E04"
        ]
    );

    let mut whole = vec![0u8; data.len() + 100];
    assert_eq!(image.read_at(0, &mut whole).unwrap(), data.len());
    assert_eq!(&whole[..data.len()], &data[..]);
    // 各种起点与长度：跨块、跨表、跨段，读到末尾截断
    let mut state = 7u64;
    for _ in 0..500 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let offset = (state >> 33) % (data.len() as u64 + 50);
        let len = ((state >> 13) % 12_000) as usize;
        let mut buf = vec![0xAA; len];
        let read = image.read_at(offset, &mut buf).unwrap();
        let start = (offset as usize).min(data.len());
        let end = (start + len).min(data.len());
        assert_eq!(read, end - start, "offset={offset} len={len}");
        assert_eq!(&buf[..read], &data[start..end], "offset={offset} len={len}");
    }
    let mut tail = [0u8; 16];
    image
        .read_exact_at(data.len() as u64 - 16, &mut tail)
        .unwrap();
    assert_eq!(&tail[..], &data[data.len() - 16..]);
    assert!(
        image
            .read_exact_at(data.len() as u64 - 8, &mut tail)
            .is_err()
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn damaged_chunks_are_read_errors_and_the_rest_still_reads() {
    let dir = test_dir("damaged");
    let data = sample_data(100 * 512);
    let layout = layout();
    let mut sample = ewf_sample(&data, &layout);
    let chunk_size = 8 * 512;
    // 第 1 块未压缩：改它的校验和；第 3 块压缩：改它中间一个字节
    let stored = sample.chunks[1];
    assert!(!stored.compressed);
    let at = (stored.offset + stored.length - 1) as usize;
    sample.segments[stored.segment][at] ^= 0xFF;
    let packed = sample.chunks[3];
    assert!(packed.compressed);
    let at = (packed.offset + packed.length / 2) as usize;
    sample.segments[packed.segment][at] ^= 0x55;
    let first = dir.join("damaged.E01");
    sample.write(&first);

    let image = EwfImage::open(&first).unwrap();
    let mut buf = vec![0u8; 100];
    for bad in [1u64, 3] {
        let error = image.read_at(bad * chunk_size + 10, &mut buf).unwrap_err();
        match error {
            IoError::Read {
                offset,
                len,
                source,
            } => {
                assert_eq!((offset, len), (bad * chunk_size, chunk_size as usize));
                assert_eq!(source.kind(), io::ErrorKind::InvalidData);
            }
            other => panic!("应是读错误：{other:?}"),
        }
    }
    // 跨进坏块的读取整个报错，不给零
    assert!(image.read_at(chunk_size - 10, &mut buf).is_err());
    for good in [0u64, 2, 4, 12] {
        let start = good * chunk_size;
        let mut chunk = vec![0u8; 512];
        image.read_exact_at(start, &mut chunk).unwrap();
        assert_eq!(&chunk[..], &data[start as usize..start as usize + 512]);
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn missing_segments_and_broken_structures_are_reported() {
    let dir = test_dir("missing");
    let data = sample_data(100 * 512);
    let (first, sample) = write_sample(&dir, &data, &layout());
    fs::remove_file(dir.join("evidence.E03")).unwrap();
    match EwfImage::open(&first) {
        Err(error @ IoError::EwfSegmentMissing { number: 3, .. }) => {
            assert!(error.to_string().contains("缺少第 3 段"), "{error}");
            assert!(error.to_string().contains("evidence.E03"), "{error}");
        }
        Err(other) => panic!("应报缺第 3 段：{other}"),
        Ok(_) => panic!("应报缺第 3 段"),
    }

    // 第二段被截短：没有以 next 结尾
    sample.write(&first);
    let second = dir.join("evidence.E02");
    let len = fs::metadata(&second).unwrap().len();
    let truncated = &sample.segments[1][..len as usize - 76];
    fs::write(&second, truncated).unwrap();
    assert!(matches!(EwfImage::open(&first), Err(IoError::Ewf { .. })));

    // 段文件放错了位置：第二段的文件头记的是第 3 段
    sample.write(&first);
    fs::copy(dir.join("evidence.E03"), &second).unwrap();
    let error = EwfImage::open(&first).err().unwrap();
    assert!(error.to_string().contains("第 3 段"), "{error}");

    // 节描述的校验和不对
    let mut broken = sample.segments[0].clone();
    broken[13] ^= 1;
    fs::write(&first, &broken).unwrap();
    assert!(matches!(EwfImage::open(&first), Err(IoError::Ewf { .. })));
    fs::remove_dir_all(&dir).unwrap();
}

/// 找第 `segment` 段里第 `nth` 个 `kind` 节的数据开头。
fn section_data(segment: &[u8], kind: &str, nth: usize) -> usize {
    let mut offset = 13;
    let mut seen = 0;
    loop {
        let name = &segment[offset..offset + 16];
        let next =
            u64::from_le_bytes(segment[offset + 16..offset + 24].try_into().unwrap()) as usize;
        if name.starts_with(kind.as_bytes()) && name[kind.len()] == 0 {
            if seen == nth {
                return offset + 76;
            }
            seen += 1;
        }
        assert!(next > offset, "没找到 {kind}");
        offset = next;
    }
}

#[test]
fn a_damaged_table_falls_back_to_its_copy() {
    let dir = test_dir("table");
    let data = sample_data(100 * 512);
    let mut sample = ewf_sample(&data, &layout());
    let table = section_data(&sample.segments[1], "table", 0);
    sample.segments[1][table + 24] ^= 0x10;
    let first = dir.join("copy.E01");
    sample.write(&first);
    let image = EwfImage::open(&first).unwrap();
    let mut all = vec![0u8; data.len()];
    image.read_exact_at(0, &mut all).unwrap();
    assert_eq!(all, data);

    // 副本也坏了：打不开
    let copy = section_data(&sample.segments[1], "table2", 0);
    sample.segments[1][copy] ^= 0x01;
    sample.write(&first);
    let error = EwfImage::open(&first).err().unwrap();
    assert!(error.to_string().contains("副本"), "{error}");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn segment_names_continue_after_e99() {
    assert_eq!(segment_extension('E', 1).as_deref(), Some("E01"));
    assert_eq!(segment_extension('E', 99).as_deref(), Some("E99"));
    assert_eq!(segment_extension('E', 100).as_deref(), Some("EAA"));
    assert_eq!(segment_extension('E', 125).as_deref(), Some("EAZ"));
    assert_eq!(segment_extension('E', 126).as_deref(), Some("EBA"));
    assert_eq!(segment_extension('E', 775).as_deref(), Some("EZZ"));
    assert_eq!(segment_extension('E', 776).as_deref(), Some("FAA"));
    assert_eq!(segment_extension('e', 101).as_deref(), Some("eab"));
    assert_eq!(
        segment_extension('E', 100 + 22 * 676 - 1).as_deref(),
        Some("ZZZ")
    );
    assert_eq!(segment_extension('E', 100 + 22 * 676), None);
    assert_eq!(
        segment_path(Path::new("d/x.e01"), 2),
        Some(PathBuf::from("d/x.e02"))
    );
    assert_eq!(segment_path(Path::new("d/x.bin"), 2), None);

    // 101 段（每段一块），小写扩展名：第 100、101 段是 .eaa、.eab
    let dir = test_dir("names");
    let data = sample_data(101 * 512);
    let layout = EwfLayout {
        sectors_per_chunk: 1,
        chunks_per_segment: 1,
        ..EwfLayout::default()
    };
    let sample = ewf_sample(&data, &layout);
    let paths = sample.write(&dir.join("many.e01"));
    assert_eq!(paths.len(), 101);
    assert!(paths[100].ends_with("many.eab"));
    let image = EwfImage::open(&paths[0]).unwrap();
    assert_eq!(image.segments(), &paths[..]);
    let mut all = vec![0u8; data.len()];
    image.read_exact_at(0, &mut all).unwrap();
    assert_eq!(all, data);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn images_are_recognised_by_signature_not_extension() {
    let dir = test_dir("format");
    let data = sample_data(40 * 512);
    let (first, _) = write_sample(&dir, &data, &layout());
    // 改名成 .img 照样认得出是 E01（只有一段时找不找得到后续段无关紧要）
    let single = ewf_sample(
        &data,
        &EwfLayout {
            sectors_per_chunk: 8,
            ..EwfLayout::default()
        },
    );
    let renamed = dir.join("renamed.img");
    fs::write(&renamed, &single.segments[0]).unwrap();
    let raw = dir.join("plain.E01");
    fs::write(&raw, &data).unwrap();
    let tiny = dir.join("tiny.img");
    fs::write(&tiny, b"EVF").unwrap();

    assert_eq!(image_format(&first).unwrap(), ImageFormat::Ewf);
    assert_eq!(image_format(&renamed).unwrap(), ImageFormat::Ewf);
    assert_eq!(image_format(&raw).unwrap(), ImageFormat::Raw);
    assert_eq!(image_format(&tiny).unwrap(), ImageFormat::Raw);

    let opened = open_image(&renamed, 4096).unwrap();
    assert_eq!(opened.format, ImageFormat::Ewf);
    assert_eq!(opened.device.sector_size(), 512);
    let mut all = vec![0u8; data.len()];
    opened.device.read_exact_at(0, &mut all).unwrap();
    assert_eq!(all, data);
    let opened = open_image(&first, 512).unwrap();
    assert_eq!(opened.files.len(), 2);
    let opened = open_image(&raw, 4096).unwrap();
    assert_eq!(
        (
            opened.format,
            opened.device.sector_size(),
            opened.files.len()
        ),
        (ImageFormat::Raw, 4096, 1)
    );

    // 第二版格式认得出是 EWF 家族，打开时说明还不支持
    let mut ex01 = b"EVF2\x0d\x0a\x81\x00".to_vec();
    ex01.resize(4096, 0);
    let ex01_path = dir.join("new.Ex01");
    fs::write(&ex01_path, &ex01).unwrap();
    assert_eq!(image_format(&ex01_path).unwrap(), ImageFormat::Ewf);
    let error = open_image(&ex01_path, 512).err().unwrap();
    assert!(error.to_string().contains("Ex01"), "{error}");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_chunk_cache_is_bounded() {
    let mut cache = ChunkCache::new(2);
    for index in 0..5u64 {
        cache.put(index, Arc::new(vec![index as u8]));
    }
    assert_eq!(cache.entries.len(), 2);
    assert!(cache.get(2).is_none());
    assert_eq!(cache.get(3).unwrap()[0], 3);
    // 刚用过的 3 留下，4 被挤掉
    cache.put(5, Arc::new(vec![5]));
    assert!(cache.get(4).is_none());
    assert!(cache.get(3).is_some());
}
