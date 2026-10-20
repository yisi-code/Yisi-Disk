use std::path::{Path, PathBuf};

use yisi_io::{BlockDevice, IoError, RawImage};

/// 在 Cargo 提供的测试临时目录里生成内容确定、不重复的镜像文件。
fn make_image(name: &str, len: usize) -> (PathBuf, Vec<u8>) {
    let data: Vec<u8> = (0..len)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, &data).unwrap();
    (path, data)
}

#[test]
fn reads_match_file_content_at_any_offset() {
    let (path, data) = make_image("raw-any-offset.img", 10_000);
    let image = RawImage::open(&path, 512).unwrap();
    assert_eq!(image.size(), 10_000);
    assert_eq!(image.sector_size(), 512);

    for offset in (0..10_000u64).step_by(997) {
        for len in [1usize, 7, 512, 513, 4096] {
            let mut buf = vec![0u8; len];
            let read = image.read_at(offset, &mut buf).unwrap();
            let start = offset as usize;
            let end = (start + len).min(data.len());
            assert_eq!(read, end - start, "offset={offset} len={len}");
            assert_eq!(&buf[..read], &data[start..end]);
        }
    }
}

#[test]
fn reads_are_truncated_at_end_and_exact_reads_report_shortfall() {
    let (path, data) = make_image("raw-end.img", 1_000);
    let image = RawImage::open(&path, 512).unwrap();

    let mut buf = [0u8; 16];
    assert_eq!(image.read_at(990, &mut buf).unwrap(), 10);
    assert_eq!(&buf[..10], &data[990..]);
    assert_eq!(image.read_at(1_000, &mut buf).unwrap(), 0);
    assert_eq!(image.read_at(u64::MAX, &mut buf).unwrap(), 0);

    image.read_exact_at(984, &mut buf).unwrap();
    assert_eq!(&buf[..], &data[984..]);
    let result = image.read_exact_at(990, &mut buf);
    assert!(matches!(
        result,
        Err(IoError::UnexpectedEof {
            offset: 990,
            expected: 16,
            read: 10
        })
    ));
}

#[test]
fn rejects_unsupported_sector_sizes() {
    let (path, _) = make_image("raw-sector.img", 512);
    for sector_size in [0, 256, 768, 131_072] {
        assert!(matches!(
            RawImage::open(&path, sector_size),
            Err(IoError::InvalidSectorSize(size)) if size == sector_size
        ));
    }
    for sector_size in [512, 4096, 65_536] {
        assert_eq!(
            RawImage::open(&path, sector_size).unwrap().sector_size(),
            sector_size
        );
    }
}

#[test]
fn missing_file_reports_its_path() {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("raw-missing.img");
    match RawImage::open(&path, 512) {
        Err(IoError::Open { path: reported, .. }) => assert_eq!(reported, path),
        other => panic!("应返回打开错误，实际为 {other:?}"),
    }
}

#[test]
fn works_as_trait_object() {
    let (path, data) = make_image("raw-dyn.img", 2_048);
    let device: Box<dyn BlockDevice> = Box::new(RawImage::open(&path, 512).unwrap());
    let mut buf = [0u8; 3];
    device.read_exact_at(511, &mut buf).unwrap();
    assert_eq!(&buf[..], &data[511..514]);
}
