use std::fs::File;
use std::path::Path;

use crate::device::validate_sector_size;
use crate::read::{read_file_at, read_unaligned};
use crate::{BlockDevice, IoError};

/// raw（dd）镜像文件，只读。
#[derive(Debug)]
pub struct RawImage {
    file: File,
    size: u64,
    sector_size: u32,
}

impl RawImage {
    /// 以只读方式打开镜像文件。
    ///
    /// 镜像文件不记录扇区大小，由调用方给出：取镜像来源磁盘的逻辑扇区大小，通常是 512。
    pub fn open(path: impl AsRef<Path>, sector_size: u32) -> Result<Self, IoError> {
        let sector_size = validate_sector_size(sector_size)?;
        let path = path.as_ref();
        let open_error = |source| IoError::Open {
            path: path.to_path_buf(),
            source,
        };
        let file = File::open(path).map_err(open_error)?;
        let size = file.metadata().map_err(open_error)?.len();
        Ok(Self {
            file,
            size,
            sector_size,
        })
    }
}

impl BlockDevice for RawImage {
    fn size(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        read_unaligned(
            |position, chunk| read_file_at(&self.file, position, chunk),
            self.size,
            offset,
            buf,
        )
    }
}
