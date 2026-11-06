//! 镜像文件：按开头的签名认格式（raw 或 E01），按格式打开。引擎读源、宿主判断能不能写都按这一份。

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::ewf::{EVF_SIGNATURE, UNSUPPORTED_SIGNATURES};
use crate::read::read_file_at;
use crate::{BlockDevice, EwfImage, IoError, RawImage};

/// 镜像文件的格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// raw（dd）：文件内容就是盘上的字节。
    Raw,
    /// EWF 家族（E01 等，开头有 EVF / LVF 签名）：只读，不能当写入计划的目标。
    Ewf,
}

/// 按开头 8 字节认 `path` 的格式：EWF 家族的签名（含还不支持的 L01、Ex01，打开时再说明）为 [`ImageFormat::Ewf`]，
/// 其余（含不到 8 字节的文件）为 [`ImageFormat::Raw`]。只看签名，不看扩展名。
pub fn image_format(path: &Path) -> Result<ImageFormat, IoError> {
    let file = File::open(path).map_err(|source| IoError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let mut head = [0u8; 8];
    let mut filled = 0;
    while filled < head.len() {
        match read_file_at(&file, filled as u64, &mut head[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(source) => {
                return Err(IoError::Open {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
    let ewf = head == EVF_SIGNATURE
        || UNSUPPORTED_SIGNATURES
            .iter()
            .any(|(signature, _)| *signature == head);
    Ok(if ewf {
        ImageFormat::Ewf
    } else {
        ImageFormat::Raw
    })
}

/// 打开的镜像文件。
pub struct OpenedImage {
    pub device: Box<dyn BlockDevice>,
    pub format: ImageFormat,
    /// 组成镜像的文件：raw 是它自己，E01 是全部段文件。
    pub files: Vec<PathBuf>,
}

/// 按格式打开镜像文件：E01 第一段按 EWF 读（后续段在同目录找，扇区大小取 E01 里记的），其他按 raw 读，
/// 扇区大小是 `raw_sector_size`（raw 镜像不记扇区大小，由调用方给出）。
pub fn open_image(path: &Path, raw_sector_size: u32) -> Result<OpenedImage, IoError> {
    match image_format(path)? {
        ImageFormat::Ewf => {
            let image = EwfImage::open(path)?;
            let files = image.segments().to_vec();
            Ok(OpenedImage {
                device: Box::new(image),
                format: ImageFormat::Ewf,
                files,
            })
        }
        ImageFormat::Raw => Ok(OpenedImage {
            device: Box::new(RawImage::open(path, raw_sector_size)?),
            format: ImageFormat::Raw,
            files: vec![path.to_path_buf()],
        }),
    }
}
