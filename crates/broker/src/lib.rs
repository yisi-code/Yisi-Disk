//! 磁盘代理：枚举磁盘、打开设备并把只读句柄交给引擎、执行写入计划。
//!
//! 库部分供宿主使用：消息格式、成帧，以及 Windows 上的建管道与提权启动；
//! 程序部分（`main.rs`）是以管理员身份运行的代理本身。
//!
//! 允许 `unsafe`，仅用于平台系统调用，每处都要写 `// SAFETY:` 说明。

pub mod frame;
#[cfg(windows)]
pub mod launcher;
pub mod protocol;
