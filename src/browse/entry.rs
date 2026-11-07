//! 目录读取：把一个目录的直属条目读成 [`Entry`] 列表，并把失败归类为界面能区分的几种情况。
//!
//! - 只读取目录枚举自带的元数据（Windows 上来自 `FindNextFileW`），不打开文件内容，
//!   因而不会触发云端占位文件下载。
//! - 与资源管理器的默认设置一致，隐藏属性的条目不列出。
//! - 文件名保留原始 `OsString` 用于拼接路径，显示用的 `name` 只做有损转换，
//!   避免含特殊字符的文件名被转换后指向错误路径。

use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::time::SystemTime;

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// 目录中的一个条目（文件或文件夹）。
#[derive(Clone, Debug)]
pub struct Entry {
    /// 原始文件名，用于拼接路径。
    pub os_name: OsString,
    /// 显示用文件名。
    pub name: String,
    /// 是否文件夹（含目录联接与目录符号链接）。
    pub is_dir: bool,
    /// 文件大小（字节），文件夹为 0。
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// 小写扩展名（不含点），文件夹为空。
    pub ext: String,
    /// 以 NUL 结尾的 UTF-16 文件名，供 `StrCmpLogicalW` 排序使用。
    pub(crate) wide: Box<[u16]>,
}

impl Entry {
    pub fn new(os_name: OsString, is_dir: bool, size: u64, modified: Option<SystemTime>) -> Entry {
        let name = os_name.to_string_lossy().into_owned();
        let ext = if is_dir {
            String::new()
        } else {
            Path::new(&os_name)
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        };
        let wide = os_name
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
            .into_boxed_slice();
        Entry {
            os_name,
            name,
            is_dir,
            size: if is_dir { 0 } else { size },
            modified,
            ext,
            wide,
        }
    }
}

/// 读取失败的类别，界面据此显示不同的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListErrorKind {
    /// 路径不存在、无效，或网络位置找不到。
    NotFound,
    /// 没有读取权限。
    AccessDenied,
    /// 路径指向文件而不是文件夹。
    NotADirectory,
    /// 其他错误（设备未就绪、I/O 错误等）。
    Other,
}

#[derive(Clone, Debug)]
pub struct ListError {
    pub kind: ListErrorKind,
    pub os_code: Option<i32>,
    /// 系统给出的错误说明（不含路径）。
    pub message: String,
}

/// 把系统错误归类。
pub fn classify(e: &std::io::Error) -> ListErrorKind {
    use std::io::ErrorKind as K;
    match e.raw_os_error() {
        // 文件/路径不存在、名称语法错误、错误路径名、找不到网络路径或网络名
        Some(2 | 3 | 53 | 67 | 123 | 161) => ListErrorKind::NotFound,
        Some(5) => ListErrorKind::AccessDenied,
        // ERROR_DIRECTORY：目录名称无效（通常是对文件执行了目录枚举）
        Some(267) => ListErrorKind::NotADirectory,
        _ => match e.kind() {
            K::NotFound => ListErrorKind::NotFound,
            K::PermissionDenied => ListErrorKind::AccessDenied,
            K::NotADirectory => ListErrorKind::NotADirectory,
            _ => ListErrorKind::Other,
        },
    }
}

fn to_error(e: std::io::Error) -> ListError {
    ListError {
        kind: classify(&e),
        os_code: e.raw_os_error(),
        message: e.to_string(),
    }
}

/// 读取目录的直属条目（未排序）。
pub fn list_dir(path: &Path) -> Result<Vec<Entry>, ListError> {
    let rd = std::fs::read_dir(path).map_err(to_error)?;
    let mut out = Vec::new();
    for de in rd {
        // 单个条目读取失败（例如枚举过程中被删除）时跳过该条目，不让整个目录失败
        let Ok(de) = de else { continue };
        let (attrs, size, modified) = match de.metadata() {
            Ok(md) => (md.file_attributes(), md.len(), md.modified().ok()),
            Err(_) => {
                let is_dir = de.file_type().map(|t| t.is_dir()).unwrap_or(false);
                (if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 }, 0, None)
            }
        };
        if attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
            continue;
        }
        out.push(Entry::new(
            de.file_name(),
            attrs & FILE_ATTRIBUTE_DIRECTORY != 0,
            size,
            modified,
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir()
            .join("sfm-unit")
            .join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lists_files_and_dirs_with_chinese_names() {
        let d = temp("list");
        fs::create_dir(d.join("子目录")).unwrap();
        fs::write(d.join("说明 文档.txt"), b"12345").unwrap();
        let mut v = list_dir(&d).unwrap();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(v.len(), 2);
        let dir = v.iter().find(|e| e.is_dir).unwrap();
        assert_eq!(dir.name, "子目录");
        assert_eq!(dir.size, 0);
        let f = v.iter().find(|e| !e.is_dir).unwrap();
        assert_eq!(f.size, 5);
        assert_eq!(f.ext, "txt");
        assert!(
            d.join(&f.os_name).is_file(),
            "原始文件名拼接后应指向真实文件"
        );
    }

    #[test]
    fn classifies_errors() {
        let d = temp("errors");
        let missing = d.join("不存在");
        assert_eq!(
            list_dir(&missing).unwrap_err().kind,
            ListErrorKind::NotFound
        );
        let file = d.join("a.txt");
        fs::write(&file, b"x").unwrap();
        assert_eq!(
            list_dir(&file).unwrap_err().kind,
            ListErrorKind::NotADirectory
        );
    }

    #[test]
    fn empty_dir_is_ok() {
        let d = temp("empty");
        assert!(list_dir(&d).unwrap().is_empty());
    }
}
