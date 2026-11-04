#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;

use serde_json::json;

use super::*;

/// 内存里的设备；`bad` 范围内的读取报错，模拟坏扇区。
struct Memory {
    data: Vec<u8>,
    bad: Option<(u64, u64)>,
}

impl BlockDevice for Memory {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn sector_size(&self) -> u32 {
        512
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        let start = (offset as usize).min(self.data.len());
        let end = (start + buf.len()).min(self.data.len());
        if let Some((bad_start, bad_end)) = self.bad
            && (start as u64) < bad_end
            && (end as u64) > bad_start
        {
            return Err(IoError::Read {
                offset,
                len: buf.len(),
                source: std::io::Error::other("坏扇区"),
            });
        }
        buf[..end - start].copy_from_slice(&self.data[start..end]);
        Ok(end - start)
    }
}

fn device() -> DeviceIdentity {
    DeviceIdentity {
        identity: "image D:\\x.img".to_owned(),
        size: 8192,
        sector_size: 512,
    }
}

fn write(offset: u64, len: usize, before: u8, after: u8) -> SectorWrite {
    SectorWrite {
        offset,
        before: vec![before; len],
        after: vec![after; len],
    }
}

fn plan(writes: Vec<SectorWrite>) -> WritePlan {
    WritePlan {
        device: device(),
        writes,
    }
}

#[test]
fn accepts_sorted_aligned_changes_inside_the_device() {
    let plan = plan(vec![write(0, 512, 0, 1), write(1024, 1024, 0, 2)]);
    assert_eq!(plan.validate(), Ok(()));
    assert_eq!(plan.total_bytes(), 1536);
}

#[test]
fn rejects_malformed_plans() {
    assert_eq!(plan(vec![]).validate(), Err(PlanError::Empty));
    assert_eq!(
        plan(vec![write(100, 512, 0, 1)]).validate(),
        Err(PlanError::Unaligned {
            offset: 100,
            length: 512,
            sector_size: 512
        })
    );
    assert_eq!(
        plan(vec![write(0, 100, 0, 1)]).validate(),
        Err(PlanError::Unaligned {
            offset: 0,
            length: 100,
            sector_size: 512
        })
    );
    assert_eq!(
        plan(vec![write(7680, 1024, 0, 1)]).validate(),
        Err(PlanError::OutOfRange {
            offset: 7680,
            length: 1024,
            size: 8192
        })
    );
    assert_eq!(
        plan(vec![write(0, 1024, 0, 1), write(512, 512, 0, 1)]).validate(),
        Err(PlanError::Overlap(512))
    );
    assert_eq!(
        plan(vec![write(1024, 512, 0, 1), write(0, 512, 0, 1)]).validate(),
        Err(PlanError::Overlap(0))
    );
    assert_eq!(
        plan(vec![write(0, 512, 7, 7)]).validate(),
        Err(PlanError::Unchanged(0))
    );
    let mut uneven = write(0, 512, 0, 1);
    uneven.after.truncate(256);
    assert_eq!(
        plan(vec![uneven]).validate(),
        Err(PlanError::LengthMismatch {
            offset: 0,
            before: 512,
            after: 256
        })
    );
    let mut odd_sector = plan(vec![write(0, 512, 0, 1)]);
    odd_sector.device.sector_size = 500;
    assert_eq!(odd_sector.validate(), Err(PlanError::SectorSize(500)));
}

#[test]
fn limits_count_and_total_size() {
    let mut big = plan(vec![write(0, 512, 0, 1)]);
    big.device.size = 1 << 30;
    big.writes = (0..=MAX_WRITES as u64)
        .map(|index| write(index * 1024, 512, 0, 1))
        .collect();
    assert_eq!(
        big.validate(),
        Err(PlanError::TooManyWrites(MAX_WRITES + 1))
    );
    big.writes = vec![write(0, MAX_PLAN_BYTES as usize + 512, 0, 1)];
    assert_eq!(
        big.validate(),
        Err(PlanError::TooLarge(MAX_PLAN_BYTES + 512))
    );
    big.writes = vec![write(0, MAX_PLAN_BYTES as usize, 0, 1)];
    assert_eq!(big.validate(), Ok(()));
}

#[test]
fn inverse_swaps_before_and_after() {
    let plan = plan(vec![write(512, 512, 3, 4)]);
    let inverse = plan.inverse();
    assert_eq!(inverse.device, plan.device);
    assert_eq!(inverse.writes, vec![write(512, 512, 4, 3)]);
    assert_eq!(inverse.inverse(), plan);
}

#[test]
fn bytes_travel_as_lowercase_hex() {
    let plan = plan(vec![SectorWrite {
        offset: 0,
        before: vec![0x00, 0xAB],
        after: vec![0xFF, 0x10],
    }]);
    let value = serde_json::to_value(&plan).unwrap();
    assert_eq!(
        value,
        json!({
            "device": {"identity": "image D:\\x.img", "size": 8192, "sectorSize": 512},
            "writes": [{"offset": 0, "before": "00ab", "after": "ff10"}],
        })
    );
    assert_eq!(serde_json::from_value::<WritePlan>(value).unwrap(), plan);
    for bad in ["0", "zz", "0g"] {
        let value = json!({
            "device": {"identity": "x", "size": 1, "sectorSize": 512},
            "writes": [{"offset": 0, "before": bad, "after": "00"}],
        });
        assert!(serde_json::from_value::<WritePlan>(value).is_err(), "{bad}");
    }
}

#[test]
fn overlay_reads_the_written_content() {
    let base: Arc<dyn BlockDevice> = Arc::new(Memory {
        data: vec![0; 4096],
        bad: None,
    });
    let overlay = Overlay::new(Arc::clone(&base), &[write(1024, 512, 0, 9)]);
    assert_eq!(overlay.size(), 4096);
    let mut buf = vec![0u8; 1024];
    // 只与写入的前半段相交
    assert_eq!(overlay.read_at(768, &mut buf).unwrap(), 1024);
    assert!(buf[..256].iter().all(|&byte| byte == 0));
    assert!(buf[256..768].iter().all(|&byte| byte == 9));
    assert!(buf[768..].iter().all(|&byte| byte == 0));
    // 读到设备末尾时照常截断
    assert_eq!(overlay.read_at(3584, &mut buf).unwrap(), 512);
    // 设备本身不受影响
    let mut original = [1u8; 512];
    base.read_at(1024, &mut original).unwrap();
    assert!(original.iter().all(|&byte| byte == 0));
}

#[test]
fn overlay_covers_unreadable_sectors_only_when_fully_written() {
    let base: Arc<dyn BlockDevice> = Arc::new(Memory {
        data: vec![0; 4096],
        bad: Some((1024, 1536)),
    });
    let overlay = Overlay::new(base, &[write(1024, 512, 0, 5)]);
    let mut sector = [0u8; 512];
    assert_eq!(overlay.read_at(1024, &mut sector).unwrap(), 512);
    assert!(sector.iter().all(|&byte| byte == 5));
    // 读的范围有一部分没被写入覆盖：设备读不出就照样报错
    let mut wider = [0u8; 1024];
    assert!(overlay.read_at(1024, &mut wider).is_err());
}

#[test]
fn image_identity_names_the_real_path() {
    assert_eq!(
        image_identity(Path::new("D:\\images\\a.img")),
        "image D:\\images\\a.img"
    );
}
