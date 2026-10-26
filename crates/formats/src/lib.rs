//! 文件格式插件与注册表。
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod bytes;
mod jpg;
mod mp4;
mod pdf;
mod plugin;
mod png;
mod registry;
#[cfg(any(test, feature = "test-samples"))]
pub mod samples;
mod walker;
mod zip;

pub use plugin::{DataCheck, FileFormat, FormatInfo, Recovering, Signature};
pub use registry::{HeaderMatch, Registry};

#[cfg(test)]
mod robustness {
    //! 随机输入与变异样本：解析器只能给出结果或「不认识」，不能 panic，给出的长度不能超过数据长度。
    use crate::samples::{self, Noise};
    use crate::walker::tests::run;
    use crate::{DataCheck, Registry};

    fn exercise(registry: &Registry, data: &[u8], noise: &mut Noise) {
        let all = vec![true; registry.formats().len()];
        let Some(found) = registry.match_header(data, &all) else {
            return;
        };
        let block_size = 1 + (noise.next_u64() % 700) as usize;
        if let Some(length) = run(found.recovering, data, block_size) {
            let format = &registry.formats()[found.format];
            assert!(
                length <= data.len() as u64 || length <= format.info().max_size,
                "{} 给出的长度 {length} 不合理",
                format.info().extension
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
