//! 文件名规则：新建与重命名时的名称校验，以及“新建文件夹 (2)”“报告 - 副本.txt”“报告 (2).txt”这类重名处理。
//!
//! 纯逻辑、与界面和磁盘后端无关，可直接单测。规则按 Windows 的命名限制：
//! - 不能为空；不能含 `< > : " / \ | ? *` 与控制字符；不能以空格或句点结尾（开头与结尾的空白会先被去掉）；
//! - 不能是设备保留名（CON、PRN、AUX、NUL、COM1–9、LPT1–9，带不带扩展名都不行）；
//! - 不超过 255 个 UTF-16 单元。

use std::path::Path;

/// 名称不合法的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameError {
    Empty,
    /// 含有不允许的字符（显示用字符）。
    IllegalChar(char),
    /// 设备保留名。
    Reserved(String),
    /// 以句点结尾（空格已被去掉）。
    TrailingDot,
    TooLong,
    /// 只有 `.` 或 `..`。
    DotOnly,
}

impl NameError {
    /// 给用户看的原因。
    pub fn text(&self) -> String {
        match self {
            NameError::Empty => "名称不能为空".into(),
            NameError::IllegalChar(c) if c.is_control() => "名称里不能包含控制字符".into(),
            NameError::IllegalChar(c) => {
                format!("名称里不能包含这些字符：\\ / : * ? \" < > |（发现“{c}”）")
            }
            NameError::Reserved(n) => format!("“{n}”是系统保留的设备名称，不能用作文件名"),
            NameError::TrailingDot => "名称不能以句点结尾".into(),
            NameError::TooLong => "名称太长（最多 255 个字符）".into(),
            NameError::DotOnly => "名称不能只有句点".into(),
        }
    }
}

const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];

fn is_reserved(stem: &str) -> bool {
    let up = stem.trim_end().to_uppercase();
    if RESERVED.contains(&up.as_str()) || up == "CONIN$" || up == "CONOUT$" {
        return true;
    }
    for p in ["COM", "LPT"] {
        if let Some(d) = up.strip_prefix(p)
            && d.len() == 1
            && d.chars().all(|c| c.is_ascii_digit())
            && d != "0"
        {
            return true;
        }
    }
    false
}

/// 清理并校验用户输入的名称：去掉首尾空白，返回可直接用于创建或重命名的名称。
pub fn clean_name(input: &str) -> Result<String, NameError> {
    let name = input.trim();
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name == "." || name == ".." {
        return Err(NameError::DotOnly);
    }
    for c in name.chars() {
        if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            return Err(NameError::IllegalChar(c));
        }
    }
    if name.ends_with('.') {
        return Err(NameError::TrailingDot);
    }
    if name.encode_utf16().count() > 255 {
        return Err(NameError::TooLong);
    }
    let stem = name.split('.').next().unwrap_or(name);
    if is_reserved(stem) {
        return Err(NameError::Reserved(stem.to_uppercase()));
    }
    Ok(name.to_string())
}

/// 把名称拆成（主名, 带点的扩展名）。文件夹不拆分；以点开头且只有一个点的名称（.gitignore）整体算主名。
pub fn split_ext(name: &str, is_dir: bool) -> (&str, &str) {
    if is_dir {
        return (name, "");
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// 重命名时默认选中的范围：主名（不含扩展名）的字节偏移 `(开始, 结束)`，与 Windows 资源管理器一致。
pub fn rename_selection(name: &str, is_dir: bool) -> (usize, usize) {
    (0, split_ext(name, is_dir).0.len())
}

/// 重名时的命名风格。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// 新建：`新建文件夹`、`新建文件夹 (2)`；模板文件为 `新建 文本文档.txt`、`新建 文本文档 (2).txt`。
    New,
    /// 在同一文件夹里复制：`报告 - 副本.txt`、`报告 - 副本 (2).txt`。
    Duplicate,
    /// 冲突时“保留两者”：`报告 (2).txt`、`报告 (3).txt`。
    KeepBoth,
}

/// 在 `exists` 判定某名称已被占用的前提下，给出第一个可用的名称。`base` 是主名（不含扩展名），`ext` 带点或为空。
pub fn unique_name(base: &str, ext: &str, style: Style, exists: &dyn Fn(&str) -> bool) -> String {
    let first = match style {
        Style::New | Style::KeepBoth => format!("{base}{ext}"),
        Style::Duplicate => format!("{base} - 副本{ext}"),
    };
    // New 与 KeepBoth 的第一候选分别是“没有后缀”；KeepBoth 面对的是已存在的原名，所以从 (2) 开始
    if style != Style::KeepBoth && !exists(&first) {
        return first;
    }
    let stem = match style {
        Style::Duplicate => format!("{base} - 副本"),
        _ => base.to_string(),
    };
    (2u32..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|c| !exists(c))
        .expect("总能找到可用名称")
}

/// 磁盘上的重名检查（不区分大小写由文件系统决定）。
pub fn unique_in(dir: &Path, base: &str, ext: &str, style: Style) -> String {
    unique_name(base, ext, style, &|n| {
        std::fs::symlink_metadata(dir.join(n)).is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_accepts_normal_names() {
        assert_eq!(
            clean_name("  报告 最终版.txt  ").unwrap(),
            "报告 最终版.txt"
        );
        assert_eq!(clean_name(".gitignore").unwrap(), ".gitignore");
        assert_eq!(clean_name("a.b.c").unwrap(), "a.b.c");
        assert_eq!(clean_name("COM10").unwrap(), "COM10");
        assert_eq!(clean_name("console").unwrap(), "console");
    }

    #[test]
    fn clean_rejects_illegal() {
        assert_eq!(clean_name("").unwrap_err(), NameError::Empty);
        assert_eq!(clean_name("   ").unwrap_err(), NameError::Empty);
        for c in ['<', '>', ':', '"', '/', '\\', '|', '?', '*'] {
            assert_eq!(
                clean_name(&format!("a{c}b")).unwrap_err(),
                NameError::IllegalChar(c)
            );
        }
        assert!(matches!(
            clean_name("a\u{1}b").unwrap_err(),
            NameError::IllegalChar(_)
        ));
        assert_eq!(clean_name("abc.").unwrap_err(), NameError::TrailingDot);
        assert_eq!(clean_name("..").unwrap_err(), NameError::DotOnly);
        assert_eq!(clean_name(".").unwrap_err(), NameError::DotOnly);
        assert_eq!(
            clean_name(&"长".repeat(256)).unwrap_err(),
            NameError::TooLong
        );
        assert!(clean_name(&"长".repeat(255)).is_ok());
    }

    #[test]
    fn clean_rejects_reserved_names() {
        for n in ["con", "PRN", "Aux.txt", "nul", "COM1", "lpt9.log", "CONIN$"] {
            assert!(
                matches!(clean_name(n).unwrap_err(), NameError::Reserved(_)),
                "{n}"
            );
        }
        assert!(clean_name("COM0").is_ok());
    }

    #[test]
    fn error_texts_are_user_facing() {
        assert!(NameError::IllegalChar('?').text().contains("不能包含"));
        assert!(NameError::Reserved("CON".into()).text().contains("CON"));
    }

    #[test]
    fn rename_selection_excludes_extension() {
        assert_eq!(rename_selection("报告.txt", false), (0, "报告".len()));
        assert_eq!(rename_selection("a.tar.gz", false), (0, "a.tar".len()));
        assert_eq!(
            rename_selection(".gitignore", false),
            (0, ".gitignore".len())
        );
        assert_eq!(rename_selection("资料.备份", true), (0, "资料.备份".len()));
        assert_eq!(rename_selection("无扩展名", false), (0, "无扩展名".len()));
    }

    #[test]
    fn unique_names_follow_windows_conventions() {
        let taken = |set: &'static [&'static str]| move |n: &str| set.contains(&n);
        assert_eq!(
            unique_name("新建文件夹", "", Style::New, &taken(&[])),
            "新建文件夹"
        );
        assert_eq!(
            unique_name("新建文件夹", "", Style::New, &taken(&["新建文件夹"])),
            "新建文件夹 (2)"
        );
        assert_eq!(
            unique_name(
                "新建 文本文档",
                ".txt",
                Style::New,
                &taken(&["新建 文本文档.txt", "新建 文本文档 (2).txt"])
            ),
            "新建 文本文档 (3).txt"
        );
        assert_eq!(
            unique_name("报告", ".txt", Style::Duplicate, &taken(&["报告.txt"])),
            "报告 - 副本.txt"
        );
        assert_eq!(
            unique_name(
                "报告",
                ".txt",
                Style::Duplicate,
                &taken(&["报告 - 副本.txt"])
            ),
            "报告 - 副本 (2).txt"
        );
        assert_eq!(
            unique_name("报告", ".txt", Style::KeepBoth, &taken(&["报告.txt"])),
            "报告 (2).txt"
        );
        assert_eq!(
            unique_name(
                "报告",
                ".txt",
                Style::KeepBoth,
                &taken(&["报告.txt", "报告 (2).txt"])
            ),
            "报告 (3).txt"
        );
    }

    #[test]
    fn split_ext_cases() {
        assert_eq!(split_ext("a.txt", false), ("a", ".txt"));
        assert_eq!(split_ext("a.txt", true), ("a.txt", ""));
        assert_eq!(split_ext(".hidden", false), (".hidden", ""));
        assert_eq!(split_ext("noext", false), ("noext", ""));
    }
}
