use yisi_broker::execute::Step;
use yisi_engine::protocol::{DeviceIdentity, SectorWrite};

use super::*;

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
        rollback_of: None,
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

    let rollback = plans.rollback(&view.plan_id).unwrap();
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
        plans.rollback(&view.plan_id).unwrap_err().code,
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
        plans.rollback(&view.plan_id).unwrap_err().code,
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
            sector_size: IMAGE_SECTOR_SIZE,
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
    };
    let expected = Writable {
        identity: image_identity(&real),
        sector_size: IMAGE_SECTOR_SIZE,
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
    let _ = std::fs::remove_dir_all(&dir);
}
