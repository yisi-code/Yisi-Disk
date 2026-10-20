//! 磁盘代理进程入口：枚举磁盘、打开设备并把只读句柄交给引擎、执行写入计划。
//!
//! 允许 `unsafe`，仅用于平台系统调用，每处都要写 `// SAFETY:` 说明。

fn main() -> anyhow::Result<()> {
    // stdout 留给与宿主之间的 JSON-RPC 通道，日志一律写 stderr
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "磁盘代理启动");
    Ok(())
}
