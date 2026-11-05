//! 写入计划：写哪块设备、写哪几段、每段写前与写后的内容（docs/architecture.md 4.2）。
//!
//! 这里只有数据与核对，没有写操作：引擎生成计划，镜像文件由宿主执行、物理盘由磁盘代理执行，执行器在 `yisi-broker`，
//! 引擎不依赖它。计划带着写前的完整内容：执行前存成备份，执行时与盘上现在的内容逐字节核对，回滚就是把它反过来执行。

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::device::validate_sector_size;
use crate::{BlockDevice, IoError};

/// 一份计划最多这么多段（修文件系统时 FAT 两份、改动分散，段数多；其余种类由宿主按种类卡得更紧）。
pub const MAX_WRITES: usize = 1024;

/// 一份计划最多写这么多字节：写前与写后各一份、按十六进制编码后约 32 MiB，装得进引擎发给宿主的一条消息
/// （`yisi_rpc::MAX_ENGINE_OUTPUT_LEN`）。
pub const MAX_PLAN_BYTES: u64 = 8 << 20;

/// 计划要写的设备。执行前与实际打开的设备逐项核对，防止写错盘或写错文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentity {
    /// 物理盘是厂商、型号、序列号，镜像文件是 [`image_identity`]，与坏区图里记的来源相同。
    pub identity: String,
    pub size: u64,
    pub sector_size: u32,
}

/// 要写的一段：从 `offset` 起，把 `before` 换成 `after`。两者等长，按扇区对齐。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SectorWrite {
    pub offset: u64,
    #[serde(with = "hex")]
    pub before: Vec<u8>,
    #[serde(with = "hex")]
    pub after: Vec<u8>,
}

impl SectorWrite {
    pub fn len(&self) -> u64 {
        self.after.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.after.is_empty()
    }

    fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.len())
    }
}

/// 按偏移排好的写入段：内容没变的段去掉，首尾相接的合并成一段。改写分区表与修文件系统生成计划时都按它收尾。
pub fn merge_writes(writes: impl IntoIterator<Item = SectorWrite>) -> Vec<SectorWrite> {
    let mut merged: Vec<SectorWrite> = Vec::new();
    for write in writes {
        if write.before == write.after {
            continue;
        }
        match merged.last_mut() {
            Some(last) if last.end() == Some(write.offset) => {
                last.before.extend_from_slice(&write.before);
                last.after.extend_from_slice(&write.after);
            }
            _ => merged.push(write),
        }
    }
    merged
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WritePlan {
    pub device: DeviceIdentity,
    /// 按偏移排序、互不重叠。
    pub writes: Vec<SectorWrite>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("计划里没有要写的内容")]
    Empty,
    #[error("计划最多 {MAX_WRITES} 段，这份有 {0} 段")]
    TooManyWrites(usize),
    #[error("计划最多写 {MAX_PLAN_BYTES} 字节，这份要写 {0} 字节")]
    TooLarge(u64),
    #[error("扇区大小 {0} 不受支持，应为 512 到 65536 之间的 2 的幂")]
    SectorSize(u32),
    #[error("偏移 {offset} 处的一段写前 {before} 字节、写后 {after} 字节，长度不同")]
    LengthMismatch {
        offset: u64,
        before: usize,
        after: usize,
    },
    #[error("偏移 {offset} 处长 {length} 字节的一段没有按扇区（{sector_size} 字节）对齐")]
    Unaligned {
        offset: u64,
        length: u64,
        sector_size: u32,
    },
    #[error("偏移 {offset} 处长 {length} 字节的一段超出了设备末尾（{size} 字节）")]
    OutOfRange { offset: u64, length: u64, size: u64 },
    #[error("偏移 {0} 处的一段与前一段重叠，或没有按偏移排序")]
    Overlap(u64),
    #[error("偏移 {0} 处的一段写前与写后相同")]
    Unchanged(u64),
}

impl WritePlan {
    /// 一共要写的字节数。
    pub fn total_bytes(&self) -> u64 {
        self.writes.iter().map(SectorWrite::len).sum()
    }

    /// 核对计划本身：段数与总量在上限内；每段写前写后等长、按扇区对齐、在设备内、确实有改动；按偏移排序、互不重叠。
    /// 生成、存备份、执行之前都核对一次。
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.writes.is_empty() {
            return Err(PlanError::Empty);
        }
        if self.writes.len() > MAX_WRITES {
            return Err(PlanError::TooManyWrites(self.writes.len()));
        }
        let total = self.total_bytes();
        if total > MAX_PLAN_BYTES {
            return Err(PlanError::TooLarge(total));
        }
        let sector_size = validate_sector_size(self.device.sector_size)
            .map_err(|_| PlanError::SectorSize(self.device.sector_size))?;
        let mut previous_end = 0;
        for write in &self.writes {
            let length = write.len();
            if write.before.len() != write.after.len() {
                return Err(PlanError::LengthMismatch {
                    offset: write.offset,
                    before: write.before.len(),
                    after: write.after.len(),
                });
            }
            let sector = u64::from(sector_size);
            if write.is_empty() || write.offset % sector != 0 || length % sector != 0 {
                return Err(PlanError::Unaligned {
                    offset: write.offset,
                    length,
                    sector_size,
                });
            }
            let in_device = write.end().is_some_and(|end| end <= self.device.size);
            if !in_device {
                return Err(PlanError::OutOfRange {
                    offset: write.offset,
                    length,
                    size: self.device.size,
                });
            }
            if write.offset < previous_end {
                return Err(PlanError::Overlap(write.offset));
            }
            if write.before == write.after {
                return Err(PlanError::Unchanged(write.offset));
            }
            previous_end = write.end().unwrap_or(u64::MAX);
        }
        Ok(())
    }

    /// 反过来的计划：把写后的内容换回写前的。回滚用它，盘上现在的内容必须仍是写后的，否则执行时拒绝。
    pub fn inverse(&self) -> Self {
        Self {
            device: self.device.clone(),
            writes: self
                .writes
                .iter()
                .map(|write| SectorWrite {
                    offset: write.offset,
                    before: write.after.clone(),
                    after: write.before.clone(),
                })
                .collect(),
        }
    }
}

/// 镜像文件的标识：`real_path` 是规范化（`canonicalize`）之后的路径。坏区图记的来源与写入计划的设备都用它。
pub fn image_identity(real_path: &Path) -> String {
    format!("image {}", real_path.display())
}

/// 把写入叠加在设备上的只读视图：读到要写的位置时给出写后的内容。生成计划时用它预演写入后的结果，不碰设备。
pub struct Overlay {
    base: Arc<dyn BlockDevice>,
    /// 按偏移排序、互不重叠。
    writes: Vec<SectorWrite>,
}

impl Overlay {
    pub fn new(base: Arc<dyn BlockDevice>, writes: &[SectorWrite]) -> Self {
        let mut writes = writes.to_vec();
        writes.sort_by_key(|write| write.offset);
        Self { base, writes }
    }

    /// `[offset, end)` 是否整段都会被写入覆盖：这样的读取不依赖设备，设备那里读不出也照样给出写后的内容。
    fn covered(&self, offset: u64, end: u64) -> bool {
        let mut cursor = offset;
        for write in &self.writes {
            let Some(write_end) = write.end() else {
                continue;
            };
            if write.offset <= cursor && write_end > cursor {
                cursor = write_end;
            }
        }
        cursor >= end
    }
}

impl BlockDevice for Overlay {
    fn size(&self) -> u64 {
        self.base.size()
    }

    fn sector_size(&self) -> u32 {
        self.base.sector_size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        let wanted = buf
            .len()
            .min(usize::try_from(self.size().saturating_sub(offset)).unwrap_or(usize::MAX));
        let end = offset.saturating_add(wanted as u64);
        let read = match self.base.read_at(offset, buf) {
            Ok(read) => read,
            Err(_) if wanted > 0 && self.covered(offset, end) => wanted,
            Err(error) => return Err(error),
        };
        let read_end = offset.saturating_add(read as u64);
        for write in &self.writes {
            let Some(write_end) = write.end() else {
                continue;
            };
            let from = write.offset.max(offset);
            let to = write_end.min(read_end);
            if from >= to {
                continue;
            }
            // 三个下标都落在各自缓冲区内：from、to 夹在两段的交集里
            let (Ok(dst), Ok(src), Ok(len)) = (
                usize::try_from(from - offset),
                usize::try_from(from - write.offset),
                usize::try_from(to - from),
            ) else {
                continue;
            };
            if let (Some(target), Some(source)) =
                (buf.get_mut(dst..dst + len), write.after.get(src..src + len))
            {
                target.copy_from_slice(source);
            }
        }
        Ok(read)
    }
}

/// 字节按十六进制小写编码，计划经 JSON 传递与存成备份时用。
mod hex {
    use std::fmt::Write;

    use serde::{Deserialize, Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        let mut text = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            let _ = write!(text, "{byte:02x}");
        }
        serializer.serialize_str(&text)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        if !text.len().is_multiple_of(2) {
            return Err(de::Error::custom("十六进制内容的长度应为偶数"));
        }
        text.as_bytes()
            .chunks(2)
            .map(|pair| {
                std::str::from_utf8(pair)
                    .ok()
                    .and_then(|digits| u8::from_str_radix(digits, 16).ok())
                    .ok_or_else(|| de::Error::custom("不是十六进制数字"))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
