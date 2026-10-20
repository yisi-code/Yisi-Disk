//! 成帧：每条消息是 4 字节小端长度，后跟这么长的 UTF-8 JSON。

use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// 单条消息的长度上限，防止对端发来过大的长度字段时一次分配过多内存。
pub const MAX_FRAME_LEN: u32 = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("对端已关闭连接")]
    Closed,

    #[error("消息长度 {0} 超过上限 {MAX_FRAME_LEN}")]
    TooLarge(u64),

    #[error("读写消息失败")]
    Io(#[from] io::Error),

    #[error("消息不是预期格式的 JSON")]
    Json(#[from] serde_json::Error),

    #[error("不是合法的 JSON-RPC 2.0 消息：{0}")]
    Invalid(String),
}

pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, message: &T) -> Result<(), FrameError> {
    let body = serde_json::to_vec(message)?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|&len| len <= MAX_FRAME_LEN)
        .ok_or(FrameError::TooLarge(body.len() as u64))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// 读一条消息。在消息边界上读到连接结束返回 [`FrameError::Closed`]，读到一半就结束视为截断。
pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, FrameError> {
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Err(FrameError::Closed),
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    let len = u32::from_le_bytes(header);
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(u64::from(len)));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn frames_round_trip_in_sequence() {
        let first = json!({"path": r"\\.\PhysicalDrive1", "targetPid": 4242});
        let second = json!(["中文", 1, null]);
        let mut wire = Vec::new();
        write_frame(&mut wire, &first).unwrap();
        write_frame(&mut wire, &second).unwrap();

        let mut reader = Cursor::new(wire);
        assert_eq!(read_frame::<_, Value>(&mut reader).unwrap(), first);
        assert_eq!(read_frame::<_, Value>(&mut reader).unwrap(), second);
        assert!(matches!(
            read_frame::<_, Value>(&mut reader),
            Err(FrameError::Closed)
        ));
    }

    #[test]
    fn truncated_frames_are_errors_not_clean_close() {
        let mut wire = Vec::new();
        write_frame(&mut wire, &json!({"result": "ok"})).unwrap();
        for cut in 1..wire.len() {
            let result = read_frame::<_, Value>(&mut Cursor::new(&wire[..cut]));
            assert!(
                matches!(&result, Err(FrameError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof),
                "cut={cut} result={result:?}"
            );
        }
    }

    #[test]
    fn oversized_length_is_rejected_before_allocating() {
        let wire = u32::MAX.to_le_bytes();
        assert!(matches!(
            read_frame::<_, Value>(&mut Cursor::new(wire)),
            Err(FrameError::TooLarge(len)) if len == u64::from(u32::MAX)
        ));
    }

    #[test]
    fn malformed_json_is_rejected() {
        let body = b"{not json";
        let mut wire = (body.len() as u32).to_le_bytes().to_vec();
        wire.extend_from_slice(body);
        assert!(matches!(
            read_frame::<_, Value>(&mut Cursor::new(wire)),
            Err(FrameError::Json(_))
        ));
    }
}
