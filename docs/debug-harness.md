# 调试观测框架（代理使用手册）

目的：让代理不靠人工观看，就能驱动界面、读取状态、做确定性截图对比、逐帧检查动画，并在真实窗口里记录帧耗时。依据 Issue #22 与 [ADR 0009](adr/0009-debug-harness.md)。

所有命令都在仓库根目录执行，编译并行度已限制为 6。

## 一分钟入口

| 目的 | 命令 | 产物 |
| --- | --- | --- |
| 导出元素树（JSON） | `powershell -File scripts/harness.ps1 tree --scenario grid-selected --out artifacts/tree.json` | JSON |
| 交互脚本（单栏浏览流程、双栏独立标签流程、重启会话恢复流程；重复 2 次核对一致） | `powershell -File scripts/harness.ps1 run` | `artifacts/state/interaction.json`（`steps` 含 `single`／`dual`／`restore`） |
| 截图（100% 与 150%） | `powershell -File scripts/harness.ps1 snap` | `artifacts/snapshots/current/*.png` + `.json` |
| 截图并与基线比较，输出差异图 | `powershell -File scripts/harness.ps1 compare` | `artifacts/snapshots/diff/*.diff.png`、`compare-report.json` |
| 更新基线（人工确认后提交） | `powershell -File scripts/harness.ps1 bless` | `tests/baselines/` |
| 动画逐帧采集 | `powershell -File scripts/harness.ps1 frames` | `artifacts/frames/<场景>_<尺寸>_<缩放>/` |
| 生成 3 级审阅清单 | `powershell -File scripts/harness.ps1 review` | `artifacts/review/REVIEW.md` |
| 真实窗口帧耗时日志 | `target\release\slint-file-manager.exe --debug-perf --open=目录 --smoke-ms=4000` | `logs/yyyy-MM-dd HH-mm-ss.log` |
| 进入目录耗时基准（60／1000／10000 个文件） | `powershell -File scripts/perf-browse.ps1` | `artifacts/perf/browse-<时间>.json` |
| 真实窗口截图（补充手段） | `powershell -File scripts/capture-window.ps1 [-Open 目录]` | `artifacts/real/<时间>/` |
| 观测框架整体验证 | `powershell -File scripts/verify-harness.ps1` | 退出码 |
| 会话恢复（真实程序 + 临时数据目录） | `powershell -File scripts/verify-session.ps1`（已含在 `scripts/verify.ps1`） | 退出码 |

`scripts/harness.ps1` 会先以 `--features harness` 构建 `harness` 程序（发布产物不含它，也不含调试信息），再转发参数。常用选项：`--size 1100x720`（默认，与产品初始窗口一致）、`--scales 1,1.5`、`--scenarios grid-selected,loading`、`--scenario loading-appear`、`--interval 16`、`--duration 600`、`--out 目录`。

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
- 离屏窗口不跟随系统主题；真实窗口在系统深色模式下，未显式指定颜色的 `Text` 会变成白色。所以**界面里的文字、背景都要显式写颜色**（本仓库界面已这样做，`Palette.color-scheme` 也固定为浅色）。
- 软件渲染器使用系统字体。字体清单计入“环境指纹”，见下节。
- 真实窗口与离屏有差异时，用 `scripts/capture-window.ps1` 看真实窗口。

## 无头驱动与元素标识约定

测试后端 = `i-slint-backend-testing`（模拟时钟、无事件循环、软件渲染器）。窗口由 `Ctx::new(尺寸, 缩放)` 或 `Ctx::open(尺寸, 缩放, Opts)`（`Opts { start, text_scale, load, split, session }`：起始目录、文字缩放、是否完成首次读取、是否从双栏开始、会话目录）创建，与产品共用 `app::wire` 接线，区别只在环境：

- **会话**：默认不挂会话存储（`session: None`，场景之间互不影响）。给 `session: Some(目录)` 才读写该目录的 `session.json`，目录放在 `%TEMP%\sfm-harness\sessions\<名称>`（`fixture::session_dir`，每次 harness 启动清空）。进程内“重启” = 在同一目录上再开一个 `Ctx`（先 `save_now()`）。给了会话时 `start` 必须为 `None`（`Some` 表示外部请求打开目录，会改动恢复出的会话）。
- **默认双栏**：产品默认就是双栏；单栏旧场景用 `Opts.split: false`（默认值）或 `Scenario.split`，窗口保持原有单栏外观，基线才与旧场景可比较。

- **目录读取**用 `Loader::Manual`：导航只把请求排队，调用 `Ctx::pump()` 才在当前线程执行并应用结果。因此“加载中”状态、连续导航后过期结果被丢弃都能确定地复现。
- **打开文件**用 `Opener::Record`：只把路径记到 `Ctx::opened`，不启动任何程序。
- **样例目录**（`src/bin/harness/fixture.rs`）：每次 harness 启动（`review` 除外）在 `%TEMP%\sfm-harness\样例目录` 重新生成，不读取用户真实目录。内容：文件夹“照片”（12 张 WebP 占位）、“空文件夹”、“无权限”（用 `icacls` 拒绝 Everyone 列目录，只作用于本夹具，下次生成前 `/reset`）、四层长名称子目录；根目录 12 张图片、中文／自然排序名（报告2、报告10）、长中文名 PDF、长英文名 TXT、各类扩展名、一个隐藏文件（不应显示）。名称、大小、修改时间全部固定，内容是占位字节。
- 路径含用户名、日期按本地时区显示，所以样例目录绝对路径与时区写入截图指纹；换机器得到 `env-mismatch`，属正常。
- 两个 harness 进程不要同时运行（会同时重建样例目录）。
- 模拟时钟在每个窗口创建前对齐到 3600ms 的整数倍，保证 `animation-tick()` 驱动的循环动画（加载进度条）在不同窗口中相位一致。

**标识约定（后续界面工单沿用）：**

1. 每个需要被查找的元素写 id，格式 kebab-case，按“区域-用途”命名，如 `sort-button`、`loading-indicator`。查找时用限定名 `BrowserPane::loading-indicator`（组件名::id）。
2. 可交互元素**同时**设置 `accessible-role` 与 `accessible-label`（用户可读的中文名，如“后退”“排序方式”）。优先按“角色 + 标签”查找（`Ctx::find`、`Ctx::button`、`Ctx::item`）：`Text` 元素会把自身文字当作标签，只按标签查常常不唯一。
   - 当前界面的标识：按钮“后退／前进／上一级／刷新／地址栏／排序方式／网格视图／列表视图／双栏（可切换，`accessible-checked` 表示双栏）／最小化／最大化（最大化后为“还原”）／关闭”，侧栏按钮“主目录”（其余入口禁用），路径分段“转到 <名称>”，表头“按名称排序／按修改日期排序／按类型排序／按大小排序”，排序菜单行“名称／修改日期／类型／大小／递增／递减”，状态面板按钮“重试／用默认应用打开”，文件项为 `list-item` 角色、标签即文件名（`accessible-item-selected` 表示选中），地址输入框“地址”，标签“浏览标签 <标题>”（位置不可用的标签描述为“位置不可用”），标签带“标签带”、按钮“新建标签”“关闭标签 <标题>”，面板 `groupbox` “左面板／右面板”（描述为“活动面板”的即活动面板），加载提示 `progress-indicator`“正在加载”，状态面板 `groupbox` 标签即标题（如“此文件夹为空”）。
   - 网格与列表都虚拟化，只有可见行能被找到；需要操作屏幕外的项时先滚动或用键盘移动。
3. 元素级状态用 Slint 的 `accessible-*` 属性表达（`accessible-checked`、`accessible-value`、`accessible-enabled` 等），元素树会导出。
4. 元素树无法通用读取的状态（如键盘焦点）：在窗口根组件上加 `out property`（如 `focus-area`），由 `dump::extras` 导出。新界面要导出的状态在那里追加。
5. 元素树导出（`tree`）的 JSON 顶层为 `meta`（窗口尺寸、缩放、渲染器、夹具）、`extras`（浏览状态：`active_pane`、`split`、`focused_pane`、`panes`（两个面板各一项：`visible`、`active_tab`、`tabs[]`（`location`、`view`、`sort`、`unavailable`、`can_back`、`can_forward`）、`selected`、`item_count`；来自 Rust 侧状态，隐藏面板也能读到）；以下是活动面板的界面属性：`location`（样例目录内显示为 `<样例目录>\…`）、`content_state`、`item_count`、`selected_count`、`selected`、`cursor_index`、`view`、`sort_key`、`sort_ascending`、`focus_area`（content／address／none）、`editing_address`、`can_back`／`can_forward`／`can_up`、`status_text`、`status_message`、`pending_loads`、`opened`）、`elements`（元素数组）。每个元素的字段：`index`、`id`（带组件名前缀，如 `BrowserPane::loading-indicator`）、`type`、`role`、`label`、`value`、`enabled`、`checked`、`selected`、`expanded`、`x`、`y`、`w`、`h`（窗口内逻辑像素）、`opacity`、`visible`；取不到的值为 `null`。
6. 元素树按深度优先先序平铺，没有层级字段；`visible` 恒为 true（不可见元素遍历不到）；`id` 为空表示未命名元素。

输入模拟：`Ctx::click`（`mock_single_click`，含 50ms 模拟时间）、`Ctx::ctrl_click`／`shift_click`（先发送修饰键按下事件，指针事件的 modifiers 来自窗口的修饰键状态）、`Ctx::double_click`（两次单击）、`Ctx::click_at`／`click_right_edge`（点元素中心以外的位置，如地址栏右侧空白）、`Ctx::key`／`special(Key, 修饰键)`、`Ctx::type_text`（向焦点文本框逐字输入）、`Ctx::enter_address`（调用地址栏的无障碍默认动作进入编辑再输入，窄面板里面包屑占满地址栏也能用；路径写绝对路径）、`Ctx::pump`（完成排队的目录读取）、`ElementHandle::scroll`。

**面板限定查找：**双栏时左右面板有同名元素（两个“后退”、两套文件项）。`scenarios.rs` 的 `find`／`find_all` 按“元素中心是否落在该面板（`左面板`／`右面板` groupbox）矩形内”过滤。隐藏面板不在元素树里，状态用 `extras.panes` 读。窄面板网格里屏幕外的项点不到，用最上面一行的项。

完整流程见 `src/bin/harness/scenarios.rs` 的 `interaction`（单栏）、`src/bin/harness/dual.rs` 的 `dual`（双栏独立标签：默认等宽、各面板新建／关闭／切换标签、前进后退归属、单栏往返不清另一侧、位置不可用、快速切换与过期结果）与 `restore`（写会话 → 重启 → 比对两侧标签、位置、排序、视图；损坏／新版本会话降级）。

## 截图、基线与环境指纹

- 差异图文件名：`<截图文件名去掉 .png>.diff.png`，位于 `artifacts/snapshots/diff/`；`compare` 发现差异时会在输出行末打印路径，详细数值在同目录 `compare-report.json`。
- 文件名：`<场景>_<逻辑宽>x<逻辑高>_<缩放百分比>.png`，如 `grid-selected_1100x720_150.png`；同名 `.json` 记录物理尺寸、缩放与环境指纹。
- **稳定性自检**：每个场景用两个全新窗口渲染两次，逐字节比较；不一致标记 `stable: false`，命令退出码 3，**不得**把这种截图当回归信号。
- **环境指纹**：渲染器、Slint 版本、操作系统、系统字体清单哈希、样例目录绝对路径、本地时区偏移。基线与当前指纹不同且像素有差异时，状态为 `env-mismatch`（退出码 2，“无法判断”），而不是 `regression`。换机器或装了新字体后，先在新环境 `bless`。
- 比较参数：`--channel-threshold 2`（单通道差不超过 2 视为相同像素）、`--max-ratio 0.0005`（不同像素占比上限）。差异图中红色为差异像素，其余为压淡后的基线。
- `compare` 退出码：0 一致（含容差内）；1 回归；2 缺基线或环境不同；3 截图不稳定。

### 产物目录

| 目录 | 内容 | 入库 |
| --- | --- | --- |
| `tests/baselines/` | 基线 PNG 与指纹 JSON | **入库** |
| `artifacts/`（含 `snapshots/`、`frames/`、`state/`、`review/`、`real/`、`verify/`、`perf/`） | 当次产物 | 忽略 |
| `logs/` | 真实窗口帧耗时日志 | 忽略 |

### 基线更新流程

1. 改样式后运行 `compare`，看 `artifacts/snapshots/diff/*.diff.png` 确认差异符合预期。
2. 符合预期：`bless` 覆盖基线，并在提交说明里写明变化。不符合预期：回去改界面。
3. 基线只在同一环境下可比。基线由某台机器生成，别的机器会得到 `env-mismatch`，属正常；需要时在该机器重新 `bless`。
4. 截图对比不替代 A2 视觉评审。需要主观判断时运行 `review` 生成 `artifacts/review/REVIEW.md`（编号、内容、预期效果、文件），连同图片推送 Telegram（3 级验收）。

### 基线变更记录

- **Issue #6（双栏独立标签）**：所有原有单栏场景的基线重新 `bless`。变化只发生在两处：顶部多了标签带（含当前标签与“新建标签”按钮）与面板顶边的活动蓝线；工具栏的“双栏”按钮由禁用态变为可用（单栏外观下为未选中态）。内容区与列表／网格像素未变（用旧基线逐像素比较确认差异只落在这两个区域）。同时新增 5 个双栏场景基线（见“新增场景”）。

## 动画逐帧采集

`frames` 用模拟时钟按固定间隔（默认 16ms）推进，在每一帧导出：

- `frame_<帧号>_t<毫秒>ms.png`：帧图。t=0 是触发动画的瞬间。
- `curve.csv` / `curve.json`：被跟踪元素（`scenarios.rs` 的 `TRACKED`）的 x、y、w、h、opacity 随时间的数值；`mock_time_ms` 为模拟时钟绝对值，便于追溯。`curve.json` 的 `summary` 给出起点、终点、极值、**过冲**与**稳定时间**。
- `contact-sheet.png`：联系表，每格上方左为帧号、右为毫秒。
- `anim.gif`：半尺寸动图，供 frame-inspect 和 3 级审阅使用。

默认值：`--interval 16`（毫秒）、`--duration 600`（毫秒）。动画时长超过 600ms 时要加大 `--duration`，否则稳定时间会被截断。输出目录默认 `artifacts/frames/<场景>_<尺寸>_<缩放>/`，**重复采集会覆盖**；要对比改动前后，先用 `--out` 指到不同目录。目前 `TRACKED` 只跟踪 `BrowserPane::loading-indicator`（动画场景 `loading-appear`：进入目录但读取未完成，提示延迟 150ms 后 180ms 淡入）。

核对方法：看 `summary`（时长≈稳定时间、过冲量、终态），再看联系表确认视觉。同一动画重复采集产物逐字节一致（`verify-harness.ps1` 会检查）。改了 `animate` 的 `duration` 或 `easing`，`summary.settle_ms`、过冲和曲线都会变。

注意：

- **模拟时钟只验证动画定义本身，不能证明真实运行时不卡顿。**真实流畅度看帧耗时日志。
- 属性必须在变化前被读取过，动画才会从旧值开始；harness 在触发前先渲染一帧，真实窗口首帧起同理。自己写新的动画场景时保留这一步（`frames::capture` 已包含）。
- 新增被跟踪的元素：在 `TRACKED` 里追加 `(列名前缀, "组件名::元素id")`。

## 真实窗口帧耗时与运行日志

- 开关：命令行 `--debug-perf` 或环境变量 `SFM_DEBUG_PERF=1`，**默认关闭**。关闭时不创建文件、不注册渲染通知，`perf::event` 只做一次空检查，发布产物无额外日志。
- 位置与命名：`logs/yyyy-MM-dd HH-mm-ss.log`（相对当前目录，可用 `SFM_LOG_DIR` 改），一次启动一个文件。
- 行格式：`HH:mm:ss.fff +启动后毫秒 内容`。行首是本机时间，可与 `capture-window.ps1` 的 `captures.txt` 或录屏时间对应。
- 内容：`start`、`frame n= interval_ms= render_ms= animating= dropped=`、`event <名称>`、`summary`（帧数、p50、p95、最大间隔、掉帧数）。
- 掉帧：动画期间相邻两帧间隔超过 25ms（`SFM_PERF_DROP_MS` 可改）。每次动画的第一帧不判定。
- 已有事件：`animation-start`、`animation-end`、`scroll`；进入目录：`dir-loaded-us`（自导航请求起，读取结果回到界面线程）、`dir-entries`（条目数）、`dir-ready-us`（新内容第一帧渲染完成，用 `perf::mark_next_frame` 在下一帧记录），基准模式额外有 `bench-iter`。
- 隐私：`perf::event` 只接受静态字符串和一个整数，写不进路径、文件名、图像内容。新增事件保持这一约束。
- 复现：`--open=目录` 指定起始目录，`--smoke-ms=N` 运行 N 毫秒后自动退出；`--bench-enter=目录 [--bench-runs=10]` 每 400ms 在目录与其上级之间切换，每次进入前记录 `bench-iter`（隐含 `--debug-perf`），`scripts/perf-browse.ps1` 据此汇总。
- Issue #22 的示例界面（切换面板、示例列表、`--demo-autoplay`）已在 Issue #4 随真实浏览界面一起移除；harness 的场景、基线、交互脚本全部改为真实界面。

## 新增场景

- 静态场景：在 `src/bin/harness/scenarios.rs` 的 `STATIC` 追加 `Scenario`（可设文字缩放 `text_scale`、是否完成首次读取 `load`；`setup` 里用 `Ctx` 的点击、按键、`pump`、推进时间把界面带到目标状态，并写好“内容”“预期效果”供审阅清单使用），然后 `bless`。需要新的文件形态时改 `fixture.rs`，并同步 `root_visible_count`。
- 动画场景：在 `ANIM` 追加 `AnimScenario`（`start` 触发动画）。
- 双栏相关静态场景（Issue #6，基线 100%／150%）：`dual-default`（等宽双栏，左面板活动）、`dual-tabs`（两侧各多个标签，右面板活动）、`dual-unavailable-tab`（右面板含位置不可用标签）、`single-right`（单栏显示右面板）、`dual-text-scale-150`（双栏 + 文字缩放 150%，右面板活动）。双栏场景在 `Scenario` 里用 `split: true`；原有场景保持 `split: false`（单栏外观）。
- 活动面板的截图审阅：`review` 的 REVIEW.md 已包含上述场景及“预期效果”；对比 `dual-default`（左活动）与 `dual-tabs`／`dual-text-scale-150`（右活动）即可看到顶边蓝线与标签底色的切换。
- 不要为尚不存在的界面预建抽象；界面出现后再在这些入口上扩展。

## 已知限制与排错

- 内部 crate `i-slint-backend-testing` 不遵循 semver，`Cargo.toml` 里固定 `=1.18.1`，与 `slint` 同步升级。
- 测试后端的 `set_size` 按缩放 1 换算，`Ctx::open` 已绕开（物理尺寸与逻辑尺寸分别设置）。不要在 `Ctx::open` 之外改窗口尺寸。
- `capture-window.ps1` 在本机对无边框主窗口抓屏时曾抓到窗口下方的其他桌面内容（2026-11-08，原因未查明，可能是窗口尚未上屏或未成功置顶）。抓到的图必须先目视确认，含私人内容立即删除；真实窗口外观改由人工验收。
- 元素树依赖调试信息，由 `build.rs` 在 `harness` 特性下开启；用默认特性构建的程序查不到元素。
- Windows 11 尚未验证（见 ADR 0008）；本框架同样只在 Windows 10 上验证过。
