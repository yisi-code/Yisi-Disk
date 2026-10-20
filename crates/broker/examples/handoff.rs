//! 句柄传递演示：以普通权限运行，提权启动磁盘代理，让代理把只读磁盘句柄复制进本进程，再用它读盘。
//!
//! 用法：`handoff <yisi-broker.exe 的路径> <设备路径，如 \\.\PhysicalDrive0>`
//!
//! 只读；会弹出一次 UAC。

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        windows_demo::run()
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("只支持 Windows")
    }
}

#[cfg(windows)]
mod windows_demo {
    use std::path::PathBuf;
    use std::time::Duration;
    use std::{mem, ptr};

    use anyhow::{Context, bail, ensure};
    use windows_sys::Wdk::Foundation::{NtQueryObject, ObjectBasicInformation};
    use windows_sys::Win32::System::WindowsProgramming::PUBLIC_OBJECT_BASIC_INFORMATION;
    use yisi_broker::launcher::BrokerConnection;
    use yisi_broker::protocol::{Request, Response};
    use yisi_io::{BlockDevice, WindowsDisk};

    /// 任何一项都意味着句柄可以改动磁盘或它的安全设置：
    /// FILE_WRITE_DATA、FILE_APPEND_DATA、FILE_WRITE_EA、FILE_WRITE_ATTRIBUTES、DELETE、WRITE_DAC、WRITE_OWNER、
    /// GENERIC_ALL、GENERIC_WRITE。
    const WRITE_RIGHTS: u32 = 0x0002
        | 0x0004
        | 0x0010
        | 0x0100
        | 0x0001_0000
        | 0x0004_0000
        | 0x0008_0000
        | 0x1000_0000
        | 0x4000_0000;

    pub fn run() -> anyhow::Result<()> {
        let mut args = std::env::args().skip(1);
        let broker = PathBuf::from(args.next().context("缺少参数：yisi-broker.exe 的路径")?);
        let device = args.next().context("缺少参数：设备路径")?;

        println!("本进程 pid={}", std::process::id());
        let mut connection = BrokerConnection::launch(&broker, Duration::from_secs(120))?;
        println!("代理已连上并核对身份：pid={}", connection.broker_pid());

        let response = connection.request(&Request::OpenDisk {
            path: device.clone(),
            target_pid: std::process::id(),
        })?;
        let Response::DiskOpened {
            handle,
            size,
            sector_size,
        } = response
        else {
            bail!("代理没有打开磁盘：{response:?}");
        };
        println!("收到句柄 {handle:#x}：size={size} sector_size={sector_size}");

        let access = granted_access(handle)?;
        println!(
            "句柄已授予的权限 {access:#010x}，其中写类权限 {:#x}",
            access & WRITE_RIGHTS
        );
        ensure!(access & WRITE_RIGHTS == 0, "句柄带有写类权限");

        let disk = WindowsDisk::adopt_handle(handle)?;
        ensure!(
            disk.size() == size && disk.sector_size() == sector_size,
            "认领后查到的容量或扇区大小与代理报告的不一致"
        );
        let mut head = vec![0u8; 64 * 1024];
        disk.read_exact_at(0, &mut head)?;
        println!(
            "用认领的句柄读开头 64 KiB 成功；第一个扇区末尾两字节 {:02x} {:02x}",
            head[510], head[511]
        );

        ensure!(
            WindowsDisk::adopt_handle(handle).is_err(),
            "同一个句柄被认领了两次"
        );
        println!("重复认领同一个句柄被拒绝");

        let rejected = connection.request(&Request::OpenDisk {
            path: r"C:\Windows\System32\config\SAM".to_owned(),
            target_pid: std::process::id(),
        })?;
        ensure!(
            matches!(rejected, Response::Failed { .. }),
            "代理替宿主打开了普通文件：{rejected:?}"
        );
        println!("非物理磁盘路径被代理拒绝：{rejected:?}");

        let reply = connection.request(&Request::Shutdown)?;
        println!("结束代理：{reply:?}");
        Ok(())
    }

    /// 用 NtQueryObject 查句柄实际被授予的权限。
    fn granted_access(handle: u64) -> anyhow::Result<u32> {
        let object = ptr::without_provenance_mut(usize::try_from(handle)?);
        let mut info = PUBLIC_OBJECT_BASIC_INFORMATION {
            Attributes: 0,
            GrantedAccess: 0,
            HandleCount: 0,
            PointerCount: 0,
            Reserved: [0; 10],
        };
        // SAFETY: `object` 是代理刚复制进本进程、尚未被认领或关闭的句柄；`info` 是大小正确的输出缓冲区，
        // 调用同步完成，返回后不再访问它的指针。
        let status = unsafe {
            NtQueryObject(
                object,
                ObjectBasicInformation,
                (&raw mut info).cast(),
                mem::size_of::<PUBLIC_OBJECT_BASIC_INFORMATION>() as u32,
                ptr::null_mut(),
            )
        };
        ensure!(status >= 0, "NtQueryObject 失败：NTSTATUS {status:#x}");
        Ok(info.GrantedAccess)
    }
}
