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
    FS_CHECK, FileListPage, FilePreview, FoundFile, FsCheckReport, HEARTBEAT, IMAGE_CREATE,
    IMAGE_RESUME, IMAGING, ImageStarted, ImageSummary, JobImaging, LIST_FILE_TYPES, LIST_FILES,
    PAUSE, PREVIEW_FILE, PREVIEW_FS_FILE, PREVIEW_RANGE, PROGRESS, READ_SCAN, RESUME,
    ReadScanReport, SCAN_DELETED, ScanResult, error_code,
};
use yisi_formats::samples;
use yisi_io::{BadMap, SegmentState};
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

    /// 收 `duration` 这么久的通知（不含心跳）。
    fn notifications_for(&self, duration: Duration) -> Vec<(String, Value)> {
        let deadline = Instant::now() + duration;
        let mut seen = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.incoming.recv_timeout(left) {
                Ok(Message::Notification(notification)) if notification.method != HEARTBEAT => {
                    seen.push((notification.method, notification.params));
                }
                Ok(Message::Notification(_)) => {}
                Ok(other) => panic!("意外的消息：{other:?}"),
                Err(_) => break,
            }
        }
        seen
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
fn carve_scan_pauses_and_resumes() {
    let size = 128 * 1024 * 1024;
    let path = make_image("engine-carve-pause.img", size);
    let source = json!({"type": "image", "path": path, "sectorSize": 512});
    let started = Instant::now();
    let mut engine = Engine::start();

    let no_job = engine.call(PAUSE, json!({"jobId": "p1"})).unwrap_err();
    assert_eq!(no_job.code, error_code::JOB_NOT_FOUND);

    engine
        .call(CARVE_START, json!({"jobId": "p1", "source": source}))
        .unwrap();
    // 接下任务后马上暂停：128 MiB 来不及扫完
    let (paused, _) = engine.call_noting(PAUSE, json!({"jobId": "p1"}));
    paused.unwrap();
    // 闸门之前已经领走的那一段可能还会报一次进度，之后应当没有动静
    engine.notifications_for(Duration::from_millis(300));
    let quiet = engine.notifications_for(Duration::from_millis(1200));
    assert!(
        quiet
            .iter()
            .all(|(method, _)| method != PROGRESS && method != COMPLETED),
        "暂停后仍有进度或已完成：{quiet:?}"
    );

    engine.call(RESUME, json!({"jobId": "p1"})).unwrap();
    let notifications = engine.notifications_until(COMPLETED);
    let wall_ms = started.elapsed().as_millis() as u64;
    let (_, completed) = notifications.last().unwrap();
    let summary: CarveSummary = serde_json::from_value(completed["result"].clone()).unwrap();
    assert!(
        summary.elapsed_ms + 1000 < wall_ms,
        "用时 {} ms 应扣掉暂停的约 1.5 秒（总共 {wall_ms} ms）",
        summary.elapsed_ms
    );
    assert!(engine.wait_exit().success());
}

#[test]
fn only_carve_scan_can_be_paused() {
    // 浏览任务一直运行到取消，不会在暂停请求到达前结束
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("engine-pause-browse");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (image, _, _) = browse_image(&dir);
    let mut engine = Engine::start();
    engine
        .call(
            BROWSE_OPEN,
            json!({"jobId": "b2", "source": {"type": "image", "path": image, "sectorSize": 512}}),
        )
        .unwrap();
    let refused = engine.call(PAUSE, json!({"jobId": "b2"})).unwrap_err();
    assert_eq!(refused.code, rpc_error::INVALID_PARAMS);
    engine.call(CANCEL, json!({"jobId": "b2"})).unwrap();
    engine.notifications_until(CANCELLED);
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
    // 另存为：直接存进选的目录（还不存在时建出来），不另建子目录
    let output = dir.join("saved");
    let started = saver
        .call(
            CARVE_EXTRACT,
            json!({"jobId": "save-1-xyz", "source": source, "outputDir": output, "items": items}),
        )
        .unwrap();
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
        .map(|file| (file.path.as_str(), file.error.is_none(), file.offset))
        .collect();
    assert_eq!(
        outcome,
        [
            (found[0].name.as_str(), true, Some(found[0].offset)),
            (found[1].name.as_str(), true, Some(found[1].offset)),
            (found[2].name.as_str(), false, Some(found[2].offset)),
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

    // 再存一次：同一个文件按原名、按另存为起的名字各一份，重名时加序号，不覆盖已有文件
    let jpg = &found[0];
    std::fs::write(output.join("photo.jpg"), b"user's own file").unwrap();
    let mut again = Engine::start();
    again
        .call(
            CARVE_EXTRACT,
            json!({"jobId": "save-2", "source": source, "outputDir": output, "items": [
                {"offset": jpg.offset, "length": jpg.length, "extension": "jpg"},
                {"offset": jpg.offset, "length": jpg.length, "extension": "jpg", "name": "photo.jpg"},
                {"offset": jpg.offset, "length": jpg.length, "extension": "jpg", "name": "a/b:c.jpg"},
            ]}),
        )
        .unwrap();
    let notifications = again.notifications_until(COMPLETED);
    let paths: Vec<String> = notifications
        .iter()
        .filter(|(method, _)| method == COPIED)
        .flat_map(|(_, params)| {
            serde_json::from_value::<Vec<CopiedFile>>(params["files"].clone()).unwrap()
        })
        .map(|file| file.path)
        .collect();
    let renamed = jpg.name.replace(".jpg", " (1).jpg");
    assert_eq!(paths, [renamed.as_str(), "photo (1).jpg", "a_b_c.jpg"]);
    assert_eq!(
        std::fs::read(output.join("photo.jpg")).unwrap(),
        b"user's own file"
    );
    for name in &paths {
        assert!(
            std::fs::read(output.join(name)).unwrap() == originals[0].1,
            "{name}"
        );
    }
    let leftovers: Vec<_> = std::fs::read_dir(&output)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".yisi-saving-") || name.ends_with(".part"))
        .collect();
    assert!(leftovers.is_empty(), "暂存目录应已删掉：{leftovers:?}");
    assert!(again.wait_exit().success());
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

/// 内容确定、不重复的源镜像。
fn make_source(path: &Path, len: usize) -> Vec<u8> {
    let data: Vec<u8> = (0..len)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    std::fs::write(path, &data).unwrap();
    data
}

fn image_summary(notifications: &[(String, Value)]) -> ImageSummary {
    let (method, params) = notifications.last().unwrap();
    assert_eq!(method, COMPLETED);
    serde_json::from_value(params["result"].clone()).unwrap()
}

#[test]
fn image_create_copies_the_source_and_guards_existing_files() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("engine-image");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("source.img");
    let size = 3 * 1024 * 1024 + 1000;
    let data = make_source(&source_path, size);
    let source = json!({"type": "image", "path": source_path, "sectorSize": 512});
    // 上级目录还不存在：核对过源之后才建
    let image_path = dir.join("out").join("copy.img");
    let map_path = dir.join("out").join("copy.img.map");

    let mut engine = Engine::start();
    let started: ImageStarted = serde_json::from_value(
        engine
            .call(
                IMAGE_CREATE,
                json!({"jobId": "image-1", "source": source, "imagePath": image_path}),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(started.image_path, image_path.display().to_string());
    assert_eq!(started.map_path, map_path.display().to_string());
    let notifications = engine.notifications_until(COMPLETED);
    let imaging: Vec<JobImaging> = notifications
        .iter()
        .filter(|(method, _)| method == IMAGING)
        .map(|(_, params)| serde_json::from_value(params.clone()).unwrap())
        .collect();
    let last = imaging.last().unwrap();
    assert_eq!(
        (last.read_bytes, last.bad_bytes, last.untried_bytes),
        (size as u64, 0, 0)
    );
    let (_, progress) = notifications
        .iter()
        .rev()
        .find(|(method, _)| method == PROGRESS)
        .unwrap();
    assert_eq!(progress["stage"], "copy");
    assert_eq!(progress["done"], size as u64);
    let summary = image_summary(&notifications);
    assert_eq!(
        (
            summary.size,
            summary.read_bytes,
            summary.bad_bytes,
            summary.unfinished_bytes
        ),
        (size as u64, size as u64, 0, 0)
    );
    assert!(summary.bad_ranges.is_empty());
    assert_eq!(summary.source_lost_at, None);
    assert!(engine.wait_exit().success());
    assert_eq!(std::fs::read(&image_path).unwrap(), data);
    let map = BadMap::parse(&std::fs::read_to_string(&map_path).unwrap()).unwrap();
    assert_eq!(map.bytes(SegmentState::Done), size as u64);
    assert_eq!(map.segments().len(), 1);

    // 已经有镜像：新建被拒绝，续做立即完成、不再读
    let mut again = Engine::start();
    let exists = again
        .call(
            IMAGE_CREATE,
            json!({"jobId": "image-2", "source": source, "imagePath": image_path}),
        )
        .unwrap_err();
    assert_eq!(exists.code, error_code::IMAGE_EXISTS);
    again
        .call(
            IMAGE_RESUME,
            json!({"jobId": "image-3", "source": source, "imagePath": image_path}),
        )
        .unwrap();
    let resumed = image_summary(&again.notifications_until(COMPLETED));
    assert_eq!(resumed.read_bytes, size as u64);
    assert!(again.wait_exit().success());

    // 换了一个源：坏区图记录的来源与容量对不上
    let other_path = dir.join("other.img");
    make_source(&other_path, 4096);
    let mut other = Engine::start();
    let mismatch = other
        .call(
            IMAGE_RESUME,
            json!({"jobId": "image-4", "source": {"type": "image", "path": other_path, "sectorSize": 512}, "imagePath": image_path}),
        )
        .unwrap_err();
    assert_eq!(mismatch.code, error_code::IMAGE_MISMATCH);

    // 镜像不能写到来源文件上，也不接受相对路径
    let onto_source = other
        .call(
            IMAGE_CREATE,
            json!({"jobId": "image-5", "source": source, "imagePath": source_path}),
        )
        .unwrap_err();
    assert_eq!(onto_source.code, rpc_error::INVALID_PARAMS);
    let relative = other
        .call(
            IMAGE_CREATE,
            json!({"jobId": "image-6", "source": source, "imagePath": "copy.img"}),
        )
        .unwrap_err();
    assert_eq!(relative.code, rpc_error::INVALID_PARAMS);
    assert_eq!(std::fs::read(&source_path).unwrap(), data, "来源没有被改动");
    other.close_input();
    assert!(other.wait_exit().success());
}

#[test]
fn image_stopped_midway_resumes_to_identical_bytes() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("engine-image-resume");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("source.img");
    let size = 128 * 1024 * 1024 + 512;
    let data = make_source(&source_path, size);
    let source = json!({"type": "image", "path": source_path, "sectorSize": 512});
    let image_path = dir.join("copy.img");

    let mut engine = Engine::start();
    engine
        .call(
            IMAGE_CREATE,
            json!({"jobId": "stop-1", "source": source, "imagePath": image_path}),
        )
        .unwrap();
    // 接下任务后马上停：128 MiB 读不完
    engine.call(CANCEL, json!({"jobId": "stop-1"})).unwrap();
    let notifications = engine.notifications_until(CANCELLED);
    assert!(engine.wait_exit().success());
    let map = BadMap::parse(&std::fs::read_to_string(dir.join("copy.img.map")).unwrap()).unwrap();
    let untried = map.bytes(SegmentState::Untried);
    assert!(untried > 0, "停下时应还有没读的部分");
    assert!(
        notifications.iter().all(|(method, _)| method != COMPLETED),
        "已停下，不应完成"
    );

    let mut resumer = Engine::start();
    resumer
        .call(
            IMAGE_RESUME,
            json!({"jobId": "stop-2", "source": source, "imagePath": image_path}),
        )
        .unwrap();
    let summary = image_summary(&resumer.notifications_until(COMPLETED));
    assert_eq!(
        (summary.read_bytes, summary.unfinished_bytes),
        (size as u64, 0)
    );
    assert!(resumer.wait_exit().success());
    assert!(
        std::fs::read(&image_path).unwrap() == data,
        "续做后的镜像应与源逐字节一致"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 三个分区：FAT32 与 NTFS 的主引导扇区被清零（备份完好），exFAT 正常。返回镜像路径与 NTFS 里那个文件的内容。
fn damaged_boot_image(dir: &Path) -> (String, Vec<u8>) {
    use yisi_fs::samples::{ExfatImage, FatImage, FatType, Layout, NTFS_ROOT, NtfsImage, pattern};
    use yisi_part::samples::{MbrEntry, mbr_disk};

    let mut fat = FatImage::new(FatType::Fat32);
    let root = fat.root();
    fat.add_file(root, "fat.bin", &pattern(1, 3000), Layout::Contiguous);
    let mut fat = fat.finish();
    fat[..512].fill(0);
    let mut exfat = ExfatImage::new();
    let root = exfat.root();
    exfat.add_file(root, "exfat.bin", &pattern(2, 3000), Layout::Contiguous);
    let exfat = exfat.finish();
    let mut ntfs = NtfsImage::new();
    ntfs.set_label("LOST BOOT");
    let docs = ntfs.mkdir(NTFS_ROOT, "Docs");
    let content = pattern(3, 30_000);
    ntfs.add_file(docs, "report.bin", &content);
    let mut ntfs = ntfs.finish();
    ntfs[..512].fill(0);

    let mut entries = Vec::new();
    let mut start = 2048usize;
    let mut placed = Vec::new();
    for (type_id, volume) in [(0x0C, &fat), (0x07, &exfat), (0x07, &ntfs)] {
        entries.push(MbrEntry::new(
            type_id,
            start as u32,
            (volume.len() / 512) as u32,
        ));
        placed.push(start);
        start += volume.len() / 512 + 2048;
    }
    let mut image = mbr_disk(start * 512, &entries);
    for (volume, start) in [&fat, &exfat, &ntfs].into_iter().zip(placed) {
        image[start * 512..][..volume.len()].copy_from_slice(volume);
    }
    let path = dir.join("damaged-boot.img");
    std::fs::write(&path, &image).unwrap();
    (path.to_string_lossy().into_owned(), content)
}

#[test]
fn boot_sectors_are_diagnosed_and_backups_browsed_and_copied() {
    use yisi_engine::protocol::{BootDiagnosis, DIAGNOSE_BOOT};

    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("engine-boot");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (image, content) = damaged_boot_image(&dir);
    let source = json!({"type": "image", "path": image, "sectorSize": 512});

    // 诊断是一次性的查询，答复后引擎照常等下一个请求
    let mut engine = Engine::start();
    let diagnosis: BootDiagnosis = serde_json::from_value(
        engine
            .call(DIAGNOSE_BOOT, json!({"source": source}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(diagnosis.scheme, "mbr");
    let summary: Vec<_> = diagnosis
        .partitions
        .iter()
        .map(|partition| {
            (
                partition.number,
                partition.filesystem.as_str(),
                partition.verdict.as_str(),
                partition
                    .backup
                    .as_ref()
                    .is_some_and(|backup| backup.opened),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (1, "fat32", "primaryDamaged", true),
            (2, "exfat", "healthy", true),
            (3, "ntfs", "primaryDamaged", true),
        ]
    );
    let ntfs = &diagnosis.partitions[2];
    assert_eq!(ntfs.label.as_deref(), Some("LOST BOOT"));
    assert_eq!(ntfs.primary.issues[0].code, "blank");
    assert!(!ntfs.primary.opened);
    let backup = ntfs.backup.as_ref().unwrap();
    assert_eq!(backup.offset, ntfs.offset + ntfs.length - 512);
    assert!(backup.issues.is_empty());
    let missing = engine
        .call(
            DIAGNOSE_BOOT,
            json!({"source": {"type": "image", "path": "Z:/no/such.img", "sectorSize": 512}}),
        )
        .unwrap_err();
    assert_eq!(missing.code, error_code::SOURCE_UNAVAILABLE);
    engine.close_input();
    assert!(engine.wait_exit().success());

    // 用主引导扇区浏览时 NTFS 打不开；用备份浏览时三个分区都能浏览
    let mut primary = Engine::start();
    let opened: BrowseOpened = serde_json::from_value(
        primary
            .call(BROWSE_OPEN, json!({"jobId": "p1", "source": source}))
            .unwrap(),
    )
    .unwrap();
    let browsable: Vec<bool> = opened.partitions.iter().map(|p| p.browsable).collect();
    assert_eq!(browsable, [false, true, false]);
    primary.call(CANCEL, json!({"jobId": "p1"})).unwrap();
    primary.notifications_until(CANCELLED);
    assert!(primary.wait_exit().success());

    let mut engine = Engine::start();
    let opened: BrowseOpened = serde_json::from_value(
        engine
            .call(
                BROWSE_OPEN,
                json!({"jobId": "k1", "source": source, "boot": "backup"}),
            )
            .unwrap(),
    )
    .unwrap();
    let summary: Vec<_> = opened
        .partitions
        .iter()
        .map(|p| (p.filesystem.as_str(), p.browsable, p.label.as_deref()))
        .collect();
    assert_eq!(
        summary,
        [
            ("fat32", true, Some("BOOTLABEL")),
            ("exfat", true, None),
            ("ntfs", true, Some("LOST BOOT")),
        ]
    );
    let list = |engine: &mut Engine, dir: Option<&str>| -> FileListPage {
        serde_json::from_value(
            engine
                .call(
                    LIST_FILES,
                    json!({"jobId": "k1", "partition": 3, "dir": dir, "offset": 0, "limit": 100}),
                )
                .unwrap(),
        )
        .unwrap()
    };
    let root = list(&mut engine, None);
    assert_eq!(root.entries[0].name, "Docs");
    let docs = list(&mut engine, Some(&root.entries[0].file));
    let report = docs.entries[0].clone();
    assert_eq!(report.name, "report.bin");
    let preview: FilePreview = serde_json::from_value(
        engine
            .call(
                PREVIEW_FS_FILE,
                json!({"jobId": "k1", "partition": 3, "file": report.file}),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(preview.length, content.len() as u64);
    engine.call(CANCEL, json!({"jobId": "k1"})).unwrap();
    engine.notifications_until(CANCELLED);
    assert!(engine.wait_exit().success());

    // 复制同样按备份打开分区；不说用备份时打不开
    let partition = &opened.partitions[2];
    let partition_ref =
        json!({"number": 3, "offset": partition.offset, "length": partition.length});
    let items = json!([{"file": root.entries[0].file, "path": ["Docs"]}]);
    let mut refused = Engine::start();
    let error = refused
        .call(
            COPY_FILES,
            json!({"jobId": "copy-boot-0", "source": source, "outputDir": dir,
                   "partition": partition_ref, "items": items}),
        )
        .unwrap_err();
    assert_eq!(error.code, error_code::FS_UNAVAILABLE);
    refused.close_input();
    assert!(refused.wait_exit().success());

    let mut copier = Engine::start();
    copier
        .call(
            COPY_FILES,
            json!({"jobId": "copy-boot-1", "source": source, "outputDir": dir,
                   "partition": partition_ref, "items": items, "boot": "backup"}),
        )
        .unwrap();
    let notifications = copier.notifications_until(COMPLETED);
    let summary: CopySummary =
        serde_json::from_value(notifications.last().unwrap().1["result"].clone()).unwrap();
    assert_eq!((summary.files, summary.failed), (1, 0));
    let output = dir.join("restored-copy-boo");
    assert_eq!(
        std::fs::read(output.join("Docs").join("report.bin")).unwrap(),
        content
    );
    assert!(copier.wait_exit().success());
}

/// 一块 MBR 盘：1 号分区是干净的 FAT16，2 号分区是改坏了的 exFAT（一个没人用的簇在位图里是已分配、卷标记为未正常卸下）。
fn fs_check_image(dir: &Path) -> (String, Vec<(u32, u64, u64)>) {
    use yisi_fs::samples::{ExfatImage, FatImage, FatType, Layout, pattern};
    use yisi_part::samples::{MbrEntry, mbr_disk};

    let mut fat = FatImage::new(FatType::Fat16);
    let root = fat.root();
    let docs = fat.mkdir(root, "Documents").as_dir();
    fat.add_file(
        docs,
        "report final.docx",
        &pattern(1, 5000),
        Layout::Fragmented,
    );
    let fat = fat.finish();

    let mut exfat = ExfatImage::new();
    let root = exfat.root();
    exfat.add_file(root, "notes.txt", &pattern(2, 9000), Layout::Contiguous);
    let mut exfat = exfat.finish();
    let heap = u32::from_le_bytes(exfat[88..92].try_into().unwrap()) as usize * 512;
    let lost = 1500usize;
    exfat[heap + (lost - 2) / 8] |= 1 << ((lost - 2) % 8);
    exfat[106] |= 0x02;

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
    let path = dir.join("fs-check.img");
    std::fs::write(&path, &image).unwrap();
    let partitions = vec![
        (1, u64::from(fat_start) * 512, fat.len() as u64),
        (2, u64::from(exfat_start) * 512, exfat.len() as u64),
    ];
    (path.to_string_lossy().into_owned(), partitions)
}

fn fs_check_report(notifications: &[(String, Value)]) -> FsCheckReport {
    let (method, params) = notifications.last().unwrap();
    assert_eq!(method, COMPLETED, "{notifications:?}");
    serde_json::from_value(params["result"].clone()).unwrap()
}

#[test]
fn fs_check_reports_problems_by_stage() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("engine-fs-check");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (image, partitions) = fs_check_image(&dir);
    let source = json!({"type": "image", "path": image, "sectorSize": 512});

    let mut engine = Engine::start();
    let capabilities = engine.call(CAPABILITIES, Value::Null).unwrap();
    assert!(
        capabilities["methods"]
            .as_array()
            .unwrap()
            .contains(&Value::from(FS_CHECK))
    );
    // 分区的位置与分区表对不上：拒绝，不开始检查
    let (number, offset, length) = partitions[0];
    let moved = engine
        .call(
            FS_CHECK,
            json!({"jobId": "c0", "source": source,
                   "partition": {"number": number, "offset": offset + 512, "length": length}}),
        )
        .unwrap_err();
    assert_eq!(moved.code, error_code::FS_UNAVAILABLE);
    engine
        .call(
            FS_CHECK,
            json!({"jobId": "c1", "source": source,
                   "partition": {"number": number, "offset": offset, "length": length}}),
        )
        .unwrap();
    let notifications = engine.notifications_until(COMPLETED);
    let report = fs_check_report(&notifications);
    assert!(engine.wait_exit().success());
    assert_eq!(report.filesystem, "fat16");
    assert!(report.issues.is_empty(), "{:#?}", report.issues);
    assert_eq!((report.files, report.directories), (1, 1));
    assert!(report.checked.contains(&"lostClusters".to_owned()));
    let stages: Vec<&str> = notifications
        .iter()
        .filter(|(method, _)| method == PROGRESS)
        .filter_map(|(_, params)| params["stage"].as_str())
        .collect();
    for stage in ["boot", "fat", "directories", "clusters"] {
        assert!(stages.contains(&stage), "没有阶段 {stage}：{stages:?}");
    }

    let mut engine = Engine::start();
    let (number, offset, length) = partitions[1];
    engine
        .call(
            FS_CHECK,
            json!({"jobId": "c2", "source": source,
                   "partition": {"number": number, "offset": offset, "length": length}}),
        )
        .unwrap();
    let report = fs_check_report(&engine.notifications_until(COMPLETED));
    assert!(engine.wait_exit().success());
    assert_eq!(report.filesystem, "exfat");
    let kinds: Vec<(&str, &str)> = report
        .issues
        .iter()
        .map(|issue| (issue.kind.as_str(), issue.severity.as_str()))
        .collect();
    assert_eq!(
        kinds,
        vec![("volumeDirty", "minor"), ("lostClusters", "structure")]
    );
    assert_eq!(report.issues[1].cluster, Some(1500));
    assert_eq!((report.totals.structure, report.totals.minor), (1, 1));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 100 万个簇的 FAT32（512 字节一簇）：只写引导扇区与两份 FAT 的开头，其余是 0。读一遍 FAT 要好几秒，来得及暂停。
fn large_fat32(path: &Path) -> (u64, u64) {
    use std::io::{Seek, SeekFrom, Write};
    use yisi_part::samples::{MbrEntry, mbr_disk};

    let clusters: u64 = 1 << 20;
    let reserved = 32u64;
    let fat_sectors = ((clusters + 2) * 4).div_ceil(512);
    let total = reserved + 2 * fat_sectors + clusters;
    let start = 2048u64;
    let mut boot = [0u8; 512];
    boot[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    boot[3..11].copy_from_slice(b"MSDOS5.0");
    boot[11..13].copy_from_slice(&512u16.to_le_bytes());
    boot[13] = 1;
    boot[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
    boot[16] = 2;
    boot[21] = 0xF8;
    boot[32..36].copy_from_slice(&(total as u32).to_le_bytes());
    boot[36..40].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
    boot[44..48].copy_from_slice(&2u32.to_le_bytes());
    boot[510] = 0x55;
    boot[511] = 0xAA;
    let mut fat = Vec::new();
    for value in [0x0FFF_FFF8u32, 0x0FFF_FFFF, 0x0FFF_FFFF] {
        fat.extend_from_slice(&value.to_le_bytes());
    }
    let mbr = mbr_disk(512, &[MbrEntry::new(0x0C, start as u32, total as u32)]);
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(&mbr).unwrap();
    file.seek(SeekFrom::Start(start * 512)).unwrap();
    file.write_all(&boot).unwrap();
    for copy in 0..2 {
        file.seek(SeekFrom::Start(
            (start + reserved + copy * fat_sectors) * 512,
        ))
        .unwrap();
        file.write_all(&fat).unwrap();
    }
    file.set_len((start + total) * 512).unwrap();
    (start * 512, total * 512)
}

#[test]
fn fs_check_pauses_resumes_and_cancels() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("engine-fs-check-pause");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("large-fat32.img");
    let (offset, length) = large_fat32(&path);
    let source = json!({"type": "image", "path": path.to_string_lossy(), "sectorSize": 512});
    let mut engine = Engine::start();
    engine
        .call(
            FS_CHECK,
            json!({"jobId": "p1", "source": source,
                   "partition": {"number": 1, "offset": offset, "length": length}}),
        )
        .unwrap();
    engine.call(PAUSE, json!({"jobId": "p1"})).unwrap();
    // 闸门之前的那一段可能还会报一次进度，之后应当没有动静
    engine.notifications_for(Duration::from_millis(300));
    let quiet = engine.notifications_for(Duration::from_millis(1200));
    assert!(
        quiet
            .iter()
            .all(|(method, _)| method != PROGRESS && method != COMPLETED),
        "暂停后仍有进度或已完成：{quiet:?}"
    );
    engine.call(RESUME, json!({"jobId": "p1"})).unwrap();
    let resumed = engine.notifications_for(Duration::from_millis(500));
    assert!(
        resumed.iter().any(|(method, _)| method == PROGRESS),
        "继续后应有进度：{resumed:?}"
    );
    engine.call(CANCEL, json!({"jobId": "p1"})).unwrap();
    let notifications = engine.notifications_until(CANCELLED);
    assert!(notifications.iter().all(|(method, _)| method != COMPLETED));
    assert!(engine.wait_exit().success());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 丢失分区搜索的镜像：分区表清空，NTFS 的主引导扇区也清零（只剩末尾的备份）。
fn lost_partitions_image(dir: &Path) -> (String, yisi_repair::samples::SearchDisk) {
    let mut disk = yisi_repair::samples::search_disk();
    disk.clear_table();
    disk.wipe_primary_boot(disk.ntfs.0);
    let path = dir.join("lost.img");
    std::fs::write(&path, &disk.image).unwrap();
    (path.to_string_lossy().into_owned(), disk)
}

#[test]
fn partition_search_finds_lost_partitions_then_browses_and_copies_one() {
    use yisi_engine::protocol::{
        BootCopy, OPEN_RANGE, PartitionInfo, RANGE_PARTITION_BASE, SEARCH_PARTITIONS, SearchSummary,
    };

    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("engine-search");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (image, disk) = lost_partitions_image(&dir);
    let source = json!({"type": "image", "path": image, "sectorSize": 512});

    let mut searcher = Engine::start();
    assert_eq!(
        searcher
            .call(SEARCH_PARTITIONS, json!({"jobId": "s1", "source": source}))
            .unwrap(),
        json!({})
    );
    let notifications = searcher.notifications_until(COMPLETED);
    let progress = notifications
        .iter()
        .rfind(|(method, _)| method == PROGRESS)
        .unwrap();
    assert_eq!(progress.1["done"], progress.1["total"]);
    let summary: SearchSummary =
        serde_json::from_value(notifications.last().unwrap().1["result"].clone()).unwrap();
    assert!(searcher.wait_exit().success());

    assert_eq!(summary.scheme, "none");
    assert_eq!(summary.size, disk.image.len() as u64);
    let found: Vec<_> = summary
        .candidates
        .iter()
        .map(|candidate| {
            (
                candidate.offset,
                candidate.filesystem.as_str(),
                candidate.found_by.as_str(),
                candidate.relation.as_str(),
                candidate.overlaps_candidate,
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            (disk.fat32.0, "fat32", "both", "lost", false),
            (disk.ntfs.0, "ntfs", "backup", "lost", true),
            (disk.nested_fat12.0, "fat12", "primary", "lost", true),
            (disk.exfat.0, "exfat", "both", "lost", false),
        ]
    );
    let ntfs = &summary.candidates[1];
    assert_eq!(ntfs.length, disk.ntfs.1);
    assert_eq!(ntfs.boot, BootCopy::Backup);
    assert_eq!(ntfs.label.as_deref(), Some("NTFS SAMPLE"));
    assert_eq!(summary.candidates[0].label.as_deref(), Some("DATA32"));

    // 浏览：按位置打开候选分区，之后与表里的分区一样列目录、预览、扫描
    let mut engine = Engine::start();
    let opened: BrowseOpened = serde_json::from_value(
        engine
            .call(BROWSE_OPEN, json!({"jobId": "b1", "source": source}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(opened.scheme, "none");
    let range = json!({"jobId": "b1", "offset": ntfs.offset, "length": ntfs.length,
                       "boot": ntfs.boot});
    let info: PartitionInfo =
        serde_json::from_value(engine.call(OPEN_RANGE, range.clone()).unwrap()).unwrap();
    assert_eq!(info.number, RANGE_PARTITION_BASE);
    assert_eq!(
        (
            info.filesystem.as_str(),
            info.label.as_deref(),
            info.browsable,
            info.needs_scan,
            info.times_utc
        ),
        ("ntfs", Some("NTFS SAMPLE"), true, true, true)
    );
    assert_eq!((info.offset, info.length), (ntfs.offset, ntfs.length));
    // 同一位置再打开：同一个编号
    let again: PartitionInfo =
        serde_json::from_value(engine.call(OPEN_RANGE, range).unwrap()).unwrap();
    assert_eq!(again.number, RANGE_PARTITION_BASE);
    let fat32: PartitionInfo = serde_json::from_value(
        engine
            .call(
                OPEN_RANGE,
                json!({"jobId": "b1", "offset": disk.fat32.0, "length": disk.fat32.1}),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (fat32.number, fat32.filesystem.as_str()),
        (RANGE_PARTITION_BASE + 1, "fat32")
    );
    let outside = engine
        .call(
            OPEN_RANGE,
            json!({"jobId": "b1", "offset": disk.exfat.0, "length": disk.image.len()}),
        )
        .unwrap_err();
    assert_eq!(outside.code, rpc_error::INVALID_PARAMS);

    let root: FileListPage = serde_json::from_value(
        engine
            .call(
                LIST_FILES,
                json!({"jobId": "b1", "partition": RANGE_PARTITION_BASE, "offset": 0, "limit": 100}),
            )
            .unwrap(),
    )
    .unwrap();
    let names: Vec<&str> = root
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["inner.img", "report.bin"]);
    let report = &root.entries[1];
    let preview: FilePreview = serde_json::from_value(
        engine
            .call(
                PREVIEW_FS_FILE,
                json!({"jobId": "b1", "partition": RANGE_PARTITION_BASE, "file": report.file}),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(preview.length, disk.report.len() as u64);
    let scanned: ScanResult = serde_json::from_value(
        engine
            .call(
                SCAN_DELETED,
                json!({"jobId": "b1", "partition": RANGE_PARTITION_BASE}),
            )
            .unwrap(),
    )
    .unwrap();
    assert!(!scanned.cancelled);
    // 扫过 MFT 后数据位置按源上的偏移给出
    let root: FileListPage = serde_json::from_value(
        engine
            .call(
                LIST_FILES,
                json!({"jobId": "b1", "partition": RANGE_PARTITION_BASE, "offset": 0, "limit": 100}),
            )
            .unwrap(),
    )
    .unwrap();
    let position = root.entries[1].position.unwrap();
    assert!(position > disk.ntfs.0 && position < disk.ntfs.0 + disk.ntfs.1);
    engine.call(CANCEL, json!({"jobId": "b1"})).unwrap();
    engine.notifications_until(CANCELLED);
    assert!(engine.wait_exit().success());

    // 复制：按位置打开的分区不核对分区表；没标明时按表核对，表里没有就拒绝
    let partition = json!({"number": RANGE_PARTITION_BASE, "offset": ntfs.offset,
                           "length": ntfs.length});
    let items = json!([{"file": report.file, "path": ["report.bin"]}]);
    let mut refused = Engine::start();
    let error = refused
        .call(
            COPY_FILES,
            json!({"jobId": "copy-x", "source": source, "outputDir": dir,
                   "partition": {"number": RANGE_PARTITION_BASE, "offset": ntfs.offset, "length": ntfs.length},
                   "items": items}),
        )
        .unwrap_err();
    assert_eq!(error.code, error_code::FS_UNAVAILABLE);
    refused.close_input();
    assert!(refused.wait_exit().success());

    let mut copier = Engine::start();
    copier
        .call(
            COPY_FILES,
            json!({"jobId": "copy-lost", "source": source, "outputDir": dir,
                   "partition": partition, "boot": ntfs.boot, "items": items}),
        )
        .unwrap();
    let notifications = copier.notifications_until(COMPLETED);
    let summary: CopySummary =
        serde_json::from_value(notifications.last().unwrap().1["result"].clone()).unwrap();
    assert_eq!((summary.files, summary.failed), (1, 0));
    assert_eq!(
        std::fs::read(dir.join("restored-copy-los").join("report.bin")).unwrap(),
        disk.report
    );
    assert!(copier.wait_exit().success());
}

#[test]
fn partition_search_pauses_and_resumes() {
    use yisi_engine::protocol::{SEARCH_PARTITIONS, SearchSummary};

    // 1 GiB 的空白镜像（只设长度，不写内容）：暂停请求到达时还远没搜完
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("engine-search-pause.img");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(1 << 30)
        .unwrap();
    let source = json!({"type": "image", "path": path, "sectorSize": 512});
    let started = Instant::now();
    let mut engine = Engine::start();
    engine
        .call(SEARCH_PARTITIONS, json!({"jobId": "sp", "source": source}))
        .unwrap();
    engine.call(PAUSE, json!({"jobId": "sp"})).unwrap();
    engine.notifications_for(Duration::from_millis(300));
    let quiet = engine.notifications_for(Duration::from_millis(1200));
    assert!(
        quiet
            .iter()
            .all(|(method, _)| method != PROGRESS && method != COMPLETED),
        "暂停后仍有进度或已完成：{quiet:?}"
    );
    engine.call(RESUME, json!({"jobId": "sp"})).unwrap();
    let notifications = engine.notifications_until(COMPLETED);
    let wall_ms = started.elapsed().as_millis() as u64;
    let summary: SearchSummary =
        serde_json::from_value(notifications.last().unwrap().1["result"].clone()).unwrap();
    assert!(summary.candidates.is_empty());
    assert!(
        summary.elapsed_ms + 1000 < wall_ms,
        "用时 {} ms 应扣掉暂停的约 1.5 秒（总共 {wall_ms} ms）",
        summary.elapsed_ms
    );
    assert!(engine.wait_exit().success());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn image_inspect_partial_image_change_check_and_describe() {
    use yisi_engine::protocol::{
        IMAGE_DESCRIBE, IMAGE_INSPECT, ImageDescriptions, ImageInspection, ImageRegion,
    };

    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("engine-image-partial");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (source_path, _, _) = browse_image(&dir);
    let source = json!({"type": "image", "path": source_path, "sectorSize": 512});
    let image_path = dir.join("out").join("part.img");
    let inspect = |engine: &mut Engine| -> ImageInspection {
        serde_json::from_value(
            engine
                .call(
                    IMAGE_INSPECT,
                    json!({"source": source, "imagePath": image_path}),
                )
                .unwrap(),
        )
        .unwrap()
    };
    let region = |inspection: &ImageInspection, number: u32| -> ImageRegion {
        inspection
            .regions
            .iter()
            .find(|region| region.partition == Some(number))
            .cloned()
            .unwrap()
    };

    // 还没有镜像：列出两个分区，全部未读
    let mut engine = Engine::start();
    let first = inspect(&mut engine);
    assert_eq!(first.scheme.as_deref(), Some("mbr"));
    let filesystems: Vec<&str> = first
        .partitions
        .iter()
        .map(|partition| partition.filesystem.as_str())
        .collect();
    assert_eq!(filesystems, vec!["fat16", "exfat"]);
    assert!(first.existing.is_none());
    let second = region(&first, 2);
    assert_eq!(second.progress.untried_bytes, second.length);
    assert!(second.check.is_none());
    assert!(!first.table_areas.is_empty());
    let Some(output) = first.output.clone() else {
        // 查不到输出位置的属性（非 Windows）：只读一部分的部分测不了
        return;
    };
    if !output.sparse {
        return;
    }

    // 只做第 2 个分区
    let partition = json!([{"offset": second.offset, "length": second.length}]);
    let started: ImageStarted = serde_json::from_value(
        engine
            .call(
                IMAGE_CREATE,
                json!({"jobId": "part-1", "source": source, "imagePath": image_path, "ranges": partition}),
            )
            .unwrap(),
    )
    .unwrap();
    assert!(started.source_identity.starts_with("image "));
    let summary = image_summary(&engine.notifications_until(COMPLETED));
    assert!(summary.partial);
    assert_eq!(summary.unfinished_bytes, 0);
    assert!(summary.device_unfinished_bytes > 0);
    assert!(engine.wait_exit().success());
    // 分区表所在的开头 1 MiB 一起读了：镜像里认得出分区
    assert_eq!(first.table_areas[0].offset, 0);
    let table_end = first.table_areas[0].length as usize;
    assert_eq!(
        std::fs::read(&image_path).unwrap()[..table_end],
        std::fs::read(&source_path).unwrap()[..table_end]
    );

    // 再看：第 2 个分区已做完、抽查一致，第 1 个还没做；分区表与上次一样
    let mut engine = Engine::start();
    let after = inspect(&mut engine);
    let existing = after.existing.clone().unwrap();
    assert_eq!(existing.state, "ok");
    assert!(existing.sparse);
    assert!(existing.partitions_recorded);
    assert!(existing.partition_changes.is_empty());
    let done = region(&after, 2);
    assert_eq!(done.progress.read_bytes, done.length);
    let check = done.check.unwrap();
    assert!(check.checked >= 1);
    assert_eq!((check.mismatched, check.unreadable), (0, 0));
    assert_eq!(region(&after, 1).progress.read_bytes, 0);

    // 源上第 2 个分区被改过：抽查发现对不上（每段第一块取最前面读出的位置）
    let mut changed = std::fs::read(&source_path).unwrap();
    let at = done.offset as usize;
    changed[at..at + 512].fill(0xee);
    std::fs::write(&source_path, &changed).unwrap();
    let mismatched = inspect(&mut engine);
    assert!(region(&mismatched, 2).check.unwrap().mismatched >= 1);
    engine.close_input();
    assert!(engine.wait_exit().success());

    // 重做这个分区之后又一致
    let mut engine = Engine::start();
    engine
        .call(
            IMAGE_RESUME,
            json!({"jobId": "part-2", "source": source, "imagePath": image_path,
                   "ranges": partition, "reset": partition}),
        )
        .unwrap();
    image_summary(&engine.notifications_until(COMPLETED));
    assert!(engine.wait_exit().success());
    let mut engine = Engine::start();
    let redone = inspect(&mut engine);
    assert_eq!(region(&redone, 2).check.unwrap().mismatched, 0);

    // 以前做过的镜像：在的、不见了的、只剩镜像文件的
    let missing = dir.join("gone.img");
    let describe = |engine: &mut Engine| -> ImageDescriptions {
        serde_json::from_value(
            engine
                .call(IMAGE_DESCRIBE, json!({"imagePaths": [image_path, missing]}))
                .unwrap(),
        )
        .unwrap()
    };
    let described = describe(&mut engine);
    let states: Vec<&str> = described
        .images
        .iter()
        .map(|image| image.state.as_str())
        .collect();
    assert_eq!(states, vec!["ok", "missing"]);
    let ok = &described.images[0];
    assert_eq!(ok.size, Some(changed.len() as u64));
    assert!(ok.progress.unwrap().untried_bytes > 0);
    assert!(ok.saved_at_ms.is_some());
    std::fs::rename(
        dir.join("out").join("part.img.map"),
        dir.join("out").join("moved.map"),
    )
    .unwrap();
    assert_eq!(describe(&mut engine).images[0].state, "mapMissing");
    engine.close_input();
    assert!(engine.wait_exit().success());
    let _ = std::fs::remove_dir_all(&dir);
}
