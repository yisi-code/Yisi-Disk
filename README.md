# Yisi-Disk

跨平台磁盘数据恢复工具，带图形界面，支持 Windows、macOS、Linux。

## 功能范围

- **分区恢复**：分析分区表，快速或深度搜索丢失的分区，重写分区表。
- **分区修复**：检查并重建引导扇区，修复 FAT 表，修复 NTFS 的 MFT。
- **文件找回**：浏览分区目录，复制文件，反删除。
- **深度恢复**：按文件签名在磁盘或镜像中恢复数百种格式的文件，支持断点续传。
- **文件系统检查与修复**：exFAT、FAT 的完整一致性检查，分阶段显示进度；先列出问题清单，按勾选修复。
- **磁盘镜像**：能识别坏区的镜像，记录坏区图，多轮重试，可以断点续传；在镜像上执行其他操作；支持 raw、分段 raw、E01。
- **安全默认**：默认只读；写盘必须显式进入修复模式，写入前自动备份，可以回滚；磁盘报读错误时先镜像、后修复。

## 技术栈

| 部分 | 技术 |
|---|---|
| 恢复引擎、特权代理 | Rust |
| 桌面宿主 | Tauri 2 |
| 界面 | Vue 3 + TypeScript |

## 目录结构

```
Yisi-Disk/
├─ crates/          Rust 引擎各模块（见 docs/architecture.md 第 5 节）
├─ app/
│  ├─ src-tauri/    Tauri 宿主
│  └─ ui/           Vue 3 界面
├─ tests/           集成测试、样本生成脚本、fuzz 目标
└─ docs/            设计文档
```

`tests/` 随开发建立。

## 文档

- [架构设计](docs/architecture.md)
- [工程约定](docs/conventions.md)

## 开发环境

| 平台 | 需要 |
|---|---|
| 全部 | Rust stable、Node.js 22、pnpm 10 |
| Windows | Visual Studio 生成工具（C++ 桌面开发组件与 Windows SDK）、WebView2（Windows 11 自带） |
| macOS | Xcode 命令行工具 |
| Linux | WebKitGTK 4.1 及 Tauri 所需的系统库 |

## 启动项目

先按上一节装好开发环境。下面的命令都在 `app/ui` 目录下执行。

### 开发模式

```bash
pnpm install
pnpm tauri dev
```

- `pnpm install` 只在第一次、或依赖有变化时需要。
- `pnpm tauri dev` 依次做三件事：编出引擎（`yisi-engine`）和磁盘代理（`yisi-broker`），启动界面的开发服务器（`http://127.0.0.1:1420`），再编译并打开桌面窗口。第一次要编译全部 Rust 依赖，需要几分钟；之后是增量编译。
- 改了界面代码，窗口会自动刷新；改了 Rust 代码，宿主会自动重新编译并重启窗口。引擎和代理只在启动时编一次，改了它们要退出后重新运行 `pnpm tauri dev`。
- 界面整页刷新（按 F5，或热更新做不了时整页重载）会取消正在运行的扫描、浏览等任务，需要重新开始。
- 1420 端口被占用时启动会失败，先关掉占用它的程序。

### 构建可直接运行的程序

```bash
pnpm tauri build --no-bundle
```

产物在仓库根目录的 `target/release/` 下：`yisi-app.exe` 是主程序，旁边的 `yisi-engine.exe`、`yisi-broker.exe` 由它按需启动。三个文件必须放在同一目录。

安装包还不能用：不加 `--no-bundle` 时会生成安装包，但目前打包配置里没有包含引擎和代理两个程序。

### 运行时的权限

- 列出物理磁盘不需要管理员权限。
- 扫描物理磁盘时，程序会启动磁盘代理，此时 Windows 弹出 UAC 请求管理员权限。代理在本次运行期间复用，关闭主程序时随之退出。
- 只操作镜像文件时，不需要管理员权限。

## 常用命令

Rust（在仓库根目录）：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

界面与桌面宿主（在 `app/ui` 下；`pnpm tauri` 会切到 `app/` 再运行 Tauri CLI；`tauri dev` / `tauri build` 会先编出引擎和磁盘代理程序，放在宿主程序旁边）：

```bash
pnpm install
pnpm typecheck
pnpm tauri dev
pnpm tauri build
```
