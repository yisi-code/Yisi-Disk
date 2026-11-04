//! 执行写入计划（docs/architecture.md 4.2）：核对设备、核对写前内容、写入、刷盘、读回。
//!
//! 执行只有这一份：镜像文件由宿主以普通权限执行（[`ImageFile`]），物理盘由磁盘代理执行。执行前必须已经存好备份，
//! 所以入口只接受 [`SavedBackup`]；备份就是计划里写前的内容，执行时先与盘上现在的内容逐字节核对，对得上才写。

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use yisi_io::{DeviceIdentity, PlanError, WritePlan, image_identity};

use crate::backup::SavedBackup;
use crate::protocol::WriteExecuted;

/// 能执行写入计划的设备。
pub trait WriteTarget {
    /// 实际打开的设备，与计划里的逐项核对。
    fn identity(&self) -> &DeviceIdentity;
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    /// 把写入交给设备并等它落盘。
    fn flush(&mut self) -> io::Result<()>;
}

/// 执行到哪一步。失败时报告停在这一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Step {
    Validate,
    Identity,
    Verify,
    Write,
    Flush,
    ReadBack,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecuteError {
    #[error("计划不合法：{0}")]
    Invalid(#[from] PlanError),
    #[error("设备与计划不符：计划写的是 {expected:?}，打开的是 {actual:?}")]
    WrongDevice {
        expected: DeviceIdentity,
        actual: DeviceIdentity,
    },
    #[error("偏移 {offset} 处的内容与生成计划时不同：设备在此之后被改动过")]
    Changed { offset: u64 },
    #[error("读取偏移 {offset} 失败")]
    Read {
        offset: u64,
        #[source]
        source: io::Error,
    },
    #[error("写入偏移 {offset} 失败")]
    Write {
        offset: u64,
        #[source]
        source: io::Error,
    },
    #[error("刷盘失败")]
    Flush(#[source] io::Error),
    #[error("读回偏移 {offset} 失败")]
    ReadBack {
        offset: u64,
        #[source]
        source: io::Error,
    },
    #[error("偏移 {offset} 处读回的内容与写入的不一致")]
    Mismatch { offset: u64 },
}

impl ExecuteError {
    pub fn step(&self) -> Step {
        match self {
            Self::Invalid(_) => Step::Validate,
            Self::WrongDevice { .. } => Step::Identity,
            Self::Changed { .. } | Self::Read { .. } => Step::Verify,
            Self::Write { .. } => Step::Write,
            Self::Flush(_) => Step::Flush,
            Self::ReadBack { .. } | Self::Mismatch { .. } => Step::ReadBack,
        }
    }
}

/// 执行失败：停在哪一步、为什么，以及失败前已经写下去的段数（为 0 时设备没被改动）。
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct Failure {
    #[source]
    pub error: ExecuteError,
    pub written: usize,
}

impl Failure {
    fn before_writing(error: ExecuteError) -> Self {
        Self { error, written: 0 }
    }
}

/// 执行备份里存着的计划。任何一步失败都停下；已经写下去的段可以用备份回滚。
pub fn execute(target: &mut dyn WriteTarget, backup: &SavedBackup) -> Result<(), Failure> {
    let plan = backup.plan();
    plan.validate()
        .map_err(|error| Failure::before_writing(error.into()))?;
    check_identity(plan, target.identity()).map_err(Failure::before_writing)?;

    // 先核对全部写前内容，一段都不写：任何一段对不上，设备就保持原样
    for write in &plan.writes {
        let current = read(target, write.offset, write.before.len()).map_err(|source| {
            Failure::before_writing(ExecuteError::Read {
                offset: write.offset,
                source,
            })
        })?;
        if current != write.before {
            return Err(Failure::before_writing(ExecuteError::Changed {
                offset: write.offset,
            }));
        }
    }

    for (index, write) in plan.writes.iter().enumerate() {
        target
            .write_all_at(write.offset, &write.after)
            .map_err(|source| Failure {
                error: ExecuteError::Write {
                    offset: write.offset,
                    source,
                },
                // 失败的这一段可能已经写下去一部分，算作已写
                written: index + 1,
            })?;
    }
    let written = plan.writes.len();
    target.flush().map_err(|source| Failure {
        error: ExecuteError::Flush(source),
        written,
    })?;

    for write in &plan.writes {
        let current = read(target, write.offset, write.after.len()).map_err(|source| Failure {
            error: ExecuteError::ReadBack {
                offset: write.offset,
                source,
            },
            written,
        })?;
        if current != write.after {
            return Err(Failure {
                error: ExecuteError::Mismatch {
                    offset: write.offset,
                },
                written,
            });
        }
    }
    Ok(())
}

/// 执行并把结果整理成答复：成功，或停在哪一步、失败前已经写下去几段。宿主执行镜像、代理执行物理盘共用。
pub fn execute_reported(target: &mut dyn WriteTarget, backup: &SavedBackup) -> WriteExecuted {
    match execute(target, backup) {
        Ok(()) => WriteExecuted {
            done: true,
            step: None,
            written: backup.plan().writes.len(),
            detail: None,
        },
        Err(failure) => {
            let detail = yisi_rpc::error_chain(&failure.error);
            tracing::warn!(%detail, written = failure.written, "写入计划执行失败");
            WriteExecuted {
                done: false,
                step: Some(failure.error.step()),
                written: failure.written,
                detail: Some(detail),
            }
        }
    }
}

fn check_identity(plan: &WritePlan, actual: &DeviceIdentity) -> Result<(), ExecuteError> {
    if &plan.device == actual {
        Ok(())
    } else {
        Err(ExecuteError::WrongDevice {
            expected: plan.device.clone(),
            actual: actual.clone(),
        })
    }
}

fn read(target: &mut dyn WriteTarget, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    target.read_exact_at(offset, &mut buf)?;
    Ok(buf)
}

/// 镜像文件。以读写、独占的方式打开：别的程序（包括正在读它的引擎）开着它时打不开，免得一边读一边改；
/// 不新建、不截断。
pub struct ImageFile {
    file: File,
    identity: DeviceIdentity,
}

impl ImageFile {
    /// `sector_size` 与打开镜像读取时用的相同。镜像正被别的程序打开时，Windows 上答复共享冲突（系统错误 32）。
    pub fn open(path: &Path, sector_size: u32) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(windows)]
        std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, 0);
        let file = options.open(path)?;
        let real = std::fs::canonicalize(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file,
            identity: DeviceIdentity {
                identity: image_identity(&real),
                size,
                sector_size,
            },
        })
    }
}

impl WriteTarget for ImageFile {
    fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)
    }

    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
}

#[cfg(test)]
mod tests;
