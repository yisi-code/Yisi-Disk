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
        "recovered_preview",
        "recovered_open",
        "recovered_reveal",
    ]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("tauri-build 执行失败");
}
