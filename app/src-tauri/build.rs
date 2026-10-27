fn main() {
    // 应用命令白名单：未登记的命令即使在宿主里注册了，界面也调用不到
    let manifest = tauri_build::AppManifest::new().commands(&[
        "app_info",
        "engine_capabilities",
        "disk_list",
        "carve_file_types",
        "job_start_image_scan",
        "job_start_disk_scan",
        "job_start_carve",
        "job_cancel",
        "job_set_paused",
        "recovered_preview",
        "recovered_open",
        "recovered_open_with",
        "recovered_copy",
        "recovered_reveal",
        "carve_preview_range",
        "job_start_extract",
        "browse_open",
        "browse_list",
        "browse_preview",
        "browse_scan",
        "browse_cancel_scan",
        "job_start_copy",
        "copied_open",
        "copied_reveal",
        "job_start_image",
        "boot_diagnose",
        "job_start_fs_check",
        "fs_check_save_report",
    ]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("tauri-build 执行失败");
}
