# 不透明原生窗口、保留 femtovg 默认渲染，并提供渲染器手动开关

状态：已采纳（2026-11-08，Issue #39）。实体显示器上的人眼确认待用户验收（docs/checklist/checklist.md）。

## 决定

| 项目 | 取值 | 依据 |
| --- | --- | --- |
| 原生窗口透明属性 | 所有 winit 窗口 `with_transparent(false)`（`src/render.rs`，经 `slint::BackendSelector::with_winit_window_attributes_hook`） | 根因修复，见下方诊断；窗口背景不透明、弹出层不是独立窗口（Slint 1.18.1 winit 后端在主窗口内绘制弹出层），不需要透明窗口 |
| 默认渲染器 | 仍是 Slint 默认顺序：femtovg（OpenGL），初始化失败回退软件渲染 | 修复后 femtovg 在开发机上可见；性能达标且 `--debug-perf` 帧计时可用；软件渲染器不支持渲染通知，现有观测全部失效 |
| 手动开关 | `--renderer=gl\|software` > 环境变量 `SFM_RENDERER` > Slint 自带 `SLINT_BACKEND` > 默认；无法识别的值提示后按默认 | 给极端环境（驱动异常、远程桌面）留出口，不需要改代码 |
| 首帧可见性自检、自动回退 | 不做 | 根因已修复；进程内自检需要自己抓自己的合成画面，复杂且本身可能误判。改为发布前必过的外部检查 |
| 发布前检查 | `scripts/verify-capture.ps1`（默认启动方式）接入 `scripts/verify.ps1`，四种外部抓取都要取到主题色 | 防止回归；只在内存比对颜色，不保存截图 |
| 新依赖特性 | `slint` 的 `unstable-winit-030` | 窗口属性钩子只在该特性下提供；`slint` 版本与 `Cargo.lock` 固定，升级 Slint 时需复核该接口 |

## 诊断（开发机实测，2026-11-08）

环境：Windows 10 22H2（19045），NVIDIA GeForce RTX 5070 Ti + AMD Radeon 核显，主显示器为远程控制软件的虚拟显示器（3840×2160，200% 缩放）。判据：`scripts/verify-capture.ps1` 对 21 个主题色点取样，四种方式为屏幕抓取（CopyFromScreen）、PrintWindow、WGC 窗口、WGC 显示器。实验用临时构建（诊断环境变量、导出符号、改边框），实验后已移除，产品代码只保留最终修复。

| 实验 | 结果（四种方式命中） | 结论 |
| --- | --- | --- |
| 修复前默认（Slint 默认 `transparent=true`） | 全部 0/21；WGC 窗口帧颜色与 alpha 全为 0，PrintWindow 全黑 | 复现 Issue #39 |
| 软件渲染（修复前） | 全部 20/21 | 对照 |
| 只改 `with_transparent(false)` | 全部 20/21 | **根因修复** |
| `with_transparent(false)` + `with_no_redirection_bitmap(true)` | 全部 20/21 | 无重定向位图不是诱因 |
| `transparent=true` + `with_no_redirection_bitmap(true)`（winit 此时跳过 `DwmEnableBlurBehindWindow`） | 全部 20/21 | 触发条件就是 winit 为透明窗口调用的 `DwmEnableBlurBehindWindow`（空模糊区域） |
| 修复前 + 导出 `NvOptimusEnablement=1` | 全部 0/21；仍加载 `nvoglv64.dll` | 指定 NVIDIA 独显无效 |
| 修复前 + 导出 `AmdPowerXpressRequestHighPerformance=1` | 全部 0/21；仍加载 `nvoglv64.dll`（没有切到 AMD 驱动） | 指定 GPU 无效 |
| 修复前 + 有边框窗口（`no-frame: false`） | 内容区 9/9 命中、侧栏取到 `#ffffff`；WGC 窗口帧 alpha 仍全为 0 | 边框只改变 DWM 的混合表现，alpha 仍为 0，不是修复 |

其他实测事实：

- Issue #39 里“窗口样式含 `WS_EX_LAYERED | WS_EX_NOREDIRECTIONBITMAP`”是误读：脚本那一行打印的是常量图例，实际扩展样式为 `0x00040118`（APPWINDOW、WINDOWEDGE、ACCEPTFILES、脚本加的 TOPMOST），修复前后、两种渲染器都相同；只有显式 `with_no_redirection_bitmap(true)` 时才出现 `0x00200000`。
- 两种透明设置下 femtovg 进程加载的图形模块相同：`opengl32.dll`、`nvoglv64.dll`、`nvwgf2umx.dll`、`dcomp.dll`；软件渲染不加载 `nvoglv64.dll`。
- Slint 1.18.1 的 winit 后端在 Windows 上总是 `with_transparent(true)`（只有 macOS 按背景是否透明决定）；glutin 的 WGL 配置模板默认 8 位 alpha，Slint 选配置时偏好支持透明的配置。修复只改窗口属性、不改 GL 配置，因此“GL 表面带 alpha 通道”本身不是问题。

推断（未能直接验证）：启用 DWM 模糊背景后，DWM 按窗口画面的 alpha 合成；在这台混合 GPU、主显示器由虚拟显示驱动承载的机器上，NVIDIA OpenGL 交给 DWM 的画面 alpha 为 0（有边框实验里颜色在而 alpha 为 0 支持这一点），于是无边框窗口完全透明。为什么驱动给出的 alpha 是 0、实体显示器或单 GPU 机器上是否同样发生，没有环境验证；修复不依赖这一点，因为关闭透明后 DWM 不再使用 alpha。

## 性能（开发机，修复后）

| 指标 | 目标 | 默认 femtovg（修复后） | 软件渲染 |
| --- | --- | --- | --- |
| 进入目录到可操作（`perf-browse.ps1`，窗口 1100×720 逻辑像素＝2200×1440 物理） | ≤ 200ms | 60／1000／10000 个文件：中位 7.6／7.3／13.3ms，最大 13.1／13.4／18.9ms | 无法测量：Slint 1.18.1 软件渲染器不支持 `set_rendering_notifier`，日志 `warn rendering-notifier-unavailable reason=Unsupported`，没有帧事件；读取完成耗时与默认相同（中位 0.8／3.1／10.1ms） |
| 首屏缩略图（`perf-thumbs.ps1 -ProductOnly`，首屏 11 项） | 冷 ≤ 1s，热 ≤ 300ms | 冷 88.9ms（n=1）；同进程再进入中位 7.7ms；重启后首次 12.5ms | 无法测量（同上，没有 `thumbs-first-screen-us`） |
| 最大化到整块 4K 200% 显示器的帧耗时（`--debug-perf`，进入 1000 个文件目录往返 10 次，两轮） | 无明确目标，避免卡顿 | 渲染 p50 0.55～0.60ms、p95 1.4～2.2ms、最大 13.5～14.8ms；可操作中位 7.3ms | 帧耗时无法测量；同一流程进程 CPU 时间 1.06～1.17s，默认为 0.89～0.94s（约多 15～30%） |

结论：修复后的默认渲染全部达标；软件渲染不是更好的默认（CPU 更高、产品自带的帧计时与首屏计时全部失效），只作为手动开关保留。

## 取舍与遗留

- `unstable-winit-030` 是不稳定接口，可能在 Slint 次版本里变化；升级 Slint 时 `cargo build` 会直接报错，且 `verify-capture.ps1` 会抓住行为回归。
- 若 Slint 以后在 Windows 上按背景透明度决定窗口透明（像 macOS 那样），可去掉钩子；届时以 `verify-capture.ps1` 为准。
- 观察到但未处理：GL 渲染下用 `ShowWindow(SW_MAXIMIZE)` 最大化后窗口为 3840×2160（盖住任务栏），软件渲染为工作区大小；修复前构建同样如此，与本决定无关。
- 实体显示器、单 GPU、Windows 11 上未验证。

使用方法见 AGENTS.md「常用入口」与 [调试观测框架手册](../debug-harness.md)。
