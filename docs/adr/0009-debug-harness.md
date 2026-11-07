# 调试观测框架：复用 Slint 测试后端，harness 不进入发布产物

状态：已采纳（2026-11-07，Issue #22）。

## 决定

| 项目 | 取值 | 依据 |
| --- | --- | --- |
| 无头驱动与元素查找 | `i-slint-backend-testing`（`=1.18.1`，随 `slint` 升级） | Slint 自带，提供元素查找、输入模拟、模拟时钟；不自造 |
| 离屏截图 | 同一测试后端的 `renderer_name: software` + `Window::take_snapshot()` | 无需显示器，输出确定；与产品默认渲染器（femtovg）不同，文档中明确 |
| 动画 | `mock_elapsed_time` 推进模拟时钟，逐帧读取属性并截图 | 只验证动画定义，不代表真实流畅度 |
| 真实窗口帧耗时 | `Window::set_rendering_notifier` + 日志，开关默认关闭 | 关闭时不注册通知、不建文件 |
| 构建隔离 | `harness` Cargo 特性，独立 `harness` 二进制（`required-features`）；`build.rs` 仅在该特性下生成调试信息 | 发布产物不含测试后端与调试信息 |
| 共享代码 | `src/lib.rs`（界面与接线），`main.rs` 与 harness 共用 | 避免界面接线写两份 |

## 取舍

- 内部 crate 不遵循 semver：用 `=` 固定版本，升级 Slint 时同步检查 `harness` 能否编译、测试是否通过。
- 截图基线依赖环境（软件渲染器版本、系统字体）：以“环境指纹”区分回归与环境差异，而不是放宽容差。
- 未选择 Slint 的 `system-testing` / `mcp` 特性：它们需要额外的服务与协议依赖，当前需求用进程内 API 已满足。

使用方法见 [调试观测框架手册](../debug-harness.md)。
