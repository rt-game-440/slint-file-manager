//! 目录读取：把一个目录的直属条目读成 [`Entry`] 列表，并把失败归类为界面能区分的几种情况。
//!
//! - 只读取目录枚举自带的元数据（Windows 上来自 `FindNextFileW`），不打开文件内容，
//!   因而不会触发云端占位文件下载。
//! - 云盘状态（Issue #14）：属性带按需下载位的直接判为“仅在线”；其余只有在目录位于同步根内时才用云文件 API
//!   读占位信息（只读元数据，不打开数据流）区分“本地可用／始终保留”。
//! - 与资源管理器的默认设置一致，隐藏属性的条目不列出。
//! - 文件名保留原始 `OsString` 用于拼接路径，显示用的 `name` 只做有损转换，
//!   避免含特殊字符的文件名被转换后指向错误路径。

use crate::cloud::{self, CloudProbe, CloudState, DirCloud};
use crate::fileops::BinItem;
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
    /// 云盘状态（仅在线／本地可用／始终保留）；不在云盘里的条目为 [`CloudState::None`]。
    pub cloud: CloudState,
    /// 回收站项目的外壳标识与原位置（只有回收站视图的条目有）；此时 `modified` 是删除时间，
    /// `os_name` 是保证唯一的键（同名项目可能多次进入回收站），显示用 `name`。
    pub bin: Option<std::sync::Arc<BinItem>>,
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
            cloud: CloudState::None,
            bin: None,
        }
    }

    /// 带上云盘状态。
    pub fn with_cloud(mut self, cloud: CloudState) -> Entry {
        self.cloud = cloud;
        self
    }

    /// 回收站里的一项。显示名与扩展名取原文件名；`os_name` 换成含外壳标识摘要的唯一键
    ///（刷新后同一项键不变，选择得以保留；同名项目互不混淆）。
    pub fn from_bin(item: BinItem) -> Entry {
        let h = item.id.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ *b as u64).wrapping_mul(0x100_0000_01b3)
        });
        let mut e = Entry::new(
            OsString::from(&item.name),
            item.is_dir,
            item.size,
            item.deleted,
        );
        e.os_name = OsString::from(format!("{}|{h:016x}", item.name));
        e.bin = Some(std::sync::Arc::new(item));
        e
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

/// 读取目录的直属条目（未排序），云盘状态用系统云文件 API 判定。
pub fn list_dir(path: &Path) -> Result<Vec<Entry>, ListError> {
    list_dir_with(path, &cloud::SYSTEM).map(|(v, _)| v)
}

/// 同 [`list_dir`]，云文件 API 由 `probe` 提供（测试与 harness 注入模拟提供方）；同时返回目录所在的同步根。
pub fn list_dir_with(
    path: &Path,
    probe: &dyn CloudProbe,
) -> Result<(Vec<Entry>, Option<cloud::SyncRoot>), ListError> {
    let rd = std::fs::read_dir(path).map_err(to_error)?;
    let dc = DirCloud::new(probe, path);
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
        let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
        let name = de.file_name();
        let state = dc.state(&name, attrs, is_dir);
        out.push(Entry::new(name, is_dir, size, modified).with_cloud(state));
    }
    Ok((out, dc.into_root()))
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

    fn attrib(flag: &str, p: &std::path::Path) {
        let ok = std::process::Command::new("attrib")
            .arg(flag)
            .arg(p)
            .status()
            .unwrap()
            .success();
        assert!(ok, "attrib {flag} 失败");
    }

    /// 云盘目录枚举：状态来自属性与（模拟的）云文件 API；仅在线文件不向提供方询问占位信息，也不会被读取。
    #[test]
    fn cloud_states_come_from_attributes_and_provider_without_touching_content() {
        use crate::cloud::{CloudState, MockCloud, Pin, Placeholder, ProviderStatus};
        let d = temp("cloud-list");
        for n in [
            "在线.docx",
            "本地.docx",
            "保留.docx",
            "普通.txt",
            "固定属性.docx",
        ] {
            fs::write(d.join(n), b"x").unwrap();
        }
        fs::create_dir(d.join("在线夹")).unwrap();
        fs::create_dir(d.join("本地夹")).unwrap();
        attrib("+O", &d.join("在线.docx"));
        attrib("+O", &d.join("在线夹"));
        attrib("+P", &d.join("固定属性.docx"));
        let m = MockCloud::default();
        let root = d.to_string_lossy().to_string();
        m.add_root(&root, "OneDrive", ProviderStatus::Connected);
        let ph = |pin| Placeholder {
            on_disk: 1,
            pin,
            in_sync: true,
        };
        m.set_placeholder(&d.join("本地.docx").to_string_lossy(), ph(Pin::Unspecified));
        m.set_placeholder(&d.join("保留.docx").to_string_lossy(), ph(Pin::Pinned));

        let (v, sr) = list_dir_with(&d, &m).unwrap();
        let state = |n: &str| v.iter().find(|e| e.name == n).unwrap().cloud;
        assert_eq!(sr.as_ref().map(|r| r.provider.as_str()), Some("OneDrive"));
        assert_eq!(state("在线.docx"), CloudState::OnlineOnly);
        assert_eq!(
            state("在线夹"),
            CloudState::OnlineOnly,
            "未填充的文件夹同样标为仅在线"
        );
        assert_eq!(state("本地.docx"), CloudState::Local);
        assert_eq!(
            state("保留.docx"),
            CloudState::AlwaysKeep,
            "固定状态来自云文件 API，不只看属性"
        );
        assert_eq!(
            state("固定属性.docx"),
            CloudState::AlwaysKeep,
            "属性带固定位也算始终保留"
        );
        assert_eq!(
            state("普通.txt"),
            CloudState::None,
            "同步根内但没有占位信息的普通文件不标记"
        );
        assert_eq!(state("本地夹"), CloudState::None, "已下载的文件夹不标记");
        // 在线文件、文件夹、固定属性文件都靠属性判定，只有 本地.docx / 保留.docx / 普通.txt 向提供方问占位信息
        assert_eq!(
            m.placeholder_calls(),
            3,
            "仅在线与带固定位的条目不询问占位信息"
        );
        assert_eq!(m.root_calls(), 1, "一个目录只问一次同步根");
    }

    #[test]
    fn provider_down_marks_online_only_as_unavailable() {
        use crate::cloud::{CloudState, MockCloud, ProviderStatus};
        let d = temp("cloud-down");
        fs::write(d.join("在线.docx"), b"x").unwrap();
        fs::write(d.join("本地.docx"), b"x").unwrap();
        attrib("+O", &d.join("在线.docx"));
        let m = MockCloud::default();
        m.add_root(
            &d.to_string_lossy(),
            "iCloudDrive",
            ProviderStatus::Disconnected,
        );
        let (v, sr) = list_dir_with(&d, &m).unwrap();
        assert!(sr.unwrap().warning().is_some());
        let state = |n: &str| v.iter().find(|e| e.name == n).unwrap().cloud;
        assert_eq!(state("在线.docx"), CloudState::OnlineUnavailable);
        assert_eq!(
            state("本地.docx"),
            CloudState::None,
            "已在本机的文件不受影响"
        );
    }

    #[test]
    fn outside_any_sync_root_nothing_is_marked_and_provider_is_not_asked() {
        use crate::cloud::MockCloud;
        let d = temp("cloud-none");
        fs::write(d.join("a.txt"), b"x").unwrap();
        let m = MockCloud::default();
        let (v, sr) = list_dir_with(&d, &m).unwrap();
        assert!(sr.is_none());
        assert!(v.iter().all(|e| !e.cloud.is_cloud()));
        assert_eq!(m.placeholder_calls(), 0);
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
