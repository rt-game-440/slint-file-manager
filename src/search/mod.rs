//! 搜索核心（与界面无关，可直接单测）。依据 Issue #11、ADR 0007、ADR 0004。
//!
//! - [`query`]：查询语法（名称／扩展名／路径，大小写不敏感，多条件 AND）。
//! - [`walk`]：基本搜索——后台线程遍历目录树，可取消、增量出结果；只读目录枚举自带的元数据，
//!   不打开文件，因此不会触发云占位文件下载；符号链接／联接点不进入，未同步到本机的云文件夹不展开。
//! - [`everything`]：可选增强（Everything IPC），未安装／未运行／索引未就绪／查询失败可区分。
//! - [`engine`]：选择后端、失败时回退、把事件流交给调用方。
//!
//! 基本搜索始终可用：增强只是更快，缺失或失败时由基本搜索完成同一个查询，且不混合两套结果。

pub mod engine;
pub mod everything;
pub mod query;
pub mod walk;

pub use engine::{
    BackendKind, BackendPref, Event, Notice, NoticeCode, NoticeKind, Request, Summary, run,
};
pub use everything::{Availability, Enhancer, EnhancerError};
pub use query::Query;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// 单次搜索显示的结果上限；超出时标记“已截断”，不无限增长内存。
pub const MAX_HITS: usize = 20_000;

/// 取消标记，可克隆共享。
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// 一个结果。名称与所在目录由路径派生，不额外保存。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    pub is_dir: bool,
}

impl Hit {
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
    }
    pub fn parent(&self) -> String {
        self.path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// 搜索范围。
#[derive(Clone, Debug)]
pub enum Scope {
    /// 一个目录及其全部子目录
    Dir(PathBuf),
    /// 全盘：本机固定盘、已连接的可移动盘、云盘本地目录（调用方在发起时枚举出根目录）
    AllDisks(Vec<PathBuf>),
}

impl Scope {
    pub fn roots(&self) -> Vec<PathBuf> {
        match self {
            Scope::Dir(d) => vec![d.clone()],
            Scope::AllDisks(r) => r.clone(),
        }
    }

    /// 路径是否在范围之内（Windows 路径按大小写不敏感比较）。
    pub fn contains(&self, p: &Path) -> bool {
        self.roots().iter().any(|r| starts_with_ci(p, r))
    }

    pub fn is_all(&self) -> bool {
        matches!(self, Scope::AllDisks(_))
    }
}

fn comp_key(c: Component) -> String {
    c.as_os_str().to_string_lossy().to_lowercase()
}

pub fn starts_with_ci(p: &Path, root: &Path) -> bool {
    let mut pi = p.components();
    for rc in root.components() {
        match pi.next() {
            Some(pc) if comp_key(pc) == comp_key(rc) => {}
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_contains_is_case_insensitive_and_component_wise() {
        let s = Scope::Dir(PathBuf::from("C:\\Users\\A"));
        assert!(s.contains(Path::new("c:\\users\\a\\x.txt")));
        assert!(s.contains(Path::new("C:\\Users\\A")));
        assert!(
            !s.contains(Path::new("C:\\Users\\AB\\x.txt")),
            "不能按字符串前缀判断"
        );
        let all = Scope::AllDisks(vec![PathBuf::from("C:\\"), PathBuf::from("D:\\")]);
        assert!(all.contains(Path::new("d:\\a")));
        assert!(!all.contains(Path::new("E:\\a")));
    }
}
