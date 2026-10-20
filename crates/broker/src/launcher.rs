//! 宿主侧：建管道、提权启动代理、等代理连上并核对身份。
//!
//! 提权进程只能通过 `ShellExecuteEx("runas")` 启动，父进程接管不了它的 stdio，
//! 所以 Windows 上宿主与代理改走命名管道。

use std::ffi::OsStr;
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};
use std::{io, iter, mem, ptr, thread};

use windows_sys::Win32::Foundation::{
    ERROR_CANCELLED, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE, LocalFree, WAIT_FAILED,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessId, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    SHELLEXECUTEINFOW_0, ShellExecuteExW,
};

use serde_json::{Value, json};
use yisi_rpc::{ErrorObject, FrameError, Id, Message, Request, read_message, write_message};

use crate::protocol::{DiskOpened, OPEN_DISK, OpenDiskParams, SHUTDOWN};

/// 管道只允许管理员组和 SYSTEM 连接：提权后的代理属于管理员组；
/// 未提权的进程即使与宿主是同一个用户，管理员组在它的令牌里也只用于拒绝，连不上。
const PIPE_SDDL: &str = "D:P(A;;GA;;;BA)(A;;GA;;;SY)";
const PIPE_BUFFER_SIZE: u32 = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// `ShowWindow` 的 `SW_HIDE`：代理是控制台程序，不显示窗口。
const SW_HIDE: i32 = 0;

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("用户取消了提权")]
    Cancelled,

    #[error("无法启动磁盘代理")]
    Start(#[source] io::Error),

    #[error("管道操作失败")]
    Pipe(#[source] io::Error),

    #[error("磁盘代理在连接前退出，退出码 {0}")]
    BrokerExited(u32),

    #[error("等待磁盘代理连接超时")]
    Timeout,

    #[error("连上管道的进程 {actual} 不是刚启动的磁盘代理 {expected}")]
    UnexpectedClient { expected: u32, actual: u32 },
}

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("与磁盘代理通信失败")]
    Frame(#[from] FrameError),

    #[error("磁盘代理拒绝了请求：{0}")]
    Remote(ErrorObject),

    #[error("磁盘代理的答复不是预期内容：{0}")]
    Unexpected(String),
}

/// 与一个已提权、已核对身份的磁盘代理之间的连接。
///
/// 连接关闭时代理读到管道结束后自行退出；宿主进程退出时代理也随之退出。
#[derive(Debug)]
pub struct BrokerConnection {
    pipe: File,
    _process: OwnedHandle,
    pid: u32,
    next_id: u64,
}

impl BrokerConnection {
    /// 建管道，弹出 UAC 以管理员身份启动 `broker_exe`，等它在 `timeout` 内连上并核对身份。
    pub fn launch(broker_exe: &Path, timeout: Duration) -> Result<Self, LaunchError> {
        let name = random_pipe_name().map_err(LaunchError::Pipe)?;
        let pipe = create_pipe(&wide(&name)).map_err(LaunchError::Pipe)?;
        let parameters = format!("--pipe {name} --parent-pid {}", std::process::id());
        let process = launch_elevated(broker_exe, &parameters)?;
        // SAFETY: 进程句柄有效，函数只读取进程 ID。
        let pid = unsafe { GetProcessId(process.as_raw_handle()) };
        if pid == 0 {
            return Err(LaunchError::Start(io::Error::last_os_error()));
        }

        wait_for_client(&pipe, &process, timeout)?;
        let client = pipe_client_pid(&pipe).map_err(LaunchError::Pipe)?;
        if client != pid {
            return Err(LaunchError::UnexpectedClient {
                expected: pid,
                actual: client,
            });
        }
        Ok(Self {
            pipe,
            _process: process,
            pid,
            next_id: 0,
        })
    }

    pub fn broker_pid(&self) -> u32 {
        self.pid
    }

    /// 请代理以只读方式打开 `path`，把句柄复制进 `target_pid` 进程。
    pub fn open_disk(&mut self, path: &str, target_pid: u32) -> Result<DiskOpened, CallError> {
        let params = OpenDiskParams {
            path: path.to_owned(),
            target_pid,
        };
        let params = serde_json::to_value(params).map_err(FrameError::Json)?;
        let result = self.call(OPEN_DISK, params)?;
        serde_json::from_value(result).map_err(|error| CallError::Unexpected(error.to_string()))
    }

    /// 请代理结束。
    pub fn shutdown(&mut self) -> Result<(), CallError> {
        self.call(SHUTDOWN, json!({})).map(|_| ())
    }

    /// 发一条请求并等待对应 id 的答复；代理发来的通知忽略。
    fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        self.next_id += 1;
        let id = Id::Number(self.next_id);
        let request = Message::Request(Request::new(id.clone(), method, params));
        write_message(&mut self.pipe, &request)?;
        loop {
            match read_message(&mut self.pipe)? {
                Message::Response(response) if response.id == id => {
                    return response.outcome.map_err(CallError::Remote);
                }
                Message::Notification(_) => {}
                other => return Err(CallError::Unexpected(format!("{other:?}"))),
            }
        }
    }
}

/// 管道名带 128 位随机数，避免与其他程序重名；被抢先占用时 `FILE_FLAG_FIRST_PIPE_INSTANCE` 让创建直接失败。
fn random_pipe_name() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    // SAFETY: 缓冲区指针与长度来自同一个数组；不传算法句柄，使用系统首选的随机数生成器。
    let status = unsafe {
        BCryptGenRandom(
            ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(io::Error::other(format!(
            "生成随机数失败：NTSTATUS {status:#x}"
        )));
    }
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(r"\\.\pipe\yisi-disk-broker-{suffix}"))
}

fn create_pipe(name: &[u16]) -> io::Result<File> {
    let sddl = wide(PIPE_SDDL);
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `sddl` 是以 0 结尾的宽字符串；`descriptor` 是有效的输出位置，成功后由下面的 LocalFree 释放。
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: `name` 以 0 结尾；`attributes` 与它指向的安全描述符在调用期间有效；句柄不可继承。
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            PIPE_BUFFER_SIZE,
            PIPE_BUFFER_SIZE,
            0,
            &attributes,
        )
    };
    let error = io::Error::last_os_error();
    // SAFETY: `descriptor` 由上面的转换函数分配，管道创建之后不再使用。
    unsafe { LocalFree(descriptor) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(error);
    }
    // SAFETY: `handle` 是刚创建、只归这里所有的有效管道句柄。
    Ok(File::from(unsafe { OwnedHandle::from_raw_handle(handle) }))
}

fn launch_elevated(exe: &Path, parameters: &str) -> Result<OwnedHandle, LaunchError> {
    let verb = wide("runas");
    let file = wide(exe.as_os_str());
    let parameters = wide(parameters);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI | SEE_MASK_NOASYNC,
        hwnd: ptr::null_mut(),
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        lpDirectory: ptr::null(),
        nShow: SW_HIDE,
        hInstApp: ptr::null_mut(),
        lpIDList: ptr::null_mut(),
        lpClass: ptr::null(),
        hkeyClass: ptr::null_mut(),
        dwHotKey: 0,
        Anonymous: SHELLEXECUTEINFOW_0 {
            hIcon: ptr::null_mut(),
        },
        hProcess: ptr::null_mut(),
    };
    // SAFETY: `info` 的指针字段要么为空，要么指向调用期间存活、以 0 结尾的宽字符串；
    // SEE_MASK_NOASYNC 保证返回时启动已完成，之后不再访问这些字符串。
    let launched = unsafe { ShellExecuteExW(&mut info) };
    if launched == 0 {
        let error = io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            LaunchError::Cancelled
        } else {
            LaunchError::Start(error)
        });
    }
    if info.hProcess.is_null() {
        return Err(LaunchError::Start(io::Error::other("没有拿到代理进程句柄")));
    }
    // SAFETY: 设置了 SEE_MASK_NOCLOSEPROCESS，`hProcess` 是交给调用方关闭的有效进程句柄。
    Ok(unsafe { OwnedHandle::from_raw_handle(info.hProcess) })
}

/// 等代理连上管道。代理提前退出或超时时，取消阻塞在 `ConnectNamedPipe` 上的线程。
fn wait_for_client(
    pipe: &File,
    broker: &OwnedHandle,
    timeout: Duration,
) -> Result<(), LaunchError> {
    let listener = pipe.try_clone().map_err(LaunchError::Pipe)?;
    let (done_sender, done_receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let connected = connect_pipe(&listener);
        let _ = done_sender.send(());
        connected
    });

    let deadline = Instant::now() + timeout;
    let mut abort = None;
    loop {
        match done_receiver.recv_timeout(POLL_INTERVAL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if abort.is_none() {
            abort = match exit_code(broker) {
                Ok(Some(code)) => Some(LaunchError::BrokerExited(code)),
                Ok(None) if Instant::now() >= deadline => Some(LaunchError::Timeout),
                Ok(None) => None,
                Err(error) => Some(LaunchError::Start(error)),
            };
        }
        if abort.is_some() {
            // 工作线程可能还没进入 ConnectNamedPipe，取消会落空，所以每轮都再取消一次，直到它返回
            // SAFETY: 线程句柄在 `worker` 被 join 之前一直有效；取消只影响该线程正在进行的同步 I/O。
            unsafe { CancelSynchronousIo(worker.as_raw_handle()) };
        }
    }

    let connected = worker
        .join()
        .map_err(|_| LaunchError::Start(io::Error::other("等待连接的线程异常退出")))?;
    match abort {
        Some(error) => Err(error),
        None => connected.map_err(LaunchError::Pipe),
    }
}

fn connect_pipe(pipe: &File) -> io::Result<()> {
    // SAFETY: 管道句柄有效；同步模式，不传 OVERLAPPED。
    if unsafe { ConnectNamedPipe(pipe.as_raw_handle(), ptr::null_mut()) } != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    // 代理在调用 ConnectNamedPipe 之前就已连上，同样算成功
    if error.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) {
        Ok(())
    } else {
        Err(error)
    }
}

fn pipe_client_pid(pipe: &File) -> io::Result<u32> {
    let mut pid = 0u32;
    // SAFETY: 管道句柄有效；`pid` 是有效的输出位置。
    if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// 进程已退出时返回退出码，仍在运行时返回 `None`。
fn exit_code(process: &OwnedHandle) -> io::Result<Option<u32>> {
    // SAFETY: 进程句柄有效；超时为 0，只查询不等待。
    let waited = unsafe { WaitForSingleObject(process.as_raw_handle(), 0) };
    if waited == WAIT_FAILED {
        return Err(io::Error::last_os_error());
    }
    if waited != WAIT_OBJECT_0 {
        return Ok(None);
    }
    let mut code = 0u32;
    // SAFETY: 进程句柄有效；`code` 是有效的输出位置。
    if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(code))
}

/// 转成以 0 结尾的 UTF-16，供 Win32 宽字符接口使用。
fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(iter::once(0)).collect()
}
