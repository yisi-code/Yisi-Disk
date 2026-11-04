//! 分区分析与搜索、结构检查、反删除；生成写入计划。
//!
//! 引导扇区诊断（[`boot`]）、丢失分区搜索（[`search`]）、文件系统检查（[`check`]）都只读；
//! 修复引导扇区（[`boot_fix`]）、改写分区表（[`table_fix`]）、`$MFT` 与 `$MFTMirr` 互修（[`mft_fix`]）生成写入计划，同样只读源，写盘不在这里。
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

pub mod boot;
pub mod boot_fix;
pub mod boot_rebuild;
pub mod check;
pub mod mft_fix;
#[cfg(any(test, feature = "test-samples"))]
pub mod samples;
pub mod search;
pub mod table_fix;
