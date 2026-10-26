# 工程约定

## 1. 文件

- 文本文件一律 UTF-8 无 BOM、LF 换行，由 `.gitattributes` 统一；只在 Windows 上执行的 `.bat`、`.cmd`、`.ps1` 用 CRLF。
- 标识符用英文；注释、文档、提交信息用中文。
- 时间格式 `yyyy-MM-dd HH:mm:ss`。

## 2. Rust

### 2.1 工程

- edition 2024，工具链跟随 stable。
- workspace 统一管理依赖版本和 lint，各 crate 继承。
- 包名统一加 `yisi-` 前缀，目录名不加（`crates/io` → `yisi-io`）。
- 库 crate 用 `thiserror` 定义错误类型；只有进程入口（`engine`、`broker`、宿主）用 `anyhow`。
- 日志用 `tracing`。

### 2.2 安全与鲁棒

| 规则 | 适用范围 |
|---|---|
| `#![forbid(unsafe_code)]` | 除 `io`、`broker` 外的全部 crate |
| 每处 `unsafe` 写 `// SAFETY:` 说明为什么成立 | `io`、`broker` |
| 禁止 `unwrap`、`expect`、直接下标访问（clippy `unwrap_used`、`expect_used`、`indexing_slicing`） | 解析磁盘数据的 crate：`part`、`fs`、`formats`、`carve`、`repair`；测试代码除外（`clippy.toml`） |
| 来自磁盘的数值做运算时一律用 `checked_*` / `saturating_*` | 同上 |
| release 构建也开启 `overflow-checks`，作为兜底 | 同上 |
| 引擎侧的块设备类型不提供写方法；写盘只能通过写入计划 | 全部 |

### 2.3 格式与检查

提交前必须通过：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 3. 界面（Vue 3 + TypeScript）

- 界面只运行在 Tauri 桌面窗口中，不考虑浏览器与移动端；窗口最小 960×640。开发服务器只绑定 `127.0.0.1`。
- TypeScript `strict`；组件一律 `<script setup lang="ts">` 加组合式 API。
- 与宿主的通信集中在一个服务层，组件不直接调用 Tauri 命令。宿主返回的错误是 `{code, detail}`，界面按 `code` 取 `errors.*` 文案显示，不直接展示宿主给出的句子。
- 用户可见文案全部走国际化资源，不在组件里硬编码。被多处使用的组件，文案放在它自己的命名空间下（例如磁盘列表用 `disk.*`），不挂在某个使用方名下。
- 任务（读取扫描、深度恢复等）的发起、取消、进度、无响应与终结统一用 `composables/useJob`，任务 ID 在调用宿主之前生成；各类任务的组合函数只处理自己的结果。
- 恢复出的文件内容不得用 `v-html` 或 `<iframe>` 渲染。

## 4. 测试

- 每个格式插件带样本测试；每个解析器带 fuzz 目标。样本由代码按格式结构生成（`yisi-formats` 的 `test-samples` 特性），并按 1–40、64、100、511、512、4096 字节等各种块大小送入，结果必须一致。接入 `cargo fuzz`（需要 nightly 工具链）之前，以固定种子的随机与变异输入测试代替。
- 测试镜像由脚本生成，生成过程可以重放；大文件不入库。
- **测试数据不得包含真实用户数据。**
- 写盘相关测试只在镜像副本上进行，不碰真实磁盘。
- 读真实磁盘的测试标 `#[ignore]`，只读，设备路径由环境变量给出（如 `YISI_IO_TEST_DEVICE`），在管理员终端手动运行：`cargo test -p yisi-io --test windows_disk -- --ignored --nocapture`。在 PowerShell 里设置环境变量；Git Bash 会吞掉 `\\.\` 开头的一个反斜杠。不对报过读错误的盘运行。
- 文件系统对照镜像：`crates/fs/tests/make-test-images.ps1` 用 diskpart 建固定大小的 VHD（就是 raw 镜像末尾多 512 字节），由 Windows 自己分区、格式化（FAT12/16/32、exFAT，MBR 与 GPT）、写入已知内容并删掉一部分，产出镜像和清单。需要管理员权限；Windows PowerShell 5.1 按系统代码页读不带 BOM 的脚本，所以按 UTF-8 读进来再执行：`& ([ScriptBlock]::Create((Get-Content -Raw -Encoding UTF8 <脚本>))) -OutDir <目录>`。之后 `$env:YISI_FS_TEST_IMAGES = '<目录>'; cargo test -p yisi-fs --test real_images -- --ignored --nocapture` 逐个核对。读真实磁盘的分区表与 FAT 根目录：`$env:YISI_FS_TEST_DEVICE = '\\.\PhysicalDrive0'; cargo test -p yisi-fs --test real_disk -- --ignored --nocapture`（管理员终端）。
- 句柄传递演示（会弹一次 UAC）：`cargo build -p yisi-broker --bins --examples` 之后，在**未提权**的 PowerShell 里运行 `target\debug\examples\handoff.exe target\debug\yisi-broker.exe \\.\PhysicalDrive0`，应看到授予的权限不含写类权限、读盘成功、重复认领与非磁盘路径都被拒绝。

## 5. 提交信息

```
<类型>: <摘要>

<正文：改了什么、为什么>
```

类型：`feat`、`fix`、`refactor`、`perf`、`test`、`docs`、`build`、`ci`、`chore`。
