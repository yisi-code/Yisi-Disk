//! 进程启动时启用自己令牌里已有的全部特权。
//!
//! Windows 给进程的特权大多默认是关着的，用到时才打开。这里在启动时一次全部打开：只打开令牌里已经有的，
//! 不会多出令牌之外的权限，也不需要用户授权。要管理员才有的特权（磁盘代理）仍然经 UAC 由用户授权后才有。

/// 启用当前进程令牌里的全部特权，返回这些特权的名字（如 `SeShutdownPrivilege`）。
/// 有的特权系统不让打开时，打开其余的照常返回成功。
#[cfg(windows)]
pub fn enable_held_privileges() -> std::io::Result<Vec<String>> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{TOKEN_ADJUST_PRIVILEGES, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess 返回本进程的伪句柄，不需要关闭；token 由系统写入。
    let opened = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    };
    if opened == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let result = enable_all(token);
    // SAFETY: token 是上面打开的有效句柄，之后不再使用。
    unsafe { CloseHandle(token) };
    result
}

#[cfg(windows)]
fn enable_all(token: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<Vec<String>> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
        SE_PRIVILEGE_REMOVED, TOKEN_PRIVILEGES, TokenPrivileges,
    };

    let mut needed = 0u32;
    // SAFETY: 只问需要多大的缓冲区；缓冲区为空、长度为 0。
    unsafe { GetTokenInformation(token, TokenPrivileges, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // 按 8 字节对齐分配，TOKEN_PRIVILEGES 里的 LUID 要求 4 字节对齐
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: buffer 至少有 needed 个字节，按 8 字节对齐。
    let filled = unsafe {
        GetTokenInformation(
            token,
            TokenPrivileges,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if filled == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let header = buffer.as_mut_ptr().cast::<TOKEN_PRIVILEGES>();
    // SAFETY: 系统按 TOKEN_PRIVILEGES 的布局填好了 buffer，PrivilegeCount 项紧跟在计数之后，都在 needed 字节之内。
    let privileges: &mut [LUID_AND_ATTRIBUTES] = unsafe {
        let count = (*header).PrivilegeCount as usize;
        std::slice::from_raw_parts_mut(std::ptr::addr_of_mut!((*header).Privileges).cast(), count)
    };
    let mut names = Vec::with_capacity(privileges.len());
    for privilege in privileges.iter_mut() {
        if privilege.Attributes & SE_PRIVILEGE_REMOVED == 0 {
            privilege.Attributes |= SE_PRIVILEGE_ENABLED;
        }
        names.push(privilege_name(&privilege.Luid));
    }
    // SAFETY: header 指向上面填好、改过属性的 TOKEN_PRIVILEGES；不要求返回原来的状态。
    let adjusted = unsafe {
        AdjustTokenPrivileges(
            token,
            0,
            header,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    // 成功时也可能只打开了一部分（错误码 ERROR_NOT_ALL_ASSIGNED），其余照常可用，按成功处理
    if adjusted == 0 {
        // SAFETY: 紧接着取上一次调用的错误码。
        let last = unsafe { GetLastError() };
        return Err(std::io::Error::from_raw_os_error(last as i32));
    }
    Ok(names)
}

/// 特权的名字；查不到时给出 LUID 数值。
#[cfg(windows)]
fn privilege_name(luid: &windows_sys::Win32::Foundation::LUID) -> String {
    use windows_sys::Win32::Security::LookupPrivilegeNameW;

    let mut name = [0u16; 64];
    let mut length = name.len() as u32;
    // SAFETY: name 有 length 个 u16 的空间，系统写入名字并把 length 改成实际长度（不含结尾的 0）。
    let found =
        unsafe { LookupPrivilegeNameW(std::ptr::null(), luid, name.as_mut_ptr(), &mut length) };
    if found == 0 {
        return format!("LUID {}:{}", luid.HighPart, luid.LowPart);
    }
    String::from_utf16_lossy(&name[..length as usize])
}

#[cfg(not(windows))]
pub fn enable_held_privileges() -> std::io::Result<Vec<String>> {
    Ok(Vec::new())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn every_process_holds_change_notify() {
        // 每个进程的令牌里都有 SeChangeNotifyPrivilege（绕过遍历检查），普通用户也有
        let names = enable_held_privileges().unwrap();
        assert!(
            names.iter().any(|name| name == "SeChangeNotifyPrivilege"),
            "{names:?}"
        );
    }
}
