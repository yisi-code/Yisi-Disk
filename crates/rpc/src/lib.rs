//! 进程间通信：成帧与 JSON-RPC 2.0 消息，宿主、引擎、磁盘代理共用。
#![forbid(unsafe_code)]

pub mod frame;
mod message;

pub use frame::{FrameError, MAX_FRAME_LEN};
pub use message::{
    ErrorObject, Id, JSONRPC_VERSION, Message, Notification, Request, Response, error_code,
    read_message, write_message,
};
