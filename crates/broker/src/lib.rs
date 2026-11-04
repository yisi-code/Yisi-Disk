//! 磁盘代理：枚举磁盘、打开设备并把只读句柄交给引擎、执行写入计划。
//!
//! 库部分供宿主使用：方法与参数，以及 Windows 上的建管道与提权启动（成帧与 JSON-RPC 在 `yisi-rpc`）；
//! 执行写入计划与它的备份文件（镜像文件由宿主执行，物理盘由代理执行，共用这一份）。引擎不依赖这个 crate，
//! 写盘能力不会出现在引擎里。程序部分（`main.rs`）是以管理员身份运行的代理本身。
//!
//! 允许 `unsafe`，仅用于平台系统调用，每处都要写 `// SAFETY:` 说明。

pub mod backup;
pub mod execute;
#[cfg(windows)]
pub mod launcher;
pub mod protocol;
