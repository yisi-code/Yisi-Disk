//! 文件格式插件与注册表。
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod asf;
mod bmp;
mod cab;
mod chm;
mod evtx;
mod flac;
mod flv;
mod font;
mod gif;
mod ico;
mod iff;
mod iso;
mod jpg;
mod metafile;
mod midi;
mod mkv;
mod mp3;
mod mp4;
mod ogg;
mod ole;
mod pdf;
mod pe;
mod plugin;
mod png;
mod psd;
mod pst;
mod raf;
mod rar;
mod regf;
mod registry;
mod riff;
#[cfg(any(test, feature = "test-samples"))]
pub mod samples;
mod sevenzip;
mod sqlite;
mod swf;
mod tar;
mod tiff;
mod walker;
mod zip;

pub use plugin::{DataCheck, FileFormat, FormatInfo, Recovering, Signature};
pub use registry::{HeaderMatch, Registry};

#[cfg(test)]
mod robustness {
    //! 随机输入与变异样本：解析器只能给出结果或「不认识」，不能 panic，给出的长度不能超过数据长度；
    //! 部分恢复给出的长度也不能超过数据长度。
    use crate::samples::{self, Noise};
    use crate::walker::tests::{run, run_intact};
    use crate::{DataCheck, Registry};

    fn exercise(registry: &Registry, data: &[u8], noise: &mut Noise) {
        let all = vec![true; registry.formats().len()];
        let Some(found) = registry.match_header(data, &all) else {
            return;
        };
        let block_size = 1 + (noise.next_u64() % 700) as usize;
        let format = &registry.formats()[found.format];
        let extension = format.info().extension;
        if let Some(length) = run(found.recovering, data, block_size) {
            assert!(
                length <= data.len() as u64 || length <= format.info().max_size,
                "{extension} 给出的长度 {length} 不合理"
            );
        }
        if let Some(found) = registry.match_header(data, &all)
            && let Some(length) = run_intact(found.recovering, data, block_size)
        {
            assert!(
                length <= data.len() as u64,
                "{extension} 不完整的长度 {length} 超出数据"
            );
        }
    }

    #[test]
    fn random_and_mutated_inputs_never_panic() {
        let registry = Registry::builtin();
        let mut noise = Noise::new(42);
        let seeds = [
            samples::jpg(300),
            samples::jpg_progressive(200),
            samples::png(&[20, 300]),
            samples::pdf(2, true),
            samples::zip(&[("a", 30, true), ("b", 40, false)], b"x"),
            samples::mp4(200, true),
            samples::mp4_fragmented(&[120, 60], Some(0), 16, true),
            samples::mp4_fragmented(&[80, 90, 40], Some(1), 0, false),
            samples::iso_media(b"heic", &[(b"meta", 80), (b"mdat", 200)]),
            samples::gif(3, true),
            samples::bmp(9, 4, 8),
            samples::tiff(&samples::TiffLayout {
                make: Some(b"NIKON CORPORATION"),
                preview: true,
                sub_ifd: true,
                ..samples::TiffLayout::default()
            }),
            samples::tiff(&samples::TiffLayout {
                big: true,
                ifd_last: true,
                ..samples::TiffLayout::default()
            }),
            samples::riff(b"AVI ", &[(b"LIST", 90), (b"idx1", 16)]),
            samples::riff(b"WAVE", &[(b"fmt ", 16), (b"data", 101)]),
            samples::mkv("webm", 300, false),
            samples::mp3(3, 1, 8, true, 0),
            samples::ole(3, 20, "Workbook", false),
            samples::sevenzip(200, 30),
            samples::rar5(&[100, 30]),
            samples::rar4(&[60, 10], false),
            samples::sqlite(1024, 2),
            samples::ogg(&[b"\x80theora", b"\x01vorbis"], 3, 2),
            samples::iff(b"", b"AIFF", &[(b"COMM", 18), (b"SSND", 200)]),
            samples::iff(b"AT&T", b"DJVM", &[(b"DIRM", 30), (b"FORM", 90)]),
            samples::midi(&[40, 7], true),
            samples::flv(5),
            samples::asf(true, 300),
            samples::cab(3, 200),
            samples::tar(&[("a.txt", 10), ("b.bin", 700)], false, false),
            samples::pe(&samples::PeLayout {
                certificate: 64,
                symbols: Some((4, 30)),
                ..samples::PeLayout::default()
            }),
            samples::sfnt(b"OTTO", &[(b"CFF ", 300), (b"head", 54)]),
            samples::ttc(2, true),
            samples::woff(false, 300),
            samples::woff(true, 300),
            samples::ico(&[(true, 100), (false, 60)], false),
            samples::psd(&samples::PsdLayout {
                rle: true,
                ..samples::PsdLayout::default()
            }),
            samples::pst(true, false, 1000),
            samples::swf(&[5, 70, 0]),
            samples::raf(200, 300),
            samples::chm(3, 100),
            samples::emf(2, 4),
            samples::wmf(true, 3),
            samples::zip_with(
                &[(
                    "mimetype",
                    b"application/vnd.oasis.opendocument.text".to_vec(),
                    false,
                )],
                b"",
            ),
            samples::flac(&samples::FlacLayout::default()),
            samples::flac(&samples::FlacLayout {
                channels: 1,
                variable: true,
                ..samples::FlacLayout::default()
            }),
            samples::cab_signed(2, 300, 500),
        ];
        for round in 0..3000 {
            let seed = &seeds[round % seeds.len()];
            let mut data = seed.clone();
            // 随机改几个字节、随机截断
            for _ in 0..(1 + noise.next_u64() % 8) {
                let at = 4 + (noise.next_u64() as usize % data.len().saturating_sub(4).max(1));
                if let Some(byte) = data.get_mut(at) {
                    *byte = noise.next_u64() as u8;
                }
            }
            data.truncate(noise.next_u64() as usize % (data.len() + 1));
            exercise(&registry, &data, &mut noise);

            // 签名后面接纯随机数据
            let mut random = seed.get(..12).unwrap_or_default().to_vec();
            let extra = 1 + noise.next_u64() as usize % 2000;
            random.extend(noise.bytes(extra));
            exercise(&registry, &random, &mut noise);
        }
    }

    /// 每个格式的每个签名放在随机数据里：结构核对要把它排除掉，不能当成一个完整的文件，也不能当成不完整的文件。
    /// ISO 9660 的签名在 32 KiB 处的那一块，单独在 `iso` 里测。
    #[test]
    fn signatures_inside_random_bytes_are_not_reported_as_files() {
        let registry = Registry::builtin();
        let all = vec![true; registry.formats().len()];
        let mut noise = Noise::new(7);
        for format in registry.formats() {
            for signature in format.signatures() {
                for _ in 0..200 {
                    let mut data = noise.bytes(16 * 1024);
                    let at = usize::from(signature.offset);
                    data[at..at + signature.bytes.len()].copy_from_slice(signature.bytes);
                    if let Some(found) = registry.match_header(&data, &all) {
                        let extension = registry.formats()[found.format].info().extension;
                        assert_eq!(run(found.recovering, &data, 512), None, "{extension}");
                    }
                    if let Some(found) = registry.match_header(&data, &all) {
                        let extension = registry.formats()[found.format].info().extension;
                        assert_eq!(
                            run_intact(found.recovering, &data, 512),
                            None,
                            "{extension} 不完整"
                        );
                    }
                }
            }
        }
    }

    /// 部分恢复：支持的格式在中途截断后给出一段，不超过截断处；只拿这一段再走一遍（另存时就是这样核对的），
    /// 得到同样的长度。不支持的格式截断后说不出。
    #[test]
    fn cut_files_keep_a_prefix_that_reproduces_itself() {
        let registry = Registry::builtin();
        let all = vec![true; registry.formats().len()];
        let intact = |data: &[u8], block_size: usize| {
            let found = registry.match_header(data, &all)?;
            run_intact(found.recovering, data, block_size)
        };
        // 完整时的长度，不完整时部分恢复给出的长度
        let outcome = |data: &[u8], block_size: usize| {
            let found = registry.match_header(data, &all)?;
            run(found.recovering, data, block_size).or_else(|| intact(data, block_size))
        };
        let supported = [
            ("jpg", samples::jpg(6000)),
            ("png", samples::png(&[3000, 3000, 3000])),
            ("gif", samples::gif(6, true)),
            ("bmp", samples::bmp(64, 64, 24)),
            ("mp4", samples::mp4(20_000, false)),
            ("webm", samples::mkv("webm", 6000, false)),
            ("mp3", samples::mp3(3, 1, 60, false, 0)),
            ("ogg", samples::ogg(&[b"\x01vorbis"], 8, 1)),
            ("flv", samples::flv(30)),
            (
                "wav",
                samples::riff(b"WAVE", &[(b"fmt ", 16), (b"data", 6000)]),
            ),
            (
                "avi",
                samples::riff(b"AVI ", &[(b"LIST", 90), (b"movi", 6000)]),
            ),
            (
                "aif",
                samples::iff(b"", b"AIFF", &[(b"COMM", 18), (b"SSND", 6000)]),
            ),
            (
                "zip",
                samples::zip(
                    &[
                        ("a", 300, false),
                        ("b", 300, true),
                        ("c", 300, false),
                        ("d", 300, false),
                    ],
                    b"",
                ),
            ),
            ("rar", samples::rar5(&[300, 300, 300, 300])),
            ("rar", samples::rar4(&[300, 300, 300, 300], true)),
            (
                "tar",
                samples::tar(&[("a", 600), ("b", 600), ("c", 600)], false, false),
            ),
            ("flac", samples::flac(&samples::FlacLayout::default())),
        ];
        for (extension, sample) in &supported {
            for tenths in [4, 6, 8] {
                let cut = &sample[..sample.len() * tenths / 10];
                for block_size in [7, 512, 4096] {
                    // 截在两个单元之间的，有的格式本来就当作完整（FLV、RAR 4 到最后一个完整的单元为止）；
                    // 只拿那一段再走一遍，也可能这样当作完整，长度一样就行
                    let length = outcome(cut, block_size).unwrap_or_else(|| {
                        panic!("{extension} 截在 {tenths}/10 处既不完整、也没给出一段")
                    });
                    assert!(length <= cut.len() as u64, "{extension}");
                    let again = &cut[..length as usize];
                    assert_eq!(
                        outcome(again, block_size),
                        Some(length),
                        "{extension} 截在 {tenths}/10 处、块大小 {block_size}"
                    );
                }
            }
        }
        let unsupported = [
            ("pdf", samples::pdf(4, false)),
            ("webp", samples::riff(b"WEBP", &[(b"VP8L", 6000)])),
            ("7z", samples::sevenzip(4000, 300)),
            ("sqlite", samples::sqlite(1024, 8)),
            (
                "heic",
                samples::iso_media(b"heic", &[(b"meta", 400), (b"mdat", 6000)]),
            ),
            ("doc", samples::ole(3, 20, "WordDocument", false)),
        ];
        for (extension, sample) in &unsupported {
            let cut = &sample[..sample.len() * 6 / 10];
            assert_eq!(intact(cut, 512), None, "{extension}");
        }
    }

    #[test]
    fn data_check_after_completion_keeps_reporting_completion() {
        let registry = Registry::builtin();
        let all = vec![true; registry.formats().len()];
        let sample = samples::png(&[5]);
        let mut found = registry.match_header(&sample, &all).unwrap();
        let first = found.recovering.data_check(0, &sample);
        assert_eq!(first, DataCheck::Complete(sample.len() as u64));
        assert_eq!(
            found.recovering.data_check(sample.len() as u64, b"more"),
            first
        );
    }
}
