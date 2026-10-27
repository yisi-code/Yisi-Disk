//! 能识别坏区的镜像：分轮读取源设备，读出的数据写进镜像，读的进度与坏区记在坏区图里，中断后据此续做。
//!
//! 这里只有读取策略与坏区图，镜像与坏区图怎样落盘由调用方实现 [`ImageStore`]。
//!
//! - 第 1 轮「复制」：按大块顺序读，遇错跳过一段（起步 [`ImagingOptions::skip_min`]，连续出错时加倍，有上限），
//!   先把完好的区域读完。正向读一遍之后，反向从远端读回被跳过的区段，最后正向不跳过地补读剩下的。
//!   读错的块记为待修边。
//! - 第 2 轮「修边」：对每个待修边的区段，从两端按扇区向内读，碰到读错误就停；读不出的那个扇区记为坏，
//!   中间剩下的记为待细读。
//! - 第 3 轮「细读」：对待细读的区段逐扇区读一遍，读不出的记为坏。
//!
//! 可以只读设备的一部分（[`ImagingOptions::scope`]，例如选中的几个分区），也可以只做第 1 轮的正向、反向读
//! （[`ImagingOptions::copy_only`]，先把完好区域读出来，坏区留到以后）。坏区图始终覆盖整个设备，
//! 范围以外与没做的部分原样留着，下次换个范围或不加限制再做时接着读。
//!
//! 每次读错之后先重读最近一个读成功的扇区：它也读不出，说明设备本身已经读不到（多半是断开了），
//! 这时不记这次错误、结束本次运行，以免把整块盘都误记成坏区。
#![forbid(unsafe_code)]

mod map;
mod scope;

use std::io;
use std::time::{Duration, Instant};

pub use map::{BadMap, MapError, MapHeader, RecordedPartition, Segment, SegmentState};
pub use scope::Scope;

use crate::{BlockDevice, IoError};

/// 镜像与坏区图的存放处。
pub trait ImageStore {
    /// 把读出的数据写进镜像的 `offset` 处。
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;

    /// 先让已写的镜像数据落盘，再保存坏区图：坏区图里记为已读的区段，数据必须已经在盘上。
    fn commit(&mut self, map: &BadMap) -> io::Result<()>;
}

/// 运行中查询取消、报告进度。
pub trait ImagingControl {
    fn is_cancelled(&self) -> bool;

    /// 每读一次报告一次，由实现方节流。
    fn progress(&self, round: Round, map: &BadMap);

    /// 一次读取出错（之后才判断设备是否还在），供记日志。
    fn read_failed(&self, offset: u64, length: u64, error: &IoError) {
        let _ = (offset, length, error);
    }
}

/// 读取轮次。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Round {
    Copy,
    Trim,
    Scrape,
}

#[derive(Debug, Clone)]
pub struct ImagingOptions {
    /// 第 1 轮每次读多少字节，必须是扇区大小的整数倍。
    pub block_size: u64,
    /// 第 1 轮遇错后第一次跳过多少字节，必须是扇区大小的整数倍。
    pub skip_min: u64,
    /// 最多隔这么久保存一次坏区图；每轮结束、停止时总会保存。
    pub commit_interval: Duration,
    /// 只读这些范围；`None` 是整个设备。
    pub scope: Option<Scope>,
    /// 只做第 1 轮的正向、反向读：先把完好的区域读出来，读错与被跳过的区段留到以后。
    pub copy_only: bool,
}

impl Default for ImagingOptions {
    fn default() -> Self {
        Self {
            block_size: 1 << 20,
            skip_min: 64 << 10,
            commit_interval: Duration::from_secs(5),
            scope: None,
            copy_only: false,
        }
    }
}

/// 跳过量的上限：容量的 1/1024，在 1 GiB 以内，不小于起步值。
const MAX_SKIP: u64 = 1 << 30;

/// 一次运行怎样结束。
#[derive(Debug)]
pub enum ImagingOutcome {
    /// 范围内该做的都做完了：不加 [`ImagingOptions::copy_only`] 时，每个位置都已读出或确认读不出。
    Finished,
    Cancelled,
    /// 读错之后，最近一个读成功的扇区也读不出了：设备多半已经断开。`offset` 是出错的位置。
    SourceLost {
        offset: u64,
        error: IoError,
    },
}

/// 按坏区图继续读 `device`，写进 `store`。新镜像传入全部未读的坏区图，续做时传入读回的坏区图。
///
/// 结束（含取消、设备断开）前总会保存一次坏区图。镜像写不进去时返回错误，坏区图尽量保存。
pub fn run_imaging(
    device: &dyn BlockDevice,
    map: &mut BadMap,
    store: &mut dyn ImageStore,
    control: &dyn ImagingControl,
    options: &ImagingOptions,
) -> io::Result<ImagingOutcome> {
    let header = map.header();
    if header.size != device.size() || header.sector_size != device.sector_size() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "坏区图的容量或扇区大小与设备不符",
        ));
    }
    let sector = u64::from(device.sector_size());
    if options.block_size == 0
        || !options.block_size.is_multiple_of(sector)
        || options.skip_min == 0
        || !options.skip_min.is_multiple_of(sector)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "读取块与跳过量必须是扇区大小的整数倍",
        ));
    }
    let whole;
    let scope = match &options.scope {
        Some(scope) => {
            if scope
                .ranges()
                .last()
                .is_some_and(|&(_, end)| end > device.size())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "要读的范围超出设备",
                ));
            }
            scope
        }
        None => {
            whole = Scope::whole(device.size());
            &whole
        }
    };
    let skip_max = align_down((device.size() / 1024).min(MAX_SKIP), sector).max(options.skip_min);
    // 续做时先拿一个已读出的扇区当作「最近读成功的位置」
    let last_good = map.next_in(SegmentState::Done, 0).map(|(start, _)| start);
    let block = usize::try_from(options.block_size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "读取块太大"))?;
    let mut imager = Imager {
        device,
        map,
        store,
        control,
        scope,
        copy_only: options.copy_only,
        sector,
        block: options.block_size,
        skip_min: options.skip_min,
        skip_max,
        commit_interval: options.commit_interval,
        // 反向读时一块可能多出不到一个扇区（设备末尾不满一个扇区）
        buffer: vec![0; block + device.sector_size() as usize],
        last_good,
        last_commit: Instant::now(),
        round: Round::Copy,
    };
    let result = imager.run_rounds();
    let committed = imager.store.commit(imager.map);
    match result {
        Ok(()) => committed.map(|()| ImagingOutcome::Finished),
        Err(Interrupt::Cancelled) => committed.map(|()| ImagingOutcome::Cancelled),
        Err(Interrupt::SourceLost { offset, error }) => {
            committed.map(|()| ImagingOutcome::SourceLost { offset, error })
        }
        // 写镜像的错误更要紧；坏区图里没有记这次写失败的块，保存失败也不影响续做
        Err(Interrupt::Store(error)) => Err(error),
    }
}

/// 本次运行中途停下的原因。
enum Interrupt {
    Cancelled,
    SourceLost { offset: u64, error: IoError },
    Store(io::Error),
}

impl From<io::Error> for Interrupt {
    fn from(error: io::Error) -> Self {
        Self::Store(error)
    }
}

struct Imager<'a> {
    device: &'a dyn BlockDevice,
    map: &'a mut BadMap,
    store: &'a mut dyn ImageStore,
    control: &'a dyn ImagingControl,
    scope: &'a Scope,
    copy_only: bool,
    sector: u64,
    block: u64,
    skip_min: u64,
    skip_max: u64,
    commit_interval: Duration,
    buffer: Vec<u8>,
    last_good: Option<u64>,
    last_commit: Instant,
    round: Round,
}

impl Imager<'_> {
    fn run_rounds(&mut self) -> Result<(), Interrupt> {
        if self.scope.bytes(self.map, SegmentState::Untried) > 0 {
            self.begin(Round::Copy)?;
            self.copy_forward(true)?;
            self.copy_backward()?;
            if self.copy_only {
                return Ok(());
            }
            self.copy_forward(false)?;
        }
        if self.copy_only {
            return Ok(());
        }
        if self.scope.bytes(self.map, SegmentState::Trim) > 0 {
            self.begin(Round::Trim)?;
            self.trim()?;
        }
        if self.scope.bytes(self.map, SegmentState::Scrape) > 0 {
            self.begin(Round::Scrape)?;
            self.scrape()?;
        }
        Ok(())
    }

    /// 进入下一轮：先保存一次坏区图，再报告进度。
    fn begin(&mut self, round: Round) -> Result<(), Interrupt> {
        self.round = round;
        self.commit()?;
        self.control.progress(round, self.map);
        Ok(())
    }

    /// 正向读还没读过的区段。`skipping` 时遇错跳过一段，跳过的区段留给反向读与补读。
    fn copy_forward(&mut self, skipping: bool) -> Result<(), Interrupt> {
        let mut cursor = 0;
        let mut skip = 0;
        while let Some((start, end)) = self.scope.next_in(self.map, SegmentState::Untried, cursor) {
            let length = (end - start).min(self.block);
            cursor = start + length;
            if self.read(start, length)? {
                skip = 0;
                continue;
            }
            self.map.set(start, length, SegmentState::Trim);
            if skipping {
                skip = self.next_skip(skip);
                cursor = cursor.saturating_add(skip).min(self.device.size());
            }
        }
        Ok(())
    }

    /// 反向读被跳过的区段：从远端往回读，碰到坏区时是从完好的一侧靠近它。遇错同样跳过。
    fn copy_backward(&mut self) -> Result<(), Interrupt> {
        let mut cursor = self.device.size();
        let mut skip = 0;
        while let Some((start, end)) = self.scope.prev_in(self.map, SegmentState::Untried, cursor) {
            // 起点按扇区对齐；设备末尾不满一个扇区时，这一块比整块多出不到一个扇区
            let offset = align_down(end.saturating_sub(self.block), self.sector).max(start);
            let length = end - offset;
            cursor = offset;
            if self.read(offset, length)? {
                skip = 0;
                continue;
            }
            self.map.set(offset, length, SegmentState::Trim);
            skip = self.next_skip(skip);
            cursor = cursor.saturating_sub(skip);
        }
        Ok(())
    }

    fn next_skip(&self, skip: u64) -> u64 {
        if skip == 0 {
            self.skip_min
        } else {
            skip.saturating_mul(2).min(self.skip_max)
        }
    }

    /// 修边：每个待修边的区段从两端按扇区向内读，碰到读错误就停；中间剩下的留给细读。
    fn trim(&mut self) -> Result<(), Interrupt> {
        let mut cursor = 0;
        while let Some((start, end)) = self.scope.next_in(self.map, SegmentState::Trim, cursor) {
            let mut front = start;
            while front < end {
                let length = self.sector.min(end - front);
                let ok = self.read(front, length)?;
                if !ok {
                    self.map.set(front, length, SegmentState::Bad);
                }
                front += length;
                if !ok {
                    break;
                }
            }
            let mut back = end;
            while back > front {
                let offset = align_down(back - 1, self.sector).max(front);
                let ok = self.read(offset, back - offset)?;
                if !ok {
                    self.map.set(offset, back - offset, SegmentState::Bad);
                }
                back = offset;
                if !ok {
                    break;
                }
            }
            self.map.set(front, back - front, SegmentState::Scrape);
            cursor = end;
        }
        Ok(())
    }

    /// 细读：逐扇区读一遍待细读的区段。
    fn scrape(&mut self) -> Result<(), Interrupt> {
        let mut cursor = 0;
        while let Some((start, end)) = self.scope.next_in(self.map, SegmentState::Scrape, cursor) {
            let mut position = start;
            while position < end {
                let length = self.sector.min(end - position);
                if !self.read(position, length)? {
                    self.map.set(position, length, SegmentState::Bad);
                }
                position += length;
            }
            cursor = end;
        }
        Ok(())
    }

    /// 读一段：读出时写进镜像、记为已读，返回 `true`；读错时返回 `false`，由调用方决定记成什么。
    /// 读错且最近读成功的扇区也读不出时，按设备已断开结束本次运行。
    fn read(&mut self, offset: u64, length: u64) -> Result<bool, Interrupt> {
        if self.control.is_cancelled() {
            return Err(Interrupt::Cancelled);
        }
        let Some(buffer) = usize::try_from(length)
            .ok()
            .and_then(|length| self.buffer.get_mut(..length))
        else {
            return Err(Interrupt::Store(io::Error::other("读取长度超出缓冲区")));
        };
        let ok = match self.device.read_exact_at(offset, buffer) {
            Ok(()) => {
                self.store.write_at(offset, buffer)?;
                self.map.set(offset, length, SegmentState::Done);
                // 记下这一段的最后一个扇区，下次出错时重读它，离磁头当前位置最近
                self.last_good = Some(align_down(offset + length - 1, self.sector));
                true
            }
            Err(error) => {
                self.control.read_failed(offset, length, &error);
                self.ensure_source_alive(offset, error)?;
                false
            }
        };
        self.control.progress(self.round, self.map);
        if self.last_commit.elapsed() >= self.commit_interval {
            self.commit()?;
        }
        Ok(ok)
    }

    fn ensure_source_alive(&mut self, offset: u64, error: IoError) -> Result<(), Interrupt> {
        let Some(good) = self.last_good else {
            return Ok(());
        };
        let length = self.sector.min(self.device.size() - good) as usize;
        let mut probe = vec![0; length];
        match self.device.read_exact_at(good, &mut probe) {
            Ok(()) => Ok(()),
            Err(probe_error) => {
                self.control.read_failed(good, length as u64, &probe_error);
                Err(Interrupt::SourceLost { offset, error })
            }
        }
    }

    fn commit(&mut self) -> Result<(), Interrupt> {
        self.store.commit(self.map)?;
        self.last_commit = Instant::now();
        Ok(())
    }
}

fn align_down(value: u64, alignment: u64) -> u64 {
    value - value % alignment
}

#[cfg(test)]
mod tests;
