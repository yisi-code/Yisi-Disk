//! 做镜像之前看一眼：源上现在的分区与空隙各做到哪了，输出位置能不能只读一部分，已有的镜像能不能续做、
//! 做镜像之后源变没变（分区表对比加抽查）。另外查看以前做过的镜像现在怎样（被删了、移走了没有）。只读。

use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use yisi_engine::protocol::{
    ByteRange, ExistingImage, ImageDescription, ImageDescriptions, ImageInspection, ImageRegion,
    OutputInfo, PartitionChange, PartitionInfo, RegionCheck, RegionProgress,
};
use yisi_fs::BootCopy;
use yisi_io::{BadMap, BlockDevice, RecordedPartition, SegmentState};

use crate::browse::Session;
use crate::image_job::{self, check_header, check_length, progress_between};
use crate::job::ProgressSender;
use crate::runtime::Outbox;

/// 分区之间、末尾的空隙超过这么大才单独列出；不超过的是对齐留下的，读一部分时已经算进分区表区域。
const MIN_GAP: u64 = 1 << 20;
/// 抽查一共比较这么多块（按各段已读出的量分配，每段至少一块），每块这么大。
const SAMPLE_BLOCKS: u64 = 32;
const SAMPLE_SIZE: u64 = 64 << 10;

/// 看一眼 `device`（标识 `source`）与 `image_path`。分区表读不出时不列分区，整个源算一段空隙。
pub fn inspect(device: Arc<dyn BlockDevice>, image_path: &Path, source: &str) -> ImageInspection {
    let size = device.size();
    // 与浏览时一样识别各分区，界面用同一套文字描述它们；不发进度
    let quiet = ProgressSender::new(String::new(), Arc::new(Outbox::new(io::sink())));
    let (scheme, partitions) = match Session::open(Arc::clone(&device), quiet, BootCopy::Primary) {
        Ok((_session, opened)) => (Some(opened.scheme), opened.partitions),
        Err(error) => {
            tracing::warn!(error = %error.message, "读不出分区表");
            (None, Vec::new())
        }
    };
    let layout = layout(size, &partitions);
    let saved = Saved::read(image_path);
    let existing = existing_image(device.as_ref(), image_path, source, &saved, &partitions);
    let map = match (&existing, &saved.map) {
        (Some(existing), MapState::Read(map)) if existing.state == "ok" => Some(map),
        _ => None,
    };
    let checks = match map {
        Some(map) => check_regions(device.as_ref(), image_path, map, &layout),
        None => vec![None; layout.len()],
    };
    let regions = layout
        .iter()
        .zip(checks)
        .map(|(&(partition, offset, length), check)| ImageRegion {
            partition,
            offset,
            length,
            progress: match map {
                Some(map) => progress_between(map, offset, offset + length),
                None => RegionProgress {
                    untried_bytes: length,
                    ..RegionProgress::default()
                },
            },
            check,
        })
        .collect();
    let (output, output_error) = output_info(image_path);
    ImageInspection {
        size,
        sector_size: device.sector_size(),
        scheme,
        table_areas: image_job::table_areas(
            size,
            &partitions
                .iter()
                .map(|partition| (partition.offset, partition.length))
                .collect::<Vec<_>>(),
        ),
        partitions,
        regions,
        output,
        output_error,
        existing,
    }
}

/// 从头到尾的各段：（分区编号，起点，长度），分区以外超过 [`MIN_GAP`] 的空隙编号为空。
fn layout(size: u64, partitions: &[PartitionInfo]) -> Vec<(Option<u32>, u64, u64)> {
    let mut sorted: Vec<&PartitionInfo> = partitions.iter().collect();
    sorted.sort_by_key(|partition| (partition.offset, partition.number));
    let mut regions = Vec::new();
    let mut cursor = 0u64;
    for partition in sorted {
        if partition.offset > cursor.saturating_add(MIN_GAP) {
            regions.push((None, cursor, partition.offset - cursor));
        }
        regions.push((Some(partition.number), partition.offset, partition.length));
        cursor = cursor.max(partition.offset.saturating_add(partition.length));
    }
    if size > cursor.saturating_add(MIN_GAP) {
        regions.push((None, cursor, size - cursor));
    }
    regions
}

/// 镜像与坏区图在盘上的样子。
struct Saved {
    image_length: Option<u64>,
    map: MapState,
    /// 坏区图最后保存的时间（毫秒，Unix 纪元起）。
    saved_at_ms: Option<u64>,
}

enum MapState {
    Missing,
    Unreadable(String),
    Read(BadMap),
}

impl Saved {
    fn read(image_path: &Path) -> Self {
        let image_length = fs::metadata(image_path)
            .ok()
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len());
        let map_path = image_job::map_path(image_path);
        let saved_at_ms = fs::metadata(&map_path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .and_then(|since| u64::try_from(since.as_millis()).ok());
        let map = match fs::read_to_string(&map_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => MapState::Missing,
            Err(error) => {
                MapState::Unreadable(format!("读不到坏区图 {}：{error}", map_path.display()))
            }
            Ok(text) => match BadMap::parse(&text) {
                Ok(map) => MapState::Read(map),
                Err(error) => {
                    MapState::Unreadable(format!("坏区图 {} 读不懂：{error}", map_path.display()))
                }
            },
        };
        Self {
            image_length,
            map,
            saved_at_ms,
        }
    }

    /// 只看文件本身：missing（两个都不在）、imageMissing、mapMissing、mapUnreadable、ok。
    fn state(&self) -> (&'static str, Option<String>) {
        match (&self.map, self.image_length) {
            (MapState::Missing, None) => ("missing", None),
            (MapState::Missing, Some(_)) => ("mapMissing", None),
            (MapState::Unreadable(detail), _) => ("mapUnreadable", Some(detail.clone())),
            (MapState::Read(_), None) => ("imageMissing", None),
            (MapState::Read(_), Some(_)) => ("ok", None),
        }
    }
}

/// 这个位置已有的镜像；镜像与坏区图都不在时为空。
fn existing_image(
    device: &dyn BlockDevice,
    image_path: &Path,
    source: &str,
    saved: &Saved,
    partitions: &[PartitionInfo],
) -> Option<ExistingImage> {
    let (mut state, mut detail) = saved.state();
    if state == "missing" {
        return None;
    }
    let map = match &saved.map {
        MapState::Read(map) => Some(map),
        _ => None,
    };
    if let (Some(map), Some(length)) = (map, saved.image_length) {
        if let Err(problem) = check_header(map, device, source) {
            (state, detail) = ("otherSource", Some(problem));
        } else if let Err(problem) = check_length(length, device.size()) {
            (state, detail) = ("lengthMismatch", Some(problem));
        }
    }
    let usable = state == "ok";
    let sparse = usable
        && File::open(image_path)
            .ok()
            .is_some_and(|image| image_job::file_is_sparse(&image));
    let recorded = map.map(BadMap::partitions).unwrap_or_default();
    Some(ExistingImage {
        state: state.to_owned(),
        detail,
        source_identity: map.map(|map| map.header().source.clone()),
        sparse,
        partitions_recorded: !recorded.is_empty(),
        partition_changes: if usable {
            partition_changes(recorded, partitions)
        } else {
            Vec::new()
        },
        progress: match map {
            Some(map) if usable => progress_between(map, 0, map.header().size),
            _ => RegionProgress::default(),
        },
    })
}

/// 与上次开始读时相比，分区表的变化（按编号对应）。上次没记分区表（旧版本做的）时不比较。
fn partition_changes(
    recorded: &[RecordedPartition],
    current: &[PartitionInfo],
) -> Vec<PartitionChange> {
    if recorded.is_empty() {
        return Vec::new();
    }
    let range = |offset: u64, length: u64| ByteRange { offset, length };
    let mut changes = Vec::new();
    for before in recorded {
        match current.iter().find(|now| now.number == before.number) {
            None => changes.push(PartitionChange {
                kind: "removed".to_owned(),
                number: before.number,
                before: Some(range(before.offset, before.length)),
                after: None,
            }),
            Some(now) if now.offset != before.offset || now.length != before.length => {
                changes.push(PartitionChange {
                    kind: "moved".to_owned(),
                    number: before.number,
                    before: Some(range(before.offset, before.length)),
                    after: Some(range(now.offset, now.length)),
                });
            }
            Some(_) => {}
        }
    }
    for now in current {
        if !recorded.iter().any(|before| before.number == now.number) {
            changes.push(PartitionChange {
                kind: "added".to_owned(),
                number: now.number,
                before: None,
                after: Some(range(now.offset, now.length)),
            });
        }
    }
    changes
}

/// 抽查各段：从镜像里已读出的部分取几块，与源上同一位置比较。每段的第一块取它最前面读出的位置，其余随机。
fn check_regions(
    device: &dyn BlockDevice,
    image_path: &Path,
    map: &BadMap,
    layout: &[(Option<u32>, u64, u64)],
) -> Vec<Option<RegionCheck>> {
    let Ok(image) = File::open(image_path) else {
        return vec![None; layout.len()];
    };
    let sector = u64::from(device.sector_size());
    let done = |start: u64, end: u64| map.bytes_within(SegmentState::Done, start, end);
    let total_done: u64 = layout
        .iter()
        .map(|&(_, offset, length)| done(offset, offset + length))
        .sum();
    let mut random = Random::seeded();
    let mut source_block = vec![0u8; SAMPLE_SIZE as usize];
    let mut image_block = vec![0u8; SAMPLE_SIZE as usize];
    layout
        .iter()
        .map(|&(_, offset, length)| {
            let end = offset + length;
            let region_done = done(offset, end);
            if region_done == 0 {
                return None;
            }
            let pieces: Vec<(u64, u64)> = map
                .ranges(SegmentState::Done)
                .map(|(start, length)| (start.max(offset), (start + length).min(end)))
                .filter(|&(start, end)| start < end)
                .collect();
            let count = (u128::from(SAMPLE_BLOCKS) * u128::from(region_done)
                / u128::from(total_done.max(1)))
            .max(1) as u64;
            let mut check = RegionCheck::default();
            for index in 0..count {
                let position = if index == 0 {
                    0
                } else {
                    random.below(region_done)
                };
                let Some((start, length)) = locate(&pieces, position, sector) else {
                    continue;
                };
                let length = length as usize;
                check.checked += 1;
                if device
                    .read_exact_at(start, &mut source_block[..length])
                    .is_err()
                {
                    check.unreadable += 1;
                } else if read_image(&image, start, &mut image_block[..length]).is_err()
                    || source_block[..length] != image_block[..length]
                {
                    check.mismatched += 1;
                }
            }
            Some(check)
        })
        .collect()
}

/// 读出部分里第 `position` 个字节所在的一块：起点按扇区向下对齐（不出这一片），最长 [`SAMPLE_SIZE`]。
fn locate(pieces: &[(u64, u64)], mut position: u64, sector: u64) -> Option<(u64, u64)> {
    for &(start, end) in pieces {
        let length = end - start;
        if position < length {
            let at = start + position;
            let at = (at - at % sector).max(start);
            return Some((at, (end - at).min(SAMPLE_SIZE)));
        }
        position -= length;
    }
    None
}

fn read_image(image: &File, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
    #[cfg(windows)]
    {
        let mut filled = 0;
        while filled < buffer.len() {
            let read = std::os::windows::fs::FileExt::seek_read(
                image,
                &mut buffer[filled..],
                offset + filled as u64,
            )?;
            if read == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            filled += read;
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(image, buffer, offset)
    }
}

/// 抽查用的伪随机数（xorshift），不要求不可预测，只要每次取的位置不同。
struct Random(u64);

impl Random {
    fn seeded() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        Self(u64::from(nanos) | 1)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound.max(1)
    }
}

fn output_info(image_path: &Path) -> (Option<OutputInfo>, Option<String>) {
    #[cfg(windows)]
    match yisi_io::volume_info(image_path) {
        Ok(volume) => (
            Some(OutputInfo {
                file_system: volume.file_system,
                sparse: volume.sparse,
                free_bytes: volume.free_bytes,
            }),
            None,
        ),
        Err(error) => (None, Some(crate::runtime::error_chain(&error))),
    }
    #[cfg(not(windows))]
    {
        let _ = image_path;
        (None, Some("当前平台查不到输出位置的属性".to_owned()))
    }
}

/// 以前做过的镜像现在怎样：只读坏区图，不碰源。
pub fn describe(image_paths: &[String]) -> ImageDescriptions {
    let images = image_paths
        .iter()
        .map(|path| {
            let saved = Saved::read(Path::new(path));
            let (state, detail) = saved.state();
            let map = match &saved.map {
                MapState::Read(map) => Some(map),
                _ => None,
            };
            ImageDescription {
                image_path: path.clone(),
                state: state.to_owned(),
                detail,
                source_identity: map.map(|map| map.header().source.clone()),
                size: map.map(|map| map.header().size),
                progress: map
                    .filter(|_| state == "ok")
                    .map(|map| progress_between(map, 0, map.header().size)),
                saved_at_ms: saved.saved_at_ms,
            }
        })
        .collect();
    ImageDescriptions { images }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn partition(number: u32, offset: u64, length: u64) -> PartitionInfo {
        PartitionInfo {
            number,
            offset,
            length,
            type_id: String::new(),
            role: None,
            name: String::new(),
            bootable: false,
            logical: false,
            filesystem: "ntfs".to_owned(),
            label: None,
            browsable: true,
            needs_scan: true,
            times_utc: true,
        }
    }

    #[test]
    fn layout_lists_partitions_and_large_gaps() {
        let partitions = [
            partition(2, 100 * MIB, 50 * MIB),
            partition(1, MIB, 60 * MIB),
        ];
        let layout = layout(200 * MIB, &partitions);
        assert_eq!(
            layout,
            vec![
                (Some(1), MIB, 60 * MIB),
                (None, 61 * MIB, 39 * MIB),
                (Some(2), 100 * MIB, 50 * MIB),
                (None, 150 * MIB, 50 * MIB),
            ]
        );
        // 开头 1 MiB 的对齐留空不单独列出；没有分区时整个源是一段空隙
        assert_eq!(layout.first().map(|region| region.1), Some(MIB));
        assert_eq!(layout_of_empty(), vec![(None, 0, 10 * MIB)]);
    }

    fn layout_of_empty() -> Vec<(Option<u32>, u64, u64)> {
        layout(10 * MIB, &[])
    }

    #[test]
    fn partition_changes_are_matched_by_number() {
        let recorded = [
            RecordedPartition {
                number: 1,
                offset: MIB,
                length: 10 * MIB,
            },
            RecordedPartition {
                number: 2,
                offset: 20 * MIB,
                length: 10 * MIB,
            },
            RecordedPartition {
                number: 3,
                offset: 40 * MIB,
                length: 10 * MIB,
            },
        ];
        let current = [
            partition(1, MIB, 10 * MIB),
            partition(2, 20 * MIB, 15 * MIB),
            partition(4, 60 * MIB, MIB),
        ];
        let kinds: Vec<(String, u32)> = partition_changes(&recorded, &current)
            .into_iter()
            .map(|change| (change.kind, change.number))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("moved".to_owned(), 2),
                ("removed".to_owned(), 3),
                ("added".to_owned(), 4)
            ]
        );
        // 旧版本的坏区图没有记分区表：不比较
        assert!(partition_changes(&[], &current).is_empty());
    }

    #[test]
    fn locate_maps_a_position_into_the_read_pieces() {
        let pieces = [(4096, 8192), (100_000, 300_000)];
        assert_eq!(locate(&pieces, 0, 512), Some((4096, 4096)));
        assert_eq!(locate(&pieces, 4095, 512), Some((7680, 512)));
        // 第二片从 100000 起，不是扇区的整数倍：对齐后不出这一片
        assert_eq!(locate(&pieces, 4096, 512), Some((100_000, SAMPLE_SIZE)));
        assert_eq!(
            locate(&pieces, 4096 + 1000, 512),
            Some((100_864, SAMPLE_SIZE))
        );
        assert_eq!(locate(&pieces, 4096 + 200_000, 512), None);
    }
}
