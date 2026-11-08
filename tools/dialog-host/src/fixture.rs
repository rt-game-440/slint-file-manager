//! 夹具：每次在 `%TEMP%` 下重建，不读写用户真实目录。
//!
//! ```text
//! <root>\docs\alpha.txt beta.txt gamma.txt 报告.txt existing.txt notes.md image.png photo.jpg noext
//! <root>\docs\sub\inner.txt
//! <root>\empty\
//! ```
//! 安全约束：根目录必须位于临时目录之下，且路径里有名为 `sfm-dialog-host` 的一级，否则拒绝（防止误删）。

use std::path::{Component, Path, PathBuf};

pub const FILES: &[&str] = &[
    "docs/alpha.txt",
    "docs/beta.txt",
    "docs/gamma.txt",
    "docs/报告.txt",
    "docs/existing.txt",
    "docs/notes.md",
    "docs/image.png",
    "docs/photo.jpg",
    "docs/noext",
    "docs/sub/inner.txt",
];

pub fn default_root() -> PathBuf {
    std::env::temp_dir().join("sfm-dialog-host").join("fixture")
}

fn norm(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_lowercase()),
            Component::Prefix(s) => Some(
                s.as_os_str()
                    .to_string_lossy()
                    .trim_start_matches(r"\\?\")
                    .to_lowercase(),
            ),
            _ => None,
        })
        .collect()
}

fn check_root(root: &Path) -> Result<PathBuf, String> {
    let abs = std::path::absolute(root).map_err(|e| e.to_string())?;
    let tmp = std::path::absolute(std::env::temp_dir()).map_err(|e| e.to_string())?;
    let (a, t) = (norm(&abs), norm(&tmp));
    let under_tmp = a.len() > t.len() && a[..t.len()] == t[..];
    let marked = a.iter().skip(t.len()).any(|s| s == "sfm-dialog-host");
    if !under_tmp || !marked {
        return Err(format!(
            "拒绝重建夹具：{} 必须位于 {} 之下且含目录名 sfm-dialog-host",
            abs.display(),
            tmp.display()
        ));
    }
    Ok(abs)
}

pub fn rebuild(root: &Path) -> Result<PathBuf, String> {
    let root = check_root(root)?;
    if root.exists() {
        std::fs::remove_dir_all(&root).map_err(|e| format!("清理旧夹具失败：{e}"))?;
    }
    for rel in FILES {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(&p, format!("夹具文件 {rel}\n").as_bytes()).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(root.join("empty")).map_err(|e| e.to_string())?;
    Ok(root)
}
