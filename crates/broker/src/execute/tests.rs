#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use std::path::PathBuf;

use yisi_io::SectorWrite;

use super::*;
use crate::backup::BackupError;

/// 内存里的设备：可以指定从第几次写开始失败、读回时故意给出别的内容。
struct Memory {
    identity: DeviceIdentity,
    data: Vec<u8>,
    fail_write_from: Option<usize>,
    writes: usize,
    corrupt_read_back: bool,
    flushed: bool,
}

impl Memory {
    fn new(size: usize) -> Self {
        Self {
            identity: DeviceIdentity {
                identity: "memory".to_owned(),
                size: size as u64,
                sector_size: 512,
            },
            data: vec![0; size],
            fail_write_from: None,
            writes: 0,
            corrupt_read_back: false,
            flushed: false,
        }
    }
}

impl WriteTarget for Memory {
    fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = offset as usize;
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        if self.corrupt_read_back && self.flushed {
            buf[0] ^= 0xFF;
        }
        Ok(())
    }

    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        if self.fail_write_from.is_some_and(|from| self.writes >= from) {
            return Err(io::Error::other("写入失败"));
        }
        self.writes += 1;
        let start = offset as usize;
        self.data[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushed = true;
        Ok(())
    }
}

fn temp_path(name: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("yisi-broker-execute-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// 两段：512 处 0→1，2048 处 0→2。
fn plan(identity: &DeviceIdentity) -> WritePlan {
    WritePlan {
        device: identity.clone(),
        writes: vec![
            SectorWrite {
                offset: 512,
                before: vec![0; 512],
                after: vec![1; 512],
            },
            SectorWrite {
                offset: 2048,
                before: vec![0; 1024],
                after: vec![2; 1024],
            },
        ],
    }
}

fn saved(name: &str, plan: WritePlan) -> SavedBackup {
    SavedBackup::save(&temp_path(name), plan).unwrap()
}

#[test]
fn writes_flushes_and_reads_back() {
    let mut target = Memory::new(8192);
    let backup = saved("ok", plan(&target.identity));
    execute(&mut target, &backup).unwrap();
    assert!(target.flushed);
    assert!(target.data[512..1024].iter().all(|&byte| byte == 1));
    assert!(target.data[2048..3072].iter().all(|&byte| byte == 2));
    assert!(target.data[1024..2048].iter().all(|&byte| byte == 0));
    let _ = std::fs::remove_file(backup.path());
}

#[test]
fn refuses_another_device_without_touching_it() {
    let mut target = Memory::new(8192);
    let mut other = target.identity.clone();
    other.identity = "another".to_owned();
    let backup = saved("device", plan(&other));
    let failure = execute(&mut target, &backup).unwrap_err();
    assert_eq!(failure.error.step(), Step::Identity);
    assert_eq!(failure.written, 0);
    assert_eq!(target.writes, 0);
    let _ = std::fs::remove_file(backup.path());
}

#[test]
fn refuses_when_any_segment_changed_since_planning() {
    let mut target = Memory::new(8192);
    target.data[3000] = 7;
    let backup = saved("changed", plan(&target.identity));
    let failure = execute(&mut target, &backup).unwrap_err();
    assert!(matches!(
        failure.error,
        ExecuteError::Changed { offset: 2048 }
    ));
    assert_eq!(failure.error.step(), Step::Verify);
    // 第一段对得上也不写：核对全部通过前一段都不动
    assert_eq!(target.writes, 0);
    assert!(target.data[512..1024].iter().all(|&byte| byte == 0));
    let _ = std::fs::remove_file(backup.path());
}

#[test]
fn reports_how_many_segments_were_written_before_a_failure() {
    let mut target = Memory::new(8192);
    target.fail_write_from = Some(1);
    let backup = saved("write", plan(&target.identity));
    let failure = execute(&mut target, &backup).unwrap_err();
    assert_eq!(failure.error.step(), Step::Write);
    assert_eq!(failure.written, 2);

    let mut target = Memory::new(8192);
    target.corrupt_read_back = true;
    let failure = execute(&mut target, &backup).unwrap_err();
    assert!(matches!(
        failure.error,
        ExecuteError::Mismatch { offset: 512 }
    ));
    assert_eq!(failure.written, 2);
    let _ = std::fs::remove_file(backup.path());
}

#[test]
fn rollback_restores_the_original_content() {
    let mut target = Memory::new(8192);
    let original = target.data.clone();
    let backup = saved("rollback-forward", plan(&target.identity));
    execute(&mut target, &backup).unwrap();
    let rollback = saved(
        "rollback-back",
        SavedBackup::load(backup.path()).unwrap().plan().inverse(),
    );
    execute(&mut target, &rollback).unwrap();
    assert_eq!(target.data, original);
    // 已经回滚过，再回滚一次时写前内容对不上
    let failure = execute(&mut target, &rollback).unwrap_err();
    assert_eq!(failure.error.step(), Step::Verify);
    let _ = std::fs::remove_file(backup.path());
    let _ = std::fs::remove_file(rollback.path());
}

#[test]
fn backup_round_trips_and_detects_damage() {
    let identity = Memory::new(8192).identity;
    let path = temp_path("backup");
    let backup = SavedBackup::save(&path, plan(&identity)).unwrap();
    assert_eq!(SavedBackup::load(&path).unwrap().plan(), backup.plan());
    // 不覆盖已有的备份
    assert!(matches!(
        SavedBackup::save(&path, plan(&identity)),
        Err(BackupError::Exists(_))
    ));
    // 改动一个字节：摘要对不上
    let text = std::fs::read_to_string(&path).unwrap();
    let damaged = text.replacen("\"after\":\"0101", "\"after\":\"0102", 1);
    assert_ne!(damaged, text);
    std::fs::write(&path, damaged).unwrap();
    assert!(matches!(
        SavedBackup::load(&path),
        Err(BackupError::Corrupted(_))
    ));
    std::fs::write(&path, "{").unwrap();
    assert!(matches!(
        SavedBackup::load(&path),
        Err(BackupError::Unreadable { .. })
    ));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn image_file_is_written_in_place_and_opened_exclusively() {
    let path = temp_path("image.img");
    std::fs::write(&path, vec![0u8; 8192]).unwrap();
    let mut image = ImageFile::open(&path, 512).unwrap();
    let identity = image.identity().clone();
    assert_eq!(identity.size, 8192);
    assert_eq!(
        identity.identity,
        image_identity(&std::fs::canonicalize(&path).unwrap())
    );
    #[cfg(windows)]
    {
        // 镜像开着时别人打不开，反过来也一样
        assert!(ImageFile::open(&path, 512).is_err());
        assert!(File::open(&path).is_err());
        drop(image);
        let reader = File::open(&path).unwrap();
        assert!(ImageFile::open(&path, 512).is_err());
        drop(reader);
        image = ImageFile::open(&path, 512).unwrap();
    }
    let backup = saved("image-backup", plan(&identity));
    execute(&mut image, &backup).unwrap();
    drop(image);
    let data = std::fs::read(&path).unwrap();
    assert_eq!(data.len(), 8192);
    assert!(data[512..1024].iter().all(|&byte| byte == 1));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(backup.path());
}
