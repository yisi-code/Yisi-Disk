//! PE 映像（微软 PE / COFF 规范）：EXE、DLL、驱动。开头是 DOS 头（`MZ`，第 0x3C 字节记着 PE 头的位置），
//! PE 头是 `PE\0\0`、20 字节的 COFF 文件头（机器类型、节数、符号表位置与个数、可选头长度、特征）、
//! 可选头（PE32 或 PE32+：文件对齐、头的总长度、子系统、数据目录）、节表（每节 40 字节，记着节在文件里的位置与长度）。
//!
//! 文件没有总长度，取以下各处结尾的最大值：头、各节的原始数据、数据目录里的证书表（这一项记的是文件位置，
//! 带数字签名的文件签名在最后）、COFF 符号表与紧随其后的字符串表（字符串表开头 4 字节是它的长度）。
//! 附加在映像之后、哪里都没登记的数据（安装包常这样带着压缩包）找不回来。
//!
//! 按特征与子系统定扩展名：DLL 标志置位存成 dll，原生子系统（驱动）存成 sys，EFI 子系统存成 efi。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Step, Structure, Walker};
use yisi_part::bytes::{le16, le32};

const LFANEW_AT: usize = 0x3C;
/// PE 头位置的合理范围：在 DOS 头之后，又不会太远。
const MIN_LFANEW: u32 = 0x40;
const MAX_LFANEW: u32 = 0x1_0000;
const PE_SIGNATURE: &[u8] = b"PE\0\0";
/// `PE\0\0` 加 COFF 文件头。
const NT_HEADER_LEN: usize = 24;
const SECTION_LEN: usize = 40;
const MAX_SECTIONS: u16 = 96;
const MAX_OPTIONAL_HEADER: u16 = 0x1000;
const SYMBOL_LEN: u64 = 18;
const PE32_MAGIC: u16 = 0x10B;
const PE32_PLUS_MAGIC: u16 = 0x20B;
/// 数据目录的项数上限；证书表是第 4 项。
const MAX_DIRECTORIES: u32 = 16;
const CERTIFICATE_DIRECTORY: u32 = 4;
const FILE_EXECUTABLE_IMAGE: u16 = 0x0002;
const FILE_DLL: u16 = 0x2000;
const SUBSYSTEM_NATIVE: u16 = 1;
const SUBSYSTEM_EFI: std::ops::RangeInclusive<u16> = 10..=13;
/// 认得的机器类型：x86、x64、ARM64、ARM64EC、ARM、ARM Thumb-2、Thumb、IA-64、EFI 字节码。
const MACHINES: [u16; 9] = [
    0x014C, 0x8664, 0xAA64, 0xA641, 0x01C0, 0x01C4, 0x01C2, 0x0200, 0x0EBC,
];

pub struct Pe;

static INFO: FormatInfo = FormatInfo {
    extension: "exe",
    variants: &["dll", "sys", "efi"],
    description: "Windows program or library",
    max_size: 4 << 30,
    enabled_by_default: true,
    // 资源里常带着图标（PNG）、安装包带着压缩包
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"MZ",
}];

impl FileFormat for Pe {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let lfanew = le32(block, LFANEW_AT)?;
        if !(MIN_LFANEW..=MAX_LFANEW).contains(&lfanew) {
            return None;
        }
        // PE 头在这一块里时当场核对，省得每个 `MZ` 开头的块都往后读
        let at = lfanew as usize;
        if let Some(signature) = block.get(at..at + PE_SIGNATURE.len())
            && signature != PE_SIGNATURE
        {
            return None;
        }
        Some(Box::new(Walker::new(
            INFO.max_size,
            PeStructure {
                lfanew: u64::from(lfanew),
                phase: Phase::NtHeader,
                end: 0,
                extension: None,
            },
        )))
    }
}

#[derive(Clone, Copy)]
enum Phase {
    /// 等 `PE\0\0` 与 COFF 文件头。
    NtHeader,
    /// 等可选头与节表。
    Headers,
    /// 等字符串表的长度。
    Strings,
}

struct PeStructure {
    lfanew: u64,
    phase: Phase,
    /// 已知各处结尾的最大值。
    end: u64,
    extension: Option<&'static str>,
}

/// COFF 文件头：（节数、可选头长度），合规时才给出。
fn coff_header(header: &[u8]) -> Option<(u16, u16)> {
    let machine = le16(header, 4)?;
    let sections = le16(header, 6)?;
    let optional_len = le16(header, 20)?;
    let characteristics = le16(header, 22)?;
    let plausible = header.get(..4) == Some(PE_SIGNATURE)
        && MACHINES.contains(&machine)
        && (1..=MAX_SECTIONS).contains(&sections)
        && optional_len <= MAX_OPTIONAL_HEADER
        && characteristics & FILE_EXECUTABLE_IMAGE != 0;
    plausible.then_some((sections, optional_len))
}

impl PeStructure {
    /// 读到全部头：算出各处结尾的最大值，定扩展名；有 COFF 符号表时返回字符串表的位置。
    fn headers(&mut self, headers: &[u8]) -> Option<Option<u64>> {
        let (sections, optional_len) = coff_header(headers)?;
        let symbols_at = le32(headers, 12)?;
        let symbols = le32(headers, 16)?;
        let characteristics = le16(headers, 22)?;
        let optional = headers.get(NT_HEADER_LEN..NT_HEADER_LEN + usize::from(optional_len))?;
        let directories_at = match le16(optional, 0)? {
            PE32_MAGIC => 96,
            PE32_PLUS_MAGIC => 112,
            _ => return None,
        };
        let file_alignment = le32(optional, 36)?;
        let headers_len = le32(optional, 60)?;
        let subsystem = le16(optional, 68)?;
        let directories = le32(optional, directories_at - 4)?;
        if !file_alignment.is_power_of_two()
            || !(16..=0x1_0000).contains(&file_alignment)
            || directories > MAX_DIRECTORIES
            || optional.len() < directories_at + 8 * directories as usize
        {
            return None;
        }
        let table_end = self.lfanew
            + (NT_HEADER_LEN + usize::from(optional_len) + SECTION_LEN * usize::from(sections))
                as u64;
        let mut end = table_end.max(u64::from(headers_len));
        let table = headers.get(NT_HEADER_LEN + usize::from(optional_len)..)?;
        for section in table.as_chunks::<SECTION_LEN>().0 {
            let raw_len = le32(section, 16)?;
            let raw_at = le32(section, 20)?;
            if raw_len > 0 {
                if raw_at == 0 {
                    return None;
                }
                end = end.max(u64::from(raw_at) + u64::from(raw_len));
            }
        }
        if directories > CERTIFICATE_DIRECTORY {
            let entry = directories_at + 8 * CERTIFICATE_DIRECTORY as usize;
            let (at, len) = (le32(optional, entry)?, le32(optional, entry + 4)?);
            if len > 0 {
                if u64::from(at) < table_end {
                    return None;
                }
                end = end.max(u64::from(at) + u64::from(len));
            }
        }
        self.end = end;
        self.extension = if characteristics & FILE_DLL != 0 {
            Some("dll")
        } else if subsystem == SUBSYSTEM_NATIVE {
            Some("sys")
        } else if SUBSYSTEM_EFI.contains(&subsystem) {
            Some("efi")
        } else {
            None
        };
        let strings = (symbols_at != 0 && symbols != 0)
            .then(|| u64::from(symbols_at) + SYMBOL_LEN * u64::from(symbols));
        Some(strings)
    }
}

impl Structure for PeStructure {
    fn first(&self) -> Step {
        Step::Need {
            at: self.lfanew,
            len: NT_HEADER_LEN,
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let Event::Bytes { at, bytes } = event else {
            return Step::Invalid;
        };
        match self.phase {
            Phase::NtHeader => match coff_header(bytes) {
                Some((sections, optional_len)) => {
                    self.phase = Phase::Headers;
                    Step::Need {
                        at,
                        len: NT_HEADER_LEN
                            + usize::from(optional_len)
                            + SECTION_LEN * usize::from(sections),
                    }
                }
                None => Step::Invalid,
            },
            Phase::Headers => match self.headers(bytes) {
                Some(Some(strings_at)) if strings_at >= at => {
                    self.phase = Phase::Strings;
                    Step::Need {
                        at: strings_at,
                        len: 4,
                    }
                }
                Some(None) => Step::Done(self.end),
                _ => Step::Invalid,
            },
            Phase::Strings => match le32(bytes, 0) {
                Some(len) if len >= 4 => Step::Done(self.end.max(at + u64::from(len))),
                _ => Step::Invalid,
            },
        }
    }

    fn extension(&self) -> Option<&'static str> {
        self.extension
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples::{self, PeLayout};
    use crate::walker::tests::{check_all_splits, run_keeping};

    fn check(data: &[u8], expected: Option<u64>) {
        check_all_splits(|| Pe.header_check(data).unwrap(), data, expected);
    }

    #[test]
    fn length_is_the_end_of_the_last_section_signature_or_symbols() {
        let layouts = [
            (PeLayout::default(), None),
            (
                PeLayout {
                    plus: true,
                    dll: true,
                    sections: vec![300, 5000, 0, 77],
                    ..PeLayout::default()
                },
                Some("dll"),
            ),
            (
                PeLayout {
                    certificate: 900,
                    ..PeLayout::default()
                },
                None,
            ),
            (
                PeLayout {
                    symbols: Some((20, 133)),
                    plus: true,
                    ..PeLayout::default()
                },
                None,
            ),
            (
                PeLayout {
                    subsystem: 1,
                    ..PeLayout::default()
                },
                Some("sys"),
            ),
            (
                PeLayout {
                    subsystem: 10,
                    ..PeLayout::default()
                },
                Some("efi"),
            ),
        ];
        for (layout, extension) in layouts {
            let sample = samples::pe(&layout);
            let mut data = sample.clone();
            data.extend_from_slice(b"MZ followed by something else");
            check(&data, Some(sample.len() as u64));
            check(&sample[..sample.len() - 1], None);
            let mut recovering = Pe.header_check(&data).unwrap();
            run_keeping(&mut recovering, &data, 512);
            assert_eq!(recovering.extension(), extension);
        }
    }

    #[test]
    fn implausible_headers_are_rejected() {
        let sample = samples::pe(&PeLayout::default());
        let lfanew = le32(&sample, LFANEW_AT).unwrap() as usize;
        let changed = |at: usize, value: &[u8]| {
            let mut data = sample.clone();
            data[at..at + value.len()].copy_from_slice(value);
            data
        };
        assert!(
            Pe.header_check(&changed(lfanew, b"NE")).is_none(),
            "不是 PE 头"
        );
        assert!(
            Pe.header_check(&changed(LFANEW_AT, &[8, 0, 0, 0]))
                .is_none()
        );
        check(&changed(lfanew + 4, &[0x34, 0x12]), None); // 机器类型
        check(&changed(lfanew + 6, &[0, 0]), None); // 没有节
        check(&changed(lfanew + 22, &[0, 0]), None); // 不是可执行映像
        check(&changed(lfanew + 24, &[0x0B, 0x03]), None); // 可选头魔数
        check(&changed(lfanew + 24 + 36, &[0, 3, 0, 0]), None); // 文件对齐不是 2 的幂
    }
}
