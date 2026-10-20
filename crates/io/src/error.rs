use std::io;
use std::path::PathBuf;

/// 块设备操作的错误。读取错误带上偏移和系统错误，便于识别介质错误、定位坏区。
#[derive(Debug, thiserror::Error)]
pub enum IoError {
    #[error("无法打开 {}", .path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("无法查询设备的{what}")]
    Query {
        what: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("读取失败：偏移 {offset}，长度 {len}")]
    Read {
        offset: u64,
        len: usize,
        #[source]
        source: io::Error,
    },

    #[error("数据不足：偏移 {offset} 处需要 {expected} 字节，只读到 {read} 字节")]
    UnexpectedEof {
        offset: u64,
        expected: usize,
        read: usize,
    },

    #[error("扇区大小 {0} 不受支持，应为 512 到 65536 之间的 2 的幂")]
    InvalidSectorSize(u32),
}
