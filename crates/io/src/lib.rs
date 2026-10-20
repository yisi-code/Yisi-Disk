//! 块设备抽象：Windows / macOS / Linux 后端，raw、分段 raw、E01 镜像，读缓存，
//! 坏扇区读取策略，能识别坏区的镜像。
//!
//! 允许 `unsafe`，仅用于平台系统调用，每处都要写 `// SAFETY:` 说明。
