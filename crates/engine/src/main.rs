//! 引擎进程入口：任务运行时、决策事件、进度节流、取消、JSON-RPC。
#![forbid(unsafe_code)]

fn main() -> anyhow::Result<()> {
    // stdout 留给与宿主之间的 JSON-RPC 通道，日志一律写 stderr
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "引擎启动");
    Ok(())
}
