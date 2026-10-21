//! PDF：以 `%PDF-` 开头，以 `%%EOF` 结束；增量更新会在后面追加内容并再写一个 `%%EOF`。
//! 文件没有可靠的总长度，遇到下一个文件头或达到最大尺寸时，取最后一个 `%%EOF`（含其后的换行）为结尾。

use crate::plugin::{FileFormat, FormatInfo, Recovering, Signature};
use crate::walker::{Event, Scanner, Step, Structure, Walker, find_bytes};

const MAGIC: &[u8] = b"%PDF-";
const EOF_MARK: &[u8] = b"%%EOF";

const EOF_SCANNER: Scanner = Scanner {
    find: |window| find_bytes(window, EOF_MARK),
    overlap: EOF_MARK.len() - 1,
};

pub struct Pdf;

static INFO: FormatInfo = FormatInfo {
    extension: "pdf",
    description: "Portable Document Format",
    max_size: 512 << 20,
    enabled_by_default: true,
};

static SIGNATURES: [Signature; 1] = [Signature {
    offset: 0,
    bytes: MAGIC,
}];

impl FileFormat for Pdf {
    fn info(&self) -> &FormatInfo {
        &INFO
    }

    fn signatures(&self) -> &[Signature] {
        &SIGNATURES
    }

    fn header_check(&self, block: &[u8]) -> Option<Box<dyn Recovering>> {
        // 版本号形如 1.7、2.0
        let version = block.get(5..8)?;
        let plausible = block.starts_with(MAGIC)
            && matches!(version, [major, b'.', minor] if major.is_ascii_digit() && minor.is_ascii_digit());
        plausible.then(|| {
            Box::new(Walker::new(PdfStructure {
                last_end: None,
                pending_eol: false,
            })) as Box<dyn Recovering>
        })
    }
}

struct PdfStructure {
    /// 目前为止最后一个 `%%EOF` 之后的位置。
    last_end: Option<u64>,
    /// 正在等 `%%EOF` 之后的两个字节，看有没有换行。
    pending_eol: bool,
}

impl Structure for PdfStructure {
    fn first(&self) -> Step {
        Step::Scan {
            from: MAGIC.len() as u64,
            scanner: EOF_SCANNER,
        }
    }

    fn step(&mut self, event: Event<'_>) -> Step {
        match event {
            Event::Found { at } => {
                let end = at + EOF_MARK.len() as u64;
                self.last_end = Some(end);
                self.pending_eol = true;
                Step::Need { at: end, len: 2 }
            }
            Event::Bytes { at, bytes } if self.pending_eol => {
                self.pending_eol = false;
                let eol = match bytes {
                    [b'\r', b'\n'] => 2,
                    [b'\r' | b'\n', ..] => 1,
                    _ => 0,
                };
                self.last_end = Some(at + eol);
                Step::Scan {
                    from: at + eol,
                    scanner: EOF_SCANNER,
                }
            }
            Event::Bytes { .. } => Step::Invalid,
        }
    }

    fn open_ended(&self) -> bool {
        true
    }

    fn finish(&self, _received: u64, _waiting_at: Option<u64>) -> Option<u64> {
        self.last_end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::samples;
    use crate::walker::tests::check_all_splits;

    fn recover(data: &[u8]) -> Box<dyn Recovering> {
        Pdf.header_check(data).unwrap()
    }

    #[test]
    fn ends_after_the_last_eof_marker() {
        let sample = samples::pdf(3, true);
        let mut data = sample.clone();
        data.extend_from_slice(b"random data that belongs to the next file %%EO");
        check_all_splits(|| recover(&data), &data, Some(sample.len() as u64));
    }

    #[test]
    fn eof_right_at_the_end_of_data_still_counts() {
        let mut sample = samples::pdf(1, false);
        // 去掉结尾换行：数据在 %%EOF 处正好用完
        while sample.last() == Some(&b'\n') {
            sample.pop();
        }
        check_all_splits(|| recover(&sample), &sample, Some(sample.len() as u64));
    }

    #[test]
    fn no_eof_marker_is_incomplete() {
        let data = b"%PDF-1.7\n1 0 obj << >> endobj\n";
        check_all_splits(|| recover(data), data, None);
        assert!(Pdf.header_check(b"%PDF-x.y").is_none());
    }
}
