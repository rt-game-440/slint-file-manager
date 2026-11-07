# 调试观测框架（代理使用手册）

目的：让代理不靠人工观看，就能驱动界面、读取状态、做确定性截图对比、逐帧检查动画，并在真实窗口里记录帧耗时。依据 Issue #22 与 [ADR 0009](adr/0009-debug-harness.md)。

所有命令都在仓库根目录执行，编译并行度已限制为 6。

## 一分钟入口

| 目的 | 命令 | 产物 |
| --- | --- | --- |
| 导出元素树（JSON） | `powershell -File scripts/harness.ps1 tree --scenario idle --out artifacts/tree.json` | JSON |
| 交互脚本（查找、点击、按键、滚动、断言，重复 2 次核对一致） | `powershell -File scripts/harness.ps1 run` | `artifacts/state/interaction.json` |
| 截图（100% 与 150%） | `powershell -File scripts/harness.ps1 snap` | `artifacts/snapshots/current/*.png` + `.json` |
| 截图并与基线比较，输出差异图 | `powershell -File scripts/harness.ps1 compare` | `artifacts/snapshots/diff/*.diff.png`、`compare-report.json` |
| 更新基线（人工确认后提交） | `powershell -File scripts/harness.ps1 bless` | `tests/baselines/` |
| 动画逐帧采集 | `powershell -File scripts/harness.ps1 frames` | `artifacts/frames/<场景>_<尺寸>_<缩放>/` |
| 生成 3 级审阅清单 | `powershell -File scripts/harness.ps1 review` | `artifacts/review/REVIEW.md` |
| 真实窗口帧耗时日志 | `target\release\slint-file-manager.exe --debug-perf --demo-autoplay --smoke-ms=4000` | `logs/yyyy-MM-dd HH-mm-ss.log` |
| 真实窗口截图（补充手段） | `powershell -File scripts/capture-window.ps1` | `artifacts/real/<时间>/` |
| 观测框架整体验证 | `powershell -File scripts/verify-harness.ps1` | 退出码 |

`scripts/harness.ps1` 会先以 `--features harness` 构建 `harness` 程序（发布产物不含它，也不含调试信息），再转发参数。常用选项：`--size 960x600`、`--scales 1,1.5`、`--scenarios idle,panel-open`、`--scenario panel-toggle`、`--interval 16`、`--duration 600`、`--out 目录`、`--fixture tests/fixtures/basic`。

## 每种手段能证明什么，不能证明什么

| 手段 | 能证明 | 不能证明 |
| --- | --- | --- |
| 元素树 / 交互脚本（`tree`、`run`） | 元素存在、几何、文字、选中、状态属性；点击、按键、滚动后的状态符合断言 | 看起来是否好看；真实渲染器的表现 |
| 截图对比（`compare`） | 与基线逐像素一致，或差异在容差内。**只说明“与基线一致”** | 视觉本身是否正确（基线可能就是错的，需 A2 视觉评审）；真实渲染器的表现 |
| 逐帧采集（`frames`） | 动画定义本身：时长、缓动、过冲、终态；同一动画重复采集一致 | 真实运行是否流畅、掉帧、卡顿（模拟时钟没有真实耗时） |
| 帧耗时日志（`--debug-perf`） | 真实窗口、真实渲染器下的帧间隔、渲染耗时、动画期间的掉帧 | 画面内容对不对（日志不含图像） |
| 真实窗口截图（`capture-window.ps1`） | 真实渲染器、真实主题下实际出现了什么；与日志时间可对应 | 确定性：抓屏含遮挡物、依赖显示缩放，不能做基线比较，也不得外传（可能含桌面私人内容） |

### 离屏截图与产品默认渲染器的差异

- 离屏截图固定使用 **Slint 软件渲染器**（测试后端内置，`i-slint-renderer-software`），与产品默认的 **femtovg（OpenGL）** 不同：抗锯齿、圆角边缘、文字光栅化都会有细微差别；不能把两者逐像素比较。
- 离屏窗口不跟随系统主题；真实窗口在系统深色模式下，未显式指定颜色的 `Text` 会变成白色。所以**界面里的文字、背景都要显式写颜色**（本仓库示例已这样做）。
- 软件渲染器使用系统字体。字体清单计入“环境指纹”，见下节。
- 真实窗口与离屏有差异时，用 `scripts/capture-window.ps1` 看真实窗口。

## 无头驱动与元素标识约定

测试后端 = `i-slint-backend-testing`（模拟时钟、无事件循环、软件渲染器）。窗口由 `Ctx::new(尺寸, 缩放)` 创建：固定逻辑尺寸、缩放比例，不读取用户真实目录；夹具目录固定为 `tests/fixtures/basic`（目前界面尚未接入目录列表，harness 只校验存在并在 `tree` 的 `meta.fixture` 里记录文件数；后续列表界面接入时从这里传入）。

**标识约定（后续界面工单沿用）：**

1. 每个需要被查找的元素写 id，格式 kebab-case，按“区域-用途”命名，如 `demo-toggle-button`、`demo-panel`。查找时用限定名 `MainWindow::demo-toggle-button`（组件名::id）。
2. 可交互元素**同时**设置 `accessible-role` 与 `accessible-label`（用户可读的中文名，如“切换示例面板”）。优先用 `accessible-label` 查找：它也是无障碍名称，一处定义两处使用。
3. 元素级状态用 Slint 的 `accessible-*` 属性表达（`accessible-checked`、`accessible-value`、`accessible-enabled` 等），元素树会导出。
4. 元素树无法通用读取的状态（如键盘焦点）：在窗口根组件上加 `out property`（如 `key-focused`），由 `dump::extras` 导出。新界面要导出的状态在那里追加。
5. 元素树导出（`tree`）的 JSON 顶层为 `meta`（窗口尺寸、缩放、渲染器、夹具）、`extras`（根组件导出的状态属性，如 `click_count`、`panel_open`、`key_focused`、`list_scroll_y`）、`elements`（元素数组）。每个元素的字段：`index`、`id`（带组件名前缀，如 `MainWindow::demo-toggle-button`）、`type`、`role`、`label`、`value`、`enabled`、`checked`、`selected`、`expanded`、`x`、`y`、`w`、`h`（窗口内逻辑像素）、`opacity`、`visible`；取不到的值为 `null`。
6. 元素树按深度优先先序平铺，没有层级字段；`visible` 恒为 true（不可见元素遍历不到）；`id` 为空表示未命名元素。

输入模拟：`ElementHandle::mock_single_click`（点击，含 50ms 模拟时间）、`Ctx::key`（按键）、`ElementHandle::scroll`（滚动）。示例脚本见 `src/bin/harness/scenarios.rs` 的 `interaction`。

## 截图、基线与环境指纹

- 差异图文件名：`<截图文件名去掉 .png>.diff.png`，位于 `artifacts/snapshots/diff/`；`compare` 发现差异时会在输出行末打印路径，详细数值在同目录 `compare-report.json`。
- 文件名：`<场景>_<逻辑宽>x<逻辑高>_<缩放百分比>.png`，如 `panel-open_960x600_150.png`；同名 `.json` 记录物理尺寸、缩放与环境指纹。
- **稳定性自检**：每个场景用两个全新窗口渲染两次，逐字节比较；不一致标记 `stable: false`，命令退出码 3，**不得**把这种截图当回归信号。
- **环境指纹**：渲染器、Slint 版本、操作系统、系统字体清单哈希。基线与当前指纹不同且像素有差异时，状态为 `env-mismatch`（退出码 2，“无法判断”），而不是 `regression`。换机器或装了新字体后，先在新环境 `bless`。
- 比较参数：`--channel-threshold 2`（单通道差不超过 2 视为相同像素）、`--max-ratio 0.0005`（不同像素占比上限）。差异图中红色为差异像素，其余为压淡后的基线。
- `compare` 退出码：0 一致（含容差内）；1 回归；2 缺基线或环境不同；3 截图不稳定。

### 产物目录

| 目录 | 内容 | 入库 |
| --- | --- | --- |
| `tests/baselines/` | 基线 PNG 与指纹 JSON | **入库** |
| `tests/fixtures/` | 夹具目录 | **入库** |
| `artifacts/`（含 `snapshots/`、`frames/`、`state/`、`review/`、`real/`、`verify/`） | 当次产物 | 忽略 |
| `logs/` | 真实窗口帧耗时日志 | 忽略 |

### 基线更新流程

1. 改样式后运行 `compare`，看 `artifacts/snapshots/diff/*.diff.png` 确认差异符合预期。
2. 符合预期：`bless` 覆盖基线，并在提交说明里写明变化。不符合预期：回去改界面。
3. 基线只在同一环境下可比。基线由某台机器生成，别的机器会得到 `env-mismatch`，属正常；需要时在该机器重新 `bless`。
4. 截图对比不替代 A2 视觉评审。需要主观判断时运行 `review` 生成 `artifacts/review/REVIEW.md`（编号、内容、预期效果、文件），连同图片推送 Telegram（3 级验收）。

## 动画逐帧采集

`frames` 用模拟时钟按固定间隔（默认 16ms）推进，在每一帧导出：

- `frame_<帧号>_t<毫秒>ms.png`：帧图。t=0 是触发动画的瞬间。
- `curve.csv` / `curve.json`：被跟踪元素（`scenarios.rs` 的 `TRACKED`）的 x、y、w、h、opacity 随时间的数值；`mock_time_ms` 为模拟时钟绝对值，便于追溯。`curve.json` 的 `summary` 给出起点、终点、极值、**过冲**与**稳定时间**。
- `contact-sheet.png`：联系表，每格上方左为帧号、右为毫秒。
- `anim.gif`：半尺寸动图，供 frame-inspect 和 3 级审阅使用。

默认值：`--interval 16`（毫秒）、`--duration 600`（毫秒）。动画时长超过 600ms 时要加大 `--duration`，否则稳定时间会被截断。输出目录默认 `artifacts/frames/<场景>_<尺寸>_<缩放>/`，**重复采集会覆盖**；要对比改动前后，先用 `--out` 指到不同目录。目前 `TRACKED` 只跟踪 `demo-panel` 一个元素。

核对方法：看 `summary`（时长≈稳定时间、过冲量、终态），再看联系表确认视觉。同一动画重复采集产物逐字节一致（`verify-harness.ps1` 会检查）。改了 `animate` 的 `duration` 或 `easing`，`summary.settle_ms`、过冲和曲线都会变。

注意：

- **模拟时钟只验证动画定义本身，不能证明真实运行时不卡顿。**真实流畅度看帧耗时日志。
- 属性必须在变化前被读取过，动画才会从旧值开始；harness 在触发前先渲染一帧，真实窗口首帧起同理。自己写新的动画场景时保留这一步（`frames::capture` 已包含）。
- 新增被跟踪的元素：在 `TRACKED` 里追加 `(列名前缀, "MainWindow::元素id")`。

## 真实窗口帧耗时与运行日志

- 开关：命令行 `--debug-perf` 或环境变量 `SFM_DEBUG_PERF=1`，**默认关闭**。关闭时不创建文件、不注册渲染通知，`perf::event` 只做一次空检查，发布产物无额外日志。
- 位置与命名：`logs/yyyy-MM-dd HH-mm-ss.log`（相对当前目录，可用 `SFM_LOG_DIR` 改），一次启动一个文件。
- 行格式：`HH:mm:ss.fff +启动后毫秒 内容`。行首是本机时间，可与 `capture-window.ps1` 的 `captures.txt` 或录屏时间对应。
- 内容：`start`、`frame n= interval_ms= render_ms= animating= dropped=`、`event <名称>`、`summary`（帧数、p50、p95、最大间隔、掉帧数）。
- 掉帧：动画期间相邻两帧间隔超过 25ms（`SFM_PERF_DROP_MS` 可改）。每次动画的第一帧不判定。
- 已有事件：`animation-start`、`animation-end`、`scroll`。进入目录等事件在对应界面工单里用 `perf::event("名称", Some(数值))` 追加。
- 隐私：`perf::event` 只接受静态字符串和一个整数，写不进路径、文件名、图像内容。新增事件保持这一约束。
- 复现：`--demo-autoplay` 每 500ms 自动切换示例面板，`--smoke-ms=N` 运行 N 毫秒后自动退出。

## 新增场景

- 静态场景：在 `src/bin/harness/scenarios.rs` 的 `STATIC` 追加 `Scenario`（`setup` 里用 `Ctx` 的点击、按键、滚动、推进时间把界面带到目标状态，并写好“内容”“预期效果”供审阅清单使用），然后 `bless`。
- 动画场景：在 `ANIM` 追加 `AnimScenario`（`start` 触发动画）。
- 不要为尚不存在的界面预建抽象；界面出现后再在这些入口上扩展。

## 已知限制与排错

- 内部 crate `i-slint-backend-testing` 不遵循 semver，`Cargo.toml` 里固定 `=1.18.1`，与 `slint` 同步升级。
- 测试后端的 `set_size` 按缩放 1 换算，`Ctx::new` 已绕开（物理尺寸与逻辑尺寸分别设置）。不要在 `Ctx::new` 之外改窗口尺寸。
- 元素树依赖调试信息，由 `build.rs` 在 `harness` 特性下开启；用默认特性构建的程序查不到元素。
- Windows 11 尚未验证（见 ADR 0008）；本框架同样只在 Windows 10 上验证过。
