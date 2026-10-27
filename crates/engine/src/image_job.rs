//! 制作能识别坏区的镜像：建好或打开镜像文件与坏区图，把 `yisi-io` 的读取进展转成任务通知。
//!
//! 镜像写在另一块盘上，与深度恢复写输出目录是同一类操作；源盘仍然只读。

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use yisi_engine::protocol::{
    ByteRange, IMAGING, ImageStarted, ImageSummary, JobImaging, MAX_UNREADABLE_RANGES, error_code,
};
use yisi_io::{
    BadMap, BlockDevice, ImageStore, ImagingControl, ImagingOptions, ImagingOutcome, IoError,
    MapHeader, Round, SegmentState, run_imaging,
};
use yisi_rpc::ErrorObject;

use crate::job::{JobContext, JobOutcome};
use crate::runtime::error_chain;

/// 两次进展通知之间至少隔这么久，即每秒最多 5 次。
const STATUS_INTERVAL: Duration = Duration::from_millis(200);
/// 读速按最近这么久读出的字节数算。
const RATE_WINDOW: Duration = Duration::from_secs(5);
/// 读速至少要有这么久的样本才给出，刚开始的几次读取不足以说明速度。
const RATE_MIN_SPAN: Duration = Duration::from_secs(1);

/// 准备好的镜像任务：源、坏区图与打开的镜像文件。
pub struct ImageJob {
    device: Box<dyn BlockDevice>,
    map: BadMap,
    store: FileStore,
    image_path: PathBuf,
}

impl ImageJob {
    pub fn started(&self) -> ImageStarted {
        ImageStarted {
            image_path: self.image_path.display().to_string(),
            map_path: self.store.map_path.display().to_string(),
        }
    }
}

/// 坏区图与镜像放在一起：同一路径加 `.map`。
pub fn map_path(image_path: &Path) -> PathBuf {
    with_suffix(image_path, ".map")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// 新建镜像：镜像文件按源的容量预留空间（读不出的区域留作 0），坏区图记为全部未读，先保存一次。
pub fn create(
    device: Box<dyn BlockDevice>,
    image_path: &Path,
    source: String,
) -> Result<ImageJob, ErrorObject> {
    let map_path = map_path(image_path);
    let exists = |path: &Path| fs::symlink_metadata(path).is_ok();
    if exists(image_path) || exists(&map_path) {
        return Err(image_exists(image_path));
    }
    let image = open_image(image_path, true).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            image_exists(image_path)
        } else {
            output_unavailable(format!(
                "无法创建镜像文件 {}：{error}",
                image_path.display()
            ))
        }
    })?;
    let mut store = FileStore::new(image, map_path);
    let size = device.size();
    let prepared = store
        .image
        .set_len(size)
        .map_err(|error| {
            format!(
                "无法为镜像预留 {size} 字节（输出盘空间不足，或者文件系统不支持这么大的文件，例如 FAT32 单个文件最大 4 GiB）：{error}"
            )
        })
        .and_then(|()| {
            let map = BadMap::new(MapHeader {
                size,
                sector_size: device.sector_size(),
                source,
            });
            store
                .commit(&map)
                .map(|()| map)
                .map_err(|error| format!("无法保存坏区图 {}：{error}", store.map_path.display()))
        });
    match prepared {
        Ok(map) => Ok(ImageJob {
            device,
            map,
            store,
            image_path: image_path.to_path_buf(),
        }),
        Err(message) => {
            // 文件是这一次刚建的，先关闭再删掉，不留下会挡住下次新建的空壳
            let FileStore {
                image,
                map_path,
                temp_path,
            } = store;
            drop(image);
            for path in [image_path, map_path.as_path(), temp_path.as_path()] {
                let _ = fs::remove_file(path);
            }
            Err(output_unavailable(message))
        }
    }
}

/// 按坏区图续做：坏区图记录的容量、扇区大小、来源都要与当前的源一致，镜像文件的长度要等于容量。
pub fn resume(
    device: Box<dyn BlockDevice>,
    image_path: &Path,
    source: String,
) -> Result<ImageJob, ErrorObject> {
    let map_path = map_path(image_path);
    let text = fs::read_to_string(&map_path)
        .map_err(|error| mismatch(format!("读不到坏区图 {}：{error}", map_path.display())))?;
    let map = BadMap::parse(&text)
        .map_err(|error| mismatch(format!("坏区图 {} 读不懂：{error}", map_path.display())))?;
    let expected = MapHeader {
        size: device.size(),
        sector_size: device.sector_size(),
        source,
    };
    let recorded = map.header();
    if recorded.size != expected.size {
        return Err(mismatch(format!(
            "坏区图记录的容量是 {} 字节，当前的源是 {} 字节",
            recorded.size, expected.size
        )));
    }
    if recorded.sector_size != expected.sector_size {
        return Err(mismatch(format!(
            "坏区图记录的扇区大小是 {}，当前的源是 {}",
            recorded.sector_size, expected.sector_size
        )));
    }
    // 新建时写进去的来源也经过同样的规整（换行换成空格），这里按规整后的比较
    if recorded.source != BadMap::new(expected.clone()).header().source {
        return Err(mismatch(format!(
            "坏区图记录的来源是「{}」，当前的源是「{}」",
            recorded.source, expected.source
        )));
    }
    let image = open_image(image_path, false).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            mismatch(format!("找不到镜像文件 {}", image_path.display()))
        } else {
            output_unavailable(format!(
                "无法打开镜像文件 {}（可能正被另一个任务使用）：{error}",
                image_path.display()
            ))
        }
    })?;
    let length = image
        .metadata()
        .map_err(|error| output_unavailable(format!("无法查询镜像文件的长度：{error}")))?
        .len();
    if length != expected.size {
        return Err(mismatch(format!(
            "镜像文件长 {length} 字节，与源的容量 {} 字节不符",
            expected.size
        )));
    }
    Ok(ImageJob {
        device,
        map,
        store: FileStore::new(image, map_path),
        image_path: image_path.to_path_buf(),
    })
}

pub fn run(job: ImageJob, context: &JobContext) -> Result<JobOutcome, String> {
    let ImageJob {
        device,
        mut map,
        mut store,
        image_path,
    } = job;
    let started = Instant::now();
    let reporter = Reporter::new(context);
    let outcome = run_imaging(
        device.as_ref(),
        &mut map,
        &mut store,
        &reporter,
        &ImagingOptions::default(),
    );
    // 终结通知之前把最后的进展发出去
    reporter.send(&map);
    let elapsed = started.elapsed();
    let source_lost_at = match outcome {
        Ok(ImagingOutcome::Finished) => None,
        Ok(ImagingOutcome::Cancelled) => return Ok(JobOutcome::Cancelled),
        Ok(ImagingOutcome::SourceLost { offset, error }) => {
            tracing::warn!(offset, error = %error_chain(&error), "源设备读不到了，停下");
            Some(offset)
        }
        Err(error) => return Err(format!("写镜像失败：{error}")),
    };
    let summary = summarize(&map, &image_path, &store.map_path, source_lost_at, elapsed);
    serde_json::to_value(summary)
        .map(JobOutcome::Completed)
        .map_err(|error| format!("无法编码镜像结果：{error}"))
}

fn summarize(
    map: &BadMap,
    image_path: &Path,
    map_path: &Path,
    source_lost_at: Option<u64>,
    elapsed: Duration,
) -> ImageSummary {
    let bad_ranges: Vec<ByteRange> = map
        .ranges(SegmentState::Bad)
        .take(MAX_UNREADABLE_RANGES + 1)
        .map(|(offset, length)| ByteRange { offset, length })
        .collect();
    let bad_ranges_truncated = bad_ranges.len() > MAX_UNREADABLE_RANGES;
    ImageSummary {
        image_path: image_path.display().to_string(),
        map_path: map_path.display().to_string(),
        size: map.header().size,
        read_bytes: map.bytes(SegmentState::Done),
        bad_bytes: map.bytes(SegmentState::Bad),
        unfinished_bytes: unfinished(map),
        bad_ranges: bad_ranges.into_iter().take(MAX_UNREADABLE_RANGES).collect(),
        bad_ranges_truncated,
        source_lost_at,
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    }
}

fn unfinished(map: &BadMap) -> u64 {
    map.bytes(SegmentState::Untried)
        + map.bytes(SegmentState::Trim)
        + map.bytes(SegmentState::Scrape)
}

fn round_name(round: Round) -> &'static str {
    match round {
        Round::Copy => "copy",
        Round::Trim => "trim",
        Round::Scrape => "scrape",
    }
}

/// 以只写方式打开镜像。Windows 上不允许别的程序同时写它，同一个镜像不会被两个任务同时续做。
fn open_image(path: &Path, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true);
    if create {
        options.create_new(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ
        options.share_mode(0x0000_0001);
    }
    options.open(path)
}

/// 镜像文件加坏区图。坏区图先写临时文件再改名替换，任何时候盘上都是一份完整的坏区图。
pub struct FileStore {
    image: File,
    map_path: PathBuf,
    temp_path: PathBuf,
}

impl FileStore {
    fn new(image: File, map_path: PathBuf) -> Self {
        let temp_path = with_suffix(&map_path, ".tmp");
        Self {
            image,
            map_path,
            temp_path,
        }
    }
}

impl ImageStore for FileStore {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < data.len() {
            let position = offset + written as u64;
            let chunk = &data[written..];
            #[cfg(windows)]
            let count = std::os::windows::fs::FileExt::seek_write(&self.image, chunk, position)?;
            #[cfg(unix)]
            let count = std::os::unix::fs::FileExt::write_at(&self.image, chunk, position)?;
            if count == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            written += count;
        }
        Ok(())
    }

    fn commit(&mut self, map: &BadMap) -> io::Result<()> {
        self.image.sync_data()?;
        let mut temp = File::create(&self.temp_path)?;
        temp.write_all(map.to_text().as_bytes())?;
        temp.sync_all()?;
        drop(temp);
        fs::rename(&self.temp_path, &self.map_path)
    }
}

/// 把读取进展转成 `job.progress`（阶段是轮次）与 `job.imaging`，节流到每秒最多 5 次。
struct Reporter<'a> {
    context: &'a JobContext,
    round: Cell<Round>,
    last_sent: Cell<Option<Instant>>,
    /// 最近一段时间里各次发送时的（时刻，已读字节数），用来算读速。
    samples: RefCell<VecDeque<(Instant, u64)>>,
}

impl<'a> Reporter<'a> {
    fn new(context: &'a JobContext) -> Self {
        Self {
            context,
            round: Cell::new(Round::Copy),
            last_sent: Cell::new(None),
            samples: RefCell::new(VecDeque::new()),
        }
    }

    fn send(&self, map: &BadMap) {
        let now = Instant::now();
        self.last_sent.set(Some(now));
        let read_bytes = map.bytes(SegmentState::Done);
        let bytes_per_second = {
            let mut samples = self.samples.borrow_mut();
            samples.push_back((now, read_bytes));
            read_rate(&mut samples, now)
        };
        let size = map.header().size;
        let round = round_name(self.round.get());
        self.context
            .stage_progress(round, size - unfinished(map), size);
        self.context.notify(
            IMAGING,
            JobImaging {
                job_id: self.context.id().to_owned(),
                round: round.to_owned(),
                size,
                read_bytes,
                bad_bytes: map.bytes(SegmentState::Bad),
                untried_bytes: map.bytes(SegmentState::Untried),
                trim_bytes: map.bytes(SegmentState::Trim),
                scrape_bytes: map.bytes(SegmentState::Scrape),
                bytes_per_second,
            },
        );
    }
}

/// 丢掉窗口以外的样本（留下最后一个早于窗口的，作为起点），样本跨度不足时不给读速。
fn read_rate(samples: &mut VecDeque<(Instant, u64)>, now: Instant) -> Option<u64> {
    while samples.len() > 2
        && samples
            .get(1)
            .is_some_and(|&(time, _)| now.duration_since(time) >= RATE_WINDOW)
    {
        samples.pop_front();
    }
    let &(first_time, first_bytes) = samples.front()?;
    let &(_, last_bytes) = samples.back()?;
    let span = now.duration_since(first_time);
    if span < RATE_MIN_SPAN {
        return None;
    }
    let bytes = last_bytes.saturating_sub(first_bytes);
    Some((u128::from(bytes) * 1000 / span.as_millis().max(1)) as u64)
}

impl ImagingControl for Reporter<'_> {
    fn is_cancelled(&self) -> bool {
        self.context.is_cancelled()
    }

    fn progress(&self, round: Round, map: &BadMap) {
        let changed = self.round.replace(round) != round;
        let due = self
            .last_sent
            .get()
            .is_none_or(|sent| sent.elapsed() >= STATUS_INTERVAL);
        if changed || due {
            self.send(map);
        }
    }

    fn read_failed(&self, offset: u64, length: u64, error: &IoError) {
        tracing::warn!(offset, length, error = %error_chain(error), "读不出");
    }
}

fn image_exists(image_path: &Path) -> ErrorObject {
    ErrorObject::new(
        error_code::IMAGE_EXISTS,
        format!("{} 或它的坏区图已经存在", image_path.display()),
    )
}

fn mismatch(message: String) -> ErrorObject {
    ErrorObject::new(error_code::IMAGE_MISMATCH, message)
}

fn output_unavailable(message: String) -> ErrorObject {
    ErrorObject::new(error_code::OUTPUT_UNAVAILABLE, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTOR: u64 = 512;
    const MIB: u64 = 1 << 20;

    /// 内容确定的内存设备，落在坏区里的读取报错。
    struct FakeDevice {
        data: Vec<u8>,
        bad: Vec<(u64, u64)>,
    }

    impl FakeDevice {
        fn new(size: u64, bad: Vec<(u64, u64)>) -> Box<Self> {
            let data = (0..size)
                .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 24) as u8 | 1)
                .collect();
            Box::new(Self { data, bad })
        }

        fn expected_image(&self) -> Vec<u8> {
            let mut image = self.data.clone();
            for &(start, end) in &self.bad {
                image[start as usize..end as usize].fill(0);
            }
            image
        }
    }

    impl BlockDevice for FakeDevice {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }

        fn sector_size(&self) -> u32 {
            SECTOR as u32
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
            let end = (offset + buf.len() as u64).min(self.size());
            if offset >= end {
                return Ok(0);
            }
            if self
                .bad
                .iter()
                .any(|&(start, bad_end)| offset < bad_end && start < end)
            {
                return Err(IoError::Read {
                    offset,
                    len: buf.len(),
                    source: io::Error::other("模拟的介质错误"),
                });
            }
            let len = (end - offset) as usize;
            buf[..len].copy_from_slice(&self.data[offset as usize..end as usize]);
            Ok(len)
        }
    }

    struct Control {
        cancel_after: Option<usize>,
        checks: Cell<usize>,
    }

    impl ImagingControl for Control {
        fn is_cancelled(&self) -> bool {
            self.checks.set(self.checks.get() + 1);
            self.cancel_after
                .is_some_and(|limit| self.checks.get() > limit)
        }

        fn progress(&self, _round: Round, _map: &BadMap) {}
    }

    fn control(cancel_after: Option<usize>) -> Control {
        Control {
            cancel_after,
            checks: Cell::new(0),
        }
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("yisi-image-job-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn damaged() -> Box<FakeDevice> {
        FakeDevice::new(
            4 * MIB + 300,
            vec![
                (MIB + SECTOR, MIB + 3 * SECTOR),
                (2 * MIB, 2 * MIB + MIB / 2),
            ],
        )
    }

    /// 在已准备好的任务上跑到底（或到取消点），返回结束方式。
    fn run_to(job: &mut ImageJob, cancel_after: Option<usize>) -> ImagingOutcome {
        run_imaging(
            job.device.as_ref(),
            &mut job.map,
            &mut job.store,
            &control(cancel_after),
            &ImagingOptions::default(),
        )
        .unwrap()
    }

    #[test]
    fn image_and_map_on_disk_match_the_device() {
        let dir = test_dir("whole");
        let image_path = dir.join("disk.img");
        let device = damaged();
        let expected = device.expected_image();
        let bad = device.bad.clone();
        let mut job = create(device, &image_path, "fake".to_owned()).unwrap();
        // 新建后马上就有一份全部未读的坏区图
        let initial = BadMap::parse(&fs::read_to_string(map_path(&image_path)).unwrap()).unwrap();
        assert_eq!(initial.bytes(SegmentState::Untried), 4 * MIB + 300);

        assert!(matches!(run_to(&mut job, None), ImagingOutcome::Finished));
        assert_eq!(fs::read(&image_path).unwrap(), expected);
        let saved = BadMap::parse(&fs::read_to_string(map_path(&image_path)).unwrap()).unwrap();
        assert_eq!(saved, job.map);
        let saved_bad: Vec<(u64, u64)> = saved
            .ranges(SegmentState::Bad)
            .map(|(offset, length)| (offset, offset + length))
            .collect();
        assert_eq!(saved_bad, bad);
        assert!(!with_suffix(&map_path(&image_path), ".tmp").exists());

        let summary = summarize(
            &job.map,
            &image_path,
            &job.store.map_path,
            None,
            Duration::ZERO,
        );
        assert_eq!(summary.bad_bytes, 2 * SECTOR + MIB / 2);
        assert_eq!(summary.read_bytes, 4 * MIB + 300 - summary.bad_bytes);
        assert_eq!(summary.unfinished_bytes, 0);
        assert_eq!(
            summary.bad_ranges,
            vec![
                ByteRange {
                    offset: MIB + SECTOR,
                    length: 2 * SECTOR
                },
                ByteRange {
                    offset: 2 * MIB,
                    length: MIB / 2
                }
            ]
        );
    }

    #[test]
    fn stopped_image_resumes_to_the_same_bytes() {
        let dir = test_dir("resume");
        let image_path = dir.join("disk.img");
        let expected = damaged().expected_image();
        for stop_at in [1, 3, 6, 50, 600, 1500] {
            let _ = fs::remove_file(&image_path);
            let _ = fs::remove_file(map_path(&image_path));
            let mut job = create(damaged(), &image_path, "fake".to_owned()).unwrap();
            assert!(
                matches!(run_to(&mut job, Some(stop_at)), ImagingOutcome::Cancelled),
                "{stop_at}"
            );
            drop(job);
            // 换一个进程续做：只靠盘上的镜像与坏区图
            let mut resumed = resume(damaged(), &image_path, "fake".to_owned()).unwrap();
            assert!(matches!(
                run_to(&mut resumed, None),
                ImagingOutcome::Finished
            ));
            drop(resumed);
            assert_eq!(fs::read(&image_path).unwrap(), expected, "{stop_at}");
        }
    }

    #[test]
    fn existing_files_block_a_new_image() {
        let dir = test_dir("exists");
        let image_path = dir.join("disk.img");
        fs::write(map_path(&image_path), "").unwrap();
        let error = create(damaged(), &image_path, "fake".to_owned())
            .err()
            .unwrap();
        assert_eq!(error.code, error_code::IMAGE_EXISTS);
        fs::remove_file(map_path(&image_path)).unwrap();
        fs::write(&image_path, "").unwrap();
        let error = create(damaged(), &image_path, "fake".to_owned())
            .err()
            .unwrap();
        assert_eq!(error.code, error_code::IMAGE_EXISTS);
        // 已有的文件原样保留
        assert_eq!(fs::read(&image_path).unwrap(), b"");
    }

    #[test]
    fn missing_folder_is_reported_and_leaves_nothing_behind() {
        let dir = test_dir("no-folder");
        let image_path = dir.join("absent").join("disk.img");
        let error = create(damaged(), &image_path, "fake".to_owned())
            .err()
            .unwrap();
        assert_eq!(error.code, error_code::OUTPUT_UNAVAILABLE);
        assert!(!dir.join("absent").exists());
    }

    #[test]
    fn resume_rejects_maps_of_another_source() {
        let dir = test_dir("mismatch");
        let image_path = dir.join("disk.img");
        let job = create(damaged(), &image_path, "disk WDC A 1".to_owned()).unwrap();
        drop(job);

        let other_source = resume(damaged(), &image_path, "disk WDC A 2".to_owned())
            .err()
            .unwrap();
        assert_eq!(other_source.code, error_code::IMAGE_MISMATCH);
        assert!(
            other_source.message.contains("来源"),
            "{}",
            other_source.message
        );

        let other_size = resume(
            FakeDevice::new(MIB, vec![]),
            &image_path,
            "disk WDC A 1".to_owned(),
        )
        .err()
        .unwrap();
        assert_eq!(other_size.code, error_code::IMAGE_MISMATCH);
        assert!(
            other_size.message.contains("容量"),
            "{}",
            other_size.message
        );

        // 镜像文件被截短了
        fs::OpenOptions::new()
            .write(true)
            .open(&image_path)
            .unwrap()
            .set_len(MIB)
            .unwrap();
        let truncated = resume(damaged(), &image_path, "disk WDC A 1".to_owned())
            .err()
            .unwrap();
        assert_eq!(truncated.code, error_code::IMAGE_MISMATCH);

        // 坏区图坏了、没有了
        fs::write(map_path(&image_path), "version 1\nsize 5\n").unwrap();
        let broken = resume(damaged(), &image_path, "disk WDC A 1".to_owned())
            .err()
            .unwrap();
        assert_eq!(broken.code, error_code::IMAGE_MISMATCH);
        fs::remove_file(map_path(&image_path)).unwrap();
        let missing = resume(damaged(), &image_path, "disk WDC A 1".to_owned())
            .err()
            .unwrap();
        assert_eq!(missing.code, error_code::IMAGE_MISMATCH);
    }

    #[cfg(windows)]
    #[test]
    fn an_image_in_use_cannot_be_resumed_twice() {
        let dir = test_dir("in-use");
        let image_path = dir.join("disk.img");
        let first = create(damaged(), &image_path, "fake".to_owned()).unwrap();
        let second = resume(damaged(), &image_path, "fake".to_owned())
            .err()
            .unwrap();
        assert_eq!(second.code, error_code::OUTPUT_UNAVAILABLE);
        drop(first);
        assert!(resume(damaged(), &image_path, "fake".to_owned()).is_ok());
    }

    #[test]
    fn rate_uses_the_recent_window() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut samples = VecDeque::new();
        samples.push_back((at(0), 0));
        samples.push_back((at(500), 50));
        assert_eq!(read_rate(&mut samples, at(500)), None);
        samples.push_back((at(2000), 200));
        assert_eq!(read_rate(&mut samples, at(2000)), Some(100));
        // 前面慢、后面快：从紧挨着 5 秒窗口之前的那个样本（2 秒处）算起，更早的丢掉
        samples.push_back((at(8000), 6200));
        samples.push_back((at(10_000), 8200));
        let rate = read_rate(&mut samples, at(10_000)).unwrap();
        assert_eq!(samples.front().map(|&(_, bytes)| bytes), Some(200));
        assert_eq!(rate, (8200 - 200) * 1000 / 8000);
    }
}
