//! 分区表的解析、校验、序列化与自动识别。
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

pub mod bytes;
pub mod crc32;
mod gpt;
mod guid;
mod mbr;
mod role;
#[cfg(any(test, feature = "test-samples"))]
pub mod samples;
mod table;

pub use guid::Guid;
pub use role::Role;
pub use table::{Note, PartError, Partition, PartitionKind, PartitionTable, Scheme, read_table};

#[cfg(test)]
mod robustness {
    //! 随机改坏的分区表：只能给出结果，不能 panic；列出的分区都在设备之内。
    use crate::samples::{self, GptEntry, MbrEntry, MemoryDisk, Noise};
    use crate::{Guid, read_table};

    fn check(image: Vec<u8>, sector: u32) {
        let disk = MemoryDisk::new(image, sector);
        let size = disk.data().len() as u64;
        let table = read_table(&disk).unwrap();
        for partition in &table.partitions {
            assert!(partition.offset < size || partition.length == 0);
            assert!(partition.offset + partition.length <= size);
        }
    }

    #[test]
    fn mutated_tables_never_panic() {
        let data = Guid::from_text("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7");
        let mut mbr = samples::mbr_disk(
            4096 * 512,
            &[MbrEntry::new(0x0C, 63, 500), MbrEntry::new(0x0F, 600, 3000)],
        );
        samples::ebr_chain(&mut mbr, 512, 600, &[(0x07, 63, 100), (0x83, 63, 100)]);
        let gpt = samples::gpt_disk(
            4096,
            512,
            &[
                GptEntry::new(data, 40, 100, "a"),
                GptEntry::new(data, 200, 900, "b"),
            ],
        );
        let mut noise = Noise::new(7);
        for round in 0..400 {
            for (base, region) in [
                (&mbr, 0..512),
                (&mbr, 600 * 512..602 * 512),
                (&gpt, 0..34 * 512),
            ] {
                let mut image = base.clone();
                let count = 1 + noise.below(8) as usize;
                noise.mutate(&mut image[region.clone()], count);
                check(image, 512);
            }
            // 整个开头换成随机字节，偶尔保留 55 AA
            let mut image = vec![0u8; 64 * 512];
            image[..2048].copy_from_slice(&noise.bytes(2048));
            if round % 2 == 0 {
                image[510] = 0x55;
                image[511] = 0xAA;
            }
            check(image, 512);
        }
    }
}
