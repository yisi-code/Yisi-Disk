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

目录会随开发逐步建立。

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
