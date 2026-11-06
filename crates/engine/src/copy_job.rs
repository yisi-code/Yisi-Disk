//! 复制任务：把分区里选中的文件与目录（连同其中全部内容）复制到输出目录，保留相对路径。
//! 源只读；单个文件读不出时记为失败、继续下一个，写输出出错（例如输出盘满）时整个任务失败。
//! NTFS 分区上选了目录时，先扫一遍 MFT，目录里已删除的项才能一起复制；选了「找不到原目录的文件」这个虚拟目录时，
//! FAT / exFAT 也先扫一遍空闲簇。
//! 复制出的文件带上卷上记的修改时间（目录不带：往里写文件时系统会改它）。

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use yisi_carve::{PARTIAL_SUFFIX, partial_path, rename_new};
use yisi_engine::protocol::{COPIED, CopiedFile, CopySummary, JobCopied};
use yisi_fs::{FileRef, Moment, Stamp, Volume};

use crate::browse::scan_stage;
use crate::job::{Batcher, JobContext, JobOutcome};
use crate::runtime::error_chain;

const CHUNK: usize = 1 << 20;
/// 展开目录时最多这么多层、这么多项，挡住构造出来的环与超大目录树。
const MAX_DEPTH: usize = 64;
const MAX_PLANNED: usize = 1_000_000;
/// Windows 一级名字最多 255 个 UTF-16 单元。
const MAX_NAME_UNITS: usize = 255;

/// 要复制的一项：文件的定位信息，以及输出目录下的相对路径（一级一项，未清洗）。
pub struct Planned {
    pub file: FileRef,
    pub path: Vec<String>,
}

pub fn run(
    volume: &Volume,
    items: Vec<Planned>,
    output_dir: &Path,
    context: &JobContext,
) -> Result<JobOutcome, String> {
    let started = Instant::now();
    let reporter = Reporter::new(context);
    let mut failed = 0u64;

    // NTFS 目录里的已删除项要扫过 MFT 才列得出；「找不到原目录的文件」这个虚拟目录要扫过才有内容
    // （FAT / exFAT 要读一遍空闲簇；里面的各个文件夹本身不靠扫描，直接打得开）
    let scan_first = (volume.needs_scan() && items.iter().any(|item| item.file.is_dir))
        || (!volume.is_scanned() && items.iter().any(|item| item.file.is_orphans()));
    if scan_first {
        let scanned = volume
            .scan(&mut |done, total| {
                context.stage_progress(scan_stage(volume), done, total);
                !context.is_cancelled()
            })
            .map_err(|error| format!("扫描已删除的项失败：{}", error_chain(&error)))?;
        if scanned.is_none() {
            return Ok(JobOutcome::Cancelled);
        }
    }

    // 先展开目录，算出总字节数，进度才有意义
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for item in items {
        expand(
            volume,
            item,
            0,
            &mut files,
            &mut dirs,
            &mut |path, message| {
                failed += 1;
                reporter.push(CopiedFile {
                    path,
                    size: 0,
                    error: Some(message),
                    all_zero: false,
                    offset: None,
                });
            },
        );
        if context.is_cancelled() {
            reporter.flush();
            return Ok(JobOutcome::Cancelled);
        }
    }
    let total: u64 = files.iter().map(|item| item.file.size).sum();
    let mut done = 0u64;
    context.progress(0, total);

    let mut names = Names::default();
    // 空目录也建出来
    for dir in &dirs {
        let relative = names.directory(dir);
        fs::create_dir_all(output_dir.join(&relative)).map_err(|error| {
            format!("无法创建 {}：{error}", output_dir.join(&relative).display())
        })?;
    }
    let mut copied = 0u64;
    let mut bytes = 0u64;
    for item in &files {
        if context.is_cancelled() {
            reporter.flush();
            return Ok(JobOutcome::Cancelled);
        }
        let relative = names.file(&item.path, output_dir);
        let target = output_dir.join(&relative);
        let shown = to_display(&relative);
        match copy_one(
            volume, &item.file, &target, context, &reporter, &mut done, total,
        ) {
            Ok(CopyResult::Done { size, all_zero }) => {
                copied += 1;
                bytes += size;
                reporter.push(CopiedFile {
                    path: shown,
                    size,
                    error: None,
                    all_zero,
                    offset: None,
                });
            }
            Ok(CopyResult::Cancelled) => {
                reporter.flush();
                return Ok(JobOutcome::Cancelled);
            }
            Ok(CopyResult::SourceFailed(message)) => {
                failed += 1;
                reporter.push(CopiedFile {
                    path: shown,
                    size: item.file.size,
                    error: Some(message),
                    all_zero: false,
                    offset: None,
                });
            }
            Err(error) => {
                reporter.flush();
                return Err(format!("写入 {} 失败：{error}", target.display()));
            }
        }
        context.progress(done, total);
    }
    reporter.flush();
    context.progress(total, total);
    let summary = CopySummary {
        output_dir: output_dir.display().to_string(),
        files: copied,
        bytes,
        failed,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    serde_json::to_value(summary)
        .map(JobOutcome::Completed)
        .map_err(|error| format!("无法编码复制结果：{error}"))
}

/// 把一项展开成要复制的文件和要建的目录；列不出的目录经 `fail` 报告。
fn expand(
    volume: &Volume,
    item: Planned,
    depth: usize,
    files: &mut Vec<Planned>,
    dirs: &mut Vec<Vec<String>>,
    fail: &mut dyn FnMut(String, String),
) {
    if files.len() + dirs.len() >= MAX_PLANNED {
        return;
    }
    if !item.file.is_dir {
        files.push(item);
        return;
    }
    if depth >= MAX_DEPTH {
        fail(item.path.join("/"), "目录层级太深，没有展开".to_owned());
        return;
    }
    let listing = match volume.list(&item.file) {
        Ok(listing) => listing,
        Err(error) => {
            fail(
                item.path.join("/"),
                format!("列不出目录：{}", error_chain(&error)),
            );
            return;
        }
    };
    dirs.push(item.path.clone());
    for entry in listing.entries {
        let mut path = item.path.clone();
        path.push(entry.name);
        expand(
            volume,
            Planned {
                file: entry.file,
                path,
            },
            depth + 1,
            files,
            dirs,
            fail,
        );
    }
}

enum CopyResult {
    /// 实际写出的长度（NTFS 以 MFT 记录为准，可能与目录里记的不同），以及内容是否全为 0。
    Done {
        size: u64,
        all_zero: bool,
    },
    Cancelled,
    /// 源读不出（簇链断了、读错误）：这个文件算失败，继续下一个。
    SourceFailed(String),
}

/// 先写 `.part`，写完再改名。读源出错返回 `SourceFailed`，写输出出错返回 `Err`。
fn copy_one(
    volume: &Volume,
    file: &FileRef,
    target: &Path,
    context: &JobContext,
    reporter: &Reporter<'_>,
    done: &mut u64,
    total: u64,
) -> io::Result<CopyResult> {
    let mut reader = match volume.open_file(file) {
        Ok(reader) => reader,
        Err(error) => return Ok(CopyResult::SourceFailed(error_chain(&error))),
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let partial = partial_path(target);
    let mut output = File::create_new(&partial)?;
    let mut buffer = vec![0u8; CHUNK];
    let mut written = 0u64;
    let mut all_zero = true;
    let outcome = loop {
        if context.is_cancelled() {
            break CopyResult::Cancelled;
        }
        let read = match reader.read(&mut buffer) {
            Ok(0) => {
                break CopyResult::Done {
                    size: written,
                    all_zero: all_zero && written > 0,
                };
            }
            Ok(read) => read,
            Err(error) => break CopyResult::SourceFailed(error.to_string()),
        };
        let data = buffer.get(..read).unwrap_or_default();
        all_zero = all_zero && data.iter().all(|&byte| byte == 0);
        written += read as u64;
        if let Err(error) = output.write_all(data) {
            drop(output);
            let _ = fs::remove_file(&partial);
            return Err(error);
        }
        *done += read as u64;
        context.progress(*done, total);
        reporter.tick();
    };
    if matches!(outcome, CopyResult::Done { .. }) {
        set_modified(&output, file.modified, target);
    }
    drop(output);
    match outcome {
        CopyResult::Done { .. } => {
            if let Err(error) = rename_new(&partial, target) {
                let _ = fs::remove_file(&partial);
                return Err(error);
            }
        }
        _ => {
            let _ = fs::remove_file(&partial);
        }
    }
    Ok(outcome)
}

/// 把卷上记的修改时间设到复制出的文件上；没记、记的不合法或设不上都不算复制失败。
fn set_modified(output: &File, stamp: Option<Stamp>, target: &Path) {
    let Some(time) = stamp.and_then(|stamp| stamp.moment()).and_then(system_time) else {
        return;
    };
    if let Err(error) = output.set_modified(time) {
        tracing::warn!(target = %target.display(), %error, "设不上修改时间");
    }
}

/// FAT 记的本地时间按本机时区换算（[`yisi_io::local_to_utc`]）。
fn system_time(moment: Moment) -> Option<SystemTime> {
    let (seconds, nanos) = match moment {
        Moment::Utc { seconds, nanos } => (seconds, nanos),
        Moment::Local { seconds } => (yisi_io::local_to_utc(seconds)?, 0),
    };
    let whole = Duration::from_secs(seconds.unsigned_abs());
    let base = if seconds >= 0 {
        UNIX_EPOCH.checked_add(whole)?
    } else {
        UNIX_EPOCH.checked_sub(whole)?
    };
    base.checked_add(Duration::from_nanos(u64::from(nanos)))
}

/// 输出路径：每一级清洗成系统允许的名字；与本任务起过的名字、目录里已有的文件（含写入中途的 `.part`）
/// 重名时加序号，不覆盖。复制与保存深度恢复结果共用。
#[derive(Default)]
pub(crate) struct Names {
    taken: HashSet<PathBuf>,
}

impl Names {
    fn directory(&mut self, path: &[String]) -> PathBuf {
        let relative: PathBuf = path.iter().map(|part| sanitize(part)).collect();
        self.taken.insert(key(&relative));
        relative
    }

    pub(crate) fn file(&mut self, path: &[String], output_dir: &Path) -> PathBuf {
        let (name, parents) = match path.split_last() {
            Some((name, parents)) => (sanitize(name), parents),
            None => ("_".to_owned(), &[][..]),
        };
        let parent: PathBuf = parents.iter().map(|part| sanitize(part)).collect();
        let (stem, extension) = match name.rfind('.') {
            Some(dot) if dot > 0 => name.split_at(dot),
            _ => (name.as_str(), ""),
        };
        for attempt in 0u32.. {
            let candidate = if attempt == 0 {
                name.clone()
            } else {
                format!("{stem} ({attempt}){extension}")
            };
            let relative = parent.join(candidate);
            let target = output_dir.join(&relative);
            if !self.taken.contains(&key(&relative))
                && !target.exists()
                && !partial_path(&target).exists()
            {
                self.taken.insert(key(&relative));
                return relative;
            }
        }
        parent.join(name)
    }
}

/// Windows 的文件名不分大小写，按小写比较是否重名。
fn key(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

fn to_display(relative: &Path) -> String {
    relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// 清洗成各系统都能用的一级名字：去掉控制字符和 `<>:"/\|?*`，去掉结尾的点和空格，
/// 避开 `CON`、`NUL`、`COM1` 这类保留名，不允许 `.`、`..` 和空名字，长度截到 255 个 UTF-16 单元；
/// 以 `.part` 结尾的把那个点换成 `_`（这个后缀留给写入中途的文件，宿主不打开）。
pub fn sanitize(name: &str) -> String {
    let mut clean: String = name
        .chars()
        .map(|ch| {
            if ch.is_control() || "<>:\"/\\|?*".contains(ch) {
                '_'
            } else {
                ch
            }
        })
        .collect();
    while clean.ends_with(['.', ' ']) {
        clean.pop();
    }
    if clean.is_empty() {
        return "_".to_owned();
    }
    let stem = clean
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.ends_with(|ch: char| ch.is_ascii_digit()));
    if reserved {
        clean.insert(0, '_');
    }
    if clean.encode_utf16().count() > MAX_NAME_UNITS {
        let mut units = 0;
        clean = clean
            .chars()
            .take_while(|ch| {
                units += ch.len_utf16();
                units <= MAX_NAME_UNITS
            })
            .collect();
    }
    if clean.to_ascii_lowercase().ends_with(PARTIAL_SUFFIX) {
        let dot = clean.len() - PARTIAL_SUFFIX.len();
        clean.replace_range(dot..=dot, "_");
    }
    clean
}

/// 把复制好（或失败）的文件攒成一批发出；保存「只扫描」的结果时也用它。
pub(crate) struct Reporter<'a> {
    context: &'a JobContext,
    batch: Batcher<CopiedFile>,
}

impl<'a> Reporter<'a> {
    pub(crate) fn new(context: &'a JobContext) -> Self {
        Self {
            context,
            batch: Batcher::new(),
        }
    }

    fn send(&self, files: Vec<CopiedFile>) {
        self.context.notify(
            COPIED,
            JobCopied {
                job_id: self.context.id().to_owned(),
                files,
            },
        );
    }

    pub(crate) fn push(&self, file: CopiedFile) {
        self.batch.push(file, |files| self.send(files));
    }

    /// 处理一个大文件的过程中也要调用，免得之前做完的几个一直等到它结束才通知。
    pub(crate) fn tick(&self) {
        self.batch.tick(|files| self.send(files));
    }

    pub(crate) fn flush(&self) {
        self.batch.flush(|files| self.send(files));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_safe_for_any_system() {
        assert_eq!(sanitize("报告 2024.docx"), "报告 2024.docx");
        assert_eq!(sanitize("a<b>c:d\"e/f\\g|h?i*j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize("trailing. . "), "trailing");
        assert_eq!(sanitize(".."), "_");
        assert_eq!(sanitize(""), "_");
        assert_eq!(sanitize("CON"), "_CON");
        assert_eq!(sanitize("nul.txt"), "_nul.txt");
        assert_eq!(sanitize("com7.log"), "_com7.log");
        assert_eq!(sanitize("COMPUTER.txt"), "COMPUTER.txt");
        assert_eq!(sanitize("tab\there"), "tab_here");
        assert_eq!(sanitize(&"字".repeat(300)).chars().count(), 255);
        // .part 留给写入中途的文件
        assert_eq!(sanitize("a.part"), "a_part");
        assert_eq!(sanitize("B.PART"), "B_PART");
        assert_eq!(sanitize("a.part.txt"), "a.part.txt");
    }

    #[test]
    fn duplicate_names_get_numbered() {
        // 不存在的输出目录：只看本次已经起过的名字
        let output = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-output-dir");
        let mut names = Names::default();
        let path = |parts: &[&str]| {
            parts
                .iter()
                .map(|part| (*part).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names.file(&path(&["d", "a.txt"]), &output),
            Path::new("d").join("a.txt")
        );
        assert_eq!(
            names.file(&path(&["d", "A.TXT"]), &output),
            Path::new("d").join("A (1).TXT")
        );
        assert_eq!(
            names.file(&path(&["d", "a.txt"]), &output),
            Path::new("d").join("a (2).txt")
        );
        assert_eq!(
            names.file(&path(&["noext"]), &output),
            PathBuf::from("noext")
        );
        assert_eq!(
            names.file(&path(&["noext"]), &output),
            PathBuf::from("noext (1)")
        );
    }

    #[test]
    fn existing_and_partial_files_are_not_reused() {
        let output = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-copy-names");
        let _ = fs::remove_dir_all(&output);
        fs::create_dir_all(&output).unwrap();
        fs::write(output.join("a.txt"), b"x").unwrap();
        fs::write(output.join("b.txt.part"), b"x").unwrap();
        let mut names = Names::default();
        let one = |name: &str| vec![name.to_owned()];
        assert_eq!(
            names.file(&one("a.txt"), &output),
            PathBuf::from("a (1).txt")
        );
        // 别的任务还在写的 .part 也算占用
        assert_eq!(
            names.file(&one("b.txt"), &output),
            PathBuf::from("b (1).txt")
        );
        // 改名时目标已存在不覆盖
        fs::write(output.join("c.tmp"), b"new").unwrap();
        let error = rename_new(&output.join("c.tmp"), &output.join("a.txt")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(output.join("a.txt")).unwrap(), b"x");
        let _ = fs::remove_dir_all(&output);
    }
}
