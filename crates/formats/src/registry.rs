//! 格式注册表：把全部签名按「偏移 + 前两个字节」建成查找表，每块只查表，不逐个格式去试。

use std::collections::HashMap;

use crate::plugin::{FileFormat, Recovering};
use crate::{jpg::Jpg, mp4::Mp4, pdf::Pdf, png::Png, zip::Zip};

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
        ])
    }

    pub fn formats(&self) -> &[Box<dyn FileFormat>] {
        &self.formats
    }

    /// 按扩展名找格式序号。
    pub fn position(&self, extension: &str) -> Option<usize> {
        self.formats
            .iter()
            .position(|format| format.info().extension == extension)
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
        let extensions: HashSet<_> = registry
            .formats()
            .iter()
            .map(|format| format.info().extension)
            .collect();
        assert_eq!(extensions.len(), registry.formats().len());
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
