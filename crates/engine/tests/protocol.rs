//! 启动真实的引擎进程，按宿主的方式收发 JSON-RPC。

use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use yisi_engine::protocol::{
    CANCEL, CAPABILITIES, CARVE_START, COMPLETED, CarveSummary, FOUND, FilePreview, FoundFile,
    HEARTBEAT, LIST_FILE_TYPES, PREVIEW_FILE, PROGRESS, READ_SCAN, ReadScanReport, error_code,
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
        self.next_id += 1;
        let id = Id::Number(self.next_id);
        let request = Message::Request(Request::new(id.clone(), method, params));
        write_message(self.stdin.as_mut().unwrap(), &request).unwrap();
        loop {
            match self.incoming.recv_timeout(TIMEOUT).unwrap() {
                Message::Response(response) if response.id == id => return response.outcome,
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

    assert_eq!(Path::new(&summary.output_dir), output);
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
