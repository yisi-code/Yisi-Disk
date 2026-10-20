//! 同步 `DeviceIoControl` 的封装。输出一律按字节接收、再按字段偏移取值，不把驱动写入的内容直接当成结构体。

use std::{io, ptr};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::IO::DeviceIoControl;

/// 发出一个 IOCTL，返回驱动写入 `output` 的字节数。
pub(crate) fn ioctl(
    handle: HANDLE,
    code: u32,
    input: &[u8],
    output: &mut [u8],
) -> io::Result<usize> {
    let input_len =
        u32::try_from(input.len()).map_err(|_| io::Error::other("IOCTL 输入缓冲区过大"))?;
    let output_len =
        u32::try_from(output.len()).map_err(|_| io::Error::other("IOCTL 输出缓冲区过大"))?;
    let input_ptr = if input.is_empty() {
        ptr::null()
    } else {
        input.as_ptr().cast()
    };
    let mut returned = 0u32;
    // SAFETY: 句柄无效时内核直接返回错误，不会访问本进程的其他内存；输入、输出的指针和长度
    // 分别来自共享借用的 `input` 与独占借用的 `output`，字节缓冲区可以接受驱动写入的任何内容；
    // 不传 OVERLAPPED，调用同步完成，返回后不再访问这些指针。
    let succeeded = unsafe {
        DeviceIoControl(
            handle,
            code,
            input_ptr,
            input_len,
            output.as_mut_ptr().cast(),
            output_len,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    usize::try_from(returned)
        .ok()
        .filter(|&len| len <= output.len())
        .ok_or_else(|| invalid_data(format!("IOCTL 报告写入了 {returned} 字节，超过缓冲区")))
}

/// 没有输入、要求驱动把 `out` 写满的 IOCTL。
pub(crate) fn ioctl_read(handle: HANDLE, code: u32, out: &mut [u8]) -> io::Result<()> {
    let returned = ioctl(handle, code, &[], out)?;
    if returned != out.len() {
        return Err(invalid_data(format!(
            "IOCTL 只返回了 {returned} 字节，应为 {} 字节",
            out.len()
        )));
    }
    Ok(())
}

/// 取出从 `offset` 起 `N` 个字节，供 `from_ne_bytes` 还原字段值。
pub(crate) fn field<const N: usize>(bytes: &[u8], offset: usize) -> io::Result<[u8; N]> {
    bytes
        .get(offset..offset + N)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| invalid_data("IOCTL 输出比结构体短".to_owned()))
}

pub(crate) fn invalid_data(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
