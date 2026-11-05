//! 读取扫描：从头到尾顺序读一遍设备，记下读不出的区段并跳过，报告结果。只读。
//! 读不出的 1 MiB 块再按 64 KiB 读一遍，读不出的 64 KiB 再对半分着读、一直分到扇区，找出具体是哪些扇区
//! （细读；读得出的一半一次就过，一个 64 KiB 里只坏一个扇区时约 14 次）。细读有上限，免得在快坏的盘上
//! 反复读坏的地方：整块的 64 KiB 都读不出时不再往下分；最多细读 [`MAX_FINE_BLOCKS`] 块、对半分着读
//! [`MAX_NARROW_READS`] 次，之后只按整块或当时那一段记。

use std::time::Instant;

use yisi_engine::protocol::{
    ByteRange, MAX_UNREADABLE_RANGES, READ_MAP_CELLS, ReadMapCell, ReadScanReport,
};
use yisi_io::{BlockDevice, SCAN_BLOCK, SLOW_READ, ScanControl, scan_device};

/// 细读时先按这么大一段读。
const FINE_PIECE: u64 = 64 << 10;
const MAX_FINE_BLOCKS: u64 = 64;
const MAX_NARROW_READS: u64 = 2048;

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

/// 扇区图的一格在扫描时的情况。
#[derive(Default, Clone, Copy)]
struct Cell {
    slow: bool,
    unreadable: u64,
}

/// 扫描整个设备；被取消时返回 `None`。
pub fn scan(device: &dyn BlockDevice, control: &dyn ScanControl) -> Option<ReadScanReport> {
    let started = Instant::now();
    let size = device.size();
    let cell_bytes = map_cell_bytes(size);
    let mut cells = vec![Cell::default(); usize::try_from(size.div_ceil(cell_bytes)).unwrap_or(0)];
    let mut report = ReadScanReport {
        size,
        bytes_read: 0,
        unreadable: Vec::new(),
        unreadable_bytes: 0,
        unreadable_ranges_truncated: false,
        elapsed_ms: 0,
        map: Vec::new(),
        map_cell_bytes: cell_bytes,
        fine_read_blocks: 0,
        fine_read_truncated: false,
    };
    let mut narrow_reads = 0u64;
    // 两次回调之间的时间就是这一块读了多久（细读的时间算进这一块）
    let mut since = Instant::now();
    let finished = scan_device(device, control, |offset, block| {
        let took = since.elapsed();
        let before = report.unreadable_bytes;
        match block {
            Ok(bytes) => report.bytes_read += bytes.len() as u64,
            Err((length, error)) => {
                tracing::warn!(offset, length, %error, "读不出，细读");
                fine_read(
                    device,
                    control,
                    &mut report,
                    &mut narrow_reads,
                    offset,
                    length,
                );
            }
        }
        if let Some(cell) = cells.get_mut((offset / cell_bytes) as usize) {
            cell.slow |= took >= SLOW_READ;
            cell.unreadable += report.unreadable_bytes - before;
        }
        since = Instant::now();
    });
    if !finished {
        return None;
    }
    report.map = cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let start = index as u64 * cell_bytes;
            let length = cell_bytes.min(size - start);
            match cell.unreadable {
                0 if cell.slow => ReadMapCell::Slow,
                0 => ReadMapCell::Good,
                bytes if bytes >= length => ReadMapCell::Unreadable,
                _ => ReadMapCell::Partly,
            }
        })
        .collect();
    report.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Some(report)
}

/// 扇区图一格多大：整 MiB，格数不超过 [`READ_MAP_CELLS`]。
fn map_cell_bytes(size: u64) -> u64 {
    let block = SCAN_BLOCK as u64;
    size.div_ceil(READ_MAP_CELLS as u64).div_ceil(block).max(1) * block
}

/// 读不出的一块（`offset` 起 `length` 字节）细读，把读出的计入已读、读不出的记下。
fn fine_read(
    device: &dyn BlockDevice,
    control: &dyn ScanControl,
    report: &mut ReadScanReport,
    narrow_reads: &mut u64,
    offset: u64,
    length: u64,
) {
    if report.fine_read_blocks >= MAX_FINE_BLOCKS {
        report.fine_read_truncated = true;
        record_unreadable(report, offset, length);
        return;
    }
    report.fine_read_blocks += 1;
    let mut buffer = vec![0u8; FINE_PIECE as usize];
    let mut bad_pieces = Vec::new();
    for start in (offset..offset + length).step_by(FINE_PIECE as usize) {
        let piece = FINE_PIECE.min(offset + length - start);
        if read(device, start, &mut buffer, piece) {
            report.bytes_read += piece;
        } else {
            bad_pieces.push((start, piece));
        }
    }
    // 整块都读不出：这一片坏得很密，不再按扇区去碰它
    if bad_pieces.len() as u64 == length.div_ceil(FINE_PIECE) {
        record_unreadable(report, offset, length);
        return;
    }
    let mut narrow = Narrow {
        device,
        control,
        sector: u64::from(device.sector_size()).max(512),
        buffer,
        reads: narrow_reads,
    };
    for (start, piece) in bad_pieces {
        narrow.split(report, start, piece);
    }
}

/// 把读不出的一段对半分着读，直到扇区。按从前到后的顺序记下，相邻的读不出区段才能合并。
struct Narrow<'a> {
    device: &'a dyn BlockDevice,
    control: &'a dyn ScanControl,
    sector: u64,
    buffer: Vec<u8>,
    reads: &'a mut u64,
}

impl Narrow<'_> {
    /// `start` 起 `length` 字节已知读不出。
    fn split(&mut self, report: &mut ReadScanReport, start: u64, length: u64) {
        if length <= self.sector {
            record_unreadable(report, start, length);
            return;
        }
        // 前一半按扇区对齐
        let half = (length / 2).div_ceil(self.sector) * self.sector;
        for (at, part) in [(start, half), (start + half, length - half)] {
            if *self.reads >= MAX_NARROW_READS || self.control.is_cancelled() {
                report.fine_read_truncated |= *self.reads >= MAX_NARROW_READS;
                record_unreadable(report, at, part);
                continue;
            }
            *self.reads += 1;
            if read(self.device, at, &mut self.buffer, part) {
                report.bytes_read += part;
            } else {
                self.split(report, at, part);
            }
        }
    }
}

fn read(device: &dyn BlockDevice, offset: u64, buffer: &mut [u8], length: u64) -> bool {
    buffer
        .get_mut(..length as usize)
        .is_some_and(|target| device.read_exact_at(offset, target).is_ok())
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
    fn unreadable_blocks_are_read_again_sector_by_sector() {
        let device = FakeDevice {
            size: 8 * MIB,
            bad: vec![
                // 第 1 MiB 里坏一个字节、第 2 MiB 开头坏一个字节：细读后各只记那一个扇区，相邻的合并
                (MIB + 7, MIB + 8),
                (MIB + 511 * 1024 + 600, MIB + 511 * 1024 + 601),
                (2 * MIB, 2 * MIB + 1),
                // 整块都坏：不按扇区去碰，整块记下
                (5 * MIB, 6 * MIB),
            ],
        };
        let report = scan(&device, &Recorder::default()).unwrap();
        let range = |offset, length| ByteRange { offset, length };
        assert_eq!(
            report.unreadable,
            vec![
                range(MIB, 512),
                range(MIB + 511 * 1024 + 512, 512),
                range(2 * MIB, 512),
                range(5 * MIB, MIB),
            ]
        );
        assert_eq!(report.unreadable_bytes, MIB + 3 * 512);
        assert_eq!(report.bytes_read, 8 * MIB - report.unreadable_bytes);
        assert_eq!(
            (report.fine_read_blocks, report.fine_read_truncated),
            (3, false)
        );
        assert!(!report.unreadable_ranges_truncated);
        // 8 MiB 的设备一格 1 MiB
        use ReadMapCell::{Good, Partly, Unreadable};
        assert_eq!(report.map_cell_bytes, MIB);
        assert_eq!(
            report.map,
            [Good, Partly, Partly, Good, Good, Unreadable, Good, Good]
        );
    }

    #[test]
    fn fine_reading_stops_at_its_limits() {
        // 每 1 MiB 坏一个扇区，共 MAX_FINE_BLOCKS + 3 块：对半分着读每块 14 次，都找到那一个扇区；超出块数的按整块记
        let blocks = MAX_FINE_BLOCKS + 3;
        let bad = (0..blocks)
            .map(|index| (index * MIB + 100, index * MIB + 101))
            .collect();
        let device = FakeDevice {
            size: blocks * MIB,
            bad,
        };
        let report = scan(&device, &Recorder::default()).unwrap();
        assert_eq!(report.fine_read_blocks, MAX_FINE_BLOCKS);
        assert!(report.fine_read_truncated);
        assert_eq!(report.unreadable_bytes, MAX_FINE_BLOCKS * 512 + 3 * MIB);
        assert_eq!(report.unreadable.last().unwrap().length, 3 * MIB);

        // 两块的前一半各 8 个 64 KiB 里每 4 个扇区坏一个（每段约 190 次）：对半分着读的次数用完，剩下的按当时那一段记
        let bad = (0..2u64)
            .flat_map(|block| (0..256u64).map(move |index| block * MIB + index * 2048))
            .map(|at| (at, at + 1))
            .collect();
        let device = FakeDevice { size: 2 * MIB, bad };
        let report = scan(&device, &Recorder::default()).unwrap();
        assert!(report.fine_read_truncated);
        // 真坏的 256 KiB 都在里面，后一半都读出来了
        assert!(report.unreadable_bytes >= 256 << 10);
        assert!(report.unreadable_bytes < MIB);
        assert!(report.bytes_read >= MIB);
    }

    #[test]
    fn the_map_has_whole_mib_cells_and_at_most_the_cell_limit() {
        assert_eq!(map_cell_bytes(100 * MIB), MIB);
        assert_eq!(map_cell_bytes(10 << 30), 10 * MIB);
        // 不整除时往上取整 MiB
        let size = (1 << 30) + 1;
        let cell = map_cell_bytes(size);
        assert_eq!(cell % MIB, 0);
        assert!(size.div_ceil(cell) <= READ_MAP_CELLS as u64);
        assert_eq!(map_cell_bytes(1), MIB);
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
        // 2010 MiB 一格 2 MiB
        assert_eq!((report.map_cell_bytes, report.map.len()), (2 * MIB, 1005));
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
