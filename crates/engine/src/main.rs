//! 引擎进程入口：每个任务一个进程，经 stdin / stdout 与宿主通信，任务结束后退出。
#![forbid(unsafe_code)]

mod browse;
mod carve_job;
mod copy_job;
mod extract_job;
mod image_job;
mod job;
mod preview;
mod read_scan;
mod runtime;

fn main() {
    // stdout 是与宿主之间的 JSON-RPC 通道，日志一律写 stderr
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "引擎启动");
    runtime::serve(std::io::stdin(), std::io::stdout());
}
