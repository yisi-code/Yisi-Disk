//! 引擎：任务运行时、决策事件、进度节流、取消、JSON-RPC。
//!
//! 库部分只有宿主与引擎之间的方法、通知与参数（[`protocol`]）；运行时在程序部分（`main.rs`）。
#![forbid(unsafe_code)]

pub mod protocol;
