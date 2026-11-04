//! 写入计划的备份文件：执行之前把整份计划（含写前内容）存下来，带 SHA-256 摘要。
//!
//! 执行时先核对盘上现在的内容与计划里写前的内容一致，所以备份就是写之前设备上的真实内容；回滚就是把备份里的计划
//! 反过来执行（[`WritePlan::inverse`]）。备份由宿主以普通权限写，磁盘代理不写任何文件。

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use yisi_io::{PlanError, WritePlan};

/// 备份文件的格式版本。
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupFile {
    version: u32,
    /// 计划按 JSON 编码后的 SHA-256（小写十六进制），读回时核对，文件被改坏或截断就不用它。
    sha256: String,
    plan: WritePlan,
}

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("计划不合法：{0}")]
    Invalid(#[from] PlanError),
    #[error("{} 已存在", .0.display())]
    Exists(PathBuf),
    #[error("无法写入或读取备份文件 {}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("备份文件 {} 读不懂：{reason}", .path.display())]
    Unreadable { path: PathBuf, reason: String },
    #[error("备份文件 {} 的摘要对不上，内容已损坏", .0.display())]
    Corrupted(PathBuf),
}

/// 已经落盘的备份。执行写入计划只接受它（[`crate::execute::execute`]），保证写之前备份已经存好。
#[derive(Debug)]
pub struct SavedBackup {
    path: PathBuf,
    plan: WritePlan,
}

impl SavedBackup {
    /// 存备份：先写 `<path>.tmp` 并同步到盘，再改成 `path`；`path` 已存在时不覆盖。目录不存在时创建。
    pub fn save(path: &Path, plan: WritePlan) -> Result<Self, BackupError> {
        plan.validate()?;
        let io_error = |source| BackupError::Io {
            path: path.to_path_buf(),
            source,
        };
        if path.exists() {
            return Err(BackupError::Exists(path.to_path_buf()));
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(io_error)?;
        }
        let file = BackupFile {
            version: VERSION,
            sha256: digest(&plan).map_err(|reason| BackupError::Unreadable {
                path: path.to_path_buf(),
                reason,
            })?,
            plan,
        };
        let text = serde_json::to_vec(&file).map_err(|error| BackupError::Unreadable {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
        let mut temp = path.as_os_str().to_owned();
        temp.push(".tmp");
        let temp = PathBuf::from(temp);
        let mut out = File::create(&temp).map_err(io_error)?;
        out.write_all(&text).map_err(io_error)?;
        out.sync_all().map_err(io_error)?;
        drop(out);
        // Windows 上 fs::rename 会覆盖已有文件：前面查过一次，这里再查一次，缩小与别的写入撞上的窗口
        if path.exists() {
            let _ = fs::remove_file(&temp);
            return Err(BackupError::Exists(path.to_path_buf()));
        }
        fs::rename(&temp, path).map_err(io_error)?;
        Ok(Self {
            path: path.to_path_buf(),
            plan: file.plan,
        })
    }

    /// 读回备份：格式版本、摘要、计划本身都要对。
    pub fn load(path: &Path) -> Result<Self, BackupError> {
        let unreadable = |reason: String| BackupError::Unreadable {
            path: path.to_path_buf(),
            reason,
        };
        let text = fs::read(path).map_err(|source| BackupError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let file: BackupFile =
            serde_json::from_slice(&text).map_err(|error| unreadable(error.to_string()))?;
        if file.version != VERSION {
            return Err(unreadable(format!("格式版本 {} 不认识", file.version)));
        }
        if digest(&file.plan).map_err(unreadable)? != file.sha256 {
            return Err(BackupError::Corrupted(path.to_path_buf()));
        }
        file.plan.validate()?;
        Ok(Self {
            path: path.to_path_buf(),
            plan: file.plan,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn plan(&self) -> &WritePlan {
        &self.plan
    }
}

fn digest(plan: &WritePlan) -> Result<String, String> {
    let bytes = serde_json::to_vec(plan).map_err(|error| error.to_string())?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
