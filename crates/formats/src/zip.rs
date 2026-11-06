//! ZIP：依次是各文件的本地文件头与数据、中央目录，最后是中央目录结束记录（带注释）。
//! 本地文件头里没有写大小（用数据描述符）时，往后找下一条记录的签名。
//!
//! 很多格式就是一个 ZIP，按里面装的东西改用它们的扩展名：Office Open XML（ECMA-376 第 2 部分：
//! 有 `[Content_Types].xml`，正文在 `word/`、`xl/`、`ppt/` 下）看中央目录里的文件名；
//! OpenDocument 与 EPUB 的第一个文件是不压缩的 `mimetype`，看它的内容；
//! Android 应用有 `AndroidManifest.xml` 与 `classes.dex`，Java 包有 `META-INF/MANIFEST.MF`。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Scanner, Step, Structure, Walker};
use yisi_part::bytes::{le16, le32, le64};

const LOCAL_FILE: u32 = 0x0403_4B50;
const CENTRAL_DIRECTORY: u32 = 0x0201_4B50;
const END_OF_CENTRAL_DIRECTORY: u32 = 0x0605_4B50;
const ZIP64_END: u32 = 0x0606_4B50;
const ZIP64_LOCATOR: u32 = 0x0706_4B50;
const DATA_DESCRIPTOR: u32 = 0x0807_4B50;

/// 本地文件头标志位 3：大小写在数据后面的数据描述符里。
const FLAG_DATA_DESCRIPTOR: u16 = 1 << 3;
/// `mimetype` 的内容最多读这么长（OpenDocument 里最长的也不到 60 字节）。
const MAX_MIMETYPE: u32 = 128;
/// `mimetype` 的内容 → 扩展名。
const MIMETYPES: [(&[u8], &str); 4] = [
    (b"application/vnd.oasis.opendocument.text", "odt"),
    (b"application/vnd.oasis.opendocument.spreadsheet", "ods"),
    (b"application/vnd.oasis.opendocument.presentation", "odp"),
    (b"application/epub+zip", "epub"),
];
/// ZIP64 扩展字段的标识。
const ZIP64_EXTRA: u16 = 0x0001;

/// 大小未知时，数据之后的第一条记录：数据描述符、下一个本地文件头或中央目录。
const NEXT_RECORD: Scanner = Scanner {
    find: |window| {
        window.windows(4).position(|candidate| {
            matches!(
                candidate,
                [b'P', b'K', 7, 8] | [b'P', b'K', 3, 4] | [b'P', b'K', 1, 2]
            )
        })
    },
    overlap: 3,
};

pub struct Zip;

static INFO: FormatInfo = FormatInfo {
    extension: "zip",
    variants: &[
        "docx", "xlsx", "pptx", "odt", "ods", "odp", "epub", "apk", "jar",
    ],
    description: "ZIP archive",
    max_size: 16 << 30,
    enabled_by_default: true,
    nests_files: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: b"PK\x03\x04",
}];

impl FileFormat for Zip {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        let version_needed = le16(block, 4)?;
        let name_length = le16(block, 26)?;
        let plausible =
            le32(block, 0) == Some(LOCAL_FILE) && version_needed <= 63 && name_length > 0;
        plausible.then(|| {
            Box::new(Walker::new(
                INFO.max_size,
                ZipStructure {
                    phase: Phase::Record,
                    mimetype: None,
                    names: Names::default(),
                    local_open: false,
                    confirmed: 0,
                },
            )) as Box<dyn Recovering>
        })
    }
}

/// 本地文件头里读名字之后还要用的几项。
#[derive(Clone, Copy)]
struct Local {
    flags: u16,
    /// 存储方式（不压缩）。
    stored: bool,
    compressed: u32,
    extra: u16,
    extra_start: u64,
}

#[derive(Clone, Copy)]
enum Phase {
    /// 等 4 字节记录签名。
    Record,
    LocalFile,
    /// 等本地文件头后面的文件名。
    LocalName(Local),
    /// 等本地文件头的扩展字段，从中读 ZIP64 大小。
    Zip64Extra {
        data_start: u64,
        flags: u16,
    },
    /// 等 `mimetype` 的内容。
    Mimetype,
    CentralDirectory,
    /// 等中央目录记录里的文件名；`next` 是下一条记录的位置。
    CentralName {
        next: u64,
    },
    EndOfCentralDirectory,
    Zip64End,
}

/// 中央目录里见到过的、能说明这是什么文件的名字。
#[derive(Default)]
struct Names {
    content_types: bool,
    word: bool,
    excel: bool,
    powerpoint: bool,
    android_manifest: bool,
    dex: bool,
    java_manifest: bool,
}

impl Names {
    fn note(&mut self, name: &[u8]) {
        match name {
            b"[Content_Types].xml" => self.content_types = true,
            b"AndroidManifest.xml" => self.android_manifest = true,
            b"classes.dex" => self.dex = true,
            b"META-INF/MANIFEST.MF" => self.java_manifest = true,
            _ if name.starts_with(b"word/") => self.word = true,
            _ if name.starts_with(b"xl/") => self.excel = true,
            _ if name.starts_with(b"ppt/") => self.powerpoint = true,
            _ => {}
        }
    }

    fn extension(&self) -> Option<&'static str> {
        if self.content_types {
            if self.word {
                return Some("docx");
            }
            if self.excel {
                return Some("xlsx");
            }
            if self.powerpoint {
                return Some("pptx");
            }
        }
        if self.android_manifest && self.dex {
            return Some("apk");
        }
        self.java_manifest.then_some("jar")
    }
}

struct ZipStructure {
    phase: Phase,
    /// 按 `mimetype` 的内容定的扩展名。
    mimetype: Option<&'static str>,
    names: Names,
    /// 读过本地文件头、还没看到它的数据之后接着的记录。
    local_open: bool,
    /// 数据之后接上了认得的记录的本地文件数。
    confirmed: u32,
}

impl ZipStructure {
    /// 读完本地文件头的名字：`mimetype` 是第一个文件、不压缩、内容不长时读它的内容，其余照常往后走。
    fn after_name(&mut self, local: Local, is_mimetype: bool) -> Step {
        let data_start = local.extra_start + u64::from(local.extra);
        if local.compressed == u32::MAX && local.extra > 0 {
            self.phase = Phase::Zip64Extra {
                data_start,
                flags: local.flags,
            };
            return Step::Need {
                at: local.extra_start,
                len: usize::from(local.extra),
            };
        }
        if local.flags & FLAG_DATA_DESCRIPTOR != 0 && local.compressed == 0 {
            return Step::Scan {
                from: data_start,
                scanner: NEXT_RECORD,
            };
        }
        if is_mimetype && local.stored && (1..=MAX_MIMETYPE).contains(&local.compressed) {
            self.phase = Phase::Mimetype;
            return Step::Need {
                at: data_start,
                len: local.compressed as usize,
            };
        }
        self.after_data(data_start + u64::from(local.compressed))
    }

    fn record_at(&mut self, at: u64) -> Step {
        self.phase = Phase::Record;
        Step::Need { at, len: 4 }
    }

    /// 文件数据之后：有数据描述符就跳过它，接着读下一条记录。
    fn after_data(&mut self, data_end: u64) -> Step {
        self.record_at(data_end)
    }
}

impl Structure for ZipStructure {
    fn first(&self) -> Step {
        Step::Need { at: 0, len: 4 }
    }

    fn extension(&self) -> Option<&'static str> {
        self.mimetype.or_else(|| self.names.extension())
    }

    /// 部分恢复：至少一个本地文件的数据之后接上了认得的记录才保存，到断点为止。
    /// 没有中央目录，解压软件要按本地文件头一个个读（多数有「修复」或「按本地头解压」的办法）。
    fn intact(&self, cut: u64) -> Option<u64> {
        (self.confirmed > 0).then_some(cut)
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        let (at, bytes) = match event {
            Event::Found { at } => return self.record_at(at),
            Event::Bytes { at, bytes } => (at, bytes),
        };
        match self.phase {
            Phase::Record => {
                let Some(signature) = le32(bytes, 0) else {
                    return Step::Invalid;
                };
                let known = matches!(
                    signature,
                    LOCAL_FILE
                        | CENTRAL_DIRECTORY
                        | END_OF_CENTRAL_DIRECTORY
                        | ZIP64_END
                        | ZIP64_LOCATOR
                        | DATA_DESCRIPTOR
                );
                if known && std::mem::take(&mut self.local_open) {
                    self.confirmed += 1;
                }
                let (phase, len) = match signature {
                    LOCAL_FILE => (Phase::LocalFile, 30),
                    CENTRAL_DIRECTORY => (Phase::CentralDirectory, 46),
                    END_OF_CENTRAL_DIRECTORY => (Phase::EndOfCentralDirectory, 22),
                    ZIP64_END => (Phase::Zip64End, 12),
                    // 定长 20 字节，直接跳过
                    ZIP64_LOCATOR => return self.record_at(at + 20),
                    // 带签名的数据描述符：签名 + CRC + 两个 4 字节大小
                    DATA_DESCRIPTOR => return self.record_at(at + 16),
                    _ => return Step::Invalid,
                };
                self.phase = phase;
                Step::Need { at, len }
            }
            Phase::LocalFile => {
                let (Some(flags), Some(method), Some(compressed), Some(name), Some(extra)) = (
                    le16(bytes, 6),
                    le16(bytes, 8),
                    le32(bytes, 18),
                    le16(bytes, 26),
                    le16(bytes, 28),
                ) else {
                    return Step::Invalid;
                };
                let local = Local {
                    flags,
                    stored: method == 0,
                    compressed,
                    extra,
                    extra_start: at + 30 + u64::from(name),
                };
                self.local_open = true;
                if name == 0 {
                    return self.after_name(local, false);
                }
                self.phase = Phase::LocalName(local);
                Step::Need {
                    at: at + 30,
                    len: usize::from(name),
                }
            }
            // 第一个文件的本地文件头在 0，名字从 30 开始
            Phase::LocalName(local) => self.after_name(local, at == 30 && bytes == b"mimetype"),
            Phase::Mimetype => {
                self.mimetype = MIMETYPES
                    .iter()
                    .find(|(content, _)| bytes.trim_ascii() == *content)
                    .map(|&(_, extension)| extension);
                self.after_data(at + bytes.len() as u64)
            }
            Phase::Zip64Extra { data_start, flags } => {
                let Some(compressed) = zip64_compressed_size(bytes) else {
                    return Step::Invalid;
                };
                if flags & FLAG_DATA_DESCRIPTOR != 0 && compressed == 0 {
                    return Step::Scan {
                        from: data_start,
                        scanner: NEXT_RECORD,
                    };
                }
                match data_start.checked_add(compressed) {
                    Some(data_end) => self.after_data(data_end),
                    None => Step::Invalid,
                }
            }
            Phase::CentralDirectory => {
                let (Some(name), Some(extra), Some(comment)) =
                    (le16(bytes, 28), le16(bytes, 30), le16(bytes, 32))
                else {
                    return Step::Invalid;
                };
                let next = at + 46 + u64::from(name) + u64::from(extra) + u64::from(comment);
                if name == 0 {
                    return self.record_at(next);
                }
                self.phase = Phase::CentralName { next };
                Step::Need {
                    at: at + 46,
                    len: usize::from(name),
                }
            }
            Phase::CentralName { next } => {
                self.names.note(bytes);
                self.record_at(next)
            }
            Phase::EndOfCentralDirectory => match le16(bytes, 20) {
                Some(comment) => Step::Done(at + 22 + u64::from(comment)),
                None => Step::Invalid,
            },
            Phase::Zip64End => {
                // 记录长度不含签名与长度字段本身
                match le64(bytes, 4).and_then(|size| size.checked_add(at + 12)) {
                    Some(next) => self.record_at(next),
                    None => Step::Invalid,
                }
            }
        }
    }
}

/// 在扩展字段里找 ZIP64 字段（标识 0x0001），取其中的压缩后大小（原始大小之后的 8 字节）。
fn zip64_compressed_size(extra: &[u8]) -> Option<u64> {
    let mut at = 0usize;
    while let (Some(id), Some(size)) = (le16(extra, at), le16(extra, at + 2)) {
        if id == ZIP64_EXTRA {
            return le64(extra, at + 4 + 8);
        }
        at = at.checked_add(4 + usize::from(size))?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn recover(data: &[u8]) -> Box<dyn Recovering> {
        Zip.header_check(data).unwrap()
    }

    #[test]
    fn follows_local_headers_central_directory_and_comment() {
        let sample = samples::zip(&[("a.txt", 100, false), ("b.bin", 3000, false)], b"comment");
        let mut data = sample.clone();
        data.extend_from_slice(b"PK\x03\x04 next archive would start here");
        check_all_splits(|| recover(&data), &data, Some(sample.len() as u64));
    }

    #[test]
    fn data_descriptors_are_found_by_scanning() {
        let sample = samples::zip(&[("stream.dat", 2000, true), ("x", 10, false)], b"");
        check_all_splits(|| recover(&sample), &sample, Some(sample.len() as u64));
    }

    /// 完整读完后按内容给出的扩展名。
    fn extension_of(data: &[u8]) -> Option<&'static str> {
        let mut recovering = recover(data);
        let length = crate::walker::tests::run_keeping(&mut recovering, data, 97);
        assert_eq!(length, Some(data.len() as u64));
        recovering.extension()
    }

    #[test]
    fn office_documents_are_named_by_their_parts() {
        let docx = samples::zip(
            &[
                ("[Content_Types].xml", 30, false),
                ("_rels/.rels", 20, false),
                ("word/document.xml", 90, true),
            ],
            b"",
        );
        assert_eq!(extension_of(&docx), Some("docx"));
        let xlsx = samples::zip(
            &[
                ("[Content_Types].xml", 30, false),
                ("xl/workbook.xml", 40, false),
            ],
            b"",
        );
        assert_eq!(extension_of(&xlsx), Some("xlsx"));
        let pptx = samples::zip(
            &[
                ("[Content_Types].xml", 30, false),
                ("ppt/slides/slide1.xml", 40, false),
            ],
            b"",
        );
        assert_eq!(extension_of(&pptx), Some("pptx"));
        // 只有 word/ 而没有 [Content_Types].xml：普通压缩包
        let plain = samples::zip(&[("word/notes.txt", 30, false)], b"");
        assert_eq!(extension_of(&plain), None);
        let apk = samples::zip(
            &[
                ("AndroidManifest.xml", 30, false),
                ("classes.dex", 30, false),
            ],
            b"",
        );
        assert_eq!(extension_of(&apk), Some("apk"));
        let jar = samples::zip(
            &[
                ("META-INF/MANIFEST.MF", 30, false),
                ("a/B.class", 30, false),
            ],
            b"",
        );
        assert_eq!(extension_of(&jar), Some("jar"));
    }

    #[test]
    fn open_document_and_epub_are_named_by_their_mimetype() {
        let make = |mimetype: &[u8]| {
            samples::zip_with(
                &[
                    ("mimetype", mimetype.to_vec(), false),
                    ("content.xml", b"<office/>".to_vec(), false),
                ],
                b"",
            )
        };
        for (mimetype, expected) in MIMETYPES {
            let data = make(mimetype);
            assert_eq!(extension_of(&data), Some(expected));
            check_all_splits(|| recover(&data), &data, Some(data.len() as u64));
        }
        assert_eq!(extension_of(&make(b"application/x-other")), None);
        // mimetype 不是第一个文件时不算
        let later = samples::zip_with(
            &[
                ("content.xml", b"<office/>".to_vec(), false),
                ("mimetype", b"application/epub+zip".to_vec(), false),
            ],
            b"",
        );
        assert_eq!(extension_of(&later), None);
    }

    #[test]
    fn missing_end_record_is_incomplete() {
        let sample = samples::zip(&[("a.txt", 50, false)], b"");
        let truncated = &sample[..sample.len() - 10];
        check_all_splits(|| recover(truncated), truncated, None);
    }
}
