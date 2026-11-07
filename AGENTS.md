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
- 调试观测框架（无头驱动、截图对比、逐帧动画、帧耗时日志）：详见 [docs/debug-harness.md](docs/debug-harness.md)。
  - 元素树 / 交互脚本：`powershell -File scripts/harness.ps1 tree|run`（基于 `%TEMP%\sfm-harness\样例目录` 的确定性样例目录，每次运行重建）
  - 截图对比基线 / 更新基线：`... harness.ps1 compare` / `... harness.ps1 bless`（退出码 0 一致、1 回归、2 无法判断、3 不稳定）
  - 动画逐帧采集（帧图、曲线、联系表、GIF）：`... harness.ps1 frames`
  - 真实窗口帧耗时日志：`slint-file-manager.exe --debug-perf`（默认关闭，写 `logs/`）；真实窗口截图：`scripts/capture-window.ps1`
  - 整体验证：`powershell -File scripts/verify-harness.ps1`
  - 界面元素需按手册的标识约定写 id 与 `accessible-label`；文字与背景要显式写颜色。
- 视觉参考：仅 [A2](docs/concepts/file-manager/concept.md)；被淘汰的概念图不入库，不得当作依据。
- Windows 11 尚无环境，兼容性未验证。
