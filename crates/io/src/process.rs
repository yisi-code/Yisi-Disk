//! 等某个进程退出。目前只有程序内更新在用：新版本启动后先等旧进程退出，再建窗口、删旧文件。

use std::time::Duration;

/// 等进程 `pid` 退出，最多等 `timeout`。进程已经不在、或打不开时立即返回。
#[cfg(windows)]
pub fn wait_for_process_exit(pid: u32, timeout: Duration) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    // SAFETY: 只请求等待用的 SYNCHRONIZE 权限；失败时返回空句柄，不使用。
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return;
    }
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    // SAFETY: process 是上面打开的有效句柄。
    unsafe { WaitForSingleObject(process, millis) };
    // SAFETY: process 是上面打开的有效句柄，之后不再使用。
    unsafe { CloseHandle(process) };
}

#[cfg(not(windows))]
pub fn wait_for_process_exit(_pid: u32, _timeout: Duration) {}
