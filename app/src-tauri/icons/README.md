# 图标

- `icon.svg`：64 px 及以上的图标；`icon-small.svg`：48 px 及以下（标题栏、任务栏）用的简化版，去掉外圈箭头、字母放大。
- 重新生成（在 `app/ui` 下执行，输出先放到临时目录）：
  1. `pnpm tauri icon ../src-tauri/icons/icon.svg -o <临时目录>/main`
  2. `pnpm tauri icon ../src-tauri/icons/icon-small.svg -o <临时目录>/small`
  3. `python ../src-tauri/icons/compose_ico.py <临时目录>/small/icon.ico <临时目录>/main/icon.ico ../src-tauri/icons/icon.ico`
  4. 复制 `small/32x32.png`，以及 `main/` 下的 `128x128.png`、`128x128@2x.png`、`icon.icns` 到本目录。
