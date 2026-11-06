//! Adler-32 校验和（RFC 1950）：E01 镜像的节描述、块表、未压缩的块都用它校验。

/// 小于 2^16 的最大素数。
const MOD_ADLER: u32 = 65_521;
/// 一次最多累加这么多字节再取模，`b` 不会溢出 u32（zlib 用的同一个上限）。
const NMAX: usize = 5552;

/// `data` 的 Adler-32。
pub fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for block in data.chunks(NMAX) {
        for &byte in block {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD_ADLER;
        b %= MOD_ADLER;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_values() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        // 长输入跨过取模的分段：与逐字节取模的算法一致
        let long: Vec<u8> = (0..100_000u32).map(|i| (i * 7 + i / 13) as u8).collect();
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in &long {
            a = (a + u32::from(byte)) % MOD_ADLER;
            b = (b + a) % MOD_ADLER;
        }
        assert_eq!(adler32(&long), (b << 16) | a);
    }
}
