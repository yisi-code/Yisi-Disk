//! 引导扇区参数只有一套规则（`yisi_fs::bpb`）：把样本主引导扇区的参数区随机改坏，打开卷、引导扇区诊断、
//! 文件系统检查对「这一份能不能用」必须给出同一个结论。
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;

use yisi_fs::bpb::{check_fat, check_ntfs};
use yisi_fs::samples::{FatImage, FatType, MemoryDisk, Noise, NtfsImage};
use yisi_io::{BlockDevice, IoError};
use yisi_part::{Partition, PartitionKind};
use yisi_repair::boot::{Verdict, diagnose_partition};
use yisi_repair::check::{self, Control, FsType, IssueKind, Stage};

/// 共用一份样本，只把开头 512 字节换成改过的主引导扇区，免得每轮复制几十 MiB。
struct Patched {
    base: Arc<MemoryDisk>,
    sector: [u8; 512],
}

impl BlockDevice for Patched {
    fn size(&self) -> u64 {
        self.base.size()
    }

    fn sector_size(&self) -> u32 {
        512
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        let read = self.base.read_at(offset, buf)?;
        for (index, byte) in buf[..read].iter_mut().enumerate() {
            if let Some(&patched) = self.sector.get(offset as usize + index) {
                *byte = patched;
            }
        }
        Ok(read)
    }
}

struct Quiet;

impl Control for Quiet {
    fn checkpoint(&self) -> bool {
        true
    }

    fn progress(&self, _: Stage, _: u64, _: u64) {}
}

/// 改 `count` 个参数区里的字节：一半取随机值，一半取 0、1、2、0xFF 这类边界值。
fn mutate(noise: &mut Noise, sector: &mut [u8; 512], range: std::ops::Range<usize>) {
    let count = 1 + noise.below(3) as usize;
    for _ in 0..count {
        let at = range.start + noise.below((range.end - range.start) as u64) as usize;
        sector[at] = if noise.below(2) == 0 {
            noise.next_u64() as u8
        } else {
            [0, 1, 2, 0xFF][noise.below(4) as usize]
        };
    }
}

fn agree(name: &str, image: Vec<u8>, usable: fn(&[u8]) -> bool, rounds: u32) {
    let length = image.len() as u64;
    let original: [u8; 512] = image[..512].try_into().unwrap();
    let base = Arc::new(MemoryDisk::new(image, 512));
    let partition = Partition {
        number: 1,
        offset: 0,
        length,
        kind: PartitionKind::Whole,
    };
    let mut noise = Noise::new(name.len() as u64);
    let mut both = [0u32; 2];
    for round in 0..rounds {
        let mut sector = original;
        mutate(&mut noise, &mut sector, 11..0x54);
        let expected = usable(&sector);
        both[usize::from(expected)] += 1;
        let device: Arc<dyn BlockDevice> = Arc::new(Patched {
            base: Arc::clone(&base),
            sector,
        });

        let browsable = yisi_fs::detect(device.as_ref(), 0, length)
            .unwrap()
            .is_browsable();
        assert_eq!(browsable, expected, "{name} 第 {round} 轮：打开卷");

        // 诊断：主引导扇区能用的结论只有「正常」与「备份有问题」（认不出文件系统时根本不核对）
        let diagnosis = diagnose_partition(&device, &partition);
        let primary_valid = matches!(diagnosis.verdict, Verdict::Healthy | Verdict::BackupDamaged);
        assert_eq!(
            primary_valid, expected,
            "{name} 第 {round} 轮：诊断 {:?} {:?}",
            diagnosis.verdict, diagnosis.primary
        );

        // 主引导扇区不能用时，检查要么认不出，要么改按备份（并记下认不出主引导扇区）
        let report = check::check(device.as_ref(), 0, length, &Quiet).unwrap();
        let primary_used = report.fs != FsType::Unknown
            && !report
                .issues
                .iter()
                .any(|issue| issue.kind == IssueKind::BootUnrecognized);
        assert_eq!(primary_used, expected, "{name} 第 {round} 轮：检查");
    }
    // 两种情况都要覆盖到，否则这个测试说明不了什么
    assert!(both[0] > 0 && both[1] > 0, "{name}：{both:?}");
}

#[test]
fn opening_diagnosis_and_check_agree_on_damaged_boot_sectors() {
    let fat_usable: fn(&[u8]) -> bool = |sector| check_fat(sector, None).geometry.is_some();
    agree(
        "fat16",
        FatImage::new(FatType::Fat16).finish(),
        fat_usable,
        50,
    );
    agree(
        "fat32",
        FatImage::new(FatType::Fat32).finish(),
        fat_usable,
        50,
    );
    agree(
        "ntfs",
        NtfsImage::new().finish(),
        |sector| check_ntfs(sector).geometry.is_some(),
        50,
    );
}
