//! 给读源设备计数、计时：读得慢时据此分辨是内容零散（小文件多，每次只读一点）还是盘有损坏（读错、单次读很久）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::{BlockDevice, IoError};

/// 单次读取超过这么久算一次「读得很吃力」：完好的盘读 1 MiB 只要几十毫秒，盘在内部反复重试坏扇区时要几秒。
pub const SLOW_READ: Duration = Duration::from_secs(2);

/// 读取的累计统计，各项从计数器建立起算。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadStats {
    /// 完成的读取次数（含读错的）。
    pub reads: u64,
    /// 读出的字节数。
    pub bytes: u64,
    /// 读错的次数。
    pub errors: u64,
    /// 用时不少于 [`SLOW_READ`] 的读取次数（含读错的）。
    pub slow_reads: u64,
    /// 花在读取上的总时间。
    pub busy: Duration,
    /// 此刻已经连续读了多久还没读完；没在读时为 0。多个线程同时读时，从最早那次开始算。
    pub stalled: Duration,
}

impl ReadStats {
    /// 从 `earlier` 那一刻到这一刻的增量；`stalled` 取这一刻的。
    pub fn since(&self, earlier: &ReadStats) -> ReadStats {
        ReadStats {
            reads: self.reads.saturating_sub(earlier.reads),
            bytes: self.bytes.saturating_sub(earlier.bytes),
            errors: self.errors.saturating_sub(earlier.errors),
            slow_reads: self.slow_reads.saturating_sub(earlier.slow_reads),
            busy: self.busy.saturating_sub(earlier.busy),
            stalled: self.stalled,
        }
    }
}

/// 读取计数器，多个线程可以同时记录。
#[derive(Debug)]
pub struct ReadMeter {
    origin: Instant,
    reads: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    slow_reads: AtomicU64,
    busy_nanos: AtomicU64,
    /// 正在进行的读取数。
    active: AtomicU32,
    /// 正在进行的读取从什么时候开始（距 `origin` 的纳秒数加 1）；0 表示没在读。
    busy_since: AtomicU64,
}

impl Default for ReadMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadMeter {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            reads: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            slow_reads: AtomicU64::new(0),
            busy_nanos: AtomicU64::new(0),
            active: AtomicU32::new(0),
            busy_since: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> ReadStats {
        let since = self.busy_since.load(Ordering::Relaxed);
        let stalled = if since == 0 {
            Duration::ZERO
        } else {
            self.elapsed()
                .saturating_sub(Duration::from_nanos(since - 1))
        };
        ReadStats {
            reads: self.reads.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            slow_reads: self.slow_reads.load(Ordering::Relaxed),
            busy: Duration::from_nanos(self.busy_nanos.load(Ordering::Relaxed)),
            stalled,
        }
    }

    fn elapsed(&self) -> Duration {
        self.origin.elapsed()
    }

    fn begin(&self) -> Instant {
        if self.active.fetch_add(1, Ordering::Relaxed) == 0 {
            let nanos = u64::try_from(self.elapsed().as_nanos()).unwrap_or(u64::MAX - 1);
            self.busy_since.store(nanos + 1, Ordering::Relaxed);
        }
        Instant::now()
    }

    fn end(&self, started: Instant, read: Result<u64, ()>) {
        let took = started.elapsed();
        if self.active.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.busy_since.store(0, Ordering::Relaxed);
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.busy_nanos.fetch_add(
            u64::try_from(took.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        if took >= SLOW_READ {
            self.slow_reads.fetch_add(1, Ordering::Relaxed);
        }
        match read {
            Ok(bytes) => {
                self.bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            Err(()) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// 经过计数器读的设备：每次读取都记进 [`ReadMeter`]，其余照原样转给里面的设备。
pub struct MeteredDevice {
    inner: Box<dyn BlockDevice>,
    meter: Arc<ReadMeter>,
}

impl MeteredDevice {
    pub fn new(inner: Box<dyn BlockDevice>, meter: Arc<ReadMeter>) -> Self {
        Self { inner, meter }
    }
}

impl BlockDevice for MeteredDevice {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        let started = self.meter.begin();
        let result = self.inner.read_at(offset, buf);
        self.meter.end(
            started,
            result.as_ref().map(|&read| read as u64).map_err(|_| ()),
        );
        result
    }

    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let started = self.meter.begin();
        let result = self.inner.read_exact_at(offset, buf);
        self.meter.end(
            started,
            result.as_ref().map(|()| buf.len() as u64).map_err(|_| ()),
        );
        result
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::thread;

    use super::*;

    /// 前 1 KiB 能读，之后读错；`delay` 是每次读要等多久。
    struct SlowDevice {
        delay: Duration,
    }

    impl BlockDevice for SlowDevice {
        fn size(&self) -> u64 {
            4096
        }

        fn sector_size(&self) -> u32 {
            512
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
            thread::sleep(self.delay);
            if offset >= 1024 {
                return Err(IoError::Read {
                    offset,
                    len: buf.len(),
                    source: io::Error::other("模拟的介质错误"),
                });
            }
            let len = buf.len().min((1024 - offset) as usize);
            buf[..len].fill(1);
            Ok(len)
        }
    }

    #[test]
    fn reads_bytes_and_errors_are_counted() {
        let meter = Arc::new(ReadMeter::new());
        let device = MeteredDevice::new(
            Box::new(SlowDevice {
                delay: Duration::ZERO,
            }),
            Arc::clone(&meter),
        );
        let mut buffer = [0u8; 512];
        device.read_exact_at(0, &mut buffer).unwrap();
        assert_eq!(device.read_at(512, &mut buffer).unwrap(), 512);
        assert!(device.read_exact_at(2048, &mut buffer).is_err());
        let stats = meter.snapshot();
        assert_eq!(
            (stats.reads, stats.bytes, stats.errors, stats.slow_reads),
            (3, 1024, 1, 0)
        );
        assert_eq!(stats.stalled, Duration::ZERO);
        assert_eq!(device.size(), 4096);
    }

    #[test]
    fn a_read_in_progress_shows_as_stalled() {
        let meter = Arc::new(ReadMeter::new());
        let device = Arc::new(MeteredDevice::new(
            Box::new(SlowDevice {
                delay: Duration::from_millis(300),
            }),
            Arc::clone(&meter),
        ));
        let reader = {
            let device = Arc::clone(&device);
            thread::spawn(move || {
                let mut buffer = [0u8; 512];
                device.read_exact_at(0, &mut buffer).unwrap();
            })
        };
        thread::sleep(Duration::from_millis(150));
        let during = meter.snapshot();
        assert!(during.stalled >= Duration::from_millis(100), "{during:?}");
        assert_eq!(during.reads, 0);
        reader.join().unwrap();
        let after = meter.snapshot();
        assert_eq!(after.stalled, Duration::ZERO);
        assert_eq!(after.reads, 1);
        assert!(after.busy >= Duration::from_millis(300), "{after:?}");
    }
}
