# Yisi-Disk

带图形界面的磁盘数据恢复工具：找回删除的文件、按文件类型深度恢复、找回丢失的分区、制作能识别坏区的磁盘镜像。扫描、浏览和制作镜像都只读源盘，找回的文件和镜像一律保存到另一块盘上。

**下载最新版：<https://github.com/yisi-code/Yisi-Disk/releases/latest>**

## 能做什么（v0.1.0）

| 功能 | 说明 |
|---|---|
| 找回删除的文件 | 浏览 FAT12/16/32、exFAT、NTFS 分区的目录，列出已删除的文件，判断能不能完整找回，另存到别的盘 |
| 按文件类型深度恢复 | 不依赖文件系统，按文件结构在整块盘或镜像里找 JPEG、PNG、PDF、ZIP、MP4 等文件；可以先扫描、预览，再挑着保存 |
| 找回丢失的分区 | 从头到尾读一遍盘，找出分区表里已经没有的 FAT、exFAT、NTFS 分区，浏览并另存里面的文件 |
| 修复打不开的分区 | 诊断各分区的引导扇区与备份；主引导扇区坏了、备份完好时按备份打开，把文件另存出来（这一版只诊断，不写盘） |
| 检查文件系统 | 只读检查 FAT、exFAT 的引导区、FAT 链、分配位图与目录项是否一致，列出问题与以后的修法 |
| 坏道检测 | 从头到尾读一遍盘，列出读不出的区段 |
| 制作磁盘镜像 | 先读完完好区域、最后才回头细读坏区，坏区位置记在坏区图里；可以随时停止、之后接着做，可以按分区多选、分几次做 |

读盘时如果碰上损坏的区域（读不出或读得很吃力），界面会提示先制作镜像，再在镜像上操作。

## 下载与运行

1. 打开 [最新版本](https://github.com/yisi-code/Yisi-Disk/releases/latest)，下载 `Yisi-Disk-<版本>-windows-x64.zip`。
2. 解压到任意文件夹（不要放在要恢复数据的那块盘上），双击 `Yisi-Disk.exe` 运行。
3. 文件夹里的 `yisi-engine.exe`、`yisi-broker.exe` 由主程序按需启动，三个程序要放在一起。

运行要求：

- Windows 10 或 11（64 位），需要 WebView2 运行时（Windows 11 自带；Windows 10 一般已随 Edge 装好，没有时从微软官网安装）。
- 读取物理磁盘时会弹出管理员权限请求（UAC），本次运行期间只请求一次；只处理镜像文件时不需要。

使用建议：

- 找回的文件、做出的镜像要保存到另一块盘上，程序会拦住保存到源盘的操作。
- 盘有坏道或读起来很慢时，先用「制作磁盘镜像」把盘复制出来，再在镜像上找回文件。

## 从源码构建

### 开发环境

| 需要 | 说明 |
|---|---|
| Rust | stable，`rustup` 安装 |
| Node.js 与 pnpm | Node.js 22、pnpm 10 |
| Visual Studio 生成工具 | 「使用 C++ 的桌面开发」组件与 Windows SDK |
| WebView2 | Windows 11 自带 |

### 开发模式

在 `app/ui` 目录下：

```bash
pnpm install
pnpm tauri dev
```

- 第一次运行要编译全部 Rust 依赖，需要几分钟；之后是增量编译。
- `pnpm tauri dev` 先编出引擎（`yisi-engine`）和磁盘代理（`yisi-broker`），再启动界面的开发服务器（`http://127.0.0.1:1420`）并打开窗口。改界面代码会自动刷新；改了引擎或代理，要退出后重新运行。

### 构建可执行程序

在 `app/ui` 目录下：

```bash
pnpm tauri build --no-bundle
```

产物在仓库根目录的 `target/release/` 下：`yisi-app.exe` 是主程序（发布时改名为 `Yisi-Disk.exe`），`yisi-engine.exe`、`yisi-broker.exe` 要和它放在同一目录。

### 检查与测试

在仓库根目录：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

在 `app/ui` 目录下：

```bash
pnpm typecheck
```

## 目录结构

```
Yisi-Disk/
├─ crates/          Rust 引擎各模块（块设备、分区表、文件系统、格式插件、深度恢复、修复、进程间通信）
├─ app/
│  ├─ src-tauri/    桌面宿主（Tauri 2）
│  └─ ui/           界面（Vue 3 + TypeScript）
└─ docs/            设计文档
```

- [架构设计](docs/architecture.md)
- [工程约定](docs/conventions.md)

## 技术栈

| 部分 | 技术 |
|---|---|
| 恢复引擎、磁盘代理 | Rust |
| 桌面宿主 | Tauri 2 |
| 界面 | Vue 3 + TypeScript |

## 许可证

源码公开，采用 [PolyForm Noncommercial License 1.0.0](LICENSE.md)：可以个人使用、学习与研究，**禁止商用**。

需要技术指导或商用授权，请通过下面的联系方式联系作者。

## 联系与反馈

技术持续更新中，有问题请在 [GitHub Issues](https://github.com/yisi-code/Yisi-Disk/issues) 提出，或通过下面的方式联系：

- 开发者：倚肆
- 微信：m1751250104
- QQ：1751250104
- 邮箱：xiejing0520@petalmail.com
