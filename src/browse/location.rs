//! 位置与地址：解析地址栏输入、生成路径导航分段、显示文本。只做字符串层面的处理，不访问磁盘。
//!
//! 回收站不是磁盘路径：它用一个虚拟位置 [`RECYCLE_BIN`]（外壳的规范名）表示，由外壳命名空间读取，
//! 从不当作普通目录枚举。地址栏显示为“回收站”，输入“回收站”或 `shell:RecycleBinFolder` 都会进入。

use std::path::{Component, Path, PathBuf};

/// 回收站的虚拟位置（外壳规范名，不是真实路径）。
pub const RECYCLE_BIN: &str = "shell:RecycleBinFolder";
/// 回收站在界面上的名称。
pub const RECYCLE_BIN_LABEL: &str = "回收站";

/// 回收站的位置值。
pub fn recycle_bin_path() -> PathBuf {
    PathBuf::from(RECYCLE_BIN)
}

/// 是否为回收站的虚拟位置。
pub fn is_recycle_bin(p: &Path) -> bool {
    p.as_os_str()
        .to_str()
        .is_some_and(|s| s.eq_ignore_ascii_case(RECYCLE_BIN))
}

/// 解析地址栏输入为绝对路径。
///
/// - 去掉首尾空白与成对引号；展开 `%VAR%` 环境变量；`/` 视为 `\`。
/// - `C:` 视为 `C:\`；相对路径相对于 `current` 解析；按字面消去 `.` 与 `..`。
/// - 输入为空、或是相对路径但没有当前位置时返回 `None`。
pub fn parse_address(input: &str, current: Option<&Path>) -> Option<PathBuf> {
    let mut s = input.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s = s[1..s.len() - 1].trim();
    }
    if s.is_empty() {
        return None;
    }
    if s == RECYCLE_BIN_LABEL || s.eq_ignore_ascii_case(RECYCLE_BIN) {
        return Some(recycle_bin_path());
    }
    let s = expand_env(s).replace('/', "\\");
    let p = PathBuf::from(&s);
    let p = if has_root(&p) {
        p
    } else if is_bare_drive(&s) {
        PathBuf::from(format!("{s}\\"))
    } else {
        current?.join(p)
    };
    Some(normalize(&p))
}

fn is_bare_drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn has_root(p: &Path) -> bool {
    let mut c = p.components();
    matches!(c.next(), Some(Component::Prefix(_))) && matches!(c.next(), Some(Component::RootDir))
}

/// 展开 `%VAR%`；未定义的变量原样保留。
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) if j > 0 => {
                let name = &after[..j];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[j + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 按字面消去 `.` 与 `..`（不越过根）。
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut depth = 0usize;
    for c in p.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if depth > 0 {
                    out.pop();
                    depth -= 1;
                }
            }
            Component::Normal(n) => {
                out.push(n);
                depth += 1;
            }
        }
    }
    out
}

/// 路径导航中的一段。
#[derive(Clone, Debug, PartialEq)]
pub struct Crumb {
    pub label: String,
    pub path: PathBuf,
}

/// 把路径拆成导航分段：根（如 `C:`、`\\server\share`）+ 各级文件夹。
pub fn crumbs(p: &Path) -> Vec<Crumb> {
    if is_recycle_bin(p) {
        return vec![Crumb {
            label: RECYCLE_BIN_LABEL.into(),
            path: recycle_bin_path(),
        }];
    }
    let mut out = Vec::new();
    let mut acc = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Prefix(pre) => {
                acc.push(c.as_os_str());
                out.push(Crumb {
                    label: pre.as_os_str().to_string_lossy().into_owned(),
                    path: PathBuf::new(),
                });
            }
            Component::RootDir => {
                acc.push(c.as_os_str());
                if let Some(last) = out.last_mut() {
                    last.path = acc.clone();
                } else {
                    out.push(Crumb {
                        label: "\\".into(),
                        path: acc.clone(),
                    });
                }
            }
            Component::Normal(n) => {
                acc.push(n);
                out.push(Crumb {
                    label: n.to_string_lossy().into_owned(),
                    path: acc.clone(),
                });
            }
            Component::CurDir | Component::ParentDir => {}
        }
    }
    out
}

/// 地址栏显示文本。
pub fn display(p: &Path) -> String {
    if is_recycle_bin(p) {
        return RECYCLE_BIN_LABEL.into();
    }
    p.to_string_lossy().into_owned()
}

/// 标签标题：最后一级名称；磁盘根显示为 `C:`。
pub fn leaf_label(p: &Path) -> String {
    if is_recycle_bin(p) {
        return RECYCLE_BIN_LABEL.into();
    }
    match p.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => crumbs(p)
            .first()
            .map(|c| c.label.clone())
            .unwrap_or_else(|| display(p)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_inputs() {
        let cur = Path::new(r"C:\用户\文档");
        assert_eq!(
            parse_address("  \"D:/照片/2024\"  ", None).unwrap(),
            PathBuf::from(r"D:\照片\2024")
        );
        assert_eq!(parse_address("c:", None).unwrap(), PathBuf::from(r"c:\"));
        assert_eq!(
            parse_address(r"..\下载", Some(cur)).unwrap(),
            PathBuf::from(r"C:\用户\下载")
        );
        assert_eq!(
            parse_address(r"C:\a\.\b\..\..\..", None).unwrap(),
            PathBuf::from(r"C:\")
        );
        assert_eq!(
            parse_address(r"\\server\share\x", None).unwrap(),
            PathBuf::from(r"\\server\share\x")
        );
        assert!(parse_address("   ", Some(cur)).is_none());
        assert!(parse_address("相对", None).is_none());
    }

    #[test]
    fn recycle_bin_is_a_virtual_location() {
        let bin = recycle_bin_path();
        assert!(is_recycle_bin(&bin));
        assert!(is_recycle_bin(Path::new("SHELL:recyclebinfolder")));
        assert!(!is_recycle_bin(Path::new(r"C:\$Recycle.Bin")));
        assert_eq!(
            parse_address("回收站", Some(Path::new(r"C:\x"))).unwrap(),
            bin
        );
        assert_eq!(
            parse_address(" shell:RecycleBinFolder ", None).unwrap(),
            bin
        );
        assert_eq!(display(&bin), "回收站");
        assert_eq!(leaf_label(&bin), "回收站");
        let c = crumbs(&bin);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].label.as_str(), c[0].path.clone()), ("回收站", bin));
    }

    #[test]
    fn expands_env_vars() {
        // SAFETY: 单测内设置本进程专用变量，不与其他测试共享名称。
        unsafe { std::env::set_var("SFM_TEST_DIR", r"E:\数据") };
        assert_eq!(
            parse_address(r"%SFM_TEST_DIR%\子", None).unwrap(),
            PathBuf::from(r"E:\数据\子")
        );
        assert_eq!(expand_env("100% 完成"), "100% 完成");
        assert_eq!(expand_env("%SFM_UNDEFINED_X%"), "%SFM_UNDEFINED_X%");
    }

    #[test]
    fn crumbs_and_labels() {
        let c = crumbs(Path::new(r"C:\用户\很长的文件夹名"));
        let labels: Vec<_> = c.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["C:", "用户", "很长的文件夹名"]);
        assert_eq!(c[0].path, PathBuf::from(r"C:\"));
        assert_eq!(c[2].path, PathBuf::from(r"C:\用户\很长的文件夹名"));
        assert_eq!(leaf_label(Path::new(r"C:\")), "C:");
        assert_eq!(leaf_label(Path::new(r"C:\用户")), "用户");
        let u = crumbs(Path::new(r"\\srv\share\a"));
        assert_eq!(u[0].label, r"\\srv\share");
        assert_eq!(u[0].path, PathBuf::from(r"\\srv\share\"));
    }
}
