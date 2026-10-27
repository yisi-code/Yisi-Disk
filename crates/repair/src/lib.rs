//! 分区分析与搜索、结构检查、反删除；生成写入计划。
//!
//! 目前有引导扇区诊断（[`boot`]）与丢失分区搜索（[`search`]）：都只读，不生成写入计划。
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

pub mod boot;
#[cfg(any(test, feature = "test-samples"))]
pub mod samples;
pub mod search;
