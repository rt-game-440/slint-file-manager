# 工具链、渲染方案与 Windows 构建基线

状态：已采纳（2026-11-07，Issue #2）。Windows 11 验证仍阻塞，见文末。

## 决定

| 项目 | 取值 | 依据 |
| --- | --- | --- |
| Rust | 1.99.1 stable，目标 `x86_64-pc-windows-msvc`，edition 2024 | 开发机现有版本；Windows 10 为该目标的官方支持下限 |
| Slint | 1.18.1（`slint` 与 `slint-build` 同版本，`Cargo.lock` 入库） | 编写时 crates.io 最新稳定版 |
| 渲染方案 | Slint 默认特性：winit 后端，femtovg（OpenGL）渲染，软件渲染作回退 | 不额外引入依赖；回退路径兼顾缺少 GPU 驱动的环境。尚未对比性能，是否改用 Skia 留待性能工单依据实测决定 |
| 最低 Windows 构建 | Windows 10（已实测 22H2，build 19045）；更早版本未验证、不承诺 | ADR 0001 要求同时支持 Win10／11 x64；Rust 标准库对该目标要求 Win10 及以上 |
| 编译并行度 | 不超过 6（`--jobs 6`，本机 12 核的一半） | 项目约定 |

## 验证状态

| 系统 | 构建 | 启动并退出 | 记录 |
| --- | --- | --- | --- |
| Windows 10 专业工作站版 22H2（build 19045） | 通过 | 通过 | `scripts/verify.ps1`（clippy、release 构建、`--smoke`） |
| Windows 11 x64 | 未验证 | 未验证 | **阻塞：没有 Windows 11 测试环境。** 不得据 Win10 结果推断兼容。 |

Windows 11 的最低版本同样待在有环境后确认。有 Win11 机器后运行 `scripts/dist.ps1` 生成的产物（或 `scripts/verify.ps1`）即可补全记录。

## 常用命令

见 AGENTS.md「常用入口」。

## 运行库与分发补充（2026-11-08，Issue #15）

- 分发包只含 exe 与 LICENSE，不带 VC++ 可再发行组件，因此用 `.cargo/config.toml` 的 `target-feature=+crt-static` 静态链接 VC 运行库；Universal CRT（`api-ms-win-crt-*`）随 Windows 10／11 提供。
- exe 静态导入 `cldapi.dll`（云文件 API，Windows 10 1709 起提供），所以实际下限不低于 1709；仍只验证过 22H2。
- `scripts/verify-dist.ps1` 解压最新 zip 到临时目录，检查导入表并以精简 PATH 启动 `--smoke`。这不等于“真正干净的机器”：开发机上没有做虚拟机、沙盒或 Win11 启动验证。
- 默认渲染器（femtovg）在开发机上的窗口对屏幕合成透明（Issue #39），是否在实体显示器上复现待确认；软件渲染只在 OpenGL 初始化失败时回退。
