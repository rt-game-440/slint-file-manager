# 项目约定

- 使用简体中文交流和编写文档，文本文件采用 UTF-8。
- 使用 gh 进行 GitHub 操作。

## Agent skills

### Issue tracker
事项和规格存放在 GitHub Issues；操作前读取 docs/agents/issue-tracker.md。

### Triage labels
使用五个默认分诊标签；分诊前读取 docs/agents/triage-labels.md。

### Domain docs
采用 single-context；探索领域概念或修改架构前读取 docs/agents/domain.md。

## 常用入口

Rust + Slint 工程，仅支持 Windows（见 ADR 0001、0008）。版本、渲染方案与验证状态见 [ADR 0008](docs/adr/0008-toolchain-and-windows-baseline.md)。编译并行度一律不超过 6。

- 构建 release：`powershell -File scripts/build.ps1`
- 定向验证（fmt、clippy、浏览核心单元测试、构建、`--smoke` 启动退出）：`powershell -File scripts/verify.ps1`
- 生成分发包（`dist/*.zip`）：`powershell -File scripts/dist.ps1`
- 手动冒烟：`target\release\slint-file-manager.exe --smoke`（约 1 秒后自动退出，退出码 0）
- 启动参数：`--open=目录` 指定起始目录（默认主目录）；`--bench-enter=目录` 进入目录耗时基准，汇总脚本 `powershell -File scripts/perf-browse.ps1`（Issue #1 目标 ≤ 200ms）
- 双栏、独立标签与会话恢复（工作区／面板／标签模型、快捷键、`%LOCALAPPDATA%\slint-file-manager\session.json` 格式与降级规则）：详见 [docs/panes-tabs-session.md](docs/panes-tabs-session.md)。会话相关启动参数：`--data-dir=目录`（测试一律用临时目录，也可用环境变量 `SFM_DATA_DIR`）、`--no-session`。真实程序会话验证：`powershell -File scripts/verify-session.ps1`（已含在 `scripts/verify.ps1`）。
- 代码结构：`src/browse/`（纯逻辑：目录读取、排序、历史、地址解析、浏览上下文 `Tab`、`Pane`／`Workspace`、会话存取 `session`，带单元测试）→ `src/app/`（Slint 接线、后台读取与结果回传）→ `ui/`（`main.slint` 窗口外壳，`browser-pane.slint` 浏览面板，`file-views.slint` 网格／列表，`theme.slint` A2 视觉常量）；`src/shell.rs` 封装 Windows 默认应用打开与系统文字缩放
- 缩略图路线与首屏测量：`powershell -File scripts/perf-thumbs.ps1 [-Dir 自己的目录] [-SkipProduct]`（冷／热／重启、路线对比、取消与慢文件隔离；`-Dir` 只读、只输出统计）；结论见 [docs/research/0005-thumbnail-route.md](docs/research/0005-thumbnail-route.md)；原型是独立 crate `prototypes/thumb-probe`，测试 `cargo test --manifest-path prototypes/thumb-probe/Cargo.toml --jobs 6`
- 图片缩略图与持久缓存（生成路线、调度与状态、缓存格式与失效、容量依据、首屏计时与实测）：详见 [docs/thumbnails.md](docs/thumbnails.md)。缓存目录 `%LOCALAPPDATA%\slint-file-manager\thumbs\v1`，测量与测试用环境变量 `SFM_THUMB_CACHE_DIR`；首屏测量 `powershell -File scripts/perf-thumbs.ps1 -ProductOnly [-Single]`。
- 调试观测框架（无头驱动、截图对比、逐帧动画、帧耗时日志）：详见 [docs/debug-harness.md](docs/debug-harness.md)。
  - 元素树 / 交互脚本：`powershell -File scripts/harness.ps1 tree|run`（基于 `%TEMP%\sfm-harness\样例目录` 的确定性样例目录，每次运行重建）
  - 截图对比基线 / 更新基线：`... harness.ps1 compare` / `... harness.ps1 bless`（退出码 0 一致、1 回归、2 无法判断、3 不稳定）
  - 动画逐帧采集（帧图、曲线、联系表、GIF）：`... harness.ps1 frames`
  - 真实窗口帧耗时日志：`slint-file-manager.exe --debug-perf`（默认关闭，写 `logs/`）；真实窗口截图：`scripts/capture-window.ps1`
  - 整体验证：`powershell -File scripts/verify-harness.ps1`
  - 界面元素需按手册的标识约定写 id 与 `accessible-label`；文字与背景要显式写颜色。
- 删除、回收站与还原（Delete 移到回收站、Shift+Delete 永久删除，都先弹应用内确认；回收站是虚拟位置 `shell:RecycleBinFolder`，用外壳命名空间读取，视图里“还原所选项目”；不提供清空回收站）：设计与安全约束见 [docs/delete-recycle-bin.md](docs/delete-recycle-bin.md)。代码：`src/fileops.rs`（后端接口、逐项汇报、临时目录“回收站” `StashBackend`）、`src/recycle.rs`（Windows 外壳实现）、`src/app/ops.rs`（确认框与后台执行）。真实回收站测试只用临时夹具，运行与清扫见 `scripts/run-recycle-tests.ps1`。
- 目录／全盘搜索（基本搜索自研遍历始终可用，Everything 为可选增强；查询语法、状态区分、规模与耗时）：详见 [docs/search.md](docs/search.md)，后端比较与集成契约见 [docs/research/0011-search-backend.md](docs/research/0011-search-backend.md)。代码：`src/search/`、`src/app/search.rs`、`ui/search.slint`。`SFM_SEARCH_BACKEND=basic` 强制只用基本搜索；真实规模测量 `harness search-bench`。
- 新建、重命名与文件传输（Ctrl+Shift+N／工具栏“新建”、F2 重命名、Ctrl+C/X/V 与资源管理器互通、应用内与资源管理器双向拖放；同卷默认移动、跨卷默认复制，Ctrl 复制、Shift 移动；同名冲突只有“保留两者／跳过／取消”，没有替换，从不静默覆盖或删除）：设计、限制与测试见 [docs/create-rename-transfer.md](docs/create-rename-transfer.md)。代码：`src/naming.rs`（命名规则）、`src/xfer.rs`（传输计划与不覆盖原语）、`src/shellop.rs`（IFileOperation）、`src/clipboard.rs`、`src/dragdrop.rs`（OLE 拖放）、`src/app/transfer.rs`（控制器）。无头交互脚本 `src/bin/harness/xfer.rs`（`scripts/harness.ps1 run`），测试只用临时夹具与内存剪贴板。
- 窗口最大化（无边框窗口最大化只占所在显示器工作区；自动隐藏任务栏让出 1 像素）：实测与限制见 [ADR 0011](docs/adr/0011-maximize-work-area.md)；代码 `src/render.rs`（`keep_taskbar_reachable`）；检查 `powershell -File scripts/verify-maximize.ps1`（已含在 `scripts/verify.ps1`）。
- 系统打开入口接管（第二阶段，#16 研究结论供 #17/#18 使用：入口清单、`HKCU\Software\Classes` 下 `Directory`／`Drive` 默认动词接管、Win+E、未覆盖项与恢复规则）：见 [docs/research/0016-system-entry-takeover.md](docs/research/0016-system-entry-takeover.md)。代码 `src/sysentry/`（`args` 外部请求参数解析纯函数；`takeover` 备份／应用／恢复与复核；`winreg` 注册表实现），尚未接入主程序。**测试只许写 `HKCU\Software\slint-file-manager-test\<随机>`（`winreg::TestRoot`），不得在开发机写真实文件关联**；真实接管只在虚拟机或测试账户验证（验收清单 Issue #16 章节）。
- 系统入口接管与单实例（“更多 → 设置…”里的“用本应用打开文件夹”，只写 `HKCU\Software\Classes` 的 `Directory`／`Drive` 动词与 Win+E；写入前备份到数据目录，停用逐值恢复；启动自检；`--entry` 参数；每个数据目录一个实例，第二个进程经命名管道转发）：行为、写入的注册表项与手工恢复见 [docs/system-entry.md](docs/system-entry.md)。代码：`src/sysentry/`（`args` 参数、`takeover` 备份恢复、`manage` 状态与启停、`instance` 单实例、`request` 请求与路由）、`src/app/entry.rs`、`ui/settings.slint`。**严禁在真实 `HKCU\Software\Classes`、HKCR、HKLM 写入**：测试一律用 `HKCU\Software\slint-file-manager-test\<随机>`（环境变量 `SFM_TEST_REG_ROOT`，其他路径会被拒绝）或内存实现；真实程序验证 `scripts/verify-instance.ps1`（已含在 `scripts/verify.ps1`）。
- “在文件夹中显示”接管（#18，与上一条共用开关；已接管时把每个标签登记进 `IShellWindows`，`SHOpenFolderAndSelectItems` 找到已打开的文件夹就切标签并选中；兼容清单与限制见 [docs/system-entry.md](docs/system-entry.md) 第 8 节，证据见研究 0016 第 11 节）。代码：`src/sysentry/shellwin.rs`、`src/app/reveal.rs`；原型宿主与测试调用器 `src/bin/reveal-probe`（不进分发包）。真实程序验证 `scripts/verify-reveal.ps1`（已含在 `scripts/verify.ps1`，测试根 + 临时目录）；`SFM_REVEAL_TRACE=1` 输出外壳调用。
- 文件选择对话框测试宿主与系统对话框基准（#23，第三阶段验证用；独立 crate `tools/dialog-host`，覆盖 `IFileOpenDialog`／`IFileSaveDialog`、`GetOpenFileNameW`／`GetSaveFileNameW`、`SHBrowseForFolderW`，x64 与 x86；命令行指定“方式＋场景＋参数”弹框并输出 JSON，另有带按钮窗口）：构建、参数、场景表与能证明／不能证明什么见 [docs/dialog-host.md](docs/dialog-host.md)。基准与比对 `powershell -File scripts/dialog-baseline.ps1 -Mode list|record|compare|diff|selftest`（基准入库 `tests/dialog-baseline/`，Windows 11 基准未覆盖）；构建＋自测＋窗口冒烟＋比对 `powershell -File scripts/verify-dialog-host.ps1 [-Quick]`（不含在 `scripts/verify.ps1`）。自动驱动只操作测试宿主进程自己的窗口，夹具在 `%TEMP%\sfm-dialog-host`。
- 视觉参考：仅 [A2](docs/concepts/file-manager/concept.md)；被淘汰的概念图不入库，不得当作依据。
- Windows 11 尚无环境，兼容性未验证。
