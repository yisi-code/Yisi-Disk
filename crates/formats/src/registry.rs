//! 格式注册表：把全部签名按「偏移 + 前两个字节」建成查找表，每块只查表，不逐个格式去试。

use std::collections::HashMap;

use crate::font::{Sfnt, Woff};
use crate::iff::{AIFF, DJVU};
use crate::metafile::{Emf, Wmf};
use crate::mp4::{Cr3, Heic, Mp4};
use crate::plugin::{FileFormat, Recovering};
use crate::riff::{AVI, WAV, WEBP};
use crate::{
    asf::Asf, bmp::Bmp, cab::Cab, chm::Chm, evtx::Evtx, flv::Flv, gif::Gif, ico::Ico, jpg::Jpg,
    midi::Midi, mkv::Mkv, mp3::Mp3, ogg::Ogg, ole::Ole, pdf::Pdf, pe::Pe, png::Png, psd::Psd,
    pst::Pst, raf::Raf, rar::Rar, regf::Regf, sevenzip::SevenZip, sqlite::Sqlite, swf::Swf,
    tar::Tar, tiff::Tiff, zip::Zip,
};

/// 一块的开头命中了某个格式的文件头。
pub struct HeaderMatch {
    /// 格式在注册表里的序号。
    pub format: usize,
    pub recovering: Box<dyn Recovering>,
}

pub struct Registry {
    formats: Vec<Box<dyn FileFormat>>,
    /// (签名偏移, 签名前两个字节) → 格式序号。
    index: HashMap<(u16, [u8; 2]), Vec<usize>>,
    /// 查找表里出现过的签名偏移，从小到大。
    offsets: Vec<u16>,
    /// 签名不足两个字节的格式，逐个检查。
    short: Vec<usize>,
}

impl Registry {
    pub fn new(formats: Vec<Box<dyn FileFormat>>) -> Self {
        let mut index: HashMap<(u16, [u8; 2]), Vec<usize>> = HashMap::new();
        let mut short = Vec::new();
        for (position, format) in formats.iter().enumerate() {
            for signature in format.signatures() {
                match signature.bytes {
                    [first, second, ..] => {
                        let candidates = index
                            .entry((signature.offset, [*first, *second]))
                            .or_default();
                        if !candidates.contains(&position) {
                            candidates.push(position);
                        }
                    }
                    _ => {
                        if !short.contains(&position) {
                            short.push(position);
                        }
                    }
                }
            }
        }
        let mut offsets: Vec<u16> = index.keys().map(|(offset, _)| *offset).collect();
        offsets.sort_unstable();
        offsets.dedup();
        Self {
            formats,
            index,
            offsets,
            short,
        }
    }

    /// 内置的全部格式。
    pub fn builtin() -> Self {
        Self::new(vec![
            Box::new(Jpg),
            Box::new(Png),
            Box::new(Pdf),
            Box::new(Zip),
            Box::new(Mp4),
            Box::new(Heic),
            Box::new(Cr3),
            Box::new(Gif),
            Box::new(Bmp),
            Box::new(Tiff),
            Box::new(WEBP),
            Box::new(AVI),
            Box::new(Mkv),
            Box::new(WAV),
            Box::new(Mp3),
            Box::new(Ole),
            Box::new(SevenZip),
            Box::new(Rar),
            Box::new(Sqlite),
            Box::new(Ogg),
            Box::new(AIFF),
            Box::new(DJVU),
            Box::new(Midi),
            Box::new(Flv),
            Box::new(Asf),
            Box::new(Cab),
            Box::new(Tar),
            Box::new(Pe),
            Box::new(Sfnt),
            Box::new(Woff),
            Box::new(Ico),
            Box::new(Psd),
            Box::new(Pst),
            Box::new(Swf),
            Box::new(Raf),
            Box::new(Evtx),
            Box::new(Regf),
            Box::new(Chm),
            Box::new(Emf),
            Box::new(Wmf),
        ])
    }

    pub fn formats(&self) -> &[Box<dyn FileFormat>] {
        &self.formats
    }

    /// 按扩展名找格式序号：格式本身的扩展名，或按内容改用的扩展名（[`FormatInfo::variants`](crate::FormatInfo)）。
    pub fn position(&self, extension: &str) -> Option<usize> {
        self.formats.iter().position(|format| {
            let info = format.info();
            info.extension == extension || info.variants.contains(&extension)
        })
    }

    /// 检查一块的开头是不是某个已启用格式的文件头。`enabled` 按格式序号给出是否启用。
    pub fn match_header(&self, block: &[u8], enabled: &[bool]) -> Option<HeaderMatch> {
        let is_enabled = |position: usize| enabled.get(position).copied().unwrap_or(false);
        for &offset in &self.offsets {
            let start = usize::from(offset);
            let Some(&[first, second]) = block.get(start..start + 2) else {
                continue;
            };
            let Some(candidates) = self.index.get(&(offset, [first, second])) else {
                continue;
            };
            for &position in candidates {
                if is_enabled(position)
                    && let Some(found) = self.check(position, block, Some(offset))
                {
                    return Some(found);
                }
            }
        }
        self.short
            .iter()
            .filter(|&&position| is_enabled(position))
            .find_map(|&position| self.check(position, block, None))
    }

    fn check(&self, position: usize, block: &[u8], offset: Option<u16>) -> Option<HeaderMatch> {
        let format = self.formats.get(position)?;
        let signature_matches = format.signatures().iter().any(|signature| {
            offset.is_none_or(|offset| offset == signature.offset)
                && block
                    .get(usize::from(signature.offset)..)
                    .is_some_and(|tail| tail.starts_with(signature.bytes))
        });
        if !signature_matches {
            return None;
        }
        let recovering = format.header_check(block)?;
        Some(HeaderMatch {
            format: position,
            recovering,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::samples;

    #[test]
    fn builtin_extensions_are_unique_and_signatures_indexed() {
        let registry = Registry::builtin();
        let all: Vec<&str> = registry
            .formats()
            .iter()
            .flat_map(|format| {
                let info = format.info();
                std::iter::once(info.extension).chain(info.variants.iter().copied())
            })
            .collect();
        let unique: HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len(), "{all:?}");
        for (position, format) in registry.formats().iter().enumerate() {
            for variant in format.info().variants {
                assert_eq!(registry.position(variant), Some(position), "{variant}");
            }
        }
        assert!(registry.short.is_empty());
    }

    #[test]
    fn each_sample_matches_its_own_format_only_when_enabled() {
        let registry = Registry::builtin();
        let all = vec![true; registry.formats().len()];
        let cases = [
            ("jpg", samples::jpg(100)),
            ("png", samples::png(&[10])),
            ("pdf", samples::pdf(1, false)),
            ("zip", samples::zip(&[("a", 10, false)], b"")),
            ("mp4", samples::mp4(100, false)),
            (
                "heic",
                samples::iso_media(b"heic", &[(b"meta", 40), (b"mdat", 100)]),
            ),
            (
                "cr3",
                samples::iso_media(b"crx ", &[(b"moov", 40), (b"mdat", 100)]),
            ),
            ("gif", samples::gif(1, true)),
            ("bmp", samples::bmp(4, 4, 24)),
            ("tif", samples::tiff(&samples::TiffLayout::default())),
            (
                "tif",
                samples::tiff(&samples::TiffLayout {
                    big: true,
                    ..samples::TiffLayout::default()
                }),
            ),
            ("webp", samples::riff(b"WEBP", &[(b"VP8L", 30)])),
            ("avi", samples::riff(b"AVI ", &[(b"LIST", 30)])),
            ("mkv", samples::mkv("matroska", 50, false)),
            ("wav", samples::riff(b"WAVE", &[(b"fmt ", 16)])),
            ("mp3", samples::mp3(3, 1, 5, false, 0)),
            ("doc", samples::ole(3, 10, "WordDocument", false)),
            ("7z", samples::sevenzip(10, 10)),
            ("rar", samples::rar5(&[10])),
            ("rar", samples::rar4(&[10], true)),
            ("sqlite", samples::sqlite(512, 1)),
            ("ogg", samples::ogg(&[b"\x01vorbis"], 2, 1)),
            ("aif", samples::iff(b"", b"AIFF", &[(b"COMM", 18)])),
            ("djvu", samples::iff(b"AT&T", b"DJVU", &[(b"INFO", 10)])),
            ("mid", samples::midi(&[20], false)),
            ("flv", samples::flv(2)),
            ("asf", samples::asf(false, 100)),
            ("cab", samples::cab(1, 50)),
            ("tar", samples::tar(&[("a.txt", 10)], false, false)),
            ("exe", samples::pe(&samples::PeLayout::default())),
            ("ttf", samples::sfnt(b"\0\x01\0\0", &[(b"head", 54)])),
            ("ttc", samples::ttc(1, false)),
            ("woff", samples::woff(false, 200)),
            ("woff2", samples::woff(true, 200)),
            ("ico", samples::ico(&[(true, 80)], false)),
            ("cur", samples::ico(&[(false, 80)], true)),
            ("psd", samples::psd(&samples::PsdLayout::default())),
            ("pst", samples::pst(false, false, 1000)),
            ("swf", samples::swf(&[4])),
            ("raf", samples::raf(100, 100)),
            ("evtx", samples::evtx(1)),
            ("dat", samples::regf(1)),
            ("chm", samples::chm(2, 10)),
            ("emf", samples::emf(1, 0)),
            ("wmf", samples::wmf(false, 1)),
        ];
        for (extension, sample) in cases {
            let position = registry.position(extension).unwrap();
            let found = registry.match_header(&sample, &all).unwrap();
            assert_eq!(found.format, position, "{extension}");

            let mut without = all.clone();
            without[position] = false;
            assert!(
                registry.match_header(&sample, &without).is_none(),
                "{extension}"
            );
        }
        assert!(registry.match_header(&[0u8; 512], &all).is_none());
        assert!(registry.match_header(&[], &all).is_none());
    }
}
