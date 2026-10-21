//! 代理侧：连上宿主建的管道、核对宿主身份、处理请求。

use std::fs::{File, OpenOptions};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::{io, ptr, thread};

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, INFINITE, OpenProcess, PROCESS_DUP_HANDLE, PROCESS_SYNCHRONIZE,
    WaitForSingleObject,
};
use yisi_broker::protocol::{DiskOpened, FailureData, OPEN_DISK, OpenDiskParams, SHUTDOWN};
use yisi_io::{BlockDevice, IoError, WindowsDisk};
use yisi_rpc::{
    ErrorObject, FrameError, Message, Response, error_code, read_message, write_message,
};

pub fn run(pipe_name: &str, parent_pid: u32) -> anyhow::Result<()> {
    exit_with_parent(parent_pid).context("无法监视宿主进程")?;
    let mut pipe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_name)
        .with_context(|| format!("无法连接管道 {pipe_name}"))?;
    let server_pid = pipe_server_pid(&pipe).context("无法查询管道的服务端进程")?;
    ensure!(
        server_pid == parent_pid,
        "管道的服务端进程 {server_pid} 不是宿主 {parent_pid}"
    );
    tracing::info!(parent_pid, "已连上宿主");

    loop {
        let message = match read_message(&mut pipe) {
            Ok(message) => message,
            Err(FrameError::Closed) => return Ok(()),
            Err(error) => return Err(error).context("读取请求失败"),
        };
        // 代理只处理请求；宿主不会发通知，也不该发响应
        let Message::Request(request) = message else {
            continue;
        };
        let stop = request.method == SHUTDOWN;
        let outcome = match request.method.as_str() {
            OPEN_DISK => request
                .params::<OpenDiskParams>()
                .and_then(|params| open_disk(&params.path, params.target_pid)),
            SHUTDOWN => Ok(json!({})),
            other => Err(ErrorObject::new(
                error_code::METHOD_NOT_FOUND,
                format!("磁盘代理没有方法 {other}"),
            )),
        };
        let response = Response {
            id: request.id,
            outcome,
        };
        write_message(&mut pipe, &Message::Response(response))?;
        if stop {
            return Ok(());
        }
    }
}

fn open_disk(path: &str, target_pid: u32) -> Result<Value, ErrorObject> {
    if !is_physical_drive_path(path) {
        return Err(failed(
            format!("只接受 \\\\.\\PhysicalDriveN 形式的路径，收到 {path}"),
            None,
        ));
    }
    let disk =
        WindowsDisk::open(path).map_err(|error| failed(error_chain(&error), os_error(&error)))?;
    let handle = duplicate_into(disk.as_handle(), target_pid).map_err(|error| {
        failed(
            format!("无法把句柄交给进程 {target_pid}：{error}"),
            error.raw_os_error(),
        )
    })?;
    let opened = DiskOpened {
        handle,
        size: disk.size(),
        sector_size: disk.sector_size(),
    };
    serde_json::to_value(opened).map_err(|error| {
        ErrorObject::new(error_code::INTERNAL_ERROR, format!("无法编码答复：{error}"))
    })
}

/// 管理员权限下任何文件都能打开，所以只接受物理磁盘路径，不替宿主打开任意文件。
fn is_physical_drive_path(path: &str) -> bool {
    path.strip_prefix(r"\\.\PhysicalDrive")
        .is_some_and(|number| {
            (1..=3).contains(&number.len()) && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// 把句柄复制进目标进程，返回它在目标进程里的值。复制出的句柄与源句柄权限相同（只读）。
fn duplicate_into(source: BorrowedHandle<'_>, target_pid: u32) -> io::Result<u64> {
    // SAFETY: 只申请复制句柄所需的权限；返回值在下面检查后由 OwnedHandle 接管。
    let raw = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, target_pid) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` 是刚打开、只归这里所有的有效进程句柄。
    let target = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut duplicated: HANDLE = ptr::null_mut();
    // SAFETY: 源句柄与两个进程句柄在调用期间都有效；`duplicated` 是有效的输出位置。
    // 复制出的句柄属于目标进程，本进程不使用也不关闭它。
    let succeeded = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source.as_raw_handle(),
            target.as_raw_handle(),
            &mut duplicated,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(duplicated.addr() as u64)
}

/// 宿主退出时代理随之退出，不留下以管理员身份运行的孤儿进程。
fn exit_with_parent(parent_pid: u32) -> io::Result<()> {
    // SAFETY: 只申请等待进程结束所需的权限；返回值在下面检查后由 OwnedHandle 接管。
    let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, parent_pid) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` 是刚打开、只归这里所有的有效进程句柄。
    let parent = unsafe { OwnedHandle::from_raw_handle(raw) };
    thread::spawn(move || {
        // SAFETY: `parent` 归这个线程所有，等待期间一直有效。
        unsafe { WaitForSingleObject(parent.as_raw_handle(), INFINITE) };
        tracing::info!("宿主已退出，代理随之退出");
        std::process::exit(0);
    });
    Ok(())
}

fn pipe_server_pid(pipe: &File) -> io::Result<u32> {
    let mut pid = 0u32;
    // SAFETY: 管道句柄有效；`pid` 是有效的输出位置。
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

fn failed(message: String, os_error: Option<i32>) -> ErrorObject {
    tracing::warn!(%message, ?os_error, "请求失败");
    let data = serde_json::to_value(FailureData { os_error }).unwrap_or(Value::Null);
    ErrorObject::new(error_code::SERVER_ERROR, message).with_data(data)
}

fn os_error(error: &IoError) -> Option<i32> {
    match error {
        IoError::Open { source, .. }
        | IoError::Query { source, .. }
        | IoError::Read { source, .. }
        | IoError::Locate { source, .. }
        | IoError::Adopt { source, .. } => source.raw_os_error(),
        IoError::UnexpectedEof { .. } | IoError::InvalidSectorSize(_) => None,
    }
}

/// 把错误及其来源串成一句话，答复里带上系统给出的原因。
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push('：');
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_physical_drive_paths_are_accepted() {
        for path in [
            r"\\.\PhysicalDrive0",
            r"\\.\PhysicalDrive12",
            r"\\.\PhysicalDrive127",
        ] {
            assert!(is_physical_drive_path(path), "{path}");
        }
        for path in [
            r"\\.\PhysicalDrive",
            r"\\.\PhysicalDrive1234",
            r"\\.\PhysicalDrive1\x",
            r"\\.\PhysicalDrive-1",
            r"\\.\C:",
            r"C:\Windows\System32\config\SAM",
            r"\\?\PhysicalDrive0",
            "",
        ] {
            assert!(!is_physical_drive_path(path), "{path}");
        }
    }
}
