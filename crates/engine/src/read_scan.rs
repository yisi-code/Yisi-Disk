//! 读取扫描：从头到尾顺序读一遍设备，记下读不出的区段并跳过，报告结果。只读。

use std::time::Instant;

use yisi_engine::protocol::{ByteRange, MAX_UNREADABLE_RANGES, ReadScanReport};
use yisi_io::{BlockDevice, ScanControl, scan_device};

use crate::job::{JobContext, JobOutcome};

impl ScanControl for JobContext {
    fn is_cancelled(&self) -> bool {
        JobContext::is_cancelled(self)
    }

    fn progress(&self, done: u64, total: u64) {
        JobContext::progress(self, done, total);
    }
}

pub fn run(device: &dyn BlockDevice, control: &dyn ScanControl) -> Result<JobOutcome, String> {
    match scan(device, control) {
        Some(report) => serde_json::to_value(report)
            .map(JobOutcome::Completed)
            .map_err(|error| format!("无法编码扫描结果：{error}")),
        None => Ok(JobOutcome::Cancelled),
    }
}

/// 扫描整个设备；被取消时返回 `None`。
pub fn scan(device: &dyn BlockDevice, control: &dyn ScanControl) -> Option<ReadScanReport> {
    let started = Instant::now();
    let mut report = ReadScanReport {
        size: device.size(),
        bytes_read: 0,
        unreadable: Vec::new(),
        unreadable_bytes: 0,
        unreadable_ranges_truncated: false,
        elapsed_ms: 0,
    };
    let finished = scan_device(device, control, |offset, block| match block {
        Ok(bytes) => report.bytes_read += bytes.len() as u64,
        Err((length, error)) => {
            tracing::warn!(offset, length, %error, "读不出，跳过");
            record_unreadable(&mut report, offset, length);
        }
    });
    if !finished {
        return None;
    }
    report.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Some(report)
}

/// 与上一段相接时合并，否则另起一段；超过上限后只计字节数。
fn record_unreadable(report: &mut ReadScanReport, offset: u64, length: u64) {
    report.unreadable_bytes += length;
    if let Some(last) = report.unreadable.last_mut()
        && last.offset + last.length == offset
    {
        last.length += length;
        return;
    }
    if report.unreadable.len() < MAX_UNREADABLE_RANGES {
        report.unreadable.push(ByteRange { offset, length });
    } else {
        report.unreadable_ranges_truncated = true;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::io;

    use yisi_io::IoError;

    use super::*;

    const MIB: u64 = 1 << 20;

    /// 不占内存的模拟设备：读出的内容全是 0，落在坏区里的读取报错。
    struct FakeDevice {
        size: u64,
        bad: Vec<(u64, u64)>,
    }

    impl BlockDevice for FakeDevice {
        fn size(&self) -> u64 {
            self.size
        }

        fn sector_size(&self) -> u32 {
            512
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
            let end = (offset + buf.len() as u64).min(self.size);
            if offset >= end {
                return Ok(0);
            }
            if self
                .bad
                .iter()
                .any(|&(bad_start, bad_end)| offset < bad_end && bad_start < end)
            {
                return Err(IoError::Read {
                    offset,
                    len: buf.len(),
                    source: io::Error::other("模拟的介质错误"),
                });
            }
            let len = (end - offset) as usize;
            buf[..len].fill(0);
            Ok(len)
        }
    }

    #[derive(Default)]
    struct Recorder {
        progress: RefCell<Vec<(u64, u64)>>,
        cancel_after: Option<usize>,
        checks: Cell<usize>,
    }

    impl ScanControl for Recorder {
        fn is_cancelled(&self) -> bool {
            self.checks.set(self.checks.get() + 1);
            self.cancel_after
                .is_some_and(|limit| self.checks.get() > limit)
        }

        fn progress(&self, done: u64, total: u64) {
            self.progress.borrow_mut().push((done, total));
        }
    }

    #[test]
    fn clean_device_is_read_completely() {
        let size = 3 * MIB + 1234;
        let recorder = Recorder::default();
        let report = scan(&FakeDevice { size, bad: vec![] }, &recorder).unwrap();
        assert_eq!(report.bytes_read, size);
        assert!(report.unreadable.is_empty());
        let progress = recorder.progress.borrow();
        assert_eq!(progress.first(), Some(&(0, size)));
        assert_eq!(progress.last(), Some(&(size, size)));
    }

    #[test]
    fn unreadable_blocks_are_skipped_and_adjacent_ones_merged() {
        let device = FakeDevice {
            size: 8 * MIB,
            bad: vec![
                (MIB + 7, MIB + 8),
                (2 * MIB, 2 * MIB + 1),
                (5 * MIB, 6 * MIB),
            ],
        };
        let report = scan(&device, &Recorder::default()).unwrap();
        assert_eq!(
            report.unreadable,
            vec![
                ByteRange {
                    offset: MIB,
                    length: 2 * MIB
                },
                ByteRange {
                    offset: 5 * MIB,
                    length: MIB
                },
            ]
        );
        assert_eq!(report.unreadable_bytes, 3 * MIB);
        assert_eq!(report.bytes_read, 5 * MIB);
        assert!(!report.unreadable_ranges_truncated);
    }

    #[test]
    fn range_list_is_capped() {
        let blocks = MAX_UNREADABLE_RANGES as u64 + 5;
        // 每隔一块坏一块，坏区互不相接
        let bad = (0..blocks)
            .map(|index| (2 * index * MIB, (2 * index + 1) * MIB))
            .collect();
        let device = FakeDevice {
            size: 2 * blocks * MIB,
            bad,
        };
        let report = scan(&device, &Recorder::default()).unwrap();
        assert_eq!(report.unreadable.len(), MAX_UNREADABLE_RANGES);
        assert!(report.unreadable_ranges_truncated);
        assert_eq!(report.unreadable_bytes, blocks * MIB);
    }

    #[test]
    fn cancellation_stops_at_next_checkpoint() {
        let recorder = Recorder {
            cancel_after: Some(2),
            ..Recorder::default()
        };
        let device = FakeDevice {
            size: 10 * MIB,
            bad: vec![],
        };
        assert!(scan(&device, &recorder).is_none());
        assert_eq!(
            recorder.progress.borrow().last(),
            Some(&(2 * MIB, 10 * MIB))
        );
    }
}
