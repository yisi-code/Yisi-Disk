//! 桌面宿主：管理引擎与代理进程，把引擎事件合并后转给界面。
// release 构建不弹出控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

mod broker;
mod engines;
mod shell;

use serde::Serialize;
use tauri::{AppHandle, State};
use yisi_engine::protocol::{
    BrowseOpened, Capabilities, CopyItem, DiskList, ExtractItem, FileListPage, FilePreview,
    FileTypeList, PartitionRef, ScanResult,
};

use crate::broker::BrokerSession;
use crate::engines::{Engines, HostError, Target, is_executable_type};

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
/// 不给输出目录时只扫描，之后用 `carve_preview_range` 预览、`job_start_extract` 保存选中的文件。
#[tauri::command(async)]
fn job_start_carve(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    job_id: String,
    target: Target,
    output_dir: Option<String>,
    extensions: Vec<String>,
) -> Result<(), HostError> {
    engines.start_carve(app, &broker, job_id, target, output_dir, extensions)
}

/// 预览只扫描、还没保存的文件：回到扫描任务的源上读。
#[tauri::command(async)]
fn carve_preview_range(
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    scan_job_id: String,
    offset: u64,
    length: u64,
) -> Result<FilePreview, HostError> {
    engines.preview_range(&broker, &scan_job_id, offset, length)
}

/// 保存只扫描时选中的文件（另存为）。不给输出目录时是为打开、复制而保存：存到程序目录下的 `open-cache`，
/// 同时在后台清掉以前的副本。
#[tauri::command(async)]
fn job_start_extract(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    job_id: String,
    scan_job_id: String,
    items: Vec<ExtractItem>,
    output_dir: Option<String>,
) -> Result<(), HostError> {
    engines.start_extract(app, &broker, job_id, &scan_job_id, items, output_dir)
}

#[tauri::command(async)]
fn job_cancel(engines: State<'_, Engines>, job_id: String) -> Result<(), HostError> {
    engines.cancel(&job_id)
}

/// 暂停或继续任务；目前只有深度恢复的扫描可以暂停。
#[tauri::command(async)]
fn job_set_paused(
    engines: State<'_, Engines>,
    job_id: String,
    paused: bool,
) -> Result<(), HostError> {
    engines.set_paused(&job_id, paused)
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
    // 另存为时可以改名，改成可执行类型的不交给系统打开
    if is_executable_type(&path) {
        return Err(HostError::new("executableBlocked", name));
    }
    tauri_plugin_opener::open_path(&path, None::<&str>)
        .map_err(|error| HostError::new("openFailed", error.to_string()))
}

/// 用系统的「打开方式」对话框选程序打开恢复出的文件。可执行与脚本类文件同样不打开。
#[tauri::command(async)]
fn recovered_open_with(
    engines: State<'_, Engines>,
    job_id: String,
    name: String,
) -> Result<(), HostError> {
    let path = engines.recovered_file(&job_id, &name)?;
    if is_executable_type(&path) {
        return Err(HostError::new("executableBlocked", name));
    }
    shell::open_with(&path)
}

/// 把恢复出的文件放到剪贴板，之后可以在资源管理器里粘贴。
#[tauri::command(async)]
fn recovered_copy(
    engines: State<'_, Engines>,
    job_id: String,
    name: String,
) -> Result<(), HostError> {
    let path = engines.recovered_file(&job_id, &name)?;
    shell::copy_to_clipboard(&path)
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

/// 浏览分区（找回删除的文件）：答复分区表，之后引擎一直运行，直到界面用 `job_cancel` 关闭浏览。
#[tauri::command(async)]
fn browse_open(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    job_id: String,
    target: Target,
) -> Result<BrowseOpened, HostError> {
    engines.open_browse(app, &broker, job_id, target)
}

#[tauri::command(async)]
fn browse_list(
    engines: State<'_, Engines>,
    job_id: String,
    partition: u32,
    dir: Option<String>,
    offset: u32,
    limit: u32,
) -> Result<FileListPage, HostError> {
    engines.list_files(job_id, partition, dir, offset, limit)
}

#[tauri::command(async)]
fn browse_preview(
    engines: State<'_, Engines>,
    job_id: String,
    partition: u32,
    file: String,
) -> Result<FilePreview, HostError> {
    engines.preview_fs_file(job_id, partition, file)
}

/// 找出分区里已删除的项（NTFS 扫一遍 MFT）：扫完才返回，进度经任务事件送达。
#[tauri::command(async)]
fn browse_scan(
    engines: State<'_, Engines>,
    job_id: String,
    partition: u32,
) -> Result<ScanResult, HostError> {
    engines.scan_deleted(job_id, partition)
}

#[tauri::command(async)]
fn browse_cancel_scan(
    engines: State<'_, Engines>,
    job_id: String,
    partition: u32,
) -> Result<(), HostError> {
    engines.cancel_scan(job_id, partition)
}

/// 把浏览中选中的项复制到另一块盘（另存为）。不给输出目录时是为打开而复制：存到程序目录下的 `open-cache`，
/// 同时在后台清掉以前的副本。
#[tauri::command(async)]
#[expect(clippy::too_many_arguments, reason = "与界面传来的参数一一对应")]
fn job_start_copy(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    job_id: String,
    target: Target,
    partition: PartitionRef,
    items: Vec<CopyItem>,
    output_dir: Option<String>,
) -> Result<(), HostError> {
    engines.start_copy(app, &broker, job_id, target, partition, items, output_dir)
}

/// 用系统默认程序打开复制出的文件。可执行与脚本类文件不打开（`executableBlocked`），只能在文件夹中显示。
#[tauri::command(async)]
fn copied_open(engines: State<'_, Engines>, job_id: String, path: String) -> Result<(), HostError> {
    let file = engines.copied_file(&job_id, &path)?;
    if is_executable_type(&file) {
        return Err(HostError::new("executableBlocked", path));
    }
    tauri_plugin_opener::open_path(&file, None::<&str>)
        .map_err(|error| HostError::new("openFailed", error.to_string()))
}

#[tauri::command(async)]
fn copied_reveal(
    engines: State<'_, Engines>,
    job_id: String,
    path: String,
) -> Result<(), HostError> {
    let file = engines.copied_file(&job_id, &path)?;
    tauri_plugin_opener::reveal_item_in_dir(&file)
        .map_err(|error| HostError::new("openFailed", error.to_string()))
}

/// 制作能识别坏区的镜像；`resume` 时按同一路径的坏区图续做。物理磁盘与读取扫描一样，第一次需要时提权启动代理。
#[tauri::command(async)]
fn job_start_image(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    job_id: String,
    target: Target,
    image_path: String,
    resume: bool,
) -> Result<(), HostError> {
    engines.start_image(app, &broker, job_id, target, image_path, resume)
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
            job_set_paused,
            recovered_preview,
            recovered_open,
            recovered_open_with,
            recovered_copy,
            recovered_reveal,
            carve_preview_range,
            job_start_extract,
            browse_open,
            browse_list,
            browse_preview,
            browse_scan,
            browse_cancel_scan,
            job_start_copy,
            copied_open,
            copied_reveal,
            job_start_image
        ])
        .run(tauri::generate_context!())?;
    Ok(())
}
