//! 桌面宿主：管理引擎与代理进程，把引擎事件合并后转给界面。
// release 构建不弹出控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

mod broker;
mod engines;

use serde::Serialize;
use tauri::{AppHandle, State};
use yisi_engine::protocol::{Capabilities, DiskList};

use crate::broker::BrokerSession;
use crate::engines::{Engines, HostError};

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

#[tauri::command(async)]
fn job_cancel(engines: State<'_, Engines>, job_id: String) -> Result<(), HostError> {
    engines.cancel(&job_id)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tauri::Builder::default()
        .manage(Engines::default())
        .manage(BrokerSession::default())
        .invoke_handler(tauri::generate_handler![
            app_info,
            engine_capabilities,
            disk_list,
            job_start_image_scan,
            job_start_disk_scan,
            job_cancel
        ])
        .run(tauri::generate_context!())?;
    Ok(())
}
