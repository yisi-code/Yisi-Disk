//! 物理磁盘的路径 `\\.\PhysicalDriveN`：列出磁盘、宿主请代理打开磁盘、代理核对路径都按这一份。

/// 磁盘编号最多三位。
pub const MAX_DISK_NUMBER: u32 = 999;

/// 第 `number` 块物理磁盘的路径；编号超出 [`MAX_DISK_NUMBER`] 时为 `None`。
pub fn physical_drive_path(number: u32) -> Option<String> {
    (number <= MAX_DISK_NUMBER).then(|| format!(r"\\.\PhysicalDrive{number}"))
}

/// 路径是 `\\.\PhysicalDriveN`（N 为一到三位数字）时给出 N，否则为 `None`。
/// 管理员权限下任何文件都能打开，代理据此只接受物理磁盘，不替宿主打开任意文件。
pub fn physical_drive_number(path: &str) -> Option<u32> {
    let digits = path.strip_prefix(r"\\.\PhysicalDrive")?;
    let valid = (1..=3).contains(&digits.len()) && digits.bytes().all(|byte| byte.is_ascii_digit());
    valid.then(|| digits.parse().ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_numbers_round_trip() {
        assert_eq!(
            physical_drive_path(7).as_deref(),
            Some(r"\\.\PhysicalDrive7")
        );
        assert_eq!(physical_drive_path(MAX_DISK_NUMBER + 1), None);
        for number in [0, 12, 127, MAX_DISK_NUMBER] {
            let path = physical_drive_path(number).unwrap_or_default();
            assert_eq!(physical_drive_number(&path), Some(number));
        }
        for bad in [
            r"\\.\PhysicalDrive",
            r"\\.\PhysicalDrive1234",
            r"\\.\PhysicalDrive1\x",
            r"\\.\PhysicalDrive-1",
            r"\\.\C:",
            r"C:\Windows\System32\config\SAM",
            r"\\?\PhysicalDrive0",
            r"\\.\physicaldrive1",
            "",
        ] {
            assert_eq!(physical_drive_number(bad), None, "{bad}");
        }
    }
}
