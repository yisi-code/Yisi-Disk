#![cfg(windows)]

use yisi_io::{BlockDevice, WindowsDisk};

/// 只读真盘测试：需要管理员权限，设备路径由环境变量 `YISI_IO_TEST_DEVICE` 指定。
///
/// 只读开头 64 KiB 与最后几个字节，核对未对齐读取与对齐读取的结果一致。
#[test]
#[ignore = "需要管理员权限与环境变量 YISI_IO_TEST_DEVICE；只读"]
fn physical_disk_reads_are_consistent() {
    let path = std::env::var("YISI_IO_TEST_DEVICE").expect("未设置 YISI_IO_TEST_DEVICE");
    let disk = WindowsDisk::open(&path).unwrap();
    println!(
        "device={path} size={} sector_size={}",
        disk.size(),
        disk.sector_size()
    );
    assert!(disk.size() > 0);
    assert_eq!(disk.size() % u64::from(disk.sector_size()), 0);

    let mut head = vec![0u8; 64 * 1024];
    disk.read_exact_at(0, &mut head).unwrap();
    for (offset, len) in [
        (0usize, 1usize),
        (1, 1),
        (510, 2),
        (511, 3),
        (1_000, 5_000),
        (4_095, 4_098),
        (12_345, 40_000),
    ] {
        let mut buf = vec![0u8; len];
        disk.read_exact_at(offset as u64, &mut buf).unwrap();
        assert_eq!(buf, head[offset..offset + len], "offset={offset} len={len}");
    }

    let mut tail = [0u8; 4];
    assert_eq!(disk.read_at(disk.size() - 1, &mut tail).unwrap(), 1);
    assert_eq!(disk.read_at(disk.size(), &mut tail).unwrap(), 0);
}
