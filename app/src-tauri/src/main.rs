//! 桌面宿主：管理引擎与代理进程，把引擎事件合并后转给界面。
// release 构建不弹出控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

use serde::Serialize;

#[derive(Serialize)]
struct AppInfo {
    version: String,
}

#[tauri::command]
fn app_info(app: tauri::AppHandle) -> AppInfo {
    AppInfo {
        version: app.package_info().version.to_string(),
    }
}

fn main() -> anyhow::Result<()> {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![app_info])
        .run(tauri::generate_context!())?;
    Ok(())
}
