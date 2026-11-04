//! 程序文件夹下 `data` 里的一份记录清单（JSON 数组）：读不到、读不懂时按空处理（读不懂的那份在下次保存时被替换）；
//! 保存时先写临时文件再改名替换。做过的镜像与写盘记录都存成这样。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// 读出全部记录。`what` 只用于日志，说明读的是什么记录。
pub fn load<T: DeserializeOwned>(file: &Path, what: &str) -> Vec<T> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(%error, path = %file.display(), "读不到{what}");
            return Vec::new();
        }
    };
    serde_json::from_str(&text).unwrap_or_else(|error| {
        tracing::warn!(%error, path = %file.display(), "{what}读不懂，按空处理");
        Vec::new()
    })
}

/// 保存全部记录，失败时给出说明（含路径）。目录不存在时创建。
pub fn save<T: Serialize>(file: &Path, records: &[T]) -> Result<(), String> {
    let failed = |error: &dyn std::fmt::Display| format!("{}：{error}", file.display());
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|error| failed(&error))?;
    }
    let text = serde_json::to_string_pretty(records).map_err(|error| failed(&error))?;
    let mut temp = file.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    std::fs::write(&temp, text).map_err(|error| failed(&error))?;
    std::fs::rename(&temp, file).map_err(|error| failed(&error))
}

/// 现在的时间（毫秒，Unix 纪元起）。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| u64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}
