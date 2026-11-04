//! 代理执行物理盘的写入计划（`broker.executeWrite`，docs/architecture.md 4.2）。
//!
//! 只在代理程序里：宿主、引擎都没有改写物理盘的代码。顺序是：读回宿主存好的备份（执行的就是备份里的计划）→
//! 拒绝 Windows 所在的盘 → 核对这个编号现在是不是计划里那块盘 → 锁住并卸下与写入范围重叠的卷 → 执行
//! （核对写前内容、写、刷盘、读回，`execute::execute`）→ 通知系统重读分区表 → 关闭磁盘、解锁卷。
//! 写之前就拒绝时答复错误，`error.data.reason` 见 [`refusal`]；开始执行之后的结果都在答复里。

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

use serde_json::Value;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, IOCTL_DISK_UPDATE_PROPERTIES,
};
use yisi_broker::backup::SavedBackup;
use yisi_broker::execute::{self, WriteTarget};
use yisi_broker::protocol::{ExecuteWriteParams, FailureData, refusal};
use yisi_io::{
    BlockDevice, DeviceIdentity, WindowsDisk, WritePlan, device_io_control, disk_identity,
    physical_drive_path, system_disks, volumes_on_disk,
};
use yisi_rpc::{ErrorObject, error_chain, error_code};

pub fn execute_write(params: &ExecuteWriteParams) -> Result<Value, ErrorObject> {
    let path = physical_drive_path(params.disk_number).ok_or_else(|| {
        refused(
            refusal::INVALID_DISK,
            format!("磁盘编号 {} 不合法", params.disk_number),
        )
    })?;
    let backup = SavedBackup::load(Path::new(&params.backup_path))
        .map_err(|error| refused(refusal::BACKUP_UNREADABLE, error_chain(&error)))?;
    match system_disks() {
        Ok(disks) if !disks.contains(&params.disk_number) => {}
        Ok(_) => {
            return Err(refused(
                refusal::SYSTEM_DISK,
                format!("磁盘 {} 上有 Windows 所在的卷", params.disk_number),
            ));
        }
        Err(error) => return Err(refused(refusal::SYSTEM_DISK, error_chain(&error))),
    }

    let disk = WindowsDisk::open(&path)
        .map_err(|error| refused(refusal::DISK_UNAVAILABLE, error_chain(&error)))?;
    let identity = DeviceIdentity {
        identity: disk_identity(&disk),
        size: disk.size(),
        sector_size: disk.sector_size(),
    };
    if identity != backup.plan().device {
        return Err(refused(
            refusal::WRONG_DISK,
            format!(
                "磁盘 {} 现在是 {identity:?}，计划写的是 {:?}",
                params.disk_number,
                backup.plan().device
            ),
        ));
    }
    drop(disk);

    let locked = lock_volumes(params.disk_number, backup.plan())?;
    let mut target = DiskTarget::open(&path, identity).map_err(|error| {
        refused(
            refusal::DISK_UNAVAILABLE,
            format!("无法以读写方式打开 {path}：{error}"),
        )
    })?;
    let executed = execute::execute_reported(&mut target, &backup);
    if executed.written > 0 {
        // 写过分区表或引导扇区后让系统重新读一遍分区布局；失败不影响已经写好的内容
        if let Err(error) =
            device_io_control(&target.file, IOCTL_DISK_UPDATE_PROPERTIES, &[], &mut [])
        {
            tracing::warn!(%error, "通知系统重读分区表失败");
        }
    }
    drop(target);
    drop(locked);
    serde_json::to_value(executed).map_err(|error| {
        ErrorObject::new(error_code::INTERNAL_ERROR, format!("无法编码答复：{error}"))
    })
}

/// 锁住并卸下的卷。丢掉时关闭句柄，卷随之解锁，系统下次访问时重新挂载（文件系统按新的引导扇区识别）。
struct LockedVolume {
    _file: File,
}

/// 锁住并卸下这块盘上与写入范围重叠的卷。Windows 不许直接写已挂载的卷所在的扇区；锁不上说明有程序正开着卷里的文件，
/// 拒绝（`volumeInUse`），不强行卸下。没有重叠的卷不动。
fn lock_volumes(disk: u32, plan: &WritePlan) -> Result<Vec<LockedVolume>, ErrorObject> {
    let volumes = volumes_on_disk(disk)
        .map_err(|error| refused(refusal::DISK_UNAVAILABLE, error_chain(&error)))?;
    let mut locked = Vec::new();
    for volume in volumes {
        let touched = plan
            .writes
            .iter()
            .any(|write| volume.overlaps(write.offset, write.len()));
        if !touched {
            continue;
        }
        let in_use = |error: io::Error| {
            refused(
                refusal::VOLUME_IN_USE,
                format!("无法锁住卷 {}：{error}", volume.path),
            )
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&volume.path)
            .map_err(in_use)?;
        device_io_control(&file, FSCTL_LOCK_VOLUME, &[], &mut []).map_err(in_use)?;
        device_io_control(&file, FSCTL_DISMOUNT_VOLUME, &[], &mut []).map_err(in_use)?;
        tracing::info!(volume = %volume.path, "已锁住并卸下卷");
        locked.push(LockedVolume { _file: file });
    }
    Ok(locked)
}

/// 以读写方式打开的物理盘。偏移与长度都按扇区对齐（计划核对过），直写，刷盘等落到盘上。
struct DiskTarget {
    file: File,
    identity: DeviceIdentity,
}

impl DiskTarget {
    fn open(path: &str, identity: DeviceIdentity) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_WRITE_THROUGH)
            .open(path)?;
        Ok(Self { file, identity })
    }
}

impl WriteTarget for DiskTarget {
    fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut filled = 0;
        while filled < buf.len() {
            let at = offset + filled as u64;
            let read = self.file.seek_read(&mut buf[filled..], at)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "读到了磁盘末尾",
                ));
            }
            filled += read;
        }
        Ok(())
    }

    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < data.len() {
            let at = offset + written as u64;
            let count = self.file.seek_write(&data[written..], at)?;
            if count == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "磁盘没有接受写入"));
            }
            written += count;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
}

/// 写之前就拒绝：错误里带上原因代码，宿主据此给出界面文案。
fn refused(reason: &str, message: String) -> ErrorObject {
    tracing::warn!(reason, %message, "拒绝执行写入计划");
    let data = serde_json::to_value(FailureData {
        os_error: None,
        reason: Some(reason.to_owned()),
    })
    .unwrap_or(Value::Null);
    ErrorObject::new(error_code::SERVER_ERROR, message).with_data(data)
}
