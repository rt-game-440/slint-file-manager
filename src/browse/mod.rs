//! 浏览核心：与界面、线程无关的目录读取、排序、历史、地址解析和浏览上下文，可直接单测。
//! 界面接线见 `crate::app`。

pub mod entry;
pub mod format;
pub mod history;
pub mod location;
pub mod sort;
pub mod tab;

pub use entry::{Entry, ListError, ListErrorKind, list_dir};
pub use sort::{SortKey, SortSpec};
pub use tab::{Activation, LoadRequest, LoadState, Loaded, Nav, Tab, ViewMode};
