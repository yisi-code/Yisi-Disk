//! 交给系统外壳的操作：选择打开方式、把文件放到剪贴板、在资源管理器里打开文件夹。路径由调用方核对过：
//! 文件只会是任务输出目录里的，文件夹只会是设置页列出的程序文件夹。

use std::path::Path;
use std::process::Command;

use crate::engines::HostError;

/// Windows 的 `CREATE_NO_WINDOW`：PowerShell 是控制台程序，不弹出控制台窗口。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 弹出系统的「打开方式」对话框，让用户挑一个程序打开 `path`。对话框由系统进程显示，这里不等它关闭。
#[cfg(windows)]
pub fn open_with(path: &Path) -> Result<(), HostError> {
    use std::os::windows::process::CommandExt;
    // OpenAs_RunDLL 把逗号之后的整段当作路径，带引号反而认不出，所以原样拼接、不加引号
    Command::new("rundll32.exe")
        .raw_arg(format!("shell32.dll,OpenAs_RunDLL {}", path.display()))
        .spawn()
        .map(|_| ())
        .map_err(|error| HostError::new("openFailed", error.to_string()))
}

/// 在资源管理器里打开文件夹 `path`，不等窗口关闭。直接交给 `explorer.exe`，不经 `explore` 动词：
/// 有的机器上外壳按 `explore` 打开文件夹会报「无法访问指定设备、路径或文件」（opener 插件走的就是这条）。
#[cfg(windows)]
pub fn open_folder(path: &Path) -> Result<(), HostError> {
    Command::new("explorer.exe")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| HostError::new("openFailed", error.to_string()))
}

/// 把 `path` 作为文件放到剪贴板，之后可以在资源管理器里粘贴。等 PowerShell 做完再返回。
#[cfg(windows)]
pub fn copy_to_clipboard(path: &Path) -> Result<(), HostError> {
    use std::os::windows::process::CommandExt;
    // 路径经环境变量传入，不拼进命令文本，不必处理引号与转义
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Set-Clipboard -LiteralPath $env:YISI_CLIPBOARD_PATH",
        ])
        .env("YISI_CLIPBOARD_PATH", path)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| HostError::new("copyFailed", error.to_string()))?;
    if output.status.success() && output.stderr.is_empty() {
        Ok(())
    } else {
        Err(HostError::new(
            "copyFailed",
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

#[cfg(not(windows))]
pub fn open_with(_path: &Path) -> Result<(), HostError> {
    Err(HostError::new("unsupportedPlatform", "open with"))
}

#[cfg(not(windows))]
pub fn open_folder(_path: &Path) -> Result<(), HostError> {
    Err(HostError::new("unsupportedPlatform", "open folder"))
}

#[cfg(not(windows))]
pub fn copy_to_clipboard(_path: &Path) -> Result<(), HostError> {
    Err(HostError::new("unsupportedPlatform", "clipboard"))
}
