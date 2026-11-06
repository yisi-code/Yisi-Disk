//! JPEG：SOI 之后是一串标记段；SOS 之后是熵编码数据，其中的 0xFF 后面跟 0x00（填充）或 RST 标记，
//! 遇到其他标记时熵编码数据结束；以 EOI（FF D9）结束。缩略图嵌在 APP 段里，随段长度整段跳过。
//!
//! 文件在哪结束照标记段的走法判断。打开两段重组（[`Recovering::enable_resumes`]）时，哈夫曼编码的顺序式 JPEG
//! 另外逐位核对熵编码数据（[`entropy`]）：核对不影响上面的判断，只用来做两段重组——照片被存成两段时，
//! 核对在第一段之后断了，从断开前的块边界接着核对候选的第二段，一路解到 EOI 且 MCU 数对得上才算拼对。

mod entropy;

use std::collections::VecDeque;

use crate::plugin::{DataCheck, FileFormat, FormatInfo, Recovering, Resume, Signature};
use crate::walker::{Event, Scanner, Step, Structure, Walker};
use entropy::Entropy;
use yisi_part::bytes::be16;

const SOI: &[u8] = b"\xFF\xD8\xFF";
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;

/// 两段重组时第一段的结尾往回最多试这么多个块边界：核对在第一段之后断开时，随机或压缩过的数据里大约每 256 字节
/// 就有一个熵编码数据里不该有的标记，走不出几百字节就对不上了；别的文件开头那种有结构的数据可能要走一两 KiB。
/// 512 字节一块时 8 个是 4 KiB（常见的一簇）。每多一个，找第二段时要试的起点就多一个。
const RESUME_POINTS: usize = 8;

/// SOI 之后第一个标记的常见取值：APPn、DQT、SOFn、DHT、DRI、COM。
const FIRST_MARKERS: [u8; 20] = [
    0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xEB, 0xEC, 0xED, 0xEE, 0xEF,
    0xDB, 0xC0, 0xC4, 0xFE,
];

/// 熵编码数据里，0xFF 后面不是 0x00、不是 RST0–RST7、也不是填充用的 0xFF 时，就是下一个标记。
const ENTROPY_END: Scanner = Scanner {
    find: entropy::find_entropy_end,
    overlap: 1,
};

pub struct Jpg;

static INFO: FormatInfo = FormatInfo {
    extension: "jpg",
    variants: &[],
    description: "JPEG image",
    max_size: 256 << 20,
    enabled_by_default: true,
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: SOI,
}];

impl FileFormat for Jpg {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let marker = *block.get(3)?;
        (block.starts_with(SOI) && FIRST_MARKERS.contains(&marker))
            .then(|| Box::new(JpgRecovering::new()) as Box<dyn Recovering>)
    }
}

/// 一个正在恢复的 JPEG：标记段的走法判断在哪结束，熵编码数据另外逐位核对。
struct JpgRecovering {
    walker: Walker<JpgStructure>,
    /// 两段重组时核对第二段：核对一对不上就不认，结尾也要核对认了才算完整。
    strict: bool,
    /// 最近几块开头时的状态（两段重组的起点），核对对不上之后不再更新。
    marks: VecDeque<(u64, Walker<JpgStructure>)>,
    /// 已经送过数据。
    started: bool,
}

impl JpgRecovering {
    fn new() -> Self {
        Self {
            walker: Walker::new(
                INFO.max_size,
                JpgStructure {
                    phase: Phase::Marker,
                    seen_scan: false,
                    // 打开两段重组（`enable_resumes`）时才逐位核对
                    entropy: Entropy::off(),
                },
            ),
            strict: false,
            marks: VecDeque::new(),
            started: false,
        }
    }

    fn entropy(&self) -> &Entropy {
        &self.walker.structure().entropy
    }
}

impl Recovering for JpgRecovering {
    fn data_check(&mut self, offset: u64, block: &[u8]) -> DataCheck {
        self.started = true;
        if !self.strict && self.entropy().in_progress() {
            if self.marks.len() == RESUME_POINTS {
                self.marks.pop_front();
            }
            self.marks.push_back((offset, self.walker.clone()));
        }
        let verdict = self.walker.data_check(offset, block);
        if !self.strict {
            return verdict;
        }
        let entropy = self.entropy();
        match verdict {
            _ if entropy.is_broken() || entropy.is_inert() => DataCheck::Invalid,
            DataCheck::Complete(_) if !entropy.is_complete() => DataCheck::Invalid,
            verdict => verdict,
        }
    }

    fn finish(&mut self, received: u64) -> Option<u64> {
        let length = self.walker.finish(received);
        if self.strict && !self.entropy().is_complete() {
            return None;
        }
        length
    }

    fn waiting_at(&self) -> Option<u64> {
        self.walker.waiting_at()
    }

    fn intact(&mut self, received: u64) -> Option<u64> {
        self.walker.intact(received)
    }

    fn enable_resumes(&mut self) {
        // 只在还没送过数据时打开（之后打开就缺了文件头里的表）
        if !self.started {
            self.walker.structure_mut().entropy = Entropy::new();
        }
    }

    /// 核对没能一路走到 EOI（断了，或标记段的走法到了 EOI 而核对不认）时，给出最近几块的开头，最近的在前。
    fn resumes(&mut self) -> Vec<Resume> {
        let entropy = self.entropy();
        if self.strict || entropy.is_complete() || entropy.is_inert() {
            return Vec::new();
        }
        self.marks
            .drain(..)
            .rev()
            .filter(|&(offset, _)| offset > 0)
            .map(|(offset, walker)| {
                let entropy = walker.structure().entropy.clone();
                Resume::new(
                    offset,
                    move || {
                        Box::new(JpgRecovering {
                            walker: walker.clone(),
                            strict: true,
                            marks: VecDeque::new(),
                            started: true,
                        }) as Box<dyn Recovering>
                    },
                    move |data, block| entropy.possible_starts(data, block),
                )
            })
            .collect()
    }
}

#[derive(Clone, Copy)]
enum Phase {
    /// 等两个字节的标记。
    Marker,
    /// 等标记段的两字节长度。
    Length { marker: u8 },
    /// 等要逐位核对的标记段（SOF、DHT、DRI、SOS）的内容。
    Segment { marker: u8 },
}

#[derive(Clone)]
struct JpgStructure {
    phase: Phase,
    seen_scan: bool,
    entropy: Entropy,
}

impl JpgStructure {
    fn after_segment(&mut self, marker: u8, segment_end: u64) -> Step {
        if marker == SOS {
            self.seen_scan = true;
            Step::Scan {
                from: segment_end,
                scanner: ENTROPY_END,
            }
        } else {
            self.phase = Phase::Marker;
            Step::Need {
                at: segment_end,
                len: 2,
            }
        }
    }
}

impl Structure for JpgStructure {
    fn first(&self) -> Step {
        Step::Need { at: 2, len: 2 }
    }

    /// 部分恢复：图像数据开始之后才保存，到断点为止（之前的标记段都核对过）。
    fn intact(&self, cut: u64) -> Option<u64> {
        self.seen_scan.then_some(cut)
    }

    fn scanned(&mut self, _at: u64, bytes: &[u8]) {
        self.entropy.entropy(bytes);
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let (at, bytes) = match event {
            // 熵编码数据结束于一个标记
            Event::Found { at } => {
                self.entropy.entropy_end();
                self.phase = Phase::Marker;
                return Step::Need { at, len: 2 };
            }
            Event::Bytes { at, bytes } => (at, bytes),
        };
        match self.phase {
            Phase::Marker => {
                let (Some(&0xFF), Some(&marker)) = (bytes.first(), bytes.get(1)) else {
                    return Step::Invalid;
                };
                match marker {
                    // 填充字节：标记前可以有任意多个 0xFF
                    0xFF => Step::Need { at: at + 1, len: 2 },
                    EOI if self.seen_scan => {
                        self.entropy.eoi();
                        Step::Done(at + 2)
                    }
                    // 没有图像数据就结束，或在顶层又出现 SOI：不是有效文件
                    EOI | 0xD8 | 0x00 => Step::Invalid,
                    // 不带长度的标记
                    0x01 | 0xD0..=0xD7 => Step::Need { at: at + 2, len: 2 },
                    _ => {
                        self.phase = Phase::Length { marker };
                        Step::Need { at: at + 2, len: 2 }
                    }
                }
            }
            Phase::Length { marker } => {
                let Some(length) = be16(bytes, 0).filter(|&length| length >= 2) else {
                    return Step::Invalid;
                };
                if length > 2 && self.entropy.wants(marker) {
                    self.phase = Phase::Segment { marker };
                    return Step::Need {
                        at: at + 2,
                        len: usize::from(length) - 2,
                    };
                }
                self.after_segment(marker, at + u64::from(length))
            }
            Phase::Segment { marker } => {
                self.entropy.segment(marker, bytes);
                self.after_segment(marker, at + bytes.len() as u64)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn recover(data: &[u8]) -> Box<dyn Recovering> {
        Jpg.header_check(data).unwrap()
    }

    #[test]
    fn skips_thumbnail_stuffing_and_restart_markers() {
        let sample = samples::jpg(4000);
        let mut data = sample.clone();
        data.extend_from_slice(b"\xFF\xD9 trailing bytes after EOI");
        check_all_splits(|| recover(&data), &data, Some(sample.len() as u64));
    }

    #[test]
    fn progressive_scans_continue_after_entropy_data() {
        let sample = samples::jpg_progressive(1500);
        check_all_splits(|| recover(&sample), &sample, Some(sample.len() as u64));
    }

    #[test]
    fn missing_eoi_or_scan_is_incomplete() {
        let sample = samples::jpg(500);
        let truncated = &sample[..sample.len() - 2];
        check_all_splits(|| recover(truncated), truncated, None);

        let no_scan = b"\xFF\xD8\xFF\xE0\x00\x04ab\xFF\xD9";
        check_all_splits(|| recover(no_scan), no_scan, None);
    }

    /// 按 `block` 大小送入，返回标记段走法的结论与送完后的状态。
    fn walk(data: &[u8], block: usize) -> (DataCheck, JpgRecovering) {
        let mut recovering = JpgRecovering::new();
        recovering.enable_resumes();
        let mut verdict = DataCheck::Continue;
        for (index, piece) in data.chunks(block).enumerate() {
            verdict = recovering.data_check((index * block) as u64, piece);
            if verdict != DataCheck::Continue {
                break;
            }
        }
        (verdict, recovering)
    }

    fn layouts() -> Vec<samples::JpegLayout> {
        let base = samples::JpegLayout::default();
        vec![
            base.clone(),
            samples::JpegLayout {
                color: false,
                width: 300,
                height: 200,
                ..base.clone()
            },
            samples::JpegLayout {
                restart: 7,
                width: 333,
                height: 97,
                seed: 9,
                ..base.clone()
            },
            samples::JpegLayout {
                restart: 1,
                color: false,
                detail: 1,
                ..base
            },
        ]
    }

    #[test]
    fn baseline_entropy_data_is_checked_to_the_end() {
        for layout in layouts() {
            let sample = samples::jpg_baseline(&layout);
            check_all_splits(|| recover(&sample), &sample, Some(sample.len() as u64));
            for block in [1, 7, 512, 4096] {
                let (verdict, mut recovering) = walk(&sample, block);
                assert_eq!(verdict, DataCheck::Complete(sample.len() as u64));
                assert!(recovering.entropy().is_complete(), "{layout:?} 块 {block}");
                assert!(recovering.resumes().is_empty());
            }
        }
    }

    #[test]
    fn nothing_is_checked_unless_reassembly_is_enabled() {
        let sample = samples::jpg_baseline(&samples::JpegLayout::default());
        let cut = sample.len() / 2 / 512 * 512;
        let data = split(&sample, cut, &samples::Noise::new(8).bytes(512 * 8));
        let mut recovering = JpgRecovering::new();
        for (index, piece) in data.chunks(512).enumerate() {
            if recovering.data_check((index * 512) as u64, piece) != DataCheck::Continue {
                break;
            }
        }
        assert!(recovering.entropy().is_inert());
        assert!(recovering.marks.is_empty());
        assert!(recovering.resumes().is_empty());
        // 送过数据之后再打开也不算数（文件头里的表已经过去了）
        recovering.enable_resumes();
        assert!(recovering.entropy().is_inert());
    }

    #[test]
    fn unsupported_or_unparsable_tables_are_not_checked() {
        // 渐进式与表不成样子的样本：照原来的走法判断，不核对、也不给两段重组的起点
        for sample in [samples::jpg_progressive(3000), samples::jpg(3000)] {
            let (verdict, mut recovering) = walk(&sample, 512);
            assert_eq!(verdict, DataCheck::Complete(sample.len() as u64));
            assert!(recovering.entropy().is_inert());
            assert!(recovering.resumes().is_empty());
        }
    }

    #[test]
    fn damaged_data_breaks_the_check_but_not_the_walk() {
        let sample = samples::jpg_baseline(&samples::JpegLayout::default());
        let mut damaged = sample.clone();
        let middle = sample.len() / 2;
        // 一段换成不含 0xFF 的随机字节：标记段的走法照样走到 EOI。
        // （只改一两个字节时哈夫曼码常常几个符号后就重新对齐，MCU 数也不变，核对不出来）
        let mut noise = samples::Noise::new(5);
        for byte in &mut damaged[middle - 1..middle + 300] {
            *byte = noise.next_u64() as u8 & 0x7F;
        }
        let (verdict, mut recovering) = walk(&damaged, 512);
        assert_eq!(verdict, DataCheck::Complete(sample.len() as u64));
        assert!(recovering.entropy().is_broken());
        let resumes = recovering.resumes();
        assert!(!resumes.is_empty() && resumes.len() <= RESUME_POINTS);
        assert!(
            resumes
                .windows(2)
                .all(|pair| pair[0].first_len > pair[1].first_len)
        );
        assert!(
            resumes
                .iter()
                .all(|resume| resume.first_len <= middle as u64)
        );
    }

    /// 第一段 `sample[..cut]`，之后是 `gap`，再之后是第二段。
    fn split(sample: &[u8], cut: usize, gap: &[u8]) -> Vec<u8> {
        let mut data = sample[..cut].to_vec();
        data.extend_from_slice(gap);
        data.extend_from_slice(&sample[cut..]);
        data
    }

    /// 从 `resume` 接着送 `second`（按 512 字节一块），返回结论。
    fn continue_with(resume: &Resume, second: &[u8]) -> DataCheck {
        let mut recovering = resume.start();
        for (index, piece) in second.chunks(512).enumerate() {
            match recovering.data_check(resume.first_len + (index * 512) as u64, piece) {
                DataCheck::Continue => {}
                verdict => return verdict,
            }
        }
        recovering
            .finish(resume.first_len + second.len() as u64)
            .map_or(DataCheck::Invalid, DataCheck::Complete)
    }

    #[test]
    fn the_second_fragment_is_accepted_only_from_where_the_first_ends() {
        let mut noise = samples::Noise::new(3);
        for layout in layouts() {
            let sample = samples::jpg_baseline(&layout);
            let cut = (sample.len() / 2) / 512 * 512;
            let data = split(&sample, cut, &noise.bytes(512 * 5));
            let (_, mut recovering) = walk(&data, 512);
            assert!(recovering.entropy().is_broken(), "{layout:?}");
            let resumes = recovering.resumes();
            let right = resumes
                .iter()
                .find(|resume| resume.first_len == cut as u64)
                .unwrap_or_else(|| panic!("{layout:?} 没有从 {cut} 接着的起点"));
            // 粗看：随机数据里的块开头大多排除了，真正的第二段开头留着。样本小，剩下的图像数据至少要占的字节
            // 只有几百，随机数据里隔这么远才出现标记的块开头还有一成多；照片大时这个下限是几十 KiB，几乎都排除
            let mut window = noise.bytes(512 * 300);
            window.extend_from_slice(&sample[cut..]);
            let starts = right.possible_starts(&window, 512);
            assert!(starts.contains(&300), "{layout:?} 把真正的开头排除了");
            let random = starts.iter().filter(|&&index| index < 300).count();
            assert!(random < 75, "{layout:?} 随机数据里留下了 {random} 个");
            assert_eq!(
                continue_with(right, &sample[cut..]),
                DataCheck::Complete(sample.len() as u64),
                "{layout:?}"
            );
            // 同一个起点可以再来一次
            assert_eq!(
                continue_with(right, &sample[cut..]),
                DataCheck::Complete(sample.len() as u64)
            );
            // 第二段从错的位置开始、或第一段结尾不对：一路核对下来对不上
            assert_eq!(
                continue_with(right, &sample[cut + 512..]),
                DataCheck::Invalid
            );
            assert_eq!(continue_with(right, &data[cut..]), DataCheck::Invalid);
            for wrong in resumes
                .iter()
                .filter(|resume| resume.first_len != cut as u64)
            {
                assert_eq!(
                    continue_with(wrong, &sample[cut..]),
                    DataCheck::Invalid,
                    "{layout:?} 从 {} 接着",
                    wrong.first_len
                );
            }
        }
    }

    #[test]
    fn another_jpegs_tail_with_the_same_tables_is_not_a_second_fragment() {
        let sample = samples::jpg_baseline(&samples::JpegLayout::default());
        let other = samples::jpg_baseline(&samples::JpegLayout {
            seed: 77,
            ..samples::JpegLayout::default()
        });
        let cut = (sample.len() / 3) / 512 * 512;
        let other_tail = &other[other.len() / 2 / 512 * 512..];
        let data = split(&sample, cut, other_tail);
        // 标记段的走法把别的照片的尾巴当成了自己的，走到它的 EOI；逐位核对不认
        let (verdict, mut recovering) = walk(&data, 512);
        assert_eq!(
            verdict,
            DataCheck::Complete((cut + other_tail.len()) as u64)
        );
        assert!(recovering.entropy().is_broken());
        for resume in recovering.resumes() {
            assert_eq!(continue_with(&resume, other_tail), DataCheck::Invalid);
            let verdict = continue_with(&resume, &sample[cut..]);
            assert_eq!(
                verdict == DataCheck::Complete(sample.len() as u64),
                resume.first_len == cut as u64
            );
        }
    }

    #[test]
    fn mutated_baseline_samples_never_panic() {
        let mut noise = samples::Noise::new(11);
        for layout in layouts() {
            let sample = samples::jpg_baseline(&layout);
            for _ in 0..200 {
                let mut data = sample.clone();
                for _ in 0..1 + noise.next_u64() % 4 {
                    let at = (noise.next_u64() % data.len() as u64) as usize;
                    data[at] = noise.next_u64() as u8;
                }
                let block = 1 + (noise.next_u64() % 700) as usize;
                let (verdict, mut recovering) = walk(&data, block);
                if let DataCheck::Complete(length) = verdict {
                    assert!(length <= data.len() as u64);
                }
                for resume in recovering.resumes() {
                    let tail = &data[resume.first_len as usize..];
                    if let DataCheck::Complete(length) = continue_with(&resume, tail) {
                        assert!(length <= data.len() as u64);
                    }
                }
            }
        }
    }

    /// 真实照片逐位核对：`YISI_JPEG_SAMPLES` 指向放着 .jpg 的目录。
    /// `cargo test -p yisi-formats jpg::tests::real_photos -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_photos_are_checked_to_the_end() {
        let dir = std::env::var("YISI_JPEG_SAMPLES").expect("设置 YISI_JPEG_SAMPLES");
        let (mut checked, mut inert, mut bytes) = (0, 0, 0);
        let mut elapsed = std::time::Duration::ZERO;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let data = std::fs::read(&path).unwrap();
            // 扩展名是 .jpg、内容是别的格式的不算
            if !data.starts_with(SOI) {
                continue;
            }
            let started = std::time::Instant::now();
            let (verdict, recovering) = walk(&data, 512);
            elapsed += started.elapsed();
            bytes += data.len();
            assert_eq!(verdict, DataCheck::Complete(data.len() as u64), "{path:?}");
            let entropy = recovering.entropy();
            if entropy.is_inert() {
                inert += 1;
            } else {
                assert!(entropy.is_complete(), "{path:?} 核对没走到底");
                checked += 1;
            }
        }
        println!(
            "逐位核对到底 {checked} 张，不支持的编码方式 {inert} 张；共 {} KiB，按 512 字节一块送入用时 {elapsed:?}",
            bytes >> 10
        );
        assert!(checked > 0);
    }
}
