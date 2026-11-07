//! 库入口：主程序与调试 harness 共用同一套界面与接线。
//!
//! - [`browse`]：与界面无关的浏览核心（目录读取、排序、历史、地址解析、浏览上下文）。
//! - [`app`]：把浏览核心接到 Slint 界面。
//! - [`fileops`]：删除、回收站浏览与恢复的操作层（后端接口、批量执行与汇报）；[`recycle`] 是 Windows 回收站实现。
//! - [`shell`]：Windows 外壳交互（默认应用打开、系统文字缩放）。
//! - [`shell_menu`]：文件列表的原生右键菜单（外壳 `IContextMenu`、NanaZip 等扩展）。
//! - [`search`]：目录／全盘搜索（基本搜索与可选增强）。
//! - [`thumbs`]：图片缩略图的解码、持久缓存与调度（与界面无关）。
//! - [`preview`]：图片大图预览的解码、资源上限与后台加载器（与界面无关；界面接线在 `app::preview`）。
//! - [`perf`]：调试开关下的帧耗时与界面事件日志。

slint::include_modules!();

pub mod app;
pub mod browse;
pub mod clipboard;
pub mod dragdrop;
pub mod fileops;
pub mod naming;
pub mod perf;
pub mod preview;
pub mod recycle;
pub mod search;
pub mod shell;
pub mod shell_menu;
pub mod shellnew;
pub mod shellop;
pub mod thumbs;
pub mod xfer;
