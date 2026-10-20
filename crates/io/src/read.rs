//! 各类设备共用的读取逻辑：定位读、按设备末尾截断、按扇区对齐。

use std::fs::File;
use std::io;

use crate::IoError;

/// 不对齐的读取经中转缓冲区完成，每次最多读这么多，限制中转缓冲区占用的内存。
/// 必须是最大扇区大小（64 KiB）的整数倍。
// 对齐读取目前只有 Windows 物理盘在用，Linux / macOS 后端接入后去掉这几处 allow
#[cfg_attr(not(windows), allow(dead_code))]
const BOUNCE_CHUNK: usize = 1 << 20;

/// 定位读：只读 `offset` 处，不依赖文件的当前位置。
pub(crate) fn read_file_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::FileExt::seek_read(file, buf, offset)
    }
}

/// 读取不要求对齐的设备（镜像文件）：截到设备末尾后读满。
pub(crate) fn read_unaligned<F>(
    mut read: F,
    size: u64,
    offset: u64,
    buf: &mut [u8],
) -> Result<usize, IoError>
where
    F: FnMut(u64, &mut [u8]) -> io::Result<usize>,
{
    let Some(buf) = clamp_to_size(size, offset, buf) else {
        return Ok(0);
    };
    read_full(&mut read, offset, buf, 1)
}

/// 读取只接受扇区对齐请求的设备（物理盘）：`read` 收到的起点、长度、缓冲区地址都按 `sector_size` 对齐。
///
/// 调用方的请求本身已经对齐时直接读进 `buf`，否则经对齐的中转缓冲区分块读取再拷贝。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn read_aligned<F>(
    read: F,
    sector_size: u32,
    size: u64,
    offset: u64,
    buf: &mut [u8],
) -> Result<usize, IoError>
where
    F: FnMut(u64, &mut [u8]) -> io::Result<usize>,
{
    read_aligned_in_chunks(read, sector_size, size, offset, buf, BOUNCE_CHUNK)
}

#[cfg_attr(not(windows), allow(dead_code))]
fn read_aligned_in_chunks<F>(
    mut read: F,
    sector_size: u32,
    size: u64,
    offset: u64,
    buf: &mut [u8],
    chunk: usize,
) -> Result<usize, IoError>
where
    F: FnMut(u64, &mut [u8]) -> io::Result<usize>,
{
    let sector = sector_size as usize;
    let sector_u64 = u64::from(sector_size);
    debug_assert!(chunk >= sector && chunk.is_multiple_of(sector));

    let Some(buf) = clamp_to_size(size, offset, buf) else {
        return Ok(0);
    };
    if offset.is_multiple_of(sector_u64)
        && buf.len().is_multiple_of(sector)
        && buf.as_ptr().align_offset(sector) == 0
    {
        return read_full(&mut read, offset, buf, sector);
    }

    let head = (offset % sector_u64) as usize;
    let span = (head + buf.len()).next_multiple_of(sector).min(chunk);
    let mut storage = vec![0u8; span + sector];
    // 运行时对 u8 指针求 align_offset 总能得到小于 `sector` 的结果
    let pad = storage.as_ptr().align_offset(sector);
    let bounce = &mut storage[pad..pad + span];

    let mut done = 0;
    while done < buf.len() {
        let position = offset + done as u64;
        let skip = (position % sector_u64) as usize;
        let request = (skip + buf.len() - done).next_multiple_of(sector).min(span);
        let read_len = read_full(
            &mut read,
            position - skip as u64,
            &mut bounce[..request],
            sector,
        )?;
        if read_len <= skip {
            break;
        }
        let take = (read_len - skip).min(buf.len() - done);
        buf[done..done + take].copy_from_slice(&bounce[skip..skip + take]);
        done += take;
        if read_len < request {
            break;
        }
    }
    Ok(done)
}

/// 把请求截到设备末尾，返回实际要读的部分；没有可读的内容时返回 `None`。
fn clamp_to_size(size: u64, offset: u64, buf: &mut [u8]) -> Option<&mut [u8]> {
    let remaining = size.checked_sub(offset).filter(|&left| left > 0)?;
    if buf.is_empty() {
        return None;
    }
    let len = usize::try_from(remaining).map_or(buf.len(), |left| left.min(buf.len()));
    Some(&mut buf[..len])
}

/// 反复调用 `read`，直到读满 `buf`、读到末尾，或读到一个长度不是 `granule` 整数倍的短块
/// （之后的请求就对不齐了）。被信号打断时重试。
fn read_full<F>(read: &mut F, offset: u64, buf: &mut [u8], granule: usize) -> Result<usize, IoError>
where
    F: FnMut(u64, &mut [u8]) -> io::Result<usize>,
{
    let mut filled = 0;
    while filled < buf.len() {
        let position = offset + filled as u64;
        match read(position, &mut buf[filled..]) {
            Ok(0) => break,
            Ok(read_len) => {
                filled += read_len;
                if !read_len.is_multiple_of(granule) {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(source) => {
                return Err(IoError::Read {
                    offset: position,
                    len: buf.len() - filled,
                    source,
                });
            }
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const SECTOR: u32 = 512;

    /// 只接受扇区对齐请求的模拟设备，记录最近一次请求的缓冲区地址。
    struct SectorDevice {
        data: Vec<u8>,
        last_request_addr: Cell<usize>,
    }

    impl SectorDevice {
        fn new(len: usize) -> Self {
            let data = (0..len)
                .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 24) as u8)
                .collect();
            Self {
                data,
                last_request_addr: Cell::new(0),
            }
        }

        fn size(&self) -> u64 {
            self.data.len() as u64
        }

        fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            let sector = SECTOR as usize;
            if !offset.is_multiple_of(u64::from(SECTOR))
                || !buf.len().is_multiple_of(sector)
                || buf.as_ptr().align_offset(sector) != 0
            {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "未对齐的请求"));
            }
            self.last_request_addr.set(buf.as_ptr().addr());
            let start = (offset as usize).min(self.data.len());
            let end = (start + buf.len()).min(self.data.len());
            buf[..end - start].copy_from_slice(&self.data[start..end]);
            Ok(end - start)
        }
    }

    #[test]
    fn unaligned_reads_match_data_across_chunks() {
        let device = SectorDevice::new(8 * 512);
        for offset in (0..=device.size() + 600).step_by(37) {
            for len in (0..=2100).step_by(53) {
                let mut buf = vec![0xAA; len];
                let read = read_aligned_in_chunks(
                    |position, chunk| device.read(position, chunk),
                    SECTOR,
                    device.size(),
                    offset,
                    &mut buf,
                    1024,
                )
                .unwrap();
                let start = (offset as usize).min(device.data.len());
                let end = (start + len).min(device.data.len());
                assert_eq!(read, end - start, "offset={offset} len={len}");
                assert_eq!(
                    &buf[..read],
                    &device.data[start..end],
                    "offset={offset} len={len}"
                );
            }
        }
    }

    #[test]
    fn aligned_request_reads_directly_into_caller_buffer() {
        let device = SectorDevice::new(8 * 512);
        let mut storage = vec![0u8; 4 * 512];
        let pad = storage.as_ptr().align_offset(512);
        let buf = &mut storage[pad..pad + 3 * 512];
        let read = read_aligned(
            |position, chunk| device.read(position, chunk),
            SECTOR,
            device.size(),
            1024,
            buf,
        )
        .unwrap();
        assert_eq!(read, 3 * 512);
        assert_eq!(device.last_request_addr.get(), buf.as_ptr().addr());
        assert_eq!(&buf[..], &device.data[1024..2560]);
    }

    #[test]
    fn device_error_reports_failing_request_offset() {
        let mut buf = [0u8; 10];
        let result = read_aligned(
            |_, _| Err(io::Error::other("坏扇区")),
            SECTOR,
            4096,
            700,
            &mut buf,
        );
        assert!(matches!(result, Err(IoError::Read { offset: 512, .. })));
    }

    #[test]
    fn interrupted_read_is_retried() {
        let device = SectorDevice::new(8 * 512);
        let mut interrupted = false;
        let mut buf = [0u8; 10];
        let read = read_aligned(
            |position, chunk| {
                if !interrupted {
                    interrupted = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                device.read(position, chunk)
            },
            SECTOR,
            device.size(),
            100,
            &mut buf,
        )
        .unwrap();
        assert_eq!(read, 10);
        assert_eq!(&buf[..], &device.data[100..110]);
    }

    #[test]
    fn requests_at_or_past_end_read_nothing() {
        let mut buf = [0u8; 4];
        assert!(clamp_to_size(100, 100, &mut buf).is_none());
        assert!(clamp_to_size(100, 200, &mut buf).is_none());
        assert!(clamp_to_size(100, 0, &mut []).is_none());
        assert_eq!(
            clamp_to_size(100, 98, &mut buf).map(|rest| rest.len()),
            Some(2)
        );
    }
}
