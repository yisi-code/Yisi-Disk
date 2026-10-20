//! 磁盘代理进程入口：以管理员身份运行，打开设备并把只读句柄交给引擎、执行写入计划。
//!
//! 允许 `unsafe`，仅用于平台系统调用，每处都要写 `// SAFETY:` 说明。

#[cfg(windows)]
mod server;

use anyhow::{Context, bail};

/// 命令行参数：`--pipe <宿主建的管道名> --parent-pid <宿主进程 ID>`。
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
struct Args {
    pipe: String,
    parent_pid: u32,
}

fn main() -> anyhow::Result<()> {
    // 日志一律写 stderr，不占用与宿主通信的通道
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "磁盘代理启动");
    let args = parse_args(std::env::args().skip(1))?;
    run(args)
}

#[cfg(windows)]
fn run(args: Args) -> anyhow::Result<()> {
    server::run(&args.pipe, args.parent_pid)
}

#[cfg(not(windows))]
fn run(_args: Args) -> anyhow::Result<()> {
    bail!("当前平台还不支持磁盘代理")
}

fn parse_args(mut args: impl Iterator<Item = String>) -> anyhow::Result<Args> {
    let mut pipe = None;
    let mut parent_pid = None;
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("参数 {flag} 缺少取值"))?;
        match flag.as_str() {
            "--pipe" => pipe = Some(value),
            "--parent-pid" => {
                parent_pid = Some(
                    value
                        .parse()
                        .with_context(|| format!("--parent-pid 不是进程 ID：{value}"))?,
                );
            }
            _ => bail!("未知参数 {flag}"),
        }
    }
    Ok(Args {
        pipe: pipe.context("缺少参数 --pipe")?,
        parent_pid: parent_pid.context("缺少参数 --parent-pid")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> impl Iterator<Item = String> {
        list.iter()
            .map(|item| (*item).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parses_pipe_and_parent_pid_in_any_order() {
        let expected = Args {
            pipe: r"\\.\pipe\x".to_owned(),
            parent_pid: 12,
        };
        assert_eq!(
            parse_args(args(&["--pipe", r"\\.\pipe\x", "--parent-pid", "12"])).unwrap(),
            expected
        );
        assert_eq!(
            parse_args(args(&["--parent-pid", "12", "--pipe", r"\\.\pipe\x"])).unwrap(),
            expected
        );
    }

    #[test]
    fn rejects_missing_or_unknown_arguments() {
        assert!(parse_args(args(&["--pipe", "x"])).is_err());
        assert!(parse_args(args(&["--pipe"])).is_err());
        assert!(parse_args(args(&["--parent-pid", "abc", "--pipe", "x"])).is_err());
        assert!(parse_args(args(&["--pipe", "x", "--parent-pid", "1", "--write", "y"])).is_err());
    }
}
