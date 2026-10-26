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

/// 写到一个目录里，文件名为 `f` 加起始扇区号（按 512 字节计），例如 `f0000012345.jpg`。
/// 写入过程中带 `.part` 后缀，确认完整、全部写出后才改成正式名字。
pub struct DirectorySink {
    directory: PathBuf,
}

impl DirectorySink {
    /// 目录不存在时创建。
    pub fn new(directory: impl AsRef<Path>) -> io::Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        fs::create_dir_all(&directory)?;
        Ok(Self { directory })
    }
}

impl Sink for DirectorySink {
    fn create(&mut self, offset: u64, extension: &str) -> io::Result<Box<dyn SinkFile>> {
        let name = format!("f{:010}.{extension}", offset / 512);
        let final_path = self.directory.join(&name);
        let partial_path = self.directory.join(format!("{name}.part"));
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
        if self.final_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} 已存在", self.final_path.display()),
            ));
        }
        fs::rename(&self.partial_path, &self.final_path)?;
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
