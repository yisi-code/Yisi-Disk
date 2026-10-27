//! 进程间通信：成帧与 JSON-RPC 2.0 消息，宿主、引擎、磁盘代理共用。
#![forbid(unsafe_code)]

pub mod frame;
mod message;

pub use frame::{FrameError, MAX_FRAME_LEN};
pub use message::{
    ErrorObject, Id, JSONRPC_VERSION, Message, Notification, Request, Response, error_code,
    read_message, write_message,
};

/// 把错误及其来源串成一句话（以「：」相连），放进答复或界面看到的错误细节里。宿主、引擎、磁盘代理共用。
pub fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push('：');
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}
