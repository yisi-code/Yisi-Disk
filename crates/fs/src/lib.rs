//! 文件系统识别；目录读取与文件提取；空闲空间位图；引导扇区与超级块的检查和重建；exFAT / FAT 一致性检查。
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
