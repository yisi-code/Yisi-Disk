//! 从头到尾顺序读一遍设备：读取扫描（坏道检测）与丢失分区搜索共用。

use crate::{BlockDevice, IoError};

/// 每次读 1 MiB：大块顺序读，同一块盘只有一条读流。
pub const SCAN_BLOCK: usize = 1 << 20;

/// 顺序读的过程中查询取消、报告进度。
pub trait ScanControl {
    /// 每读一段查一次。实现可以在这里阻塞（暂停），这时不读盘。
    fn is_cancelled(&self) -> bool;
    fn progress(&self, done: u64, total: u64);
}

/// 从头到尾按 [`SCAN_BLOCK`] 一段读 `device`，每段交给 `each(偏移, 结果)`：读出时是内容，读不出时整段跳过、
/// 给出这一段的字节数与读错误。每段之后报一次进度。被取消时返回 `false`。
pub fn scan_device(
    device: &dyn BlockDevice,
    control: &dyn ScanControl,
    mut each: impl FnMut(u64, Result<&[u8], (u64, IoError)>),
) -> bool {
    let size = device.size();
    let mut buffer = vec![0u8; SCAN_BLOCK];
    let mut offset = 0u64;
    control.progress(0, size);
    while offset < size {
        if control.is_cancelled() {
            return false;
        }
        let length = (size - offset).min(SCAN_BLOCK as u64);
        let Some(block) = buffer.get_mut(..length as usize) else {
            return false;
        };
        match device.read_exact_at(offset, block) {
            Ok(()) => each(offset, Ok(block)),
            Err(error) => each(offset, Err((length, error))),
        }
        offset += length;
        control.progress(offset, size);
    }
    true
}
