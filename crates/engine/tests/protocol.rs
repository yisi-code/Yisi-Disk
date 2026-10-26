//! 启动真实的引擎进程，按宿主的方式收发 JSON-RPC。

use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use yisi_engine::protocol::{
    BROWSE_OPEN, BrowseOpened, CANCEL, CANCEL_SCAN, CANCELLED, CAPABILITIES, CARVE_EXTRACT,
    CARVE_START, COMPLETED, COPIED, COPY_FILES, CarveSummary, CopiedFile, CopySummary, FOUND,
    FileListPage, FilePreview, FoundFile, HEARTBEAT, LIST_FILE_TYPES, LIST_FILES, PREVIEW_FILE,
    PREVIEW_FS_FILE, PREVIEW_RANGE, PROGRESS, READ_SCAN, ReadScanReport, SCAN_DELETED, ScanResult,
    error_code,
};
use yisi_formats::samples;
use yisi_rpc::{Id, Message, Request, error_code as rpc_error, read_message, write_message};

const TIMEOUT: Duration = Duration::from_secs(20);

struct Engine {
    child: Child,
    stdin: Option<ChildStdin>,
    incoming: Receiver<Message>,
    next_id: u64,
}

impl Engine {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_yisi-engine"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().unwrap();
        let (sender, incoming) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(message) = read_message(&mut stdout) {
                if sender.send(message).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            stdin,
            incoming,
            next_id: 0,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, yisi_rpc::ErrorObject> {
        self.call_noting(method, params).0
    }

    /// 发请求、等答复，同时收下途中的通知（不含心跳）。
    fn call_noting(
        &mut self,
        method: &str,
        params: Value,
    ) -> (Result<Value, yisi_rpc::ErrorObject>, Vec<(String, Value)>) {
        self.next_id += 1;
        let id = Id::Number(self.next_id);
        let request = Message::Request(Request::new(id.clone(), method, params));
        write_message(self.stdin.as_mut().unwrap(), &request).unwrap();
        let mut seen = Vec::new();
        loop {
            match self.incoming.recv_timeout(TIMEOUT).unwrap() {
                Message::Response(response) if response.id == id => {
                    return (response.outcome, seen);
                }
                Message::Notification(notification) if notification.method != HEARTBEAT => {
                    seen.push((notification.method, notification.params));
                }
                Message::Notification(_) => {}
                other => panic!("意外的消息：{other:?}"),
            }
        }
    }

    /// 收通知直到出现 `method`，返回途中收到的全部通知（不含心跳）。
    fn notifications_until(&self, method: &str) -> Vec<(String, Value)> {
        let mut seen = Vec::new();
        loop {
            match self.incoming.recv_timeout(TIMEOUT).unwrap() {
                Message::Notification(notification) if notification.method == HEARTBEAT => {}
                Message::Notification(notification) => {
                    let done = notification.method == method;
                    seen.push((notification.method, notification.params));
                    if done {
                        return seen;
                    }
                }
                other => panic!("意外的消息：{other:?}"),
            }
        }
    }

    fn close_input(&mut self) {
        self.stdin = None;
    }

    fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "引擎没有按时退出");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn make_image(name: &str, len: usize) -> String {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, vec![0x5a; len]).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn capabilities_and_errors_keep_the_engine_alive() {
    let mut engine = Engine::start();

    let capabilities = engine.call(CAPABILITIES, Value::Null).unwrap();
    assert_eq!(capabilities["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        capabilities["methods"]
            .as_array()
            .unwrap()
            .contains(&Value::from(READ_SCAN))
    );

    let unknown = engine.call("disk.format", json!({})).unwrap_err();
    assert_eq!(unknown.code, rpc_error::METHOD_NOT_FOUND);

    let invalid = engine.call(READ_SCAN, json!({"jobId": 1})).unwrap_err();
    assert_eq!(invalid.code, rpc_error::INVALID_PARAMS);

    let missing = engine
        .call(
            READ_SCAN,
            json!({"jobId": "j", "source": {"type": "image", "path": "Z:/no/such.img", "sectorSize": 512}}),
        )
        .unwrap_err();
    assert_eq!(missing.code, error_code::SOURCE_UNAVAILABLE);

    let not_running = engine.call(CANCEL, json!({"jobId": "j"})).unwrap_err();
    assert_eq!(not_running.code, error_code::JOB_NOT_FOUND);

    // 宿主断开、没有任务时，引擎直接退出
    engine.close_input();
    assert!(engine.wait_exit().success());
}

/// 只查询设备属性，不读磁盘内容，普通权限即可。
#[cfg(windows)]
#[test]
fn disk_list_reports_sizes_without_privilege() {
    let mut engine = Engine::start();
    let list = engine.call("disk.list", Value::Null).unwrap();
    let disks = list["disks"].as_array().unwrap();
    assert!(!disks.is_empty(), "至少应有系统盘");
    for disk in disks {
        assert!(disk["size"].as_u64().unwrap() > 0, "{disk}");
        assert!(disk["sectorSize"].as_u64().unwrap() >= 512, "{disk}");
        assert!(
            disk["path"]
                .as_str()
                .unwrap()
                .starts_with(r"\\.\PhysicalDrive"),
            "{disk}"
        );
    }
    engine.close_input();
    assert!(engine.wait_exit().success());
}

#[test]
fn disk_source_rejects_bogus_handle() {
    let mut engine = Engine::start();
    let error = engine
        .call(
            READ_SCAN,
            json!({"jobId": "j", "source": {"type": "disk", "handle": 0x7fff_fff0u64}}),
        )
        .unwrap_err();
    assert_eq!(error.code, error_code::SOURCE_UNAVAILABLE);
    engine.close_input();
    assert!(engine.wait_exit().success());
}

#[test]
fn read_scan_reports_progress_then_completes_and_exits() {
    let size = 5 * 1024 * 1024 + 100;
    let path = make_image("engine-read-scan.img", size);
    let mut engine = Engine::start();

    let accepted = engine
        .call(
            READ_SCAN,
            json!({"jobId": "scan-1", "source": {"type": "image", "path": path, "sectorSize": 512}}),
        )
        .unwrap();
    assert_eq!(accepted, json!({}));

    let notifications = engine.notifications_until(COMPLETED);
    let (_, last_progress) = notifications
        .iter()
        .rev()
        .find(|(method, _)| method == PROGRESS)
        .unwrap();
    assert_eq!(last_progress["jobId"], "scan-1");
    assert_eq!(last_progress["done"], size as u64);
    assert_eq!(last_progress["total"], size as u64);

    let (_, completed) = notifications.last().unwrap();
    assert_eq!(completed["jobId"], "scan-1");
    let report: ReadScanReport = serde_json::from_value(completed["result"].clone()).unwrap();
    assert_eq!(report.size, size as u64);
    assert_eq!(report.bytes_read, size as u64);
    assert!(report.unreadable.is_empty());

    // 任务结束后引擎自行退出
    assert!(engine.wait_exit().success());
}

#[test]
fn carve_recovers_all_five_formats_from_an_image() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("engine-carve");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let originals = [
        ("jpg", samples::jpg(6000)),
        ("png", samples::png(&[200, 80_000])),
        (
            "zip",
            samples::zip(&[("readme.txt", 700, false), ("data.bin", 5000, true)], b""),
        ),
        ("pdf", samples::pdf(3, true)),
        ("mp4", samples::mp4(30_000, true)),
    ];
    // 每个文件从扇区边界开始，前后用 0x20 填充（不会像任何文件头）
    let mut image = vec![0x20u8; 2048];
    let mut offsets = Vec::new();
    for (_, bytes) in &originals {
        offsets.push(image.len() as u64);
        image.extend_from_slice(bytes);
        let padded = image.len().div_ceil(512) * 512 + 1024;
        image.resize(padded, 0x20);
    }
    let image_path = dir.join("disk.img");
    std::fs::write(&image_path, &image).unwrap();

    let mut engine = Engine::start();
    let types = engine.call(LIST_FILE_TYPES, Value::Null).unwrap();
    let extensions: Vec<_> = types["types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["extension"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(extensions, ["jpg", "png", "pdf", "zip", "mp4"]);

    let unknown = engine
        .call(
            CARVE_START,
            json!({"jobId": "c0", "source": {"type": "image", "path": image_path, "sectorSize": 512},
                   "outputDir": dir, "extensions": ["exe"]}),
        )
        .unwrap_err();
    assert_eq!(unknown.code, rpc_error::INVALID_PARAMS);
    // 不是磁盘的句柄认领不了；源打不开时不创建输出目录
    let disk = engine
        .call(
            CARVE_START,
            json!({"jobId": "c0", "source": {"type": "disk", "handle": 4}, "outputDir": dir}),
        )
        .unwrap_err();
    assert_eq!(disk.code, error_code::SOURCE_UNAVAILABLE);
    assert!(!dir.join("recovered-c0").exists());

    let accepted = engine
        .call(
            CARVE_START,
            json!({"jobId": "carve-1-abcdef", "source": {"type": "image", "path": image_path, "sectorSize": 512},
                   "outputDir": dir}),
        )
        .unwrap();
    let output = dir.join("recovered-carve-1-");
    assert_eq!(accepted, json!({"outputDir": output.display().to_string()}));

    let notifications = engine.notifications_until(COMPLETED);
    let found: Vec<FoundFile> = notifications
        .iter()
        .filter(|(method, _)| method == FOUND)
        .flat_map(|(_, params)| {
            assert_eq!(params["jobId"], "carve-1-abcdef");
            serde_json::from_value::<Vec<FoundFile>>(params["files"].clone()).unwrap()
        })
        .collect();
    let (_, completed) = notifications.last().unwrap();
    let summary: CarveSummary = serde_json::from_value(completed["result"].clone()).unwrap();

    assert_eq!(
        summary.output_dir.as_deref().map(Path::new),
        Some(output.as_path())
    );
    assert_eq!(summary.files, 5);
    assert_eq!(found.len(), 5);
    for ((extension, bytes), (file, offset)) in originals.iter().zip(found.iter().zip(&offsets)) {
        assert_eq!(
            (file.extension.as_str(), file.offset),
            (*extension, *offset)
        );
        let recovered = std::fs::read(output.join(&file.name)).unwrap();
        assert!(&recovered == bytes, "{extension} 内容不一致");
    }
    assert!(engine.wait_exit().success());

    // 任务结束后引擎已退出，预览另起一个引擎，与宿主的做法一致
    let mut viewer = Engine::start();
    let png = found.iter().find(|file| file.extension == "png").unwrap();
    let preview: FilePreview = serde_json::from_value(
        viewer
            .call(PREVIEW_FILE, json!({"path": output.join(&png.name)}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(preview.length, png.length);
    // 样本 PNG 的 IDAT 是随机数据：认得出是 PNG、尝试解码，解不出时给出原因
    assert!(preview.image.is_none());
    assert!(preview.image_error.is_some());
    let missing = viewer
        .call(
            PREVIEW_FILE,
            json!({"path": output.join("f9999999999.png")}),
        )
        .unwrap_err();
    assert_eq!(missing.code, error_code::PREVIEW_UNAVAILABLE);
}

/// 一块 MBR 盘：1 号分区 FAT16（大目录、删除的文件、删除的目录），2 号分区 exFAT。
fn browse_image(dir: &Path) -> (String, Vec<u8>, Vec<u8>) {
    use yisi_fs::samples::{ExfatImage, FatImage, FatType, Layout, pattern};
    use yisi_part::samples::{MbrEntry, mbr_disk};

    let mut fat = FatImage::new(FatType::Fat16);
    fat.set_label(b"BROWSE     ");
    let root = fat.root();
    let many = fat.mkdir(root, "many").as_dir();
    for index in 0..1500 {
        fat.add_file(
            many,
            &format!("file {index:04}.txt"),
            &pattern(index as u8, 10),
            Layout::Contiguous,
        );
    }
    let photos = fat.mkdir(root, "Photos").as_dir();
    let photo = pattern(1, 5000);
    fat.add_file(photos, "海边.jpg", &photo, Layout::Fragmented);
    let report = pattern(2, 7000);
    fat.add_file(root, "report.docx", &pattern(3, 100), Layout::Contiguous);
    let old_report = fat.add_file(root, "report.docx", &report, Layout::Contiguous);
    fat.delete(&old_report);
    let fat = fat.finish();

    let mut exfat = ExfatImage::new();
    exfat.set_label("SECOND");
    let root = exfat.root();
    exfat.add_file(root, "readme.txt", b"hello", Layout::Contiguous);
    let exfat = exfat.finish();

    let fat_start = 2048u32;
    let exfat_start = fat_start + (fat.len() / 512) as u32;
    let mut image = mbr_disk(
        exfat_start as usize * 512 + exfat.len(),
        &[
            MbrEntry::new(0x06, fat_start, (fat.len() / 512) as u32),
            MbrEntry::new(0x07, exfat_start, (exfat.len() / 512) as u32),
        ],
    );
    image[fat_start as usize * 512..][..fat.len()].copy_from_slice(&fat);
    image[exfat_start as usize * 512..][..exfat.len()].copy_from_slice(&exfat);
    let path = dir.join("browse.img");
    std::fs::write(&path, &image).unwrap();
    (path.to_string_lossy().into_owned(), photo, report)
}

#[test]
fn browse_lists_pages_previews_and_copies_deleted_files() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("engine-browse");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (image, photo, report) = browse_image(&dir);
    let source = json!({"type": "image", "path": image, "sectorSize": 512});

    let mut engine = Engine::start();
    let opened: BrowseOpened = serde_json::from_value(
        engine
            .call(BROWSE_OPEN, json!({"jobId": "b1", "source": source}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(opened.scheme, "mbr");
    let summary: Vec<_> = opened
        .partitions
        .iter()
        .map(|p| {
            (
                p.number,
                p.filesystem.as_str(),
                p.label.as_deref(),
                p.browsable,
                p.role.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (1, "fat16", Some("BROWSE"), true, Some("fat16")),
            (2, "exfat", Some("SECOND"), true, Some("ntfsOrExfat")),
        ]
    );

    let list = |engine: &mut Engine, dir: Option<&str>, offset: u32, limit: u32| -> FileListPage {
        serde_json::from_value(
            engine
                .call(
                    LIST_FILES,
                    json!({"jobId": "b1", "partition": 1, "dir": dir, "offset": offset, "limit": limit}),
                )
                .unwrap(),
        )
        .unwrap()
    };
    // 目录在前，名字不分大小写；同名时未删除的在前
    let root = list(&mut engine, None, 0, 100);
    let names: Vec<_> = root
        .entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.deleted, entry.state.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("many", false, "live"),
            ("Photos", false, "live"),
            ("report.docx", false, "live"),
            ("report.docx", true, "intact"),
        ]
    );
    let many = root.entries[0].file.clone();
    let first = list(&mut engine, Some(&many), 0, 1000);
    assert_eq!((first.total, first.entries.len()), (1500, 1000));
    assert_eq!(first.entries[0].name, "file 0000.txt");
    let rest = list(&mut engine, Some(&many), 1000, 1000);
    assert_eq!(rest.entries.len(), 500);
    assert_eq!(rest.entries[499].name, "file 1499.txt");

    let deleted = root.entries[3].clone();
    assert!(deleted.position.is_some());
    let preview: FilePreview = serde_json::from_value(
        engine
            .call(
                PREVIEW_FS_FILE,
                json!({"jobId": "b1", "partition": 1, "file": deleted.file}),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(preview.length, report.len() as u64);

    let wrong_job = engine
        .call(
            LIST_FILES,
            json!({"jobId": "other", "partition": 1, "offset": 0, "limit": 10}),
        )
        .unwrap_err();
    assert_eq!(wrong_job.code, error_code::JOB_NOT_FOUND);
    let bad_token = engine
        .call(
            LIST_FILES,
            json!({"jobId": "b1", "partition": 1, "dir": "x", "offset": 0, "limit": 10}),
        )
        .unwrap_err();
    assert_eq!(bad_token.code, rpc_error::INVALID_PARAMS);
    let no_partition = engine
        .call(
            LIST_FILES,
            json!({"jobId": "b1", "partition": 9, "offset": 0, "limit": 10}),
        )
        .unwrap_err();
    assert_eq!(no_partition.code, error_code::FS_UNAVAILABLE);

    // 关闭浏览：取消任务，引擎随后退出
    engine.call(CANCEL, json!({"jobId": "b1"})).unwrap();
    engine.notifications_until(CANCELLED);
    assert!(engine.wait_exit().success());

    // 复制另起一个引擎：整个 Photos 目录、同名的现存文件与已删除文件
    let photos = root.entries[1].file.clone();
    let partition = &opened.partitions[0];
    let partition_ref =
        json!({"number": 1, "offset": partition.offset, "length": partition.length});
    let mut copier = Engine::start();
    let moved = copier
        .call(
            COPY_FILES,
            json!({"jobId": "copy-1-xyz", "source": source, "outputDir": dir,
                   "partition": {"number": 1, "offset": 0, "length": partition.length},
                   "items": [{"file": photos, "path": ["Photos"]}]}),
        )
        .unwrap_err();
    assert_eq!(moved.code, error_code::FS_UNAVAILABLE);
    let started = copier
        .call(
            COPY_FILES,
            json!({"jobId": "copy-1-xyz", "source": source, "outputDir": dir, "partition": partition_ref,
                   "items": [
                       {"file": photos, "path": ["Photos"]},
                       {"file": root.entries[2].file, "path": ["report.docx"]},
                       {"file": deleted.file, "path": ["report.docx"]},
                   ]}),
        )
        .unwrap();
    let output = dir.join("restored-copy-1-x");
    assert_eq!(started, json!({"outputDir": output.display().to_string()}));
    let notifications = copier.notifications_until(COMPLETED);
    let copied: Vec<CopiedFile> = notifications
        .iter()
        .filter(|(method, _)| method == COPIED)
        .flat_map(|(_, params)| {
            serde_json::from_value::<Vec<CopiedFile>>(params["files"].clone()).unwrap()
        })
        .collect();
    let paths: Vec<_> = copied
        .iter()
        .map(|file| (file.path.as_str(), file.error.is_none()))
        .collect();
    assert_eq!(
        paths,
        [
            ("Photos/海边.jpg", true),
            ("report.docx", true),
            ("report (1).docx", true)
        ]
    );
    let summary: CopySummary =
        serde_json::from_value(notifications.last().unwrap().1["result"].clone()).unwrap();
    assert_eq!((summary.files, summary.failed), (3, 0));
    assert_eq!(
        std::fs::read(output.join("Photos").join("海边.jpg")).unwrap(),
        photo
    );
    assert_eq!(
        std::fs::read(output.join("report (1).docx")).unwrap(),
        report
    );
    assert!(copier.wait_exit().success());
}

#[test]
fn scan_only_then_preview_and_save_selected_files() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("engine-scan-only");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let originals = [
        ("jpg", samples::jpg(6000)),
        ("pdf", samples::pdf(3, true)),
        ("zip", samples::zip(&[("a.txt", 3000, true)], b"")),
    ];
    let mut image = vec![0x20u8; 2048];
    let mut offsets = Vec::new();
    for (_, bytes) in &originals {
        offsets.push(image.len());
        image.extend_from_slice(bytes);
        let padded = image.len().div_ceil(512) * 512 + 1024;
        image.resize(padded, 0x20);
    }
    let image_path = dir.join("disk.img");
    std::fs::write(&image_path, &image).unwrap();
    let source = json!({"type": "image", "path": image_path, "sectorSize": 512});

    // 只扫描：不给输出目录，什么都不写
    let mut engine = Engine::start();
    let accepted = engine
        .call(
            CARVE_START,
            json!({"jobId": "scan-1-abcdef", "source": source}),
        )
        .unwrap();
    assert_eq!(accepted, json!({"outputDir": null}));
    let notifications = engine.notifications_until(COMPLETED);
    let found: Vec<FoundFile> = notifications
        .iter()
        .filter(|(method, _)| method == FOUND)
        .flat_map(|(_, params)| {
            serde_json::from_value::<Vec<FoundFile>>(params["files"].clone()).unwrap()
        })
        .collect();
    let summary: CarveSummary =
        serde_json::from_value(notifications.last().unwrap().1["result"].clone()).unwrap();
    assert_eq!((summary.files, summary.output_dir), (3, None));
    assert_eq!(found.len(), 3);
    assert_eq!(found[0].name, format!("f{:010}.jpg", offsets[0] / 512));
    assert!(
        std::fs::read_dir(&dir).unwrap().count() == 1,
        "只扫描时不应建任何目录"
    );
    assert!(engine.wait_exit().success());

    // 从源上预览还没保存的文件
    let mut viewer = Engine::start();
    let preview: FilePreview = serde_json::from_value(
        viewer
            .call(
                PREVIEW_RANGE,
                json!({"source": source, "offset": found[1].offset, "length": found[1].length}),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(preview.length, found[1].length);

    // 扫描之后源上的 ZIP 被改了：保存时重新校验，不保留
    let zip_at = offsets[2];
    image[zip_at..zip_at + 4].copy_from_slice(b"XXXX");
    std::fs::write(&image_path, &image).unwrap();

    let mut saver = Engine::start();
    let items: Vec<_> = found
        .iter()
        .map(|file| json!({"offset": file.offset, "length": file.length, "extension": file.extension}))
        .collect();
    let bogus = saver
        .call(
            CARVE_EXTRACT,
            json!({"jobId": "save-1", "source": source, "outputDir": dir,
                   "items": [{"offset": 0, "length": 0, "extension": "jpg"}]}),
        )
        .unwrap_err();
    assert_eq!(bogus.code, rpc_error::INVALID_PARAMS);
    let started = saver
        .call(
            CARVE_EXTRACT,
            json!({"jobId": "save-1-xyz", "source": source, "outputDir": dir, "items": items}),
        )
        .unwrap();
    let output = dir.join("recovered-save-1-x");
    assert_eq!(started, json!({"outputDir": output.display().to_string()}));
    let notifications = saver.notifications_until(COMPLETED);
    let saved: Vec<CopiedFile> = notifications
        .iter()
        .filter(|(method, _)| method == COPIED)
        .flat_map(|(_, params)| {
            serde_json::from_value::<Vec<CopiedFile>>(params["files"].clone()).unwrap()
        })
        .collect();
    let outcome: Vec<_> = saved
        .iter()
        .map(|file| (file.path.as_str(), file.error.is_none()))
        .collect();
    assert_eq!(
        outcome,
        [
            (found[0].name.as_str(), true),
            (found[1].name.as_str(), true),
            (found[2].name.as_str(), false),
        ]
    );
    let summary: CopySummary =
        serde_json::from_value(notifications.last().unwrap().1["result"].clone()).unwrap();
    assert_eq!((summary.files, summary.failed), (2, 1));
    for ((_, bytes), file) in originals.iter().zip(&found).take(2) {
        assert!(
            std::fs::read(output.join(&file.name)).unwrap() == *bytes,
            "{} 内容不一致",
            file.name
        );
    }
    assert!(!output.join(&found[2].name).exists());
    assert!(saver.wait_exit().success());
}

/// 一块 MBR 盘，1 号分区是 NTFS：现存与删除的文件、删除的目录、找不到原目录的文件。
fn ntfs_image(dir: &Path) -> (String, Vec<u8>, Vec<u8>) {
    use yisi_fs::samples::{NTFS_ROOT, NtfsImage, pattern};
    use yisi_part::samples::{MbrEntry, mbr_disk};

    let mut ntfs = NtfsImage::new();
    ntfs.set_label("NTFSDATA");
    ntfs.add_file(NTFS_ROOT, "keep.txt", b"keep");
    let docs = ntfs.mkdir(NTFS_ROOT, "Docs");
    let live = pattern(1, 9000);
    ntfs.add_file(docs, "a.bin", &live);
    let gone = pattern(2, 20_000);
    let placed = ntfs.add_file(docs, "deleted.bin", &gone);
    ntfs.delete(placed);
    let old = ntfs.mkdir(NTFS_ROOT, "Old");
    let inner = ntfs.add_file(old, "old.txt", b"old");
    ntfs.delete(inner);
    ntfs.delete(old);
    let temp = ntfs.mkdir(NTFS_ROOT, "Temp");
    let lost = ntfs.add_file(temp, "lost.txt", b"lost");
    ntfs.delete(lost);
    ntfs.delete(temp);
    ntfs.reuse(temp, NTFS_ROOT, "reused.txt", b"new");
    let ntfs = ntfs.finish();

    let start = 2048u32;
    let mut image = mbr_disk(
        start as usize * 512 + ntfs.len(),
        &[MbrEntry::new(0x07, start, (ntfs.len() / 512) as u32)],
    );
    image[start as usize * 512..][..ntfs.len()].copy_from_slice(&ntfs);
    let path = dir.join("ntfs.img");
    std::fs::write(&path, &image).unwrap();
    (path.to_string_lossy().into_owned(), live, gone)
}

#[test]
fn ntfs_partition_is_scanned_listed_and_copied() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("engine-ntfs");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (image, live, gone) = ntfs_image(&dir);
    let source = json!({"type": "image", "path": image, "sectorSize": 512});

    let mut engine = Engine::start();
    let opened: BrowseOpened = serde_json::from_value(
        engine
            .call(BROWSE_OPEN, json!({"jobId": "n1", "source": source}))
            .unwrap(),
    )
    .unwrap();
    let partition = &opened.partitions[0];
    assert_eq!(
        (
            partition.filesystem.as_str(),
            partition.label.as_deref(),
            partition.browsable,
            partition.needs_scan,
            partition.times_utc
        ),
        ("ntfs", Some("NTFSDATA"), true, true, true)
    );
    let list = |engine: &mut Engine, dir: Option<&str>| -> FileListPage {
        serde_json::from_value(
            engine
                .call(
                    LIST_FILES,
                    json!({"jobId": "n1", "partition": 1, "dir": dir, "offset": 0, "limit": 100}),
                )
                .unwrap(),
        )
        .unwrap()
    };
    let before = list(&mut engine, None);
    assert!(before.entries.iter().all(|entry| !entry.deleted));

    // 没在扫的分区：停止扫描什么也不做
    engine
        .call(CANCEL_SCAN, json!({"jobId": "n1", "partition": 1}))
        .unwrap();
    let (result, notifications) =
        engine.call_noting(SCAN_DELETED, json!({"jobId": "n1", "partition": 1}));
    let result: ScanResult = serde_json::from_value(result.unwrap()).unwrap();
    assert!(!result.cancelled);
    assert_eq!((result.deleted, result.orphans), (4, 1));
    let last = notifications
        .iter()
        .rfind(|(method, _)| method == PROGRESS)
        .unwrap();
    assert_eq!(last.1["stage"], "scanMft");
    assert_eq!(last.1["done"], last.1["total"]);

    // 扫完之后目录缓存已清掉，根目录带上已删除的项与「找不到原目录的文件」
    let root = list(&mut engine, None);
    let names: Vec<_> = root
        .entries
        .iter()
        .map(|entry| {
            (
                entry.name.as_str(),
                entry.deleted,
                entry.virtual_dir.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            ("$Orphans", false, Some("orphans")),
            ("Docs", false, None),
            ("Old", true, None),
            ("keep.txt", false, None),
            ("reused.txt", false, None),
        ]
    );
    assert!(
        root.entries
            .iter()
            .all(|entry| entry.record.is_some() || entry.virtual_dir.is_some())
    );
    let orphans = list(&mut engine, Some(&root.entries[0].file));
    assert_eq!(orphans.entries[0].name, "lost.txt");
    let docs = root.entries[1].file.clone();
    let inside = list(&mut engine, Some(&docs));
    let deleted = inside
        .entries
        .iter()
        .find(|entry| entry.name == "deleted.bin")
        .unwrap();
    assert_eq!((deleted.deleted, deleted.state.as_str()), (true, "intact"));
    assert!(deleted.position.is_some());

    engine.call(CANCEL, json!({"jobId": "n1"})).unwrap();
    engine.notifications_until(CANCELLED);
    assert!(engine.wait_exit().success());

    // 复制整个 Docs 目录：复制引擎先扫一遍 MFT，删除的文件一起复制出来
    let partition_ref =
        json!({"number": 1, "offset": partition.offset, "length": partition.length});
    let mut copier = Engine::start();
    copier
        .call(
            COPY_FILES,
            json!({"jobId": "copy-ntfs", "source": source, "outputDir": dir, "partition": partition_ref,
                   "items": [{"file": docs, "path": ["Docs"]}]}),
        )
        .unwrap();
    let notifications = copier.notifications_until(COMPLETED);
    assert!(
        notifications
            .iter()
            .any(|(method, params)| method == PROGRESS && params["stage"] == "scanMft")
    );
    let copied: Vec<CopiedFile> = notifications
        .iter()
        .filter(|(method, _)| method == COPIED)
        .flat_map(|(_, params)| {
            serde_json::from_value::<Vec<CopiedFile>>(params["files"].clone()).unwrap()
        })
        .collect();
    let mut paths: Vec<_> = copied
        .iter()
        .map(|file| (file.path.as_str(), file.size, file.all_zero))
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        [
            ("Docs/a.bin", 9000, false),
            ("Docs/deleted.bin", 20_000, false)
        ]
    );
    let output = dir.join("restored-copy-ntf");
    assert_eq!(
        std::fs::read(output.join("Docs").join("a.bin")).unwrap(),
        live
    );
    assert_eq!(
        std::fs::read(output.join("Docs").join("deleted.bin")).unwrap(),
        gone
    );
    assert!(copier.wait_exit().success());
}
