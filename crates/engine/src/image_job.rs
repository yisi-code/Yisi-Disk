//! 制作能识别坏区的镜像：建好或打开镜像文件与坏区图，把 `yisi-io` 的读取进展转成任务通知。
//!
//! 镜像写在另一块盘上，与深度恢复写输出目录是同一类操作；源盘仍然只读。
//!
//! 可以只读源的一部分（选中的分区），分几次做完：坏区图始终覆盖整个源，记着哪里读过；镜像在支持稀疏文件的卷上
//! 建成稀疏文件，没读的区域不占空间。每次开始时把源上的分区表记进坏区图，下次续做前据此看出源有没有变。

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use yisi_engine::protocol::{
    ByteRange, IMAGING, ImageStarted, ImageSummary, JobImaging, MAX_UNREADABLE_RANGES,
    RegionProgress, error_code,
};
use yisi_io::{
    BadMap, BlockDevice, ImageStore, ImagingControl, ImagingOptions, ImagingOutcome, IoError,
    MapHeader, RecordedPartition, Round, Scope, SegmentState, run_imaging,
};
use yisi_part::Partition;
use yisi_rpc::{ErrorObject, error_code as rpc_error_code};

use crate::job::{JobContext, JobOutcome};
use crate::runtime::error_chain;

/// 两次进展通知之间至少隔这么久，即每秒最多 5 次。
const STATUS_INTERVAL: Duration = Duration::from_millis(200);
/// 读速按最近这么久读出的字节数算。
const RATE_WINDOW: Duration = Duration::from_secs(5);
/// 读速至少要有这么久的样本才给出，刚开始的几次读取不足以说明速度。
const RATE_MIN_SPAN: Duration = Duration::from_secs(1);
/// 只读一部分时总会一起读的分区表区域：开头、末尾、每个分区之前各这么多字节。
const TABLE_AREA: u64 = 1 << 20;

/// 这一次怎样做：读哪些范围、先清掉哪些范围的进度、是否只做复制这一轮。
#[derive(Debug, Clone, Default)]
pub struct ImageRequest {
    /// 不给时读整个源。
    pub ranges: Option<Vec<ByteRange>>,
    pub reset: Vec<ByteRange>,
    pub copy_only: bool,
}

/// 准备好的镜像任务：源、坏区图、打开的镜像文件，以及这一次的范围。
pub struct ImageJob {
    device: Box<dyn BlockDevice>,
    map: BadMap,
    store: FileStore,
    image_path: PathBuf,
    scope: Scope,
    copy_only: bool,
}

impl ImageJob {
    pub fn started(&self) -> ImageStarted {
        ImageStarted {
            image_path: self.image_path.display().to_string(),
            map_path: self.store.map_path.display().to_string(),
            source_identity: self.map.header().source.clone(),
            size: self.map.header().size,
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

/// 只读一部分时总会一起读的几小段：开头与末尾各 1 MiB（MBR、GPT 与它的备份），每个分区之前的 1 MiB
/// （逻辑分区的链表项就在它前面），都只取不属于任何分区的部分。没有这些，镜像里认不出分区。
/// `partitions` 是各分区的（起点，长度）；结果按起点排好、去掉了重复。
pub fn table_areas(size: u64, partitions: &[(u64, u64)]) -> Vec<ByteRange> {
    let mut candidates = vec![
        (0, TABLE_AREA.min(size)),
        (size.saturating_sub(TABLE_AREA), size),
    ];
    candidates.extend(
        partitions
            .iter()
            .map(|&(offset, _)| (offset.saturating_sub(TABLE_AREA), offset.min(size))),
    );
    let mut areas = Vec::new();
    for (start, end) in candidates {
        let mut pieces = vec![(start, end)];
        for &(offset, length) in partitions {
            let covered = (offset, offset.saturating_add(length));
            pieces = pieces
                .into_iter()
                .flat_map(|(from, to)| {
                    [(from, to.min(covered.0)), (from.max(covered.1), to)]
                        .into_iter()
                        .filter(|&(from, to)| from < to)
                })
                .collect();
        }
        areas.extend(pieces);
    }
    areas.sort_unstable();
    areas.dedup();
    areas
        .into_iter()
        .map(|(start, end)| ByteRange {
            offset: start,
            length: end - start,
        })
        .collect()
}

/// 源上现在的分区；分区表读不出时为空。
pub fn current_partitions(device: &dyn BlockDevice) -> Vec<Partition> {
    yisi_part::read_table(device)
        .map(|table| table.partitions)
        .unwrap_or_default()
}

/// `[start, end)` 之内各状态的字节数。
pub fn progress_between(map: &BadMap, start: u64, end: u64) -> RegionProgress {
    RegionProgress {
        read_bytes: map.bytes_within(SegmentState::Done, start, end),
        bad_bytes: map.bytes_within(SegmentState::Bad, start, end),
        untried_bytes: map.bytes_within(SegmentState::Untried, start, end),
        trim_bytes: map.bytes_within(SegmentState::Trim, start, end),
        scrape_bytes: map.bytes_within(SegmentState::Scrape, start, end),
    }
}

/// 范围之内各状态的字节数。
fn scope_progress(map: &BadMap, scope: &Scope) -> RegionProgress {
    scope
        .ranges()
        .iter()
        .map(|&(start, end)| progress_between(map, start, end))
        .fold(RegionProgress::default(), |sum, part| RegionProgress {
            read_bytes: sum.read_bytes + part.read_bytes,
            bad_bytes: sum.bad_bytes + part.bad_bytes,
            untried_bytes: sum.untried_bytes + part.untried_bytes,
            trim_bytes: sum.trim_bytes + part.trim_bytes,
            scrape_bytes: sum.scrape_bytes + part.scrape_bytes,
        })
}

/// 还没读完的字节数（未读、待修边、待细读）。
pub fn unfinished(progress: &RegionProgress) -> u64 {
    progress.untried_bytes + progress.trim_bytes + progress.scrape_bytes
}

/// 坏区图记录的容量、扇区大小、来源要与当前的源一致；不一致时给出哪一项不符。续做与做镜像前查看共用。
pub fn check_header(map: &BadMap, device: &dyn BlockDevice, source: &str) -> Result<(), String> {
    let recorded = map.header();
    if recorded.size != device.size() {
        return Err(format!(
            "坏区图记录的容量是 {} 字节，当前的源是 {} 字节",
            recorded.size,
            device.size()
        ));
    }
    if recorded.sector_size != device.sector_size() {
        return Err(format!(
            "坏区图记录的扇区大小是 {}，当前的源是 {}",
            recorded.sector_size,
            device.sector_size()
        ));
    }
    // 新建时写进去的来源也经过同样的规整（换行换成空格），这里按规整后的比较
    let normalized = BadMap::new(MapHeader {
        size: 0,
        sector_size: recorded.sector_size,
        source: source.to_owned(),
    });
    if recorded.source != normalized.header().source {
        return Err(format!(
            "坏区图记录的来源是「{}」，当前的源是「{source}」",
            recorded.source
        ));
    }
    Ok(())
}

/// 镜像文件的长度要等于源的容量。
pub fn check_length(length: u64, size: u64) -> Result<(), String> {
    if length == size {
        Ok(())
    } else {
        Err(format!(
            "镜像文件长 {length} 字节，与源的容量 {size} 字节不符"
        ))
    }
}

/// 新建镜像：在支持稀疏文件的卷上建成稀疏文件，否则按源的容量整块预留空间（读不出的区域留作 0）；
/// 坏区图记为全部未读、记下源上现在的分区表，先保存一次。
pub fn create(
    device: Box<dyn BlockDevice>,
    image_path: &Path,
    source: String,
    request: &ImageRequest,
) -> Result<ImageJob, ErrorObject> {
    let map_path = map_path(image_path);
    let exists = |path: &Path| fs::symlink_metadata(path).is_ok();
    if exists(image_path) || exists(&map_path) {
        return Err(image_exists(image_path));
    }
    let sparse = sparse_volume(image_path);
    let partitions = current_partitions(device.as_ref());
    let scope = plan_scope(device.as_ref(), &partitions, request, sparse)?;
    ensure_space(image_path, sparse, scope.total())?;
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
    let prepared = mark_sparse(&store.image, sparse)
        .map_err(|error| format!("无法把镜像设为稀疏文件：{error}"))
        .and_then(|()| {
            store.image.set_len(size).map_err(|error| {
                format!(
                    "无法为镜像预留 {size} 字节（输出盘空间不足，或者文件系统不支持这么大的文件，例如 FAT32 单个文件最大 4 GiB）：{error}"
                )
            })
        })
        .and_then(|()| {
            let mut map = BadMap::new(MapHeader {
                size,
                sector_size: device.sector_size(),
                source,
            });
            map.set_partitions(record(&partitions));
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
            scope,
            copy_only: request.copy_only,
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
/// 先清掉 `request.reset` 范围的进度，再记下源上现在的分区表（第一次保存坏区图时落盘）。
pub fn resume(
    device: Box<dyn BlockDevice>,
    image_path: &Path,
    source: String,
    request: &ImageRequest,
) -> Result<ImageJob, ErrorObject> {
    let map_path = map_path(image_path);
    let text = fs::read_to_string(&map_path)
        .map_err(|error| mismatch(format!("读不到坏区图 {}：{error}", map_path.display())))?;
    let mut map = BadMap::parse(&text)
        .map_err(|error| mismatch(format!("坏区图 {} 读不懂：{error}", map_path.display())))?;
    check_header(&map, device.as_ref(), &source).map_err(mismatch)?;
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
    check_length(length, device.size()).map_err(mismatch)?;
    let sparse = file_is_sparse(&image);
    let partitions = current_partitions(device.as_ref());
    let scope = plan_scope(device.as_ref(), &partitions, request, sparse)?;
    if !request.reset.is_empty() {
        let reset: Vec<(u64, u64)> = request
            .reset
            .iter()
            .map(|range| (range.offset, range.length))
            .collect();
        let reset = Scope::new(&reset, device.size(), device.sector_size())
            .map_err(|error| invalid(format!("要重做的范围不对：{error}")))?;
        for &(start, end) in reset.ranges() {
            map.set(start, end - start, SegmentState::Untried);
        }
    }
    map.set_partitions(record(&partitions));
    ensure_space(image_path, sparse, scope.unfinished(&map))?;
    Ok(ImageJob {
        device,
        map,
        store: FileStore::new(image, map_path),
        image_path: image_path.to_path_buf(),
        scope,
        copy_only: request.copy_only,
    })
}

/// 这一次的范围：不限范围时是整个源；只读一部分时加上分区表区域，而且镜像必须是稀疏文件——
/// 普通文件往远处写之前，文件系统要先把前面没写过的部分全部填 0，几百 GB 要填好几个小时。
fn plan_scope(
    device: &dyn BlockDevice,
    partitions: &[Partition],
    request: &ImageRequest,
    sparse: bool,
) -> Result<Scope, ErrorObject> {
    let size = device.size();
    let Some(ranges) = &request.ranges else {
        return Ok(Scope::whole(size));
    };
    let mut ranges: Vec<(u64, u64)> = ranges
        .iter()
        .map(|range| (range.offset, range.length))
        .collect();
    let selected = Scope::new(&ranges, size, device.sector_size())
        .map_err(|error| invalid(format!("要读的范围不对：{error}")))?;
    if selected.is_whole(size) {
        return Ok(selected);
    }
    if !sparse {
        return Err(invalid(
            "这个镜像只能整盘做：它不是稀疏文件（输出盘不支持稀疏文件，例如 FAT32、exFAT，或者镜像是旧版本建的）".to_owned(),
        ));
    }
    ranges.extend(
        table_areas(
            size,
            &partitions
                .iter()
                .map(|partition| (partition.offset, partition.length))
                .collect::<Vec<_>>(),
        )
        .into_iter()
        .map(|area| (area.offset, area.length)),
    );
    Scope::new(&ranges, size, device.sector_size())
        .map_err(|error| invalid(format!("要读的范围不对：{error}")))
}

fn record(partitions: &[Partition]) -> Vec<RecordedPartition> {
    partitions
        .iter()
        .map(|partition| RecordedPartition {
            number: partition.number,
            offset: partition.offset,
            length: partition.length,
        })
        .collect()
}

/// 输出位置所在的卷支持稀疏文件；查不到时按不支持处理（只能整盘做，不会出错）。
fn sparse_volume(image_path: &Path) -> bool {
    #[cfg(windows)]
    {
        yisi_io::volume_info(image_path).is_ok_and(|volume| volume.sparse)
    }
    #[cfg(not(windows))]
    {
        let _ = image_path;
        false
    }
}

pub(crate) fn file_is_sparse(image: &File) -> bool {
    #[cfg(windows)]
    {
        yisi_io::is_sparse(image).unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = image;
        false
    }
}

fn mark_sparse(image: &File, sparse: bool) -> io::Result<()> {
    #[cfg(windows)]
    if sparse {
        return yisi_io::make_sparse(image);
    }
    let _ = (image, sparse);
    Ok(())
}

/// 稀疏镜像不预留空间，开始前确认输出盘还放得下这一次最多要写的量；普通镜像新建时已经整块预留。
/// 查不到剩余空间时不拦：真写满了，写镜像出错停下，坏区图里不会记下没写进去的部分。
fn ensure_space(image_path: &Path, sparse: bool, needed: u64) -> Result<(), ErrorObject> {
    if !sparse {
        return Ok(());
    }
    #[cfg(windows)]
    match yisi_io::volume_info(image_path) {
        Ok(volume) if volume.free_bytes < needed => {
            return Err(output_unavailable(format!(
                "输出盘剩余 {} 字节，这一次最多还要写入 {needed} 字节",
                volume.free_bytes
            )));
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "查不到输出盘的剩余空间"),
    }
    let _ = (image_path, needed);
    Ok(())
}

pub fn run(job: ImageJob, context: &JobContext) -> Result<JobOutcome, String> {
    let ImageJob {
        device,
        mut map,
        mut store,
        image_path,
        scope,
        copy_only,
    } = job;
    let started = Instant::now();
    let reporter = Reporter::new(context, &scope, copy_only);
    let options = ImagingOptions {
        scope: Some(scope.clone()),
        copy_only,
        ..ImagingOptions::default()
    };
    let outcome = run_imaging(device.as_ref(), &mut map, &mut store, &reporter, &options);
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
    let summary = summarize(
        &map,
        &scope,
        copy_only,
        &image_path,
        &store.map_path,
        source_lost_at,
        elapsed,
    );
    serde_json::to_value(summary)
        .map(JobOutcome::Completed)
        .map_err(|error| format!("无法编码镜像结果：{error}"))
}

fn summarize(
    map: &BadMap,
    scope: &Scope,
    copy_only: bool,
    image_path: &Path,
    map_path: &Path,
    source_lost_at: Option<u64>,
    elapsed: Duration,
) -> ImageSummary {
    let progress = scope_progress(map, scope);
    let bad_ranges: Vec<ByteRange> = map
        .ranges(SegmentState::Bad)
        .flat_map(|(offset, length)| {
            scope.ranges().iter().filter_map(move |&(start, end)| {
                let from = offset.max(start);
                let to = (offset + length).min(end);
                (from < to).then(|| ByteRange {
                    offset: from,
                    length: to - from,
                })
            })
        })
        .take(MAX_UNREADABLE_RANGES + 1)
        .collect();
    let bad_ranges_truncated = bad_ranges.len() > MAX_UNREADABLE_RANGES;
    let device_size = map.header().size;
    let device = progress_between(map, 0, device_size);
    ImageSummary {
        image_path: image_path.display().to_string(),
        map_path: map_path.display().to_string(),
        size: scope.total(),
        read_bytes: progress.read_bytes,
        bad_bytes: progress.bad_bytes,
        unfinished_bytes: unfinished(&progress),
        bad_ranges: bad_ranges.into_iter().take(MAX_UNREADABLE_RANGES).collect(),
        bad_ranges_truncated,
        partial: !scope.is_whole(device_size),
        copy_only,
        device_size,
        device_read_bytes: device.read_bytes,
        device_unfinished_bytes: unfinished(&device),
        source_lost_at,
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    }
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

/// 把读取进展转成 `job.progress`（阶段是轮次）与 `job.imaging`，节流到每秒最多 5 次。字节数只算本次的范围。
struct Reporter<'a> {
    context: &'a JobContext,
    scope: &'a Scope,
    copy_only: bool,
    round: Cell<Round>,
    last_sent: Cell<Option<Instant>>,
    /// 最近一段时间里各次发送时的（时刻，已读字节数），用来算读速。
    samples: RefCell<VecDeque<(Instant, u64)>>,
}

impl<'a> Reporter<'a> {
    fn new(context: &'a JobContext, scope: &'a Scope, copy_only: bool) -> Self {
        Self {
            context,
            scope,
            copy_only,
            round: Cell::new(Round::Copy),
            last_sent: Cell::new(None),
            samples: RefCell::new(VecDeque::new()),
        }
    }

    fn send(&self, map: &BadMap) {
        let now = Instant::now();
        self.last_sent.set(Some(now));
        let progress = scope_progress(map, self.scope);
        let bytes_per_second = {
            let mut samples = self.samples.borrow_mut();
            samples.push_back((now, progress.read_bytes));
            read_rate(&mut samples, now)
        };
        let size = self.scope.total();
        let round = round_name(self.round.get());
        // 只做复制这一轮时，读错与跳过的部分留到以后，过完一遍就算完
        let pending = if self.copy_only {
            progress.untried_bytes
        } else {
            unfinished(&progress)
        };
        self.context.stage_progress(round, size - pending, size);
        self.context.notify(
            IMAGING,
            JobImaging {
                job_id: self.context.id().to_owned(),
                round: round.to_owned(),
                size,
                read_bytes: progress.read_bytes,
                bad_bytes: progress.bad_bytes,
                untried_bytes: progress.untried_bytes,
                trim_bytes: progress.trim_bytes,
                scrape_bytes: progress.scrape_bytes,
                bytes_per_second,
                copy_only: self.copy_only,
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

fn invalid(message: String) -> ErrorObject {
    ErrorObject::new(rpc_error_code::INVALID_PARAMS, message)
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

    /// 在已准备好的任务上按它的范围跑到底（或到取消点），返回结束方式。
    fn run_to(job: &mut ImageJob, cancel_after: Option<usize>) -> ImagingOutcome {
        let options = ImagingOptions {
            scope: Some(job.scope.clone()),
            copy_only: job.copy_only,
            ..ImagingOptions::default()
        };
        run_imaging(
            job.device.as_ref(),
            &mut job.map,
            &mut job.store,
            &control(cancel_after),
            &options,
        )
        .unwrap()
    }

    fn whole() -> ImageRequest {
        ImageRequest::default()
    }

    fn summary_of(job: &ImageJob, image_path: &Path) -> ImageSummary {
        summarize(
            &job.map,
            &job.scope,
            job.copy_only,
            image_path,
            &job.store.map_path,
            None,
            Duration::ZERO,
        )
    }

    #[test]
    fn image_and_map_on_disk_match_the_device() {
        let dir = test_dir("whole");
        let image_path = dir.join("disk.img");
        let device = damaged();
        let expected = device.expected_image();
        let bad = device.bad.clone();
        let mut job = create(device, &image_path, "fake".to_owned(), &whole()).unwrap();
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

        let summary = summary_of(&job, &image_path);
        assert!(!summary.partial);
        assert_eq!(summary.device_unfinished_bytes, 0);
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
            let mut job = create(damaged(), &image_path, "fake".to_owned(), &whole()).unwrap();
            assert!(
                matches!(run_to(&mut job, Some(stop_at)), ImagingOutcome::Cancelled),
                "{stop_at}"
            );
            drop(job);
            // 换一个进程续做：只靠盘上的镜像与坏区图
            let mut resumed = resume(damaged(), &image_path, "fake".to_owned(), &whole()).unwrap();
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
        let error = create(damaged(), &image_path, "fake".to_owned(), &whole())
            .err()
            .unwrap();
        assert_eq!(error.code, error_code::IMAGE_EXISTS);
        fs::remove_file(map_path(&image_path)).unwrap();
        fs::write(&image_path, "").unwrap();
        let error = create(damaged(), &image_path, "fake".to_owned(), &whole())
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
        let error = create(damaged(), &image_path, "fake".to_owned(), &whole())
            .err()
            .unwrap();
        assert_eq!(error.code, error_code::OUTPUT_UNAVAILABLE);
        assert!(!dir.join("absent").exists());
    }

    #[test]
    fn resume_rejects_maps_of_another_source() {
        let dir = test_dir("mismatch");
        let image_path = dir.join("disk.img");
        let job = create(damaged(), &image_path, "disk WDC A 1".to_owned(), &whole()).unwrap();
        drop(job);

        let other_source = resume(damaged(), &image_path, "disk WDC A 2".to_owned(), &whole())
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
            &whole(),
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
        let truncated = resume(damaged(), &image_path, "disk WDC A 1".to_owned(), &whole())
            .err()
            .unwrap();
        assert_eq!(truncated.code, error_code::IMAGE_MISMATCH);

        // 坏区图坏了、没有了
        fs::write(map_path(&image_path), "version 1\nsize 5\n").unwrap();
        let broken = resume(damaged(), &image_path, "disk WDC A 1".to_owned(), &whole())
            .err()
            .unwrap();
        assert_eq!(broken.code, error_code::IMAGE_MISMATCH);
        fs::remove_file(map_path(&image_path)).unwrap();
        let missing = resume(damaged(), &image_path, "disk WDC A 1".to_owned(), &whole())
            .err()
            .unwrap();
        assert_eq!(missing.code, error_code::IMAGE_MISMATCH);
    }

    #[cfg(windows)]
    #[test]
    fn an_image_in_use_cannot_be_resumed_twice() {
        let dir = test_dir("in-use");
        let image_path = dir.join("disk.img");
        let first = create(damaged(), &image_path, "fake".to_owned(), &whole()).unwrap();
        let second = resume(damaged(), &image_path, "fake".to_owned(), &whole())
            .err()
            .unwrap();
        assert_eq!(second.code, error_code::OUTPUT_UNAVAILABLE);
        drop(first);
        assert!(resume(damaged(), &image_path, "fake".to_owned(), &whole()).is_ok());
    }

    #[test]
    fn table_areas_cover_both_ends_and_the_space_before_each_partition() {
        let pairs = |size: u64, partitions: &[(u64, u64)]| -> Vec<(u64, u64)> {
            table_areas(size, partitions)
                .iter()
                .map(|area| (area.offset, area.length))
                .collect()
        };
        assert_eq!(
            pairs(100 * MIB, &[(MIB, 8 * MIB), (10 * MIB, 80 * MIB)]),
            vec![(0, MIB), (9 * MIB, MIB), (99 * MIB, MIB)]
        );
        // 紧挨着的分区：前一个分区的末尾不算；从第 63 个扇区开始的分区：只取它之前的部分
        assert_eq!(
            pairs(
                100 * MIB,
                &[(63 * 512, 10 * MIB), (63 * 512 + 10 * MIB, 89 * MIB)]
            ),
            vec![(0, 63 * 512), (63 * 512 + 99 * MIB, MIB - 63 * 512)]
        );
        // 比 1 MiB 还小的源；没有分区表、整个源是一个分区
        assert_eq!(pairs(4096, &[]), vec![(0, 4096)]);
        assert_eq!(pairs(4096, &[(0, 4096)]), vec![]);
    }

    /// 测试目录所在的卷支持稀疏文件时才能测只读一部分。
    fn sparse_test_dir(name: &str) -> Option<PathBuf> {
        let dir = test_dir(name);
        sparse_volume(&dir).then_some(dir)
    }

    fn range(offset: u64, length: u64) -> ByteRange {
        ByteRange { offset, length }
    }

    #[test]
    fn partial_image_reads_the_selection_then_fills_in_later() {
        let Some(dir) = sparse_test_dir("partial") else {
            return;
        };
        let image_path = dir.join("disk.img");
        let expected = damaged().expected_image();
        let request = ImageRequest {
            ranges: Some(vec![range(2 * MIB, MIB / 2)]),
            ..ImageRequest::default()
        };
        let mut job = create(damaged(), &image_path, "fake".to_owned(), &request).unwrap();
        // 这个源没有分区表，整个源算一个分区，不用另读分区表区域（有分区表的见协议测试）
        assert_eq!(job.scope.ranges(), &[(2 * MIB, 2 * MIB + MIB / 2)]);
        assert!(file_is_sparse(&job.store.image));
        assert!(matches!(run_to(&mut job, None), ImagingOutcome::Finished));
        let summary = summary_of(&job, &image_path);
        assert!(summary.partial);
        assert_eq!(summary.size, job.scope.total());
        assert_eq!(summary.unfinished_bytes, 0);
        assert_eq!(summary.device_unfinished_bytes, 4 * MIB + 300 - MIB / 2);
        drop(job);

        let image = fs::read(&image_path).unwrap();
        let (start, end) = (2 * MIB as usize, 2 * MIB as usize + MIB as usize / 2);
        assert_eq!(image[start..end], expected[start..end]);
        assert!(
            image[..start]
                .iter()
                .chain(&image[end..])
                .all(|&byte| byte == 0)
        );

        // 之后不限范围续做，补齐其余部分
        let mut rest = resume(damaged(), &image_path, "fake".to_owned(), &whole()).unwrap();
        assert!(matches!(run_to(&mut rest, None), ImagingOutcome::Finished));
        drop(rest);
        assert_eq!(fs::read(&image_path).unwrap(), expected);
    }

    #[test]
    fn reset_reads_a_range_again() {
        let dir = test_dir("reset");
        let image_path = dir.join("disk.img");
        let expected = damaged().expected_image();
        let mut job = create(damaged(), &image_path, "fake".to_owned(), &whole()).unwrap();
        run_to(&mut job, None);
        drop(job);
        // 镜像里这一段被改坏了：重做这一段之后又与源一致
        {
            let file = OpenOptions::new().write(true).open(&image_path).unwrap();
            let garbage = vec![0x5a; 8192];
            #[cfg(windows)]
            std::os::windows::fs::FileExt::seek_write(&file, &garbage, 3 * MIB).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::FileExt::write_at(&file, &garbage, 3 * MIB).unwrap();
        }
        let request = ImageRequest {
            reset: vec![range(3 * MIB, 8192)],
            ..ImageRequest::default()
        };
        let mut again = resume(damaged(), &image_path, "fake".to_owned(), &request).unwrap();
        assert_eq!(again.map.bytes(SegmentState::Untried), 8192);
        run_to(&mut again, None);
        drop(again);
        assert_eq!(fs::read(&image_path).unwrap(), expected);
    }

    #[test]
    fn an_image_that_is_not_sparse_can_only_be_done_whole() {
        let dir = test_dir("not-sparse");
        let image_path = dir.join("disk.img");
        let device = damaged();
        // 旧版本建的镜像：普通文件
        fs::write(&image_path, vec![0u8; device.data.len()]).unwrap();
        let map = BadMap::new(MapHeader {
            size: device.size(),
            sector_size: SECTOR as u32,
            source: "fake".to_owned(),
        });
        fs::write(map_path(&image_path), map.to_text()).unwrap();
        let request = ImageRequest {
            ranges: Some(vec![range(MIB, MIB)]),
            ..ImageRequest::default()
        };
        let error = resume(damaged(), &image_path, "fake".to_owned(), &request)
            .err()
            .unwrap();
        assert_eq!(error.code, rpc_error_code::INVALID_PARAMS);
        assert!(error.message.contains("整盘"), "{}", error.message);
        // 选了整个源也可以
        let request = ImageRequest {
            ranges: Some(vec![range(0, device.size())]),
            ..ImageRequest::default()
        };
        assert!(resume(damaged(), &image_path, "fake".to_owned(), &request).is_ok());
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
