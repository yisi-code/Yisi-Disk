//! 深度恢复：不依赖文件系统，按块顺序读设备，块开头命中文件头时开始恢复一个文件，
//! 由格式插件判断它在哪里结束，写到输出目录。
//!
//! 目前只处理连续存放的文件；碎片重组、块大小探测、断点续传在后续阶段加入。
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod sink;

use std::collections::BTreeMap;
use std::io;
use std::time::Instant;

use yisi_formats::{DataCheck, Recovering, Registry};
use yisi_io::BlockDevice;

pub use sink::{DirectorySink, RecoveredFile, Sink, SinkFile};

/// 每次从设备读这么多再按块处理，进度也按这个粒度报告。
const CHUNK_SIZE: usize = 1 << 20;

pub struct CarveOptions {
    /// 文件只可能从这个大小的整数倍处开始，通常是扇区大小。
    pub block_size: u32,
    /// 按注册表里的格式序号给出是否启用。
    pub enabled: Vec<bool>,
}

/// 扫描过程中查询取消、报告进度与找到的文件。
pub trait CarveControl {
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
pub fn carve(
    device: &dyn BlockDevice,
    registry: &Registry,
    options: &CarveOptions,
    sink: &mut dyn Sink,
    control: &dyn CarveControl,
) -> Result<Option<CarveReport>, CarveError> {
    let block_size = usize::try_from(options.block_size)
        .ok()
        .filter(|size| size.is_power_of_two() && (512..=CHUNK_SIZE).contains(size))
        .ok_or(CarveError::InvalidBlockSize(options.block_size))?;
    let block = block_size as u64;
    let started = Instant::now();
    let size = device.size();
    let mut reader = ChunkReader::new(device, block_size);
    let mut report = CarveReport::default();
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
                    commit(current, length, &mut report, control)?;
                    break;
                }
                None => {
                    position = current.start + block;
                    current.file.discard().map_err(CarveError::Output)?;
                    continue;
                }
            }
        }
        if position >= next_progress {
            control.progress(position, size);
            next_progress = position + CHUNK_SIZE as u64;
        }
        if control.is_cancelled() {
            if let Some(current) = active.take() {
                current.file.discard().map_err(CarveError::Output)?;
            }
            return Ok(None);
        }

        let Some(data) = reader.block(position) else {
            // 读不出的块：正在恢复的文件缺了一段，放弃它
            report.unreadable_bytes += block.min(size - position);
            if let Some(current) = active.take() {
                current.file.discard().map_err(CarveError::Output)?;
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
                Some(length) => commit(current, length, &mut report, control)?,
                None => current.file.discard().map_err(CarveError::Output)?,
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
            let file = sink
                .create(position, info.extension)
                .map_err(CarveError::Output)?;
            active = Some(Active {
                start: position,
                extension: info.extension,
                max_size: info.max_size,
                limit: info.max_size.min(size - position),
                recovering: found.recovering,
                file,
                received: 0,
                known_length: None,
            });
        }
        let Some(current) = active.as_mut() else {
            continue;
        };

        match feed(current, data)? {
            Fed::More => position += block,
            Fed::Complete {
                length,
                before_block,
            } => {
                if let Some(current) = active.take() {
                    commit(current, length, &mut report, control)?;
                }
                if !before_block {
                    position += block;
                }
            }
            Fed::Invalid => {
                if let Some(current) = active.take() {
                    // 误识别的文件头：从它的下一块重新找，它覆盖过的块里可能有真正的文件
                    position = current.start + block;
                    current.file.discard().map_err(CarveError::Output)?;
                }
            }
        }
    }

    control.progress(size, size);
    report.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(Some(report))
}

struct Active {
    start: u64,
    extension: &'static str,
    max_size: u64,
    /// 文件长度的上限：格式的最大尺寸与设备剩余容量中较小的一个。
    limit: u64,
    recovering: Box<dyn Recovering>,
    file: Box<dyn SinkFile>,
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

fn feed(active: &mut Active, data: &[u8]) -> Result<Fed, CarveError> {
    let data_len = data.len() as u64;
    if let Some(length) = active.known_length {
        let take = length.saturating_sub(active.received).min(data_len);
        write(active, data.get(..take as usize).unwrap_or_default())?;
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
            write(active, data)?;
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
                    data.get(..in_this_block as usize).unwrap_or_default(),
                )?;
                Ok(Fed::Complete {
                    length,
                    before_block: false,
                })
            } else {
                write(active, data)?;
                active.known_length = Some(length);
                Ok(Fed::More)
            }
        }
        DataCheck::Invalid => Ok(Fed::Invalid),
    }
}

fn write(active: &mut Active, data: &[u8]) -> Result<(), CarveError> {
    active.file.write(data).map_err(CarveError::Output)?;
    active.received += data.len() as u64;
    Ok(())
}

fn commit(
    active: Active,
    length: u64,
    report: &mut CarveReport,
    control: &dyn CarveControl,
) -> Result<(), CarveError> {
    let extension = active.extension;
    let file = active.file.commit(length).map_err(CarveError::Output)?;
    report.files += 1;
    report.bytes += length;
    let totals = report.by_type.entry(extension.to_owned()).or_default();
    totals.count += 1;
    totals.bytes += length;
    control.found(&file);
    Ok(())
}

/// 按 1 MiB 一段读设备；整段读不出时逐块重读，只把真正读不出的块标出来。
struct ChunkReader<'a> {
    device: &'a dyn BlockDevice,
    block_size: usize,
    data: Vec<u8>,
    start: u64,
    len: usize,
    unreadable: Vec<bool>,
}

impl<'a> ChunkReader<'a> {
    fn new(device: &'a dyn BlockDevice, block_size: usize) -> Self {
        Self {
            device,
            block_size,
            data: vec![0; CHUNK_SIZE],
            start: 0,
            len: 0,
            unreadable: Vec::new(),
        }
    }

    /// `offset` 必须是块大小的整数倍且小于设备容量。读不出时返回 `None`。
    fn block(&mut self, offset: u64) -> Option<&[u8]> {
        let loaded = offset >= self.start && offset < self.start + self.len as u64;
        if !loaded {
            self.load(offset - offset % CHUNK_SIZE as u64);
        }
        let begin = usize::try_from(offset.checked_sub(self.start)?).ok()?;
        if self
            .unreadable
            .get(begin / self.block_size)
            .copied()
            .unwrap_or(true)
        {
            return None;
        }
        let end = (begin + self.block_size).min(self.len);
        self.data.get(begin..end)
    }

    fn load(&mut self, start: u64) {
        let remaining = self.device.size().saturating_sub(start);
        let len = usize::try_from(remaining.min(CHUNK_SIZE as u64)).unwrap_or(CHUNK_SIZE);
        self.start = start;
        self.len = len;
        self.unreadable.clear();
        self.unreadable.resize(len.div_ceil(self.block_size), false);
        let Some(buffer) = self.data.get_mut(..len) else {
            self.unreadable.fill(true);
            return;
        };
        if self.device.read_exact_at(start, buffer).is_ok() {
            return;
        }
        for (index, piece) in buffer.chunks_mut(self.block_size).enumerate() {
            let at = start + (index * self.block_size) as u64;
            if self.device.read_exact_at(at, piece).is_err()
                && let Some(flag) = self.unreadable.get_mut(index)
            {
                *flag = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

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

    type Saved = Rc<RefCell<Vec<(RecoveredFile, Vec<u8>)>>>;

    #[derive(Default)]
    struct MemorySink {
        saved: Saved,
    }

    struct MemoryFile {
        saved: Saved,
        offset: u64,
        extension: String,
        data: Vec<u8>,
    }

    impl Sink for MemorySink {
        fn create(&mut self, offset: u64, extension: &str) -> io::Result<Box<dyn SinkFile>> {
            Ok(Box::new(MemoryFile {
                saved: Rc::clone(&self.saved),
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
            self.saved.borrow_mut().push((file.clone(), self.data));
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
        let registry = Registry::builtin();
        let options = CarveOptions {
            block_size: BLOCK as u32,
            enabled: enabled.unwrap_or_else(|| vec![true; registry.formats().len()]),
        };
        let mut sink = MemorySink::default();
        let report = carve(device, &registry, &options, &mut sink, control).unwrap();
        let saved = sink.saved.borrow().clone();
        (report, saved)
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
}
