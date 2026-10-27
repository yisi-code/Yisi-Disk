use std::cell::{Cell, RefCell};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

const SECTOR: u64 = 512;
const MIB: u64 = 1 << 20;

/// 内容确定的内存设备：落在坏区里的读取报错；`lost_after` 次读取之后整个设备都读不出（模拟断开）。
struct FakeDevice {
    data: Vec<u8>,
    bad: Vec<(u64, u64)>,
    lost_after: Option<usize>,
    reads: AtomicUsize,
    /// 每次读取的（起点，长度，是否成功）。
    log: Mutex<Vec<(u64, u64, bool)>>,
}

impl FakeDevice {
    fn new(size: u64, bad: Vec<(u64, u64)>) -> Self {
        let data = (0..size)
            .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 24) as u8 | 1)
            .collect();
        Self {
            data,
            bad,
            lost_after: None,
            reads: AtomicUsize::new(0),
            log: Mutex::new(Vec::new()),
        }
    }

    fn is_bad(&self, offset: u64, end: u64) -> bool {
        self.bad
            .iter()
            .any(|&(start, bad_end)| offset < bad_end && start < end)
    }

    /// 坏区以外的内容，坏区里是 0：做出的镜像应当与它逐字节一致。
    fn expected_image(&self) -> Vec<u8> {
        let mut image = self.data.clone();
        for &(start, end) in &self.bad {
            image[start as usize..end as usize].fill(0);
        }
        image
    }

    fn sector_reads_of(&self, offset: u64) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|&&(start, length, _)| start == offset && length <= SECTOR)
            .count()
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
        let count = self.reads.fetch_add(1, Ordering::Relaxed);
        let end = (offset + buf.len() as u64).min(self.size());
        if offset >= end {
            return Ok(0);
        }
        let lost = self.lost_after.is_some_and(|limit| count >= limit);
        let ok = !lost && !self.is_bad(offset, end);
        self.log.lock().unwrap().push((offset, end - offset, ok));
        if !ok {
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

/// 内存里的镜像，记下每次保存的坏区图。
struct MemoryStore {
    image: Vec<u8>,
    commits: Vec<BadMap>,
}

impl MemoryStore {
    fn new(size: u64) -> Self {
        Self {
            image: vec![0; size as usize],
            commits: Vec::new(),
        }
    }
}

impl ImageStore for MemoryStore {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.image[offset as usize..offset as usize + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn commit(&mut self, map: &BadMap) -> io::Result<()> {
        // 保存的必须是能读回的完整坏区图
        assert_eq!(&BadMap::parse(&map.to_text()).unwrap(), map);
        self.commits.push(map.clone());
        Ok(())
    }
}

#[derive(Default)]
struct Recorder {
    cancel_after: Option<usize>,
    checks: Cell<usize>,
    rounds: RefCell<Vec<Round>>,
}

impl ImagingControl for Recorder {
    fn is_cancelled(&self) -> bool {
        self.checks.set(self.checks.get() + 1);
        self.cancel_after
            .is_some_and(|limit| self.checks.get() > limit)
    }

    fn progress(&self, round: Round, _map: &BadMap) {
        let mut rounds = self.rounds.borrow_mut();
        if rounds.last() != Some(&round) {
            rounds.push(round);
        }
    }
}

fn new_map(device: &FakeDevice) -> BadMap {
    BadMap::new(MapHeader {
        size: device.size(),
        sector_size: SECTOR as u32,
        source: "fake".to_owned(),
    })
}

fn quick_commits() -> ImagingOptions {
    ImagingOptions {
        commit_interval: Duration::ZERO,
        ..ImagingOptions::default()
    }
}

/// 不中断地做完，返回坏区图与镜像。
fn image_in_one_go(device: &FakeDevice) -> (BadMap, Vec<u8>) {
    let mut map = new_map(device);
    let mut store = MemoryStore::new(device.size());
    let outcome = run_imaging(
        device,
        &mut map,
        &mut store,
        &Recorder::default(),
        &ImagingOptions::default(),
    )
    .unwrap();
    assert!(matches!(outcome, ImagingOutcome::Finished), "{outcome:?}");
    assert_eq!(store.commits.last(), Some(&map), "结束时保存了最终的坏区图");
    (map, store.image)
}

/// 坏区：开头第一个扇区、块内单个扇区、跨 1 MiB 边界的三个扇区、连续 2.5 MiB、末尾不满一个扇区的那一段。
fn damaged_device() -> FakeDevice {
    let size = 12 * MIB + 300;
    FakeDevice::new(
        size,
        vec![
            (0, SECTOR),
            (MIB + 7 * SECTOR, MIB + 8 * SECTOR),
            (3 * MIB - SECTOR, 3 * MIB + 2 * SECTOR),
            (5 * MIB + 3 * SECTOR, 7 * MIB + MIB / 2 + 3 * SECTOR),
            (12 * MIB, size),
        ],
    )
}

fn bad_ranges(map: &BadMap) -> Vec<(u64, u64)> {
    map.ranges(SegmentState::Bad)
        .map(|(offset, length)| (offset, offset + length))
        .collect()
}

#[test]
fn clean_device_is_copied_completely() {
    let device = FakeDevice::new(3 * MIB + 1234, vec![]);
    let (map, image) = image_in_one_go(&device);
    assert_eq!(image, device.data);
    assert_eq!(map.bytes(SegmentState::Done), device.size());
    assert_eq!(map.segments().len(), 1);
}

#[test]
fn bad_areas_are_found_exactly_and_good_areas_match() {
    let device = damaged_device();
    let recorder = Recorder::default();
    let mut map = new_map(&device);
    let mut store = MemoryStore::new(device.size());
    run_imaging(
        &device,
        &mut map,
        &mut store,
        &recorder,
        &ImagingOptions::default(),
    )
    .unwrap();

    assert_eq!(bad_ranges(&map), device.bad);
    assert_eq!(map.bytes(SegmentState::Untried), 0);
    assert_eq!(map.bytes(SegmentState::Trim), 0);
    assert_eq!(map.bytes(SegmentState::Scrape), 0);
    let bad: u64 = device.bad.iter().map(|(start, end)| end - start).sum();
    assert_eq!(map.bytes(SegmentState::Bad), bad);
    assert_eq!(map.bytes(SegmentState::Done), device.size() - bad);
    assert_eq!(store.image, device.expected_image());
    assert_eq!(
        *recorder.rounds.borrow(),
        vec![Round::Copy, Round::Trim, Round::Scrape]
    );

    // 每个坏扇区只被单独读过一次：修边碰到的那个不再细读
    for &(start, end) in &device.bad {
        for sector in (start..end).step_by(SECTOR as usize) {
            assert_eq!(device.sector_reads_of(sector), 1, "扇区 {sector}");
        }
    }
}

#[test]
fn skipping_keeps_large_bad_areas_from_being_hammered_in_the_copy_round() {
    let device = damaged_device();
    image_in_one_go(&device);
    // 整块读（长于一个扇区）出错的次数：正向 7 次（其中 2.5 MiB 的坏区 3 次，跳过量 64→128→256 KiB），
    // 反向读被跳过的 64 KiB 小段 4 次。不跳过时这块坏区要碰 3 次以上的整块读错
    let failed_blocks = device
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|&&(_, length, ok)| !ok && length > SECTOR)
        .count();
    assert_eq!(failed_blocks, 11);
}

#[test]
fn every_cancellation_point_resumes_to_the_same_result() {
    let device = damaged_device();
    let (expected_map, expected_image) = image_in_one_go(&device);
    let total_checks = {
        let recorder = Recorder::default();
        let mut map = new_map(&device);
        run_imaging(
            &device,
            &mut map,
            &mut MemoryStore::new(device.size()),
            &recorder,
            &quick_commits(),
        )
        .unwrap();
        recorder.checks.get()
    };

    // 约 1.4 万个检查点，均匀取 40 个，加上开头与结尾
    let step = total_checks / 40;
    for stop_at in (0..total_checks)
        .step_by(step)
        .chain([1, 2, total_checks - 1])
    {
        let mut map = new_map(&device);
        let mut store = MemoryStore::new(device.size());
        let interrupted = Recorder {
            cancel_after: Some(stop_at),
            ..Recorder::default()
        };
        let outcome = run_imaging(
            &device,
            &mut map,
            &mut store,
            &interrupted,
            &quick_commits(),
        )
        .unwrap();
        assert!(matches!(outcome, ImagingOutcome::Cancelled), "{stop_at}");
        assert_eq!(store.commits.last(), Some(&map), "取消时保存了坏区图");

        // 只拿保存下来的坏区图续做
        let mut resumed = BadMap::parse(&store.commits.last().unwrap().to_text()).unwrap();
        let outcome = run_imaging(
            &device,
            &mut resumed,
            &mut store,
            &Recorder::default(),
            &quick_commits(),
        )
        .unwrap();
        assert!(matches!(outcome, ImagingOutcome::Finished), "{stop_at}");
        assert_eq!(resumed, expected_map, "{stop_at}");
        assert_eq!(store.image, expected_image, "{stop_at}");
    }
}

#[test]
fn crash_with_an_older_map_still_resumes_to_the_same_result() {
    let device = damaged_device();
    let (expected_map, expected_image) = image_in_one_go(&device);
    let mut map = new_map(&device);
    let mut store = MemoryStore::new(device.size());
    let interrupted = Recorder {
        cancel_after: Some(4000),
        ..Recorder::default()
    };
    run_imaging(
        &device,
        &mut map,
        &mut store,
        &interrupted,
        &quick_commits(),
    )
    .unwrap();
    // 崩溃：盘上的坏区图停在较早的一次保存，镜像里已经多写了之后读出的数据
    for older in [0, store.commits.len() / 3, store.commits.len() / 2] {
        let mut resumed = store.commits[older].clone();
        let mut image = MemoryStore {
            image: store.image.clone(),
            commits: Vec::new(),
        };
        run_imaging(
            &device,
            &mut resumed,
            &mut image,
            &Recorder::default(),
            &ImagingOptions::default(),
        )
        .unwrap();
        assert_eq!(resumed, expected_map, "{older}");
        assert_eq!(image.image, expected_image, "{older}");
    }
}

#[test]
fn disconnection_is_not_recorded_as_bad_area() {
    let mut device = FakeDevice::new(6 * MIB, vec![(5 * MIB, 5 * MIB + SECTOR)]);
    device.lost_after = Some(4);
    let mut map = new_map(&device);
    let mut store = MemoryStore::new(device.size());
    let outcome = run_imaging(
        &device,
        &mut map,
        &mut store,
        &Recorder::default(),
        &ImagingOptions::default(),
    )
    .unwrap();
    let ImagingOutcome::SourceLost { offset, .. } = outcome else {
        panic!("应按设备断开结束：{outcome:?}");
    };
    assert_eq!(offset, 4 * MIB);
    assert_eq!(map.bytes(SegmentState::Done), 4 * MIB);
    assert_eq!(map.bytes(SegmentState::Bad), 0);
    assert_eq!(map.bytes(SegmentState::Trim), 0);
    assert_eq!(store.commits.last(), Some(&map));

    // 重新接上之后续做，结果与一次做完相同
    device.lost_after = None;
    run_imaging(
        &device,
        &mut map,
        &mut store,
        &Recorder::default(),
        &ImagingOptions::default(),
    )
    .unwrap();
    assert_eq!(bad_ranges(&map), device.bad);
    assert_eq!(store.image, device.expected_image());
}

#[test]
fn first_read_failing_is_recorded_without_probe() {
    // 还没有读成功过的位置可以拿来核对，只能照常记录
    let device = FakeDevice::new(2 * MIB, vec![(0, 4 * SECTOR)]);
    let (map, image) = image_in_one_go(&device);
    assert_eq!(bad_ranges(&map), device.bad);
    assert_eq!(image, device.expected_image());
}

#[test]
fn finished_map_reads_nothing_more() {
    let device = damaged_device();
    let (mut map, image) = image_in_one_go(&device);
    let before = device.reads.load(Ordering::Relaxed);
    let mut store = MemoryStore {
        image,
        commits: Vec::new(),
    };
    let outcome = run_imaging(
        &device,
        &mut map,
        &mut store,
        &Recorder::default(),
        &ImagingOptions::default(),
    )
    .unwrap();
    assert!(matches!(outcome, ImagingOutcome::Finished));
    assert_eq!(device.reads.load(Ordering::Relaxed), before);
}

#[test]
fn empty_device_finishes_immediately() {
    let device = FakeDevice::new(0, vec![]);
    let (map, image) = image_in_one_go(&device);
    assert!(map.segments().is_empty());
    assert!(image.is_empty());
}

#[test]
fn mismatched_map_or_options_are_rejected() {
    let device = FakeDevice::new(MIB, vec![]);
    let mut other = BadMap::new(MapHeader {
        size: 2 * MIB,
        sector_size: 512,
        source: "fake".to_owned(),
    });
    let mut store = MemoryStore::new(2 * MIB);
    let error = run_imaging(
        &device,
        &mut other,
        &mut store,
        &Recorder::default(),
        &ImagingOptions::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    let mut map = new_map(&device);
    let unaligned = ImagingOptions {
        block_size: 1000,
        ..ImagingOptions::default()
    };
    let error = run_imaging(
        &device,
        &mut map,
        &mut store,
        &Recorder::default(),
        &unaligned,
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}
