//! 任务读源的统计：源设备一律经同一个计数器读（每个引擎进程只运行一个任务），任务运行期间每秒把
//! 从任务开始算的累计统计发给宿主，界面据此分辨读得慢是内容零散还是盘有损坏。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::Duration;

use yisi_engine::protocol::{JobReading, READING, SLOW_READ_MS};
use yisi_io::{BlockDevice, MeteredDevice, ReadMeter, ReadStats, SLOW_READ};

use crate::runtime::Outbox;

/// 两次统计通知之间隔这么久。
const REPORT_INTERVAL: Duration = Duration::from_secs(1);

const _: () = assert!(SLOW_READ.as_millis() == SLOW_READ_MS as u128);

static METER: LazyLock<Arc<ReadMeter>> = LazyLock::new(|| Arc::new(ReadMeter::new()));

/// 让 `device` 经计数器读。打开源的地方都要经过它。
pub fn metered(device: Box<dyn BlockDevice>) -> Box<dyn BlockDevice> {
    Box::new(MeteredDevice::new(device, Arc::clone(&METER)))
}

/// 开始为任务 `job_id` 每秒发一次统计（没有变化、也没有卡在一次读取上时不发），直到返回的标志被置位。
pub fn spawn_reporter(job_id: String, outbox: Arc<Outbox>) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let baseline = METER.snapshot();
    thread::spawn(move || {
        let mut last = ReadStats::default();
        loop {
            thread::sleep(REPORT_INTERVAL);
            if stopped.load(Ordering::Relaxed) {
                return;
            }
            let stats = METER.snapshot().since(&baseline);
            if stats == last && stats.stalled.is_zero() {
                continue;
            }
            last = stats;
            outbox.notify(READING, to_notification(&job_id, &stats));
        }
    });
    stop
}

fn to_notification(job_id: &str, stats: &ReadStats) -> JobReading {
    let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    JobReading {
        job_id: job_id.to_owned(),
        reads: stats.reads,
        bytes: stats.bytes,
        errors: stats.errors,
        slow_reads: stats.slow_reads,
        busy_ms: millis(stats.busy),
        stalled_ms: millis(stats.stalled),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};
    use std::sync::Mutex;

    use yisi_io::IoError;
    use yisi_rpc::{Message, read_message};

    use super::*;

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 前一半能读，后一半读错。
    struct HalfBad;

    impl BlockDevice for HalfBad {
        fn size(&self) -> u64 {
            8192
        }

        fn sector_size(&self) -> u32 {
            512
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
            if offset >= 4096 {
                return Err(IoError::Read {
                    offset,
                    len: buf.len(),
                    source: std::io::Error::other("模拟的介质错误"),
                });
            }
            let len = buf.len().min((4096 - offset) as usize);
            buf[..len].fill(7);
            Ok(len)
        }
    }

    #[test]
    fn counts_since_the_job_started_are_reported_every_second() {
        let device = metered(Box::new(HalfBad));
        let mut buffer = [0u8; 1024];
        // 任务开始前的读取不算
        device.read_exact_at(0, &mut buffer).unwrap();
        let written = SharedBuffer::default();
        let stop = spawn_reporter("j1".to_owned(), Arc::new(Outbox::new(written.clone())));
        for offset in [0, 1024, 2048] {
            device.read_exact_at(offset, &mut buffer).unwrap();
        }
        assert!(device.read_exact_at(6144, &mut buffer).is_err());
        thread::sleep(REPORT_INTERVAL + Duration::from_millis(500));
        stop.store(true, Ordering::Relaxed);

        let bytes = written.0.lock().unwrap().clone();
        let mut reader = Cursor::new(bytes);
        let mut reports = Vec::new();
        while let Ok(message) = read_message(&mut reader) {
            if let Message::Notification(notification) = message {
                assert_eq!(notification.method, READING);
                reports.push(serde_json::from_value::<JobReading>(notification.params).unwrap());
            }
        }
        let last = reports.last().expect("一秒之后应有一次统计");
        assert_eq!(last.job_id, "j1");
        // 别的测试可能也在经同一个计数器读，只核对至少有这几次
        assert!(last.reads >= 4, "{last:?}");
        assert!(last.bytes >= 3072, "{last:?}");
        assert!(last.errors >= 1, "{last:?}");
    }
}
