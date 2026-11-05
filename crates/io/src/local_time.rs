//! 本机时区：把不带时区的本地时间（FAT 记的修改时间）换算成 UTC。

/// 1601-01-01（NTFS 与 Windows 的时间起点）到 1970-01-01 的秒数。
pub const NT_UNIX_EPOCH_SECONDS: i64 = 11_644_473_600;

/// `wall` 是本地的年月日时分秒当作 UTC 算出的、1970-01-01 起的秒数；返回同一时刻真正的 UTC 秒数。
/// Windows 按本机时区与那一天的夏令时规则换算；其他系统暂按 UTC 处理。换算不了时为 `None`。
pub fn local_to_utc(wall: i64) -> Option<i64> {
    imp::local_to_utc(wall)
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{
        FileTimeToSystemTime, SystemTimeToFileTime, TzSpecificLocalTimeToSystemTime,
    };

    use super::NT_UNIX_EPOCH_SECONDS as NT_UNIX_SECONDS;

    pub(super) fn local_to_utc(wall: i64) -> Option<i64> {
        let ticks =
            u64::try_from(wall.checked_add(NT_UNIX_SECONDS)?.checked_mul(10_000_000)?).ok()?;
        let file_time = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        // SAFETY: 三个结构都在栈上、按值初始化，指针只在调用期间使用；返回 0 表示失败，不读输出
        unsafe {
            let mut local: SYSTEMTIME = std::mem::zeroed();
            if FileTimeToSystemTime(&file_time, &mut local) == 0 {
                return None;
            }
            let mut utc: SYSTEMTIME = std::mem::zeroed();
            if TzSpecificLocalTimeToSystemTime(std::ptr::null(), &local, &mut utc) == 0 {
                return None;
            }
            let mut out = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            if SystemTimeToFileTime(&utc, &mut out) == 0 {
                return None;
            }
            let ticks = (u64::from(out.dwHighDateTime) << 32) | u64::from(out.dwLowDateTime);
            i64::try_from(ticks / 10_000_000)
                .ok()?
                .checked_sub(NT_UNIX_SECONDS)
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub(super) fn local_to_utc(wall: i64) -> Option<i64> {
        Some(wall)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_times_shift_by_whole_quarters_of_an_hour() {
        // 2024-03-05 13:45:30 本地时间：换算后与原值相差的是本机时区的偏移，整 15 分钟、不超过 14 小时
        let wall = 19_787 * 86_400 + 13 * 3600 + 45 * 60 + 30;
        let utc = local_to_utc(wall).unwrap();
        let shift = wall - utc;
        assert_eq!(shift % (15 * 60), 0, "{shift}");
        assert!(shift.abs() <= 14 * 3600, "{shift}");
        assert_eq!(local_to_utc(i64::MIN), None);
    }
}
