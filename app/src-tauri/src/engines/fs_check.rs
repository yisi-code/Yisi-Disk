//! 文件系统检查：启动只读的检查任务；把界面生成的问题清单存成文本文件（另存为）。

use std::path::Path;

use tauri::AppHandle;
use yisi_engine::protocol::{FS_CHECK, FsCheckParams, PartitionRef};

use super::{Engines, HostError, Target, resolve_source, to_params};
use crate::broker::BrokerSession;

/// 问题清单最多这么大：1000 个问题、每个几行，远用不了这么多。
const MAX_REPORT_BYTES: usize = 16 << 20;

impl Engines {
    /// 检查分区（只读）。分区的位置由引擎对照分区表再核对一次。
    pub fn start_fs_check(
        &self,
        app: AppHandle,
        broker: &BrokerSession,
        job_id: String,
        target: Target,
        partition: PartitionRef,
    ) -> Result<(), HostError> {
        target.validate()?;
        let params_job_id = job_id.clone();
        self.start_job(app, job_id, FS_CHECK, |engine_pid| {
            to_params(&FsCheckParams {
                job_id: params_job_id,
                source: resolve_source(target, broker, engine_pid)?,
                partition,
            })
        })
        .map(|_| ())
    }
}

/// 把问题清单存到用户在「另存为」对话框里选的 `path`（`.txt`，已存在时覆盖：对话框已经问过）。
/// 检查的是物理磁盘时，清单不能存到这块盘上：往有问题的盘上写东西会让情况更糟。
pub fn save_report(target: &Target, path: &str, text: &str) -> Result<(), HostError> {
    let file = Path::new(path);
    let is_text = file
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("txt"));
    if !file.is_absolute() || !is_text || file.file_name().is_none() {
        return Err(HostError::new("invalidReportPath", path));
    }
    if text.len() > MAX_REPORT_BYTES {
        return Err(HostError::new("invalidReportPath", "too large"));
    }
    ensure_not_on_disk(target, file)?;
    std::fs::write(file, text)
        .map_err(|error| HostError::new("outputUnavailable", error.to_string()))
}

#[cfg(windows)]
fn ensure_not_on_disk(target: &Target, file: &Path) -> Result<(), HostError> {
    let Target::Disk { disk_number } = target else {
        return Ok(());
    };
    // 规则与引擎核对输出目录相同：查不到所在的磁盘也拒绝
    yisi_io::ensure_off_source(Ok(*disk_number), yisi_io::volume_disk_numbers(file))
        .map_err(|error| HostError::new("outputOnSource", error.to_string()))
}

/// 其他平台还打不开物理盘，检查的只会是镜像文件。
#[cfg(not(windows))]
fn ensure_not_on_disk(_target: &Target, _file: &Path) -> Result<(), HostError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_absolute_text_paths_are_written() {
        let image = Target::Image {
            path: "a.img".to_owned(),
        };
        for path in ["report.txt", "C:/reports/report.exe", "C:/reports/"] {
            let error = save_report(&image, path, "x").unwrap_err();
            assert_eq!(error.code, "invalidReportPath", "{path}");
        }
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-fs-check-report");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("report.TXT");
        save_report(&image, &file.display().to_string(), "问题清单").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "问题清单");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
