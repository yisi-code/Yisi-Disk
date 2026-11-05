//! 桌面宿主：管理引擎与代理进程，把引擎事件合并后转给界面。
// release 构建不弹出控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

mod broker;
mod carve_history;
mod engines;
mod image_history;
mod record_file;
mod shell;
mod write_plans;

use std::sync::Arc;

use serde::Serialize;
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, State, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use yisi_engine::protocol::PartitionInfo;
use yisi_engine::protocol::{BootCopy, BootDiagnosis, BootFix, ImageDescription, ImageInspection};
use yisi_engine::protocol::{
    BrowseOpened, Capabilities, CopyItem, DiskList, ExtractItem, FileListPage, FilePreview,
    FileTypeList, FoundFile, PartitionRef, ScanResult,
};
use yisi_engine::protocol::{ClusterRun, FatRating, FsFix, MftFix, PartitionTableFix, SearchMode};

use crate::broker::BrokerSession;
use crate::carve_history::{CarveHistory, CarveRecord};
use crate::engines::{
    Engines, HostError, ImagePlan, Target, refuse_executable, save_fs_check_report,
};
use crate::image_history::{ImageHistory, ImageRecord};
use crate::write_plans::{PlanView, WriteHistory, WritePlans, WriteRecord};

/// 程序文件夹里建不了 `data` 时的提示（界面还没加载，中英文一起给出）。
const DATA_DIR_UNWRITABLE: &str = "程序要在自己所在的文件夹里建 data 文件夹，存放做过的镜像、写盘记录与备份、界面缓存，但建不出来。\
请把整个程序文件夹放到可以写入的位置（不要放在要恢复数据的那块盘上），再重新打开。\n\n\
The program keeps its data in a data folder next to it, but cannot create it. Move the whole program folder \
to a writable location (not on the disk you are recovering from) and open it again.";

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

/// 读取扫描（坏道检测）。第一次读物理磁盘或卷时会弹出 UAC，命令一直等到用户作答、代理打开设备为止。
#[tauri::command(async)]
fn job_start_read_scan(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    job_id: String,
    target: Target,
) -> Result<(), HostError> {
    engines.start_read_scan(app, &broker, job_id, target)
}

/// 深度恢复。源为物理磁盘时与读取扫描一样，第一次需要时提权启动磁盘代理。
/// 不给输出目录时只扫描，之后用 `carve_preview_range` 预览、`job_start_extract` 保存选中的文件。
/// 每次扫描记进扫描记录（`source_label` 是界面给的来源描述），停下后可以接着扫、关掉程序后还能接着挑选保存。
#[expect(clippy::too_many_arguments, reason = "与界面命令的参数一一对应")]
#[tauri::command(async)]
fn job_start_carve(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    history: State<'_, Arc<CarveHistory>>,
    job_id: String,
    target: Target,
    output_dir: Option<String>,
    extensions: Vec<String>,
    source_label: Option<String>,
) -> Result<(), HostError> {
    engines.start_carve(
        app,
        &broker,
        &history,
        job_id,
        target,
        output_dir,
        extensions,
        source_label.unwrap_or_default(),
    )
}

/// 接着扫一次停下的扫描（扫描记录 `record_id`），从最后一个检查点接着扫。
#[tauri::command(async)]
fn job_resume_carve(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    history: State<'_, Arc<CarveHistory>>,
    job_id: String,
    record_id: String,
) -> Result<(), HostError> {
    engines.resume_carve(app, &broker, &history, job_id, &record_id)
}

/// 扫描记录，最近开始的在前。
#[tauri::command(async)]
fn carve_history_list(history: State<'_, Arc<CarveHistory>>) -> Vec<CarveRecord> {
    history.records()
}

/// 打开一条扫描记录（之后按记录 ID 预览、另存、打开）：记录与找到的文件。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CarveRecordOpened {
    record: CarveRecord,
    files: Vec<FoundFile>,
}

#[tauri::command(async)]
fn carve_history_open(
    engines: State<'_, Engines>,
    history: State<'_, Arc<CarveHistory>>,
    record_id: String,
) -> Result<CarveRecordOpened, HostError> {
    let (record, files) = engines.open_carve_record(&history, &record_id)?;
    Ok(CarveRecordOpened { record, files })
}

/// 从扫描记录里移除（只删记录与找到的文件清单，不碰恢复出的文件）。
#[tauri::command(async)]
fn carve_history_remove(
    history: State<'_, Arc<CarveHistory>>,
    record_id: String,
) -> Result<(), HostError> {
    history.remove(&record_id)
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

/// 保存只扫描时选中的文件（另存为）。不给输出目录时是为打开、复制而保存：存到程序文件夹下 `data` 里的 `open-cache`，
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

/// 暂停或继续任务；目前深度恢复的扫描、丢失分区搜索与文件系统检查可以暂停。
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
    refuse_executable(&path, &name)?;
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
    refuse_executable(&path, &name)?;
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
/// `boot` 为 `backup` 时各分区按备份引导扇区打开（修复打不开的分区）；不给时用主引导扇区。
#[tauri::command(async)]
fn browse_open(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    job_id: String,
    target: Target,
    boot: Option<BootCopy>,
) -> Result<BrowseOpened, HostError> {
    engines.open_browse(app, &broker, job_id, target, boot.unwrap_or_default())
}

/// 诊断各分区的引导扇区（修复打不开的分区）：只读，答复后引擎退出。物理盘第一次需要时弹出 UAC。
#[tauri::command(async)]
fn boot_diagnose(
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    target: Target,
) -> Result<BootDiagnosis, HostError> {
    engines.diagnose_boot(&broker, target)
}

/// 生成修复引导扇区的写入计划：引擎重新诊断并预演，宿主存好备份后交给界面确认。只读，不写盘。
/// 镜像文件与整块物理盘可以写（Windows 所在的盘不行），卷不行。
#[tauri::command(async)]
fn write_plan_boot_sector(
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    plans: State<'_, WritePlans>,
    target: Target,
    partition: u32,
    fix: BootFix,
) -> Result<PlanView, HostError> {
    plans.plan_boot_sector(&engines, &broker, target, partition, fix)
}

/// 生成改写分区表的写入计划（把找到的丢失分区写回分区表、把类型不符的分区改成对的类型），存好备份后交给界面确认。
/// 只读，不写盘；可写的源与修复引导扇区相同。
#[tauri::command(async)]
fn write_plan_partition_table(
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    plans: State<'_, WritePlans>,
    target: Target,
    fix: PartitionTableFix,
) -> Result<PlanView, HostError> {
    plans.plan_partition_table(&engines, &broker, target, fix)
}

/// 生成 NTFS `$MFT` 与 `$MFTMirr` 互相修复的写入计划（分区与检查文件系统时相同），存好备份后交给界面确认。只读，不写盘。
#[tauri::command(async)]
fn write_plan_mft(
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    plans: State<'_, WritePlans>,
    target: Target,
    partition: PartitionRef,
    fix: MftFix,
) -> Result<PlanView, HostError> {
    plans.plan_mft(&engines, &broker, target, partition, fix)
}

/// 生成文件系统修复（FAT、exFAT，按勾选的几类问题）的写入计划，存好备份后交给界面确认。只读，不写盘。
#[tauri::command(async)]
fn write_plan_fs_fix(
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    plans: State<'_, WritePlans>,
    target: Target,
    partition: PartitionRef,
    fixes: Vec<FsFix>,
    bad_clusters: Vec<ClusterRun>,
) -> Result<PlanView, HostError> {
    plans.plan_fs_fix(&engines, &broker, target, partition, fixes, bad_clusters)
}

/// 两份 FAT 不一致时，以第一份、以第二份为准各自修完还剩多少问题。只读。
#[tauri::command(async)]
fn fs_rate_fat_copies(
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    target: Target,
    partition: PartitionRef,
) -> Result<Vec<FatRating>, HostError> {
    engines.rate_fat_copies(&broker, target, partition)
}

/// 把分区里的空闲簇读一遍，找出读不出的（任务，只读），之后可以标成坏簇。
#[tauri::command(async)]
fn job_start_scan_free(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    job_id: String,
    target: Target,
    partition: PartitionRef,
) -> Result<(), HostError> {
    engines.start_scan_free(app, &broker, job_id, target, partition)
}

/// 把分区里的丢失簇另存到另一块盘上（任务，只读源）。源为物理磁盘时输出目录不能在这块盘上。
#[tauri::command(async)]
fn job_start_save_lost(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    job_id: String,
    target: Target,
    partition: PartitionRef,
    output_dir: String,
) -> Result<(), HostError> {
    engines.start_save_lost(app, &broker, job_id, target, partition, output_dir)
}

/// 执行界面已确认的计划（只接受计划 ID，写的内容是宿主存着的那份），结果记进写盘记录。物理盘交给磁盘代理。
#[tauri::command(async)]
fn write_plan_execute(
    broker: State<'_, BrokerSession>,
    plans: State<'_, WritePlans>,
    plan_id: String,
) -> Result<WriteRecord, HostError> {
    plans.execute(&broker, &plan_id)
}

/// 放弃还没执行的计划，删掉它的备份。
#[tauri::command(async)]
fn write_plan_discard(plans: State<'_, WritePlans>, plan_id: String) -> Result<(), HostError> {
    plans.discard(&plan_id)
}

/// 为写盘记录里的一次写入生成回滚计划，交给界面确认。`target` 是现在查看的源，必须就是当时写的那块盘。
#[tauri::command(async)]
fn write_plan_rollback(
    plans: State<'_, WritePlans>,
    plan_id: String,
    target: Target,
) -> Result<PlanView, HostError> {
    plans.rollback(&plan_id, &target)
}

/// 这个源（现在这块盘、这个镜像文件）上的写盘记录，最新的在前；按认盘标识认，不按编号认。
#[tauri::command(async)]
fn write_history_list(plans: State<'_, WritePlans>, target: Target) -> WriteHistory {
    plans.history(&target)
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

/// 搜索丢失的分区（只读，可以暂停；快速或深度）。源为物理磁盘时与读取扫描一样，第一次需要时提权启动磁盘代理。
#[tauri::command(async)]
fn job_start_partition_search(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    job_id: String,
    target: Target,
    mode: SearchMode,
) -> Result<(), HostError> {
    engines.start_partition_search(app, &broker, job_id, target, mode)
}

/// 在浏览会话里按位置打开搜索找到的分区；答复的编号之后用于 `browse_list` 等命令。
/// `boot` 为 `backup` 时按备份引导扇区打开（只靠备份找到的分区）；不给时用主引导扇区。
#[tauri::command(async)]
fn browse_open_range(
    engines: State<'_, Engines>,
    job_id: String,
    offset: u64,
    length: u64,
    boot: Option<BootCopy>,
) -> Result<PartitionInfo, HostError> {
    engines.open_range(job_id, offset, length, boot.unwrap_or_default())
}

/// 把浏览中选中的项复制到另一块盘（另存为）。不给输出目录时是为打开而复制：存到程序文件夹下 `data` 里的 `open-cache`，
/// 同时在后台清掉以前的副本。`boot` 与浏览时一致。
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
    boot: Option<BootCopy>,
) -> Result<(), HostError> {
    engines.start_copy(
        app,
        &broker,
        job_id,
        target,
        partition,
        items,
        output_dir,
        boot.unwrap_or_default(),
    )
}

/// 用系统默认程序打开复制出的文件。可执行与脚本类文件不打开（`executableBlocked`），只能在文件夹中显示。
#[tauri::command(async)]
fn copied_open(engines: State<'_, Engines>, job_id: String, path: String) -> Result<(), HostError> {
    let file = engines.copied_file(&job_id, &path)?;
    refuse_executable(&file, &path)?;
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

/// 制作能识别坏区的镜像；`resume` 时按同一路径的坏区图续做，`plan` 指定只读哪些范围、只做复制这一轮、先重做哪些范围。
/// 物理磁盘与读取扫描一样，第一次需要时提权启动代理。开始后把镜像记进做过的镜像列表（`source_label` 是界面给的来源描述）。
#[expect(clippy::too_many_arguments, reason = "与界面命令的参数一一对应")]
#[tauri::command(async)]
fn job_start_image(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    history: State<'_, ImageHistory>,
    job_id: String,
    target: Target,
    image_path: String,
    resume: bool,
    plan: Option<ImagePlan>,
    source_label: Option<String>,
) -> Result<(), HostError> {
    let started = engines.start_image(
        app,
        &broker,
        job_id,
        target.clone(),
        image_path,
        resume,
        plan.unwrap_or_default(),
    )?;
    let recorded = history.record(ImageRecord {
        image_path: started.image_path,
        target,
        source_identity: started.source_identity,
        source_label: source_label.unwrap_or_default(),
        size: started.size,
        used_at_ms: 0,
    });
    // 记不下来不影响做镜像，下次只是不能从列表里选
    if let Err(error) = recorded {
        tracing::warn!(?error, "没能记下做过的镜像");
    }
    Ok(())
}

/// 做镜像之前看一眼源与镜像（只读）：源上的分区、输出位置、已有镜像能不能续做、源变没变。物理磁盘第一次需要时弹出 UAC。
#[tauri::command(async)]
fn image_inspect(
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    target: Target,
    image_path: String,
) -> Result<ImageInspection, HostError> {
    engines.inspect_image(&broker, target, image_path)
}

/// 做过的镜像，各带现在的状态（还在不在、做到哪了）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImageHistoryItem {
    record: ImageRecord,
    status: ImageDescription,
}

#[tauri::command(async)]
fn image_history_list(
    engines: State<'_, Engines>,
    history: State<'_, ImageHistory>,
) -> Result<Vec<ImageHistoryItem>, HostError> {
    let records = history.records();
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let paths = records
        .iter()
        .map(|record| record.image_path.clone())
        .collect();
    let described = engines.describe_images(paths)?;
    Ok(records
        .into_iter()
        .zip(described.images)
        .map(|(record, status)| ImageHistoryItem { record, status })
        .collect())
}

/// 从做过的镜像列表移除（不删除文件）。
#[tauri::command(async)]
fn image_history_remove(
    history: State<'_, ImageHistory>,
    image_path: String,
) -> Result<(), HostError> {
    history.remove(&image_path)
}

/// 在文件夹中显示一个做过的镜像：只认做过的镜像列表里记着的路径，不接受别的。
#[tauri::command(async)]
fn image_reveal(history: State<'_, ImageHistory>, image_path: String) -> Result<(), HostError> {
    let record = history
        .find(&image_path)
        .ok_or_else(|| HostError::new("imageRecordNotFound", image_path.clone()))?;
    tauri_plugin_opener::reveal_item_in_dir(std::path::Path::new(&record.image_path))
        .map_err(|error| HostError::new("openFailed", error.to_string()))
}

/// 镜像被移走了：用户重新指定的位置上要有同名的坏区图，而且记的来源与容量与原来一致，才改记录。
#[tauri::command(async)]
fn image_history_relocate(
    engines: State<'_, Engines>,
    history: State<'_, ImageHistory>,
    image_path: String,
    new_path: String,
) -> Result<ImageHistoryItem, HostError> {
    let record = history
        .find(&image_path)
        .ok_or_else(|| HostError::new("imageRecordNotFound", image_path.clone()))?;
    let status = engines
        .describe_images(vec![new_path.clone()])?
        .images
        .into_iter()
        .next()
        .ok_or_else(|| HostError::new("engineProtocol", "没有答复"))?;
    if status.state != "ok" {
        return Err(HostError::new("imageRelocateInvalid", status.state));
    }
    if status.source_identity.as_deref() != Some(record.source_identity.as_str())
        || status.size != Some(record.size)
    {
        return Err(HostError::new(
            "imageRelocateMismatch",
            status.source_identity.unwrap_or_default(),
        ));
    }
    let record = history.relocate(&image_path, &new_path)?;
    Ok(ImageHistoryItem { record, status })
}

/// 检查文件系统（只读）。物理磁盘与读取扫描一样，第一次需要时提权启动代理。
#[tauri::command(async)]
fn job_start_fs_check(
    app: AppHandle,
    engines: State<'_, Engines>,
    broker: State<'_, BrokerSession>,
    job_id: String,
    target: Target,
    partition: PartitionRef,
) -> Result<(), HostError> {
    engines.start_fs_check(app, &broker, job_id, target, partition)
}

/// 把检查出的问题清单存成文本文件（界面已按当前语言生成内容，路径来自另存为对话框）。
/// 检查的是物理磁盘时，不能存到这块盘上（`outputOnSource`）。
#[tauri::command(async)]
fn fs_check_save_report(target: Target, path: String, text: String) -> Result<(), HostError> {
    save_fs_check_report(&target, &path, &text)
}

/// 界面选中物理磁盘或卷时先提权启动磁盘代理（弹出 UAC），开始任务时不用再等；已经启动过就直接返回。
#[tauri::command(async)]
fn broker_prepare(broker: State<'_, BrokerSession>) -> Result<(), HostError> {
    broker.prepare()
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Engines::default())
        .manage(BrokerSession::default())
        .setup(|app| {
            // 程序写的东西只放在程序文件夹下的 data 里，界面（WebView2）的缓存也是；建不出来就提示后退出，不换地方
            let webview_dir = engines::data_dir().and_then(|dir| {
                let webview = dir.join("webview");
                std::fs::create_dir_all(&webview)?;
                Ok((dir, webview))
            });
            let (data_dir, webview_dir) = match webview_dir {
                Ok(dirs) => dirs,
                Err(error) => {
                    tracing::error!(%error, "程序文件夹里建不了 data");
                    let handle = app.handle().clone();
                    app.dialog()
                        .message(format!("{DATA_DIR_UNWRITABLE}\n\n{error}"))
                        .kind(MessageDialogKind::Error)
                        .title("yisi-磁盘工具")
                        .show(move |_| handle.exit(1));
                    return Ok(());
                }
            };
            app.manage(ImageHistory::new(data_dir.join("image-history.json")));
            app.manage(Arc::new(CarveHistory::new(&data_dir)));
            app.manage(WritePlans::new(data_dir));
            // 主窗口不由配置自动创建（tauri.conf.json 里 create 为 false），在这里按配置建，数据目录指到 data 下
            let window = app
                .config()
                .app
                .windows
                .first()
                .cloned()
                .ok_or("tauri.conf.json 里没有主窗口的配置")?;
            let main_window = WebviewWindowBuilder::from_config(app.handle(), &window)?
                .data_directory(webview_dir)
                .build()?;
            // UAC 以主窗口为请求方才会直接弹到最前面
            #[cfg(windows)]
            if let Ok(hwnd) = main_window.hwnd() {
                app.state::<BrokerSession>().set_owner(hwnd.0 as isize);
            }
            #[cfg(not(windows))]
            let _ = main_window;
            Ok(())
        })
        // 页面开始（重新）加载时，旧页面发起的任务已经没人管了：全部取消，引擎随之退出
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started {
                webview.state::<Engines>().cancel_all();
            }
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            engine_capabilities,
            disk_list,
            carve_file_types,
            job_start_read_scan,
            job_start_carve,
            job_resume_carve,
            carve_history_list,
            carve_history_open,
            carve_history_remove,
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
            job_start_partition_search,
            browse_open_range,
            job_start_copy,
            copied_open,
            copied_reveal,
            job_start_image,
            image_inspect,
            image_history_list,
            image_history_remove,
            image_history_relocate,
            image_reveal,
            boot_diagnose,
            write_plan_boot_sector,
            write_plan_partition_table,
            write_plan_mft,
            write_plan_fs_fix,
            fs_rate_fat_copies,
            job_start_save_lost,
            job_start_scan_free,
            write_plan_execute,
            write_plan_discard,
            write_plan_rollback,
            write_history_list,
            job_start_fs_check,
            broker_prepare,
            fs_check_save_report
        ])
        .run(tauri::generate_context!())?;
    Ok(())
}
