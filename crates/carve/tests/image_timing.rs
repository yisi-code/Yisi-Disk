//! 对一个镜像文件只扫描、不保存地跑一遍深度恢复，报告找到多少个、其中多少个由两段拼成、用时。
//! 镜像路径由环境变量给出，另设 `YISI_CARVE_JOIN`（任意值）时打开两段重组；手动运行（release 构建才有参考意义）：
//! `$env:YISI_CARVE_IMAGE = '<镜像>'; cargo test --release -p yisi-carve --test image_timing -- --ignored --nocapture`

use std::cell::Cell;
use std::time::Instant;

use yisi_carve::{CarveControl, CarveOptions, RecoveredFile, ScanOnlySink, carve};
use yisi_formats::Registry;
use yisi_io::{BlockDevice, RawImage};

#[derive(Default)]
struct Count {
    files: Cell<u64>,
    reassembled: Cell<u64>,
}

impl CarveControl for Count {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn progress(&self, _done: u64, _total: u64) {}

    fn found(&self, file: &RecoveredFile) {
        self.files.set(self.files.get() + 1);
        if !file.fragments.is_empty() {
            self.reassembled.set(self.reassembled.get() + 1);
        }
    }
}

#[test]
#[ignore]
fn scan_an_image_and_report_the_time() {
    let path = std::env::var("YISI_CARVE_IMAGE").expect("设置 YISI_CARVE_IMAGE");
    let device = RawImage::open(&path, 512).unwrap();
    let registry = Registry::builtin();
    let options = CarveOptions {
        block_size: device.sector_size(),
        enabled: vec![true; registry.formats().len()],
        start: 0,
        keep_incomplete: false,
        join_fragments: std::env::var_os("YISI_CARVE_JOIN").is_some(),
    };
    let control = Count::default();
    let started = Instant::now();
    carve(&device, &registry, &options, &mut ScanOnlySink, &control)
        .unwrap()
        .unwrap();
    println!(
        "{path}：{} MiB，找到 {} 个，其中两段拼成 {} 个，用时 {:?}",
        device.size() >> 20,
        control.files.get(),
        control.reassembled.get(),
        started.elapsed()
    );
}
