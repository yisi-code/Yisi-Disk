//! 引擎进程入口：每个任务一个进程，经 stdin / stdout 与宿主通信，任务结束后退出。
#![forbid(unsafe_code)]

mod boot_check;
mod browse;
mod carve_job;
mod copy_job;
mod extract_job;
mod free_scan_job;
mod fs_check_job;
mod fs_fix_plan;
mod image_inspect;
mod image_job;
mod job;
mod lost_job;
mod mft_plan;
mod preview;
mod read_scan;
mod reading;
mod runtime;
mod search_job;
mod shown;
mod table_plan;

fn main() {
    // stdout 是与宿主之间的 JSON-RPC 通道，日志一律写 stderr
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "引擎启动");
    // 启动时把令牌里已有的特权全部打开（不需要用户授权）
    match yisi_io::enable_held_privileges() {
        Ok(names) => tracing::debug!(?names, "已启用特权"),
        Err(error) => tracing::warn!(%error, "启用特权失败"),
    }
    runtime::serve(std::io::stdin(), std::io::stdout());
}
