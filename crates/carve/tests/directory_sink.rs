//! 恢复结果写进真实目录：文件名按起始扇区编号，内容与原文件一致，不留 `.part` 临时文件。

use std::cell::Cell;
use std::fs;
use std::path::Path;

use yisi_carve::{CarveControl, CarveOptions, DirectorySink, RecoveredFile, carve};
use yisi_formats::Registry;
use yisi_formats::samples::{self, Noise};
use yisi_io::{BlockDevice, RawImage};

struct Quiet {
    found: Cell<u32>,
}

impl CarveControl for Quiet {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn progress(&self, _done: u64, _total: u64) {}

    fn found(&self, _file: &RecoveredFile) {
        self.found.set(self.found.get() + 1);
    }
}

#[test]
fn writes_recovered_files_with_sector_based_names() {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR")).join("carve-directory-sink");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let jpg = samples::jpg(3000);
    let pdf = samples::pdf(2, false);
    let mut noise = Noise::new(3);
    let mut image = vec![0u8; 1024];
    image.extend_from_slice(&jpg);
    image.resize(image.len().div_ceil(512) * 512 + 512, 0);
    let pdf_offset = image.len();
    image.extend_from_slice(&pdf);
    image.resize(image.len().div_ceil(512) * 512, 0);
    // ZIP 里装的是 Word 文档：按内容存成 .docx
    let docx = samples::zip(
        &[
            ("[Content_Types].xml", 40, false),
            ("word/document.xml", 300, false),
        ],
        b"",
    );
    let docx_offset = image.len();
    image.extend_from_slice(&docx);
    image.extend(noise.bytes(2000).into_iter().map(|byte| byte & 0x7F));
    let image_path = tmp.join("disk.img");
    fs::write(&image_path, &image).unwrap();

    let device = RawImage::open(&image_path, 512).unwrap();
    let registry = Registry::builtin();
    let options = CarveOptions {
        block_size: device.sector_size(),
        enabled: vec![true; registry.formats().len()],
        start: 0,
    };
    let output = tmp.join("recovered");
    let mut sink = DirectorySink::new(&output).unwrap();
    let control = Quiet {
        found: Cell::new(0),
    };
    let report = carve(&device, &registry, &options, &mut sink, &control)
        .unwrap()
        .unwrap();

    assert_eq!(report.files, 3);
    assert_eq!(control.found.get(), 3);
    assert_eq!(fs::read(output.join("f0000000002.jpg")).unwrap(), jpg);
    let pdf_name = format!("f{:010}.pdf", pdf_offset / 512);
    assert_eq!(fs::read(output.join(pdf_name)).unwrap(), pdf);
    let docx_name = format!("f{:010}.docx", docx_offset / 512);
    assert_eq!(fs::read(output.join(docx_name)).unwrap(), docx);
    assert_eq!(fs::read_dir(&output).unwrap().count(), 3);
    // 续扫从已经存好的文件前面接着扫（检查点保守、或中途退出留下半份 .part）：留着原来那份，不报错
    fs::write(
        output.join(format!("f{:010}.pdf.part", pdf_offset / 512)),
        b"half",
    )
    .unwrap();
    let mut again = DirectorySink::new(&output).unwrap();
    let report = carve(&device, &registry, &options, &mut again, &control)
        .unwrap()
        .unwrap();
    assert_eq!(report.files, 3);
    assert_eq!(fs::read_dir(&output).unwrap().count(), 3);
    assert_eq!(fs::read(output.join("f0000000002.jpg")).unwrap(), jpg);

    let leftovers: Vec<_> = fs::read_dir(&output)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "part"))
        .collect();
    assert!(leftovers.is_empty());
}
