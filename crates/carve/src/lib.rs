//! 深度恢复：不依赖文件系统，按块顺序读设备，块开头命中文件头时开始恢复一个文件，
//! 由格式插件判断它在哪里结束，写到输出目录。
//!
//! 三段流水线：读线程顺序读设备，调用线程识别文件，写线程写出，读盘、识别、写盘同时进行。
//!
//! 目前只处理连续存放的文件；碎片重组、块大小探测、断点续传在后续阶段加入。
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod sink;

use std::collections::BTreeMap;
use std::io;
use std::panic;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;
use std::time::Instant;

use yisi_formats::{DataCheck, Recovering, Registry};
use yisi_io::BlockDevice;

pub use sink::{DirectorySink, RecoveredFile, ScanOnlySink, Sink, SinkFile, recovered_name};

/// 每次从设备读这么多再按块处理，进度也按这个粒度报告。
const CHUNK_SIZE: usize = 1 << 20;

/// 读线程最多领先识别这么多段（每段 [`CHUNK_SIZE`]）。
const READ_AHEAD: usize = 4;

/// 一个文件的数据攒到这么多再交给写线程。
const FLUSH_SIZE: usize = 1 << 20;

/// 写线程的队列最多排这么多条；排满时扫描等待。每条数据不超过约 [`FLUSH_SIZE`]，
/// 在途数据因此不超过约 32 MiB。
const QUEUE_DEPTH: usize = 32;

pub struct CarveOptions {
    /// 文件只可能从这个大小的整数倍处开始，通常是扇区大小。
    pub block_size: u32,
    /// 按注册表里的格式序号给出是否启用。
    pub enabled: Vec<bool>,
}

/// 扫描过程中查询取消、报告进度与找到的文件。
pub trait CarveControl {
    /// 每处理一块查询一次。实现可以在这里阻塞（暂停），这时识别停下，读线程预读满队列后也停下。
    fn is_cancelled(&self) -> bool;
    fn progress(&self, done: u64, total: u64);
    fn found(&self, file: &RecoveredFile);
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TypeTotals {
    pub count: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CarveReport {
    pub files: u64,
    pub bytes: u64,
    /// 按扩展名统计。
    pub by_type: BTreeMap<String, TypeTotals>,
    pub unreadable_bytes: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum CarveError {
    #[error("块大小 {0} 不受支持，应为 512 到 1 MiB 之间的 2 的幂")]
    InvalidBlockSize(u32),

    #[error("写出恢复文件失败")]
    Output(#[source] io::Error),
}

/// 扫描整个设备，把找到的文件写进 `sink`。被取消时返回 `None`，已写出的完整文件保留。
///
/// `sink` 在写线程里使用；`control` 只在调用线程里调用。返回前等写线程把排队的文件写完，
/// 报告与 [`CarveControl::found`] 只计入已经写完、改好名的文件。
pub fn carve(
    device: &dyn BlockDevice,
    registry: &Registry,
    options: &CarveOptions,
    sink: &mut (dyn Sink + Send),
    control: &dyn CarveControl,
) -> Result<Option<CarveReport>, CarveError> {
    let block_size = usize::try_from(options.block_size)
        .ok()
        .filter(|size| size.is_power_of_two() && (512..=CHUNK_SIZE).contains(size))
        .ok_or(CarveError::InvalidBlockSize(options.block_size))?;
    let started = Instant::now();
    let mut report = CarveReport::default();

    let finished = thread::scope(|scope| {
        let (commands, inbox) = mpsc::sync_channel(QUEUE_DEPTH);
        let (written, done) = mpsc::channel();
        let writer = scope.spawn(move || write_files(sink, inbox, written));
        let mut output = Output {
            commands,
            done,
            pending: Vec::new(),
            current: None,
            opened: false,
        };
        let mut reader = ChunkReader::spawn(scope, device, block_size);
        let scanned = scan(
            device.size(),
            &mut reader,
            registry,
            options,
            block_size,
            &mut output,
            &mut report,
            control,
        );
        // 关掉读线程的通道，它随即退出
        drop(reader);
        // 关掉队列，等写线程把排着的文件写完
        let Output { commands, done, .. } = output;
        drop(commands);
        let writer_result = writer
            .join()
            .unwrap_or_else(|payload| panic::resume_unwind(payload));
        for file in done.try_iter() {
            record(&file, &mut report, control);
        }
        // 写线程出错时，扫描那边只知道队列断了，真正的原因在这里
        writer_result.map_err(CarveError::Output)?;
        scanned
    })?;

    if !finished {
        return Ok(None);
    }
    let size = device.size();
    control.progress(size, size);
    report.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(Some(report))
}

/// 读设备、识别文件，把要写的内容排进 `output`。扫完返回 `true`，被取消返回 `false`。
#[expect(
    clippy::too_many_arguments,
    reason = "扫描所需的各方都由 carve 现场建好传入"
)]
fn scan(
    size: u64,
    reader: &mut ChunkReader<'_>,
    registry: &Registry,
    options: &CarveOptions,
    block_size: usize,
    output: &mut Output,
    report: &mut CarveReport,
    control: &dyn CarveControl,
) -> Result<bool, CarveError> {
    let block = block_size as u64;
    let mut active: Option<Active> = None;
    let mut position = 0u64;
    let mut next_progress = 0u64;

    loop {
        if position >= size {
            // 设备到头：已知长度且数据够了的文件，或插件认为可以在此结束的文件，算完整；
            // 否则当作误识别，从它的下一块重新找
            let Some(mut current) = active.take() else {
                break;
            };
            let length = current
                .known_length
                .filter(|&length| length <= current.received)
                .or_else(|| current.recovering.finish(current.received));
            match length {
                Some(length) => {
                    output.commit(length)?;
                    break;
                }
                None => {
                    position = current.start + block;
                    output.discard()?;
                    continue;
                }
            }
        }
        if position >= next_progress {
            control.progress(position, size);
            output.collect(report, control);
            next_progress = position + CHUNK_SIZE as u64;
        }
        if control.is_cancelled() {
            if active.take().is_some() {
                output.discard()?;
            }
            return Ok(false);
        }

        let Some(data) = reader.block(position) else {
            // 读不出的块：正在恢复的文件缺了一段，放弃它
            report.unreadable_bytes += block.min(size - position);
            if active.take().is_some() {
                output.discard()?;
            }
            position += block;
            continue;
        };

        // 没有结束标志的格式遇到下一个文件头时结束，这一块留给新文件
        if let Some(current) = &active
            && position > current.start
            && current.recovering.open_ended()
            && registry.match_header(data, &options.enabled).is_some()
            && let Some(mut current) = active.take()
        {
            match current.recovering.finish(current.received) {
                Some(length) => output.commit(length)?,
                None => output.discard()?,
            }
            continue;
        }

        if active.is_none() {
            let Some(found) = registry.match_header(data, &options.enabled) else {
                position += block;
                continue;
            };
            let Some(format) = registry.formats().get(found.format) else {
                position += block;
                continue;
            };
            let info = format.info();
            output.begin(position, info.extension);
            active = Some(Active {
                start: position,
                max_size: info.max_size,
                limit: info.max_size.min(size - position),
                recovering: found.recovering,
                received: 0,
                known_length: None,
            });
        }
        let Some(current) = active.as_mut() else {
            continue;
        };

        match feed(current, output, data)? {
            Fed::More => position += block,
            Fed::Complete {
                length,
                before_block,
            } => {
                active = None;
                output.commit(length)?;
                if !before_block {
                    position += block;
                }
            }
            Fed::Invalid => {
                if let Some(current) = active.take() {
                    // 误识别的文件头：从它的下一块重新找，它覆盖过的块里可能有真正的文件
                    position = current.start + block;
                    output.discard()?;
                }
            }
        }
    }
    Ok(true)
}

struct Active {
    start: u64,
    max_size: u64,
    /// 文件长度的上限：格式的最大尺寸与设备剩余容量中较小的一个。
    limit: u64,
    recovering: Box<dyn Recovering>,
    received: u64,
    /// 插件给出的文件长度超过已收到的数据时，记下长度，之后按长度取数据。
    known_length: Option<u64>,
}

enum Fed {
    More,
    /// 文件完整。`before_block`：文件在这一块之前就结束了，这一块不属于它，要重新当作可能的文件头检查。
    Complete {
        length: u64,
        before_block: bool,
    },
    Invalid,
}

fn feed(active: &mut Active, output: &mut Output, data: &[u8]) -> Result<Fed, CarveError> {
    let data_len = data.len() as u64;
    if let Some(length) = active.known_length {
        let take = length.saturating_sub(active.received).min(data_len);
        write(
            active,
            output,
            data.get(..take as usize).unwrap_or_default(),
        )?;
        return Ok(if active.received >= length {
            Fed::Complete {
                length,
                before_block: false,
            }
        } else {
            Fed::More
        });
    }

    match active.recovering.data_check(active.received, data) {
        DataCheck::Continue => {
            if active.received + data_len > active.max_size {
                // 超过最大尺寸：没有结束标志的格式在此收尾，其他格式视为误识别
                if active.recovering.open_ended() {
                    return Ok(match active.recovering.finish(active.received) {
                        Some(length) => Fed::Complete {
                            length,
                            before_block: true,
                        },
                        None => Fed::Invalid,
                    });
                }
                return Ok(Fed::Invalid);
            }
            // 等待的位置已超出上限：长度字段离谱，多半是误识别，不必再读下去
            if active
                .recovering
                .waiting_at()
                .is_some_and(|at| at > active.limit)
            {
                return Ok(Fed::Invalid);
            }
            write(active, output, data)?;
            Ok(Fed::More)
        }
        DataCheck::Complete(length) => {
            if length > active.limit || length == 0 {
                return Ok(Fed::Invalid);
            }
            if length <= active.received {
                return Ok(Fed::Complete {
                    length,
                    before_block: true,
                });
            }
            let in_this_block = length - active.received;
            if in_this_block <= data_len {
                write(
                    active,
                    output,
                    data.get(..in_this_block as usize).unwrap_or_default(),
                )?;
                Ok(Fed::Complete {
                    length,
                    before_block: false,
                })
            } else {
                write(active, output, data)?;
                active.known_length = Some(length);
                Ok(Fed::More)
            }
        }
        DataCheck::Invalid => Ok(Fed::Invalid),
    }
}

fn write(active: &mut Active, output: &mut Output, data: &[u8]) -> Result<(), CarveError> {
    output.write(data)?;
    active.received += data.len() as u64;
    Ok(())
}

fn record(file: &RecoveredFile, report: &mut CarveReport, control: &dyn CarveControl) {
    report.files += 1;
    report.bytes += file.length;
    let totals = report.by_type.entry(file.extension.clone()).or_default();
    totals.count += 1;
    totals.bytes += file.length;
    control.found(file);
}

/// 交给写线程的操作，按顺序执行；同一时间只有一个文件在写。
enum Command {
    Create {
        offset: u64,
        extension: &'static str,
    },
    Write(Vec<u8>),
    Commit(u64),
    Discard,
}

/// 扫描这一侧的写出接口：把当前文件的数据攒够 [`FLUSH_SIZE`] 再排进队列。
/// 文件在第一次交出数据之前就被放弃时，写线程完全不知道它，输出盘上不留任何操作。
struct Output {
    commands: SyncSender<Command>,
    done: Receiver<RecoveredFile>,
    pending: Vec<u8>,
    /// 当前文件的起始偏移与扩展名。
    current: Option<(u64, &'static str)>,
    /// 写线程是否已经为当前文件建了 `.part`。
    opened: bool,
}

impl Output {
    fn begin(&mut self, offset: u64, extension: &'static str) {
        self.pending.clear();
        self.current = Some((offset, extension));
        self.opened = false;
    }

    fn write(&mut self, data: &[u8]) -> Result<(), CarveError> {
        self.pending.extend_from_slice(data);
        if self.pending.len() >= FLUSH_SIZE {
            self.flush()?;
        }
        Ok(())
    }

    fn commit(&mut self, length: u64) -> Result<(), CarveError> {
        self.flush()?;
        self.current = None;
        self.opened = false;
        self.send(Command::Commit(length))
    }

    fn discard(&mut self) -> Result<(), CarveError> {
        self.pending.clear();
        self.current = None;
        if std::mem::take(&mut self.opened) {
            self.send(Command::Discard)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), CarveError> {
        if !self.opened
            && let Some((offset, extension)) = self.current
        {
            self.send(Command::Create { offset, extension })?;
            self.opened = true;
        }
        if !self.pending.is_empty() {
            let data = std::mem::take(&mut self.pending);
            self.send(Command::Write(data))?;
        }
        Ok(())
    }

    /// 队列排满时等待；写线程已经退出时报错，原因由 [`carve`] 从写线程取回。
    fn send(&self, command: Command) -> Result<(), CarveError> {
        self.commands
            .send(command)
            .map_err(|_| CarveError::Output(io::Error::other("写线程已退出")))
    }

    /// 把写线程已经写完的文件计入报告并通知。
    fn collect(&self, report: &mut CarveReport, control: &dyn CarveControl) {
        for file in self.done.try_iter() {
            record(&file, report, control);
        }
    }
}

/// 写线程：按顺序执行队列里的操作，每写完一个文件把结果送回。出错即停，返回错误。
fn write_files(
    sink: &mut (dyn Sink + Send),
    commands: Receiver<Command>,
    written: Sender<RecoveredFile>,
) -> io::Result<()> {
    let mut current: Option<Box<dyn SinkFile>> = None;
    for command in commands {
        match command {
            Command::Create { offset, extension } => {
                current = Some(sink.create(offset, extension)?);
            }
            Command::Write(data) => {
                if let Some(file) = current.as_mut() {
                    file.write(&data)?;
                }
            }
            Command::Commit(length) => {
                if let Some(file) = current.take() {
                    // 调用线程 panic 时没人接收，文件照样留在磁盘上
                    let _ = written.send(file.commit(length)?);
                }
            }
            Command::Discard => {
                if let Some(file) = current.take() {
                    file.discard()?;
                }
            }
        }
    }
    Ok(())
}

/// 读线程读好的一段。`generation` 标明它属于哪一次连续读：扫描回头重找时换一个编号，旧编号的段丢掉。
struct Chunk {
    generation: u64,
    start: u64,
    data: Vec<u8>,
    unreadable: Vec<bool>,
}

/// 扫描这一侧的读取接口。读线程按 1 MiB 一段顺序读，读好的排在有界队列里，读盘与识别同时进行；
/// 要的位置不是紧接着的下一段时（误识别后回头重找），请读线程从新位置重读。
struct ChunkReader<'scope> {
    block_size: usize,
    chunks: Receiver<Chunk>,
    seeks: Sender<(u64, u64)>,
    /// 用完的缓冲区还给读线程，不必每段重新分配 1 MiB。
    spare: Sender<Vec<u8>>,
    reader: Option<thread::ScopedJoinHandle<'scope, ()>>,
    current: Option<Chunk>,
    generation: u64,
    /// 读线程按顺序接下来会送来的那一段的起点。
    next_start: u64,
}

impl<'scope> ChunkReader<'scope> {
    fn spawn<'env>(
        scope: &'scope thread::Scope<'scope, 'env>,
        device: &'env dyn BlockDevice,
        block_size: usize,
    ) -> Self {
        let (chunk_sender, chunks) = mpsc::sync_channel(READ_AHEAD);
        let (seeks, seek_requests) = mpsc::channel();
        let (spare, spare_buffers) = mpsc::channel();
        let reader = scope.spawn(move || {
            read_chunks(
                device,
                block_size,
                chunk_sender,
                seek_requests,
                spare_buffers,
            );
        });
        Self {
            block_size,
            chunks,
            seeks,
            spare,
            reader: Some(reader),
            current: None,
            generation: 0,
            next_start: 0,
        }
    }

    /// `offset` 必须是块大小的整数倍且小于设备容量。读不出时返回 `None`。
    fn block(&mut self, offset: u64) -> Option<&[u8]> {
        let loaded = self.current.as_ref().is_some_and(|chunk| {
            offset >= chunk.start && offset < chunk.start + chunk.data.len() as u64
        });
        if !loaded {
            self.load(offset - offset % CHUNK_SIZE as u64);
        }
        let chunk = self.current.as_ref()?;
        let begin = usize::try_from(offset.checked_sub(chunk.start)?).ok()?;
        if chunk
            .unreadable
            .get(begin / self.block_size)
            .copied()
            .unwrap_or(true)
        {
            return None;
        }
        let end = (begin + self.block_size).min(chunk.data.len());
        chunk.data.get(begin..end)
    }

    /// 换成从 `start` 开始的那一段。
    fn load(&mut self, start: u64) {
        if let Some(used) = self.current.take() {
            let _ = self.spare.send(used.data);
        }
        if start != self.next_start {
            self.seek(start);
        }
        loop {
            let Ok(chunk) = self.chunks.recv() else {
                self.reader_stopped();
                return;
            };
            if chunk.generation != self.generation {
                let _ = self.spare.send(chunk.data);
                continue;
            }
            if chunk.start != start {
                // 按顺序读来的段对不上要的位置，不该发生；按新位置重读
                let _ = self.spare.send(chunk.data);
                self.seek(start);
                continue;
            }
            self.next_start = chunk.start + chunk.data.len() as u64;
            self.current = Some(chunk);
            return;
        }
    }

    fn seek(&mut self, start: u64) {
        self.generation += 1;
        let _ = self.seeks.send((self.generation, start));
    }

    /// 读线程只在 panic 时才会先于扫描退出：把 panic 原样续抛，不把整块盘当成读不出。
    fn reader_stopped(&mut self) {
        if let Some(reader) = self.reader.take()
            && let Err(payload) = reader.join()
        {
            panic::resume_unwind(payload);
        }
    }
}

/// 读线程：从 0 开始按段顺序读；收到重定位请求就从新位置接着读；读到设备末尾后等请求，扫描结束（通道关闭）时退出。
fn read_chunks(
    device: &dyn BlockDevice,
    block_size: usize,
    chunks: SyncSender<Chunk>,
    seeks: Receiver<(u64, u64)>,
    spare: Receiver<Vec<u8>>,
) {
    let size = device.size();
    let (mut generation, mut next) = (0u64, 0u64);
    loop {
        if let Some((latest, start)) = seeks.try_iter().last() {
            (generation, next) = (latest, start);
        }
        if next >= size {
            match seeks.recv() {
                Ok((latest, start)) => (generation, next) = (latest, start),
                Err(_) => return,
            }
            continue;
        }
        let buffer = spare.try_recv().unwrap_or_default();
        let chunk = read_chunk(device, block_size, generation, next, buffer);
        next += chunk.data.len() as u64;
        if chunks.send(chunk).is_err() {
            return;
        }
    }
}

/// 读一段；整段读不出时逐块重读，只把真正读不出的块标出来。
fn read_chunk(
    device: &dyn BlockDevice,
    block_size: usize,
    generation: u64,
    start: u64,
    mut data: Vec<u8>,
) -> Chunk {
    let remaining = device.size().saturating_sub(start);
    let len = usize::try_from(remaining.min(CHUNK_SIZE as u64)).unwrap_or(CHUNK_SIZE);
    data.resize(len, 0);
    let mut unreadable = vec![false; len.div_ceil(block_size)];
    if device.read_exact_at(start, &mut data).is_err() {
        for (index, piece) in data.chunks_mut(block_size).enumerate() {
            let at = start + (index * block_size) as u64;
            if device.read_exact_at(at, piece).is_err()
                && let Some(flag) = unreadable.get_mut(index)
            {
                *flag = true;
            }
        }
    }
    Chunk {
        generation,
        start,
        data,
        unreadable,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::sync::{Arc, Mutex};

    use yisi_formats::samples::{self, Noise};
    use yisi_io::IoError;

    use super::*;

    const BLOCK: usize = 512;

    struct MemoryDevice {
        data: Vec<u8>,
        bad: Vec<(u64, u64)>,
    }

    impl BlockDevice for MemoryDevice {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }

        fn sector_size(&self) -> u32 {
            BLOCK as u32
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
            let end = offset + buf.len() as u64;
            if self
                .bad
                .iter()
                .any(|&(start, stop)| offset < stop && start < end)
            {
                return Err(IoError::Read {
                    offset,
                    len: buf.len(),
                    source: io::Error::other("模拟的坏扇区"),
                });
            }
            let start = (offset as usize).min(self.data.len());
            let stop = (start + buf.len()).min(self.data.len());
            buf[..stop - start].copy_from_slice(&self.data[start..stop]);
            Ok(stop - start)
        }
    }

    type Saved = Arc<Mutex<Vec<(RecoveredFile, Vec<u8>)>>>;

    #[derive(Default)]
    struct MemorySink {
        saved: Saved,
        /// 建过多少个文件，包括后来丢弃的。
        created: usize,
    }

    struct MemoryFile {
        saved: Saved,
        offset: u64,
        extension: String,
        data: Vec<u8>,
    }

    impl Sink for MemorySink {
        fn create(&mut self, offset: u64, extension: &str) -> io::Result<Box<dyn SinkFile>> {
            self.created += 1;
            Ok(Box::new(MemoryFile {
                saved: Arc::clone(&self.saved),
                offset,
                extension: extension.to_owned(),
                data: Vec::new(),
            }))
        }
    }

    impl SinkFile for MemoryFile {
        fn write(&mut self, data: &[u8]) -> io::Result<()> {
            self.data.extend_from_slice(data);
            Ok(())
        }

        fn commit(mut self: Box<Self>, length: u64) -> io::Result<RecoveredFile> {
            self.data.truncate(length as usize);
            let file = RecoveredFile {
                name: format!("f{}.{}", self.offset, self.extension),
                extension: self.extension.clone(),
                offset: self.offset,
                length,
            };
            self.saved.lock().unwrap().push((file.clone(), self.data));
            Ok(file)
        }

        fn discard(self: Box<Self>) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct Recorder {
        cancel_after_progress: Option<usize>,
        progress_calls: Cell<usize>,
        found: RefCell<Vec<RecoveredFile>>,
    }

    impl CarveControl for Recorder {
        fn is_cancelled(&self) -> bool {
            self.cancel_after_progress
                .is_some_and(|limit| self.progress_calls.get() > limit)
        }

        fn progress(&self, _done: u64, _total: u64) {
            self.progress_calls.set(self.progress_calls.get() + 1);
        }

        fn found(&self, file: &RecoveredFile) {
            self.found.borrow_mut().push(file.clone());
        }
    }

    /// 测试镜像：文件都从块边界开始，文件之间、末尾补随机数据。
    struct ImageBuilder {
        data: Vec<u8>,
        noise: Noise,
        placed: Vec<(u64, &'static str, Vec<u8>)>,
    }

    impl ImageBuilder {
        fn new() -> Self {
            Self {
                data: Vec::new(),
                noise: Noise::new(7),
                placed: Vec::new(),
            }
        }

        /// 补随机数据到块边界，再追加 `blocks` 块随机数据。块开头不会恰好像文件头。
        fn garbage(&mut self, blocks: usize) -> &mut Self {
            let registry = Registry::builtin();
            let all = vec![true; registry.formats().len()];
            let pad = (BLOCK - self.data.len() % BLOCK) % BLOCK;
            let pad_bytes = self.noise.bytes(pad);
            self.data.extend(pad_bytes);
            for _ in 0..blocks {
                loop {
                    let candidate = self.noise.bytes(BLOCK);
                    if registry.match_header(&candidate, &all).is_none() {
                        self.data.extend(candidate);
                        break;
                    }
                }
            }
            self
        }

        fn file(&mut self, extension: &'static str, bytes: Vec<u8>) -> &mut Self {
            self.garbage(0);
            self.placed
                .push((self.data.len() as u64, extension, bytes.clone()));
            self.data.extend(bytes);
            self
        }

        fn device(&self, bad: Vec<(u64, u64)>) -> MemoryDevice {
            MemoryDevice {
                data: self.data.clone(),
                bad,
            }
        }
    }

    fn run(
        device: &MemoryDevice,
        enabled: Option<Vec<bool>>,
        control: &Recorder,
    ) -> (Option<CarveReport>, Vec<(RecoveredFile, Vec<u8>)>) {
        let (report, sink) = run_into_sink(device, enabled, control);
        let saved = sink.saved.lock().unwrap().clone();
        (report, saved)
    }

    fn run_into_sink(
        device: &MemoryDevice,
        enabled: Option<Vec<bool>>,
        control: &Recorder,
    ) -> (Option<CarveReport>, MemorySink) {
        let registry = Registry::builtin();
        let options = CarveOptions {
            block_size: BLOCK as u32,
            enabled: enabled.unwrap_or_else(|| vec![true; registry.formats().len()]),
        };
        let mut sink = MemorySink::default();
        let report = carve(device, &registry, &options, &mut sink, control).unwrap();
        (report, sink)
    }

    fn five_formats() -> ImageBuilder {
        let mut image = ImageBuilder::new();
        image
            .garbage(3)
            .file("jpg", samples::jpg(5000))
            .garbage(1)
            .file("png", samples::png(&[100, 70_000]))
            .file(
                "zip",
                samples::zip(&[("a.txt", 900, true), ("b", 40, false)], b"note"),
            )
            .garbage(2)
            .file("pdf", samples::pdf(4, true))
            .garbage(1)
            .file("mp4", samples::mp4(20_000, false))
            .garbage(2);
        image
    }

    #[test]
    fn recovers_every_file_byte_for_byte() {
        let image = five_formats();
        let control = Recorder::default();
        let (report, saved) = run(&image.device(vec![]), None, &control);
        let report = report.unwrap();

        assert_eq!(saved.len(), image.placed.len());
        for ((offset, extension, bytes), (file, data)) in image.placed.iter().zip(&saved) {
            assert_eq!(
                (file.offset, file.extension.as_str()),
                (*offset, *extension)
            );
            assert_eq!(file.length, bytes.len() as u64, "{extension}");
            assert!(data == bytes, "{extension} 内容不一致");
        }
        assert_eq!(report.files, 5);
        assert_eq!(report.by_type.len(), 5);
        assert_eq!(report.unreadable_bytes, 0);
        assert_eq!(control.found.borrow().len(), 5);
    }

    #[test]
    fn disabled_formats_are_skipped() {
        let image = five_formats();
        let registry = Registry::builtin();
        let mut enabled = vec![true; registry.formats().len()];
        enabled[registry.position("png").unwrap()] = false;
        let (_, saved) = run(&image.device(vec![]), Some(enabled), &Recorder::default());
        let extensions: Vec<_> = saved
            .iter()
            .map(|(file, _)| file.extension.as_str())
            .collect();
        assert_eq!(extensions, ["jpg", "zip", "pdf", "mp4"]);
    }

    #[test]
    fn unreadable_block_drops_only_the_file_it_hits() {
        let image = five_formats();
        let (png_offset, _, png) = image.placed[1].clone();
        let bad_start = png_offset + 4 * BLOCK as u64;
        let bad = vec![(bad_start, bad_start + BLOCK as u64)];
        let (report, saved) = run(&image.device(bad), None, &Recorder::default());
        let extensions: Vec<_> = saved
            .iter()
            .map(|(file, _)| file.extension.as_str())
            .collect();
        assert_eq!(extensions, ["jpg", "zip", "pdf", "mp4"]);
        assert_eq!(report.unwrap().unreadable_bytes, BLOCK as u64);
        assert!(png.len() > 5 * BLOCK);
    }

    #[test]
    fn false_header_is_abandoned_and_files_inside_it_still_found() {
        let mut image = ImageBuilder::new();
        // 像 ZIP 本地文件头的垃圾，后面不是 ZIP 结构
        let mut fake = b"PK\x03\x04\x14\x00\x00\x00\x00\x00".to_vec();
        fake.resize(BLOCK, 0x41);
        image
            .file("fake", fake)
            .file("png", samples::png(&[50]))
            .garbage(1);
        let (_, saved) = run(&image.device(vec![]), None, &Recorder::default());
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].0.extension, "png");
        assert!(saved[0].1 == image.placed[1].2);
    }

    #[test]
    fn candidate_unfinished_at_device_end_is_rescanned_from_its_next_block() {
        let mut image = ImageBuilder::new();
        // 像 JPEG 的开头，扫描段之后再也没有 0xFF：要一直读到设备末尾才知道它不完整
        let mut fake = b"\xFF\xD8\xFF\xE0\x00\x04ab\xFF\xDA\x00\x04cd".to_vec();
        fake.resize(BLOCK, 0x41);
        image.file("fake", fake);
        let png = samples::png(&[60]);
        let mut png_block = png.clone();
        png_block.resize(BLOCK, 0x41);
        image.file("png", png_block);
        image.data.resize(image.data.len() + 3 * BLOCK, 0x41);
        let (_, saved) = run(&image.device(vec![]), None, &Recorder::default());
        assert_eq!(saved.len(), 1);
        assert!(saved[0].1 == png);
    }

    #[test]
    fn rescan_after_a_long_false_candidate_reads_earlier_chunks_again() {
        let mut image = ImageBuilder::new();
        // 像 JPEG 的开头，之后再也没有 0xFF：一直读到 3 MiB 外的设备末尾才知道它不完整，
        // 回头重找时要让读线程退回好几段重读
        let mut fake = b"\xFF\xD8\xFF\xE0\x00\x04ab\xFF\xDA\x00\x04cd".to_vec();
        fake.resize(BLOCK, 0x41);
        image.file("fake", fake);
        let png = samples::png(&[60]);
        let mut png_block = png.clone();
        png_block.resize(BLOCK, 0x41);
        image.file("png", png_block);
        image.data.resize(image.data.len() + 3 * CHUNK_SIZE, 0x41);
        let (report, saved) = run(&image.device(vec![]), None, &Recorder::default());
        assert_eq!(saved.len(), 1);
        assert!(saved[0].1 == png);
        assert_eq!(report.unwrap().unreadable_bytes, 0);
    }

    #[test]
    fn unreadable_block_in_a_later_chunk_drops_only_its_file() {
        let mut image = ImageBuilder::new();
        image
            .garbage(CHUNK_SIZE / BLOCK + 100)
            .file("png", samples::png(&[3000]))
            .garbage(1)
            .file("jpg", samples::jpg(2000))
            .garbage(1);
        let (png_offset, _, _) = image.placed[0].clone();
        let bad_start = png_offset + 2 * BLOCK as u64;
        let bad = vec![(bad_start, bad_start + BLOCK as u64)];
        let (report, saved) = run(&image.device(bad), None, &Recorder::default());
        let extensions: Vec<_> = saved
            .iter()
            .map(|(file, _)| file.extension.as_str())
            .collect();
        assert_eq!(extensions, ["jpg"]);
        assert!(saved[0].1 == image.placed[1].2);
        assert_eq!(report.unwrap().unreadable_bytes, BLOCK as u64);
        assert!(png_offset > CHUNK_SIZE as u64);
    }

    #[test]
    fn file_at_the_very_end_of_the_device_is_kept() {
        let mut image = ImageBuilder::new();
        image.garbage(1).file("mp4", samples::mp4(3000, false));
        let (_, saved) = run(&image.device(vec![]), None, &Recorder::default());
        assert_eq!(saved.len(), 1);
        assert!(saved[0].1 == image.placed[0].2);
    }

    #[test]
    fn cancellation_returns_none() {
        let mut image = ImageBuilder::new();
        image.garbage(4096); // 2 MiB，进度至少报告两次
        let control = Recorder {
            cancel_after_progress: Some(1),
            ..Recorder::default()
        };
        let (report, _) = run(&image.device(vec![]), None, &control);
        assert!(report.is_none());
    }

    #[test]
    fn rejects_unsupported_block_size() {
        let image = ImageBuilder::new();
        let registry = Registry::builtin();
        let options = CarveOptions {
            block_size: 300,
            enabled: vec![true; registry.formats().len()],
        };
        let result = carve(
            &image.device(vec![]),
            &registry,
            &options,
            &mut MemorySink::default(),
            &Recorder::default(),
        );
        assert!(matches!(result, Err(CarveError::InvalidBlockSize(300))));
    }

    #[test]
    fn file_larger_than_one_flush_is_written_in_pieces_intact() {
        let mut image = ImageBuilder::new();
        image
            .garbage(1)
            .file("mp4", samples::mp4(FLUSH_SIZE * 3 + 100, false))
            .garbage(1);
        let (_, sink) = run_into_sink(&image.device(vec![]), None, &Recorder::default());
        let saved = sink.saved.lock().unwrap();
        assert_eq!(saved.len(), 1);
        assert!(saved[0].1 == image.placed[0].2);
        assert_eq!(sink.created, 1);
    }

    #[test]
    fn candidate_abandoned_before_its_first_flush_never_touches_the_sink() {
        let mut image = ImageBuilder::new();
        let mut fake = b"PK\x03\x04\x14\x00\x00\x00\x00\x00".to_vec();
        fake.resize(BLOCK, 0x41);
        image
            .file("fake", fake)
            .file("png", samples::png(&[50]))
            .garbage(1);
        let (_, sink) = run_into_sink(&image.device(vec![]), None, &Recorder::default());
        assert_eq!(sink.saved.lock().unwrap().len(), 1);
        assert_eq!(sink.created, 1, "误识别的 ZIP 不应建文件");
    }

    #[test]
    fn cancellation_keeps_and_reports_files_already_queued() {
        let mut image = five_formats();
        image.garbage(4096); // 五个文件都在第一个 1 MiB 里，第二次报进度后取消
        let control = Recorder {
            cancel_after_progress: Some(1),
            ..Recorder::default()
        };
        let (report, saved) = run(&image.device(vec![]), None, &control);
        assert!(report.is_none());
        assert_eq!(saved.len(), 5);
        assert_eq!(
            *control.found.borrow(),
            saved
                .iter()
                .map(|(file, _)| file.clone())
                .collect::<Vec<_>>()
        );
    }

    struct FailingSink;

    impl Sink for FailingSink {
        fn create(&mut self, _offset: u64, _extension: &str) -> io::Result<Box<dyn SinkFile>> {
            Err(io::Error::other("输出盘已满"))
        }
    }

    #[test]
    fn writer_error_is_returned_with_its_own_cause() {
        let registry = Registry::builtin();
        let options = CarveOptions {
            block_size: BLOCK as u32,
            enabled: vec![true; registry.formats().len()],
        };
        // 五个文件：写线程在第一个上出错后，扫描还要再交出后面几个，发送失败。
        // 一个文件：扫描先结束，错误要在等写线程时取回。
        let mut single = ImageBuilder::new();
        single.garbage(1).file("png", samples::png(&[80]));
        for image in [five_formats(), single] {
            let control = Recorder::default();
            let result = carve(
                &image.device(vec![]),
                &registry,
                &options,
                &mut FailingSink,
                &control,
            );
            match result {
                Err(CarveError::Output(error)) => assert_eq!(error.to_string(), "输出盘已满"),
                other => panic!("应返回写出错误，实际 {other:?}"),
            }
            assert!(control.found.borrow().is_empty());
        }
    }
}
