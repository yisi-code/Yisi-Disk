use yisi_broker::execute::Step;
use yisi_engine::protocol::{DeviceIdentity, SectorWrite};

use super::*;
use crate::engines::IMAGE_SECTOR_SIZE;

/// 每个测试一个数据目录（代替程序文件夹下的 `data`）与一个 8 KiB 的镜像（全 0）。
fn setup(name: &str) -> (WritePlans, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("yisi-write-plans-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("disk.img");
    std::fs::write(&image, vec![0u8; 8192]).unwrap();
    (WritePlans::new(dir.join("data")), dir, image)
}

fn plan_for(image: &Path) -> WritePlan {
    WritePlan {
        device: DeviceIdentity {
            identity: image_identity(&std::fs::canonicalize(image).unwrap()),
            size: 8192,
            sector_size: IMAGE_SECTOR_SIZE,
        },
        writes: vec![SectorWrite {
            offset: 1024,
            before: vec![0; 512],
            after: vec![7; 512],
        }],
    }
}

fn summary(image: &Path) -> PlanSummary {
    PlanSummary {
        kind: PlanKind::BootSector,
        target: Target::Image {
            path: image.display().to_string(),
        },
        writes: Vec::new(),
        boot_sector: None,
        partition_table: None,
        mft: None,
        fs_fix: None,
        rollback_of: None,
        device: None,
    }
}

fn image_target(image: &Path) -> Target {
    Target::Image {
        path: image.display().to_string(),
    }
}

#[test]
fn executes_records_and_rolls_back() {
    let (plans, dir, image) = setup("flow");
    let view = plans.propose(plan_for(&image), summary(&image)).unwrap();
    assert_eq!(
        view.summary.writes,
        vec![WriteRange {
            offset: 1024,
            length: 512
        }]
    );
    assert!(Path::new(&view.backup_path).exists());
    // 确认之前什么都没写
    assert!(std::fs::read(&image).unwrap().iter().all(|&byte| byte == 0));

    let record = plans
        .execute(&BrokerSession::default(), &view.plan_id)
        .unwrap();
    assert!(record.outcome.done);
    assert_eq!(record.outcome.written, 1);
    let written = std::fs::read(&image).unwrap();
    assert!(written[1024..1536].iter().all(|&byte| byte == 7));
    assert_eq!(plans.records(), vec![record.clone()]);
    // 同一份计划不能执行两次
    assert_eq!(
        plans
            .execute(&BrokerSession::default(), &view.plan_id)
            .unwrap_err()
            .code,
        "planNotFound"
    );

    let rollback = plans
        .rollback(&view.plan_id, &image_target(&image))
        .unwrap();
    assert_eq!(rollback.summary.kind, PlanKind::Rollback);
    assert_eq!(
        rollback.summary.rollback_of.as_deref(),
        Some(view.plan_id.as_str())
    );
    let undone = plans
        .execute(&BrokerSession::default(), &rollback.plan_id)
        .unwrap();
    assert!(undone.outcome.done);
    assert!(std::fs::read(&image).unwrap().iter().all(|&byte| byte == 0));
    assert_eq!(plans.records()[0].plan_id, rollback.plan_id);
    assert_eq!(
        plans
            .rollback(&view.plan_id, &image_target(&image))
            .unwrap_err()
            .code,
        "rollbackDone"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn refuses_a_changed_image_and_keeps_it_intact() {
    let (plans, dir, image) = setup("changed");
    let view = plans.propose(plan_for(&image), summary(&image)).unwrap();
    let mut bytes = std::fs::read(&image).unwrap();
    bytes[1100] = 1;
    std::fs::write(&image, &bytes).unwrap();
    let record = plans
        .execute(&BrokerSession::default(), &view.plan_id)
        .unwrap();
    assert!(!record.outcome.done);
    assert_eq!(record.outcome.step, Some(Step::Verify));
    assert_eq!(record.outcome.written, 0);
    assert_eq!(std::fs::read(&image).unwrap(), bytes);
    // 一段都没写，没什么可回滚的
    assert_eq!(
        plans
            .rollback(&view.plan_id, &image_target(&image))
            .unwrap_err()
            .code,
        "rollbackNothing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(windows)]
#[test]
fn image_in_use_keeps_the_plan_for_a_retry() {
    let (plans, dir, image) = setup("in-use");
    let view = plans.propose(plan_for(&image), summary(&image)).unwrap();
    let reader = std::fs::File::open(&image).unwrap();
    assert_eq!(
        plans
            .execute(&BrokerSession::default(), &view.plan_id)
            .unwrap_err()
            .code,
        "imageInUse"
    );
    drop(reader);
    assert!(
        plans
            .execute(&BrokerSession::default(), &view.plan_id)
            .unwrap()
            .outcome
            .done
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn discarding_removes_the_backup() {
    let (plans, dir, image) = setup("discard");
    let view = plans.propose(plan_for(&image), summary(&image)).unwrap();
    plans.discard(&view.plan_id).unwrap();
    assert!(!Path::new(&view.backup_path).exists());
    assert_eq!(
        plans
            .execute(&BrokerSession::default(), &view.plan_id)
            .unwrap_err()
            .code,
        "planNotFound"
    );
    assert!(plans.records().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn volumes_and_the_system_disk_are_not_written() {
    let (_, dir, image) = setup("writable");
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let volume = Target::Volume {
        letter: "D".to_owned(),
    };
    assert_eq!(
        writable(&volume, &data).unwrap_err().code,
        "writeTargetUnsupported"
    );
    let image_target = Target::Image {
        path: image.display().to_string(),
    };
    assert_eq!(
        writable(&image_target, &data).unwrap(),
        Writable {
            identity: image_identity(&std::fs::canonicalize(&image).unwrap()),
            size: std::fs::metadata(&image).unwrap().len(),
            sector_size: IMAGE_SECTOR_SIZE,
            model: String::new(),
            serial: String::new(),
        }
    );
    #[cfg(windows)]
    for disk_number in yisi_io::system_disks().unwrap() {
        assert_eq!(
            writable(&Target::Disk { disk_number }, &data)
                .unwrap_err()
                .code,
            "writeSystemDisk"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn boot_sector_plans_must_match_the_request() {
    let (_, dir, image) = setup("check");
    let real = std::fs::canonicalize(&image).unwrap();
    let planned = || BootSectorPlan {
        plan: plan_for(&image),
        partition: 1,
        partition_offset: 1024,
        partition_length: 4096,
        filesystem: "ntfs".to_owned(),
        fix: BootFix::PrimaryFromBackup,
        from: 4608,
        to: 1024,
        length: 512,
        rebuilt: None,
    };
    let expected = Writable {
        identity: image_identity(&real),
        size: std::fs::metadata(&real).map_or(0, |meta| meta.len()),
        sector_size: IMAGE_SECTOR_SIZE,
        model: String::new(),
        serial: String::new(),
    };
    assert!(check_boot_sector_plan(&planned(), &expected).is_ok());

    let mut other_file = planned();
    other_file.plan.device.identity = "image D:\\other.img".to_owned();
    let mut outside = planned();
    outside.partition_offset = 2048;
    let mut elsewhere = planned();
    elsewhere.to = 1536;
    let mut two = planned();
    two.plan.writes.push(SectorWrite {
        offset: 2048,
        before: vec![0; 512],
        after: vec![1; 512],
    });
    let mut sector = planned();
    sector.plan.device.sector_size = 4096;
    for (name, bad) in [
        ("other_file", other_file),
        ("outside", outside),
        ("elsewhere", elsewhere),
        ("two", two),
        ("sector", sector),
    ] {
        assert_eq!(
            check_boot_sector_plan(&bad, &expected).unwrap_err().code,
            "planMismatch",
            "{name}"
        );
    }

    // 重建：主引导扇区与备份两段，都要在分区里；再多一段、或第二段出了分区都不行
    let rebuild = || {
        let mut plan = planned();
        plan.fix = BootFix::Rebuild;
        plan.from = 1024;
        plan.plan.writes.push(SectorWrite {
            offset: 1024 + 3584,
            before: vec![0; 512],
            after: vec![1; 512],
        });
        plan
    };
    assert!(check_boot_sector_plan(&rebuild(), &expected).is_ok());
    let mut three = rebuild();
    three.plan.writes.push(SectorWrite {
        offset: 4608 + 512,
        before: vec![0; 512],
        after: vec![1; 512],
    });
    let mut past_end = rebuild();
    past_end.plan.writes[1].offset = 1024 + 4096;
    for (name, bad) in [("three", three), ("past_end", past_end)] {
        assert_eq!(
            check_boot_sector_plan(&bad, &expected).unwrap_err().code,
            "planMismatch",
            "{name}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn partition_table_plans_only_touch_the_table_areas() {
    use yisi_engine::protocol::{PartitionTableFix, PartitionTablePlan};

    let (_, dir, image) = setup("table");
    let real = std::fs::canonicalize(&image).unwrap();
    let expected = Writable {
        identity: image_identity(&real),
        size: std::fs::metadata(&real).map_or(0, |meta| meta.len()),
        sector_size: IMAGE_SECTOR_SIZE,
        model: String::new(),
        serial: String::new(),
    };
    let planned = |offset: u64| {
        let mut plan = plan_for(&image);
        plan.writes[0].offset = offset;
        PartitionTablePlan {
            plan,
            fix: PartitionTableFix::SetType { partition: 1 },
            scheme: "mbr".to_owned(),
            filesystem: "fat32".to_owned(),
            partition: 1,
            offset: 1024,
            length: 4096,
            type_id: "0x0C".to_owned(),
            role: None,
            gpt_repair: None,
        }
    };
    // 8 KiB 的设备整个都在开头与末尾 64 KiB 之内
    assert!(check_table_plan(&planned(0), &expected).is_ok());
    let mut big = planned(0);
    big.plan.device.size = 1 << 30;
    big.plan.writes[0].offset = 1 << 20;
    let mut big_expected = expected.clone();
    big_expected.sector_size = IMAGE_SECTOR_SIZE;
    assert_eq!(
        check_table_plan(&big, &big_expected).unwrap_err().code,
        "planMismatch"
    );
    big.plan.writes[0].offset = (1 << 30) - 512;
    assert!(check_table_plan(&big, &big_expected).is_ok());

    // 盘中间只许写 EBR：一个扇区、以 55 AA 结尾、只改分区表项区
    let mut ebr = big.clone();
    let mut after = vec![0u8; 512];
    after[446..462].copy_from_slice(&[
        0, 0xFE, 0xFF, 0xFF, 7, 0xFE, 0xFF, 0xFF, 63, 0, 0, 0, 100, 0, 0, 0,
    ]);
    after[510] = 0x55;
    after[511] = 0xAA;
    ebr.plan.writes[0] = yisi_engine::protocol::SectorWrite {
        offset: 1 << 20,
        before: vec![0u8; 512],
        after: after.clone(),
    };
    assert!(check_table_plan(&ebr, &big_expected).is_ok());
    // 动了引导代码区、没有签名、不止一个扇区：都不行
    for broken in [
        {
            let mut bytes = after.clone();
            bytes[10] = 1;
            bytes
        },
        {
            let mut bytes = after.clone();
            bytes[511] = 0;
            bytes
        },
    ] {
        ebr.plan.writes[0].after = broken;
        assert_eq!(
            check_table_plan(&ebr, &big_expected).unwrap_err().code,
            "planMismatch"
        );
    }
    ebr.plan.writes[0].before = vec![0u8; 1024];
    ebr.plan.writes[0].after = [after.clone(), vec![0u8; 512]].concat();
    assert_eq!(
        check_table_plan(&ebr, &big_expected).unwrap_err().code,
        "planMismatch"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fs_fix_plans_stay_inside_the_partition() {
    let (_, dir, image) = setup("fsfix");
    let real = std::fs::canonicalize(&image).unwrap();
    let expected = Writable {
        identity: image_identity(&real),
        size: std::fs::metadata(&real).map_or(0, |meta| meta.len()),
        sector_size: IMAGE_SECTOR_SIZE,
        model: String::new(),
        serial: String::new(),
    };
    let partition = PartitionRef {
        number: 1,
        offset: 1024,
        length: 4096,
    };
    let planned = || FsFixPlan {
        plan: plan_for(&image),
        partition: 1,
        partition_offset: 1024,
        partition_length: 4096,
        filesystem: "fat16".to_owned(),
        counts: Vec::new(),
    };
    assert!(check_fs_fix_plan(&planned(), &expected, partition).is_ok());
    // 引擎回传的分区位置不算数：按宿主请求的位置核对
    let mut before = planned();
    before.partition_offset = 0;
    before.plan.writes[0].offset = 512;
    let mut after = planned();
    after.plan.writes.push(SectorWrite {
        offset: 1024 + 4096,
        before: vec![0; 512],
        after: vec![1; 512],
    });
    let mut empty = planned();
    empty.plan.writes.clear();
    for (name, bad) in [("before", before), ("after", after), ("empty", empty)] {
        assert_eq!(
            check_fs_fix_plan(&bad, &expected, partition)
                .unwrap_err()
                .code,
            "planMismatch",
            "{name}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn images_made_from_4k_disks_open_with_the_recorded_sector_size() {
    let (_plans, dir, image) = setup("sector");
    let target = image_target(&image);
    // 旁边没有坏区图：按 512
    assert_eq!(device_of(&target).unwrap().sector_size, IMAGE_SECTOR_SIZE);
    // 坏区图记着 4096：按 4096（读源与写入计划同一个函数）
    let map = yisi_io::map_path(&image);
    std::fs::write(
        &map,
        "version 2\nsize 8192\nsector 4096\nsource fake\n0 8192 done\n",
    )
    .unwrap();
    assert_eq!(device_of(&target).unwrap().sector_size, 4096);
    assert_eq!(crate::engines::image_sector_size(&image), 4096);
    // 记的不合法：按 512
    std::fs::write(&map, "version 2\nsector 1000\n").unwrap();
    assert_eq!(crate::engines::image_sector_size(&image), IMAGE_SECTOR_SIZE);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_follows_the_device_not_the_path() {
    let (plans, dir, image) = setup("history");
    let view = plans.propose(plan_for(&image), summary(&image)).unwrap();
    // 记下了写到哪个镜像：规范路径、长度、扇区大小
    let device = view.summary.device.clone().unwrap();
    assert_eq!((device.size, device.sector_size), (8192, IMAGE_SECTOR_SIZE));
    let record = plans
        .execute(&BrokerSession::default(), &view.plan_id)
        .unwrap();
    let history = plans.history(&image_target(&image));
    assert_eq!(history.records, vec![record]);
    assert!(!history.indistinct);

    // 同一个路径上换成了另一个（长度不同的）文件：以前的记录不算它的，也不能拿它回滚
    std::fs::write(&image, vec![0u8; 16384]).unwrap();
    assert!(plans.history(&image_target(&image)).records.is_empty());
    assert_eq!(
        plans
            .rollback(&view.plan_id, &image_target(&image))
            .unwrap_err()
            .code,
        "rollbackOtherDevice"
    );
    // 另一个路径上的镜像也不算
    let other = dir.join("other.img");
    std::fs::write(&other, vec![0u8; 8192]).unwrap();
    assert!(plans.history(&image_target(&other)).records.is_empty());
    // 文件不在了：没有记录
    assert!(
        plans
            .history(&image_target(&dir.join("gone.img")))
            .records
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
