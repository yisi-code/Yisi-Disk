//! 恢复出的文件写到哪里。

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

/// 一个恢复完成的文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredFile {
    pub name: String,
    pub extension: String,
    /// 在源设备上的起始偏移。
    pub offset: u64,
    pub length: u64,
}

pub trait Sink {
    /// 开始写一个从 `offset` 起、扩展名为 `extension` 的候选文件。
    fn create(&mut self, offset: u64, extension: &str) -> io::Result<Box<dyn SinkFile>>;
}

pub trait SinkFile {
    fn write(&mut self, data: &[u8]) -> io::Result<()>;

    /// 确认是完整文件，截到 `length` 字节保存。
    fn commit(self: Box<Self>, length: u64) -> io::Result<RecoveredFile>;

    /// 不是完整文件，丢弃已写的内容。
    fn discard(self: Box<Self>) -> io::Result<()>;
}

/// 写入中途的文件名后缀：先写 `名字.part`，写完再改成正式名字。正式名字不会以它结尾
/// （复制时清洗名字会避开），宿主据此不打开还在写的文件。
pub const PARTIAL_SUFFIX: &str = ".part";

/// `path` 写入中途用的名字：后面加 [`PARTIAL_SUFFIX`]。
pub fn partial_path(path: &Path) -> PathBuf {
    let mut partial = path.as_os_str().to_owned();
    partial.push(PARTIAL_SUFFIX);
    PathBuf::from(partial)
}

/// 把写好的文件改成正式名字；`to` 已存在时不覆盖，答复 `AlreadyExists`（Windows 上 `fs::rename` 会覆盖）。
pub fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    if to.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} 已存在", to.display()),
        ));
    }
    fs::rename(from, to)
}

/// 恢复出的文件的名字：`f` 加起始扇区号（按 512 字节计），例如 `f0000012345.jpg`。
pub fn recovered_name(offset: u64, extension: &str) -> String {
    format!("f{:010}.{extension}", offset / 512)
}

/// 写到一个目录里，文件名见 [`recovered_name`]。
/// 写入过程中带 `.part` 后缀，确认完整、全部写出后才改成正式名字。
pub struct DirectorySink {
    directory: PathBuf,
    /// 扫描的设备在源上的起点：只扫源上的一段时，名字与报告里的偏移仍按在源上的位置算。
    origin: u64,
}

impl DirectorySink {
    /// 目录不存在时创建。
    pub fn new(directory: impl AsRef<Path>) -> io::Result<Self> {
        Self::with_origin(directory, 0)
    }

    /// 扫描的是源上从 `origin` 起的一段时用它，名字和偏移按在源上的位置算。
    pub fn with_origin(directory: impl AsRef<Path>, origin: u64) -> io::Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        fs::create_dir_all(&directory)?;
        Ok(Self { directory, origin })
    }
}

impl Sink for DirectorySink {
    fn create(&mut self, offset: u64, extension: &str) -> io::Result<Box<dyn SinkFile>> {
        let offset = self.origin.saturating_add(offset);
        let name = recovered_name(offset, extension);
        let final_path = self.directory.join(&name);
        let partial_path = partial_path(&final_path);
        let file = File::create_new(&partial_path)?;
        Ok(Box::new(DirectoryFile {
            writer: BufWriter::new(file),
            partial_path,
            final_path,
            name,
            extension: extension.to_owned(),
            offset,
        }))
    }
}

struct DirectoryFile {
    writer: BufWriter<File>,
    partial_path: PathBuf,
    final_path: PathBuf,
    name: String,
    extension: String,
    offset: u64,
}

impl SinkFile for DirectoryFile {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.writer.write_all(data)
    }

    fn commit(self: Box<Self>, length: u64) -> io::Result<RecoveredFile> {
        let file = self
            .writer
            .into_inner()
            .map_err(|error| error.into_error())?;
        // 不逐个 sync：USB 机械盘上每次约 100 ms，文件密集处会把扫描拖到每秒十来个文件。
        // 数据已交给操作系统，引擎异常退出不影响落盘；系统崩溃或断电时，
        // 最近改名的文件可能内容不全，源盘只读，重扫即可。
        file.set_len(length)?;
        drop(file);
        rename_new(&self.partial_path, &self.final_path)?;
        Ok(RecoveredFile {
            name: self.name,
            extension: self.extension,
            offset: self.offset,
            length,
        })
    }

    fn discard(self: Box<Self>) -> io::Result<()> {
        drop(self.writer);
        fs::remove_file(&self.partial_path)
    }
}

/// 只扫描、不保存：识别流程照常，只记下每个文件在源上的位置和长度，什么都不写。
/// 名字与 [`DirectorySink`] 起的一样，之后选中保存时用的是同一个名字。
#[derive(Debug, Default)]
pub struct ScanOnlySink;

impl Sink for ScanOnlySink {
    fn create(&mut self, offset: u64, extension: &str) -> io::Result<Box<dyn SinkFile>> {
        Ok(Box::new(ScanOnlyFile {
            name: recovered_name(offset, extension),
            extension: extension.to_owned(),
            offset,
        }))
    }
}

struct ScanOnlyFile {
    name: String,
    extension: String,
    offset: u64,
}

impl SinkFile for ScanOnlyFile {
    fn write(&mut self, _data: &[u8]) -> io::Result<()> {
        Ok(())
    }

    fn commit(self: Box<Self>, length: u64) -> io::Result<RecoveredFile> {
        Ok(RecoveredFile {
            name: self.name,
            extension: self.extension,
            offset: self.offset,
            length,
        })
    }

    fn discard(self: Box<Self>) -> io::Result<()> {
        Ok(())
    }
}
