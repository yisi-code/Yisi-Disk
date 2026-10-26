//! 桌面宿主：管理引擎与代理进程，把引擎事件合并后转给界面。
// release 构建不弹出控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

mod broker;
mod engines;

use serde::Serialize;
use tauri::{AppHandle, State};
use yisi_engine::protocol::{Capabilities, DiskList, FilePreview, FileTypeList};

use crate::broker::BrokerSession;
use crate::engines::{Engines, HostError, Target};

#[derive(Serialize)]
struct AppInfo {
    version: String,
}

#[tauri::command]
fn app_info(app: AppHandle) -> AppInfo {
    AppInfo {
        version: app.package_info().version.to_string(),
    }
}

// 下面几个命令会等待引擎答复，标成 async 让它们在独立线程上运行，不阻塞界面
#[tauri::command(async)]
fn engine_capabilities(engines: State<'_, Engines>) -> Result<Capabilities, HostError> {
    engines.capabilities()
}

#[tauri::command(async)]
fn disk_list(engines: State<'_, Engines>) -> Result<DiskList, HostError> {
    engines.list_disks()
}

#[tauri::command(async)]
fn carve_file_types(engines: State<'_, Engines>) -> Result<FileTypeList, HostError> {
    engines.file_types()
}

#[tauri::command(async)]
fn job_start_image_scan(
    app: AppHandle,
    engines: State<'_, Engines>,
    job_id: String,
    path: String,
) -> Result<(), HostError> {
    engines.start_image_scan(app, job_id, path)
}

/// 第一次扫描物理磁盘时会弹出 UAC，命令一直等到用户作答、代理打开磁盘为止。
#[tauri::command(async)]
fn job_start_disk_scan(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    job_id: String,
    disk_number: u32,
) -> Result<(), HostError> {
    engines.start_disk_scan(app, &broker, job_id, disk_number)
}

/// 深度恢复。源为物理磁盘时与读取扫描一样，第一次需要时提权启动磁盘代理。
#[tauri::command(async)]
fn job_start_carve(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    job_id: String,
    target: Target,
    output_dir: String,
    extensions: Vec<String>,
) -> Result<(), HostError> {
    engines.start_carve(app, &broker, job_id, target, output_dir, extensions)
}

#[tauri::command(async)]
fn job_cancel(engines: State<'_, Engines>, job_id: String) -> Result<(), HostError> {
    engines.cancel(&job_id)
}

/// 预览深度恢复找到的文件：只认任务 ID 加恢复时起的文件名，由引擎读取与解码。
#[tauri::command(async)]
fn recovered_preview(
    engines: State<'_, Engines>,
    job_id: String,
    name: String,
) -> Result<FilePreview, HostError> {
    engines.preview(&job_id, &name)
}

/// 用系统默认程序打开恢复出的文件。
#[tauri::command(async)]
fn recovered_open(
    engines: State<'_, Engines>,
    job_id: String,
    name: String,
) -> Result<(), HostError> {
    let path = engines.recovered_file(&job_id, &name)?;
    tauri_plugin_opener::open_path(&path, None::<&str>)
        .map_err(|error| HostError::new("openFailed", error.to_string()))
}

/// 在系统的文件管理器里显示恢复出的文件。
#[tauri::command(async)]
fn recovered_reveal(
    engines: State<'_, Engines>,
    job_id: String,
    name: String,
) -> Result<(), HostError> {
    let path = engines.recovered_file(&job_id, &name)?;
    tauri_plugin_opener::reveal_item_in_dir(&path)
        .map_err(|error| HostError::new("openFailed", error.to_string()))
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Engines::default())
        .manage(BrokerSession::default())
        .invoke_handler(tauri::generate_handler![
            app_info,
            engine_capabilities,
            disk_list,
            carve_file_types,
            job_start_image_scan,
            job_start_disk_scan,
            job_start_carve,
            job_cancel,
            recovered_preview,
            recovered_open,
            recovered_reveal
        ])
        .run(tauri::generate_context!())?;
    Ok(())
}
