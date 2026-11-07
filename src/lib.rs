//! 库入口：主程序与调试 harness 共用同一套界面与接线。
//!
//! - [`browse`]：与界面无关的浏览核心（目录读取、排序、历史、地址解析、浏览上下文）。
//! - [`app`]：把浏览核心接到 Slint 界面。
//! - [`shell`]：Windows 外壳交互（默认应用打开、系统文字缩放）。
//! - [`perf`]：调试开关下的帧耗时与界面事件日志。

slint::include_modules!();

pub mod app;
pub mod browse;
pub mod perf;
pub mod shell;
