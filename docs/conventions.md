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
| 禁止 `unwrap`、`expect`、直接下标访问（clippy `unwrap_used`、`expect_used`、`indexing_slicing`） | 解析磁盘数据的 crate：`part`、`fs`、`formats`、`carve`、`repair` |
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

- TypeScript `strict`；组件一律 `<script setup lang="ts">` 加组合式 API。
- 与宿主的通信集中在一个服务层，组件不直接调用 Tauri 命令。
- 用户可见文案全部走国际化资源，不在组件里硬编码。
- 恢复出的文件内容不得用 `v-html` 或 `<iframe>` 渲染。

## 4. 测试

- 每个格式插件带样本测试；每个解析器带 fuzz 目标。
- 测试镜像由脚本生成，生成过程可以重放；大文件不入库。
- **测试数据不得包含真实用户数据。**
- 写盘相关测试只在镜像副本上进行，不碰真实磁盘。

## 5. 提交信息

```
<类型>: <摘要>

<正文：改了什么、为什么>
```

类型：`feat`、`fix`、`refactor`、`perf`、`test`、`docs`、`build`、`ci`、`chore`。
