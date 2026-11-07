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
- 定向验证（fmt、clippy、构建、`--smoke` 启动退出）：`powershell -File scripts/verify.ps1`
- 生成分发包（`dist/*.zip`）：`powershell -File scripts/dist.ps1`
- 手动冒烟：`target\release\slint-file-manager.exe --smoke`（约 1 秒后自动退出，退出码 0）
- 视觉参考：仅 [A2](docs/concepts/file-manager/concept.md)；被淘汰的概念图不入库，不得当作依据。
- Windows 11 尚无环境，兼容性未验证。
