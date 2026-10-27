//! 分区分析与搜索、结构检查、反删除；生成写入计划。
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

#[cfg(any(test, feature = "test-samples"))]
pub mod samples;
pub mod search;
