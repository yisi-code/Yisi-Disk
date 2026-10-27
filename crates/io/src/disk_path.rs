//! 物理磁盘的路径 `\\.\PhysicalDriveN` 与卷的路径 `\\.\C:`：列出磁盘、宿主请代理打开、代理核对路径都按这一份。

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

/// 盘符 `letter` 上的卷的设备路径 `\\.\C:`（大写）；不是 A 到 Z 的字母时为 `None`。
pub fn volume_path(letter: char) -> Option<String> {
    letter
        .is_ascii_alphabetic()
        .then(|| format!(r"\\.\{}:", letter.to_ascii_uppercase()))
}

/// 盘符 `letter` 上的卷的根目录 `C:\`，用来向系统查这个卷的属性。
pub fn volume_root(letter: char) -> String {
    format!(r"{}:\", letter.to_ascii_uppercase())
}

/// 路径是 `\\.\C:`（一个大写字母加冒号，后面没有别的）时给出盘符，否则为 `None`。
/// 代理据此只接受物理磁盘与卷，不替宿主打开任意文件。
pub fn volume_letter(path: &str) -> Option<char> {
    let rest = path.strip_prefix(r"\\.\")?;
    let mut chars = rest.chars();
    let letter = chars.next()?;
    let valid = letter.is_ascii_uppercase() && chars.next() == Some(':') && chars.next().is_none();
    valid.then_some(letter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_paths_and_letters_round_trip() {
        assert_eq!(volume_path('c').as_deref(), Some(r"\\.\C:"));
        assert_eq!(volume_path('1'), None);
        for letter in ['A', 'C', 'Z'] {
            assert_eq!(volume_letter(&volume_path(letter).unwrap()), Some(letter));
        }
        for bad in [
            r"\\.\c:",
            r"\\.\C",
            r"\\.\C:\",
            r"\\.\C:\Windows",
            r"\\.\CC:",
            r"\\?\C:",
            r"C:",
            r"\\.\PhysicalDrive0",
            "",
        ] {
            assert_eq!(volume_letter(bad), None, "{bad}");
        }
    }

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
