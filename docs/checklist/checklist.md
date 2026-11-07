# 验收清单

需要人工操作验收的项目按时间往后追加。每项写明内容、操作说明、预期效果，并留出用户反馈。

## Issue #3 系统菜单与云文件兼容原型（2026-11-07）

结论文档：[docs/research/0003-shell-menu-and-cloud-compat.md](../research/0003-shell-menu-and-cloud-compat.md)

- [ ] **真人键盘／鼠标操作原生右键菜单**
  - 操作：在 `prototypes/shell-cloud-probe` 下执行 `cargo build --jobs 6`。准备夹具目录 `%TEMP%\slint-fm-probe-fixture`（含 a.txt），然后在终端运行 `target\debug\shell-cloud-probe.exe menu --items "%TEMP%\slint-fm-probe-fixture\a.txt" --mode track-cancel --manual`（`--manual` 表示程序不投递模拟按键）。菜单弹出后，用鼠标悬停或键盘方向键进入 NanaZip 子菜单，再按 Esc 逐层关闭，或点击菜单外部关闭。再把参数换成 `--mode track-select --manual`，手动点击“添加到 "a.7z"”。
  - 预期：菜单出现在鼠标位置，NanaZip 子菜单内容完整；用 Esc 或点击外部关闭后，程序输出“返回 0：菜单已取消”，进程正常退出；菜单没有残留在屏幕上。选择模式下会生成 a.7z。
  - 用户反馈：

- [ ] **Windows 11 上重跑菜单与云盘原型**（有 Win11 环境后）
  - 操作：按结论文档第 6 节的复现命令，在 Win11 上运行 `menu` 的 list / invoke / track-select / track-cancel，以及装有 OneDrive 或 iCloud 时的 `cloud-info` / `cloud-scan` / `thumb-scan --mode incache`。
  - 预期：记录 Win11 版本号；确认 NanaZip 子菜单能出现并执行，INCACHEONLY 的水合计数为 0。结果写回结论文档的“未覆盖”一节。
  - 用户反馈：
