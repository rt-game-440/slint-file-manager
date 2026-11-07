//! 位置入口：磁盘列表、收藏、云盘本地目录。依据 Issue #7。
//!
//! - **磁盘**：只列本机固定磁盘与可移动磁盘（U 盘、移动硬盘、读卡器），即带盘符且类型为 `DRIVE_FIXED`／`DRIVE_REMOVABLE` 的卷。
//!   网络驱动器、光驱、RAM 盘以及手机／相机这类没有盘符的 MTP 设备不纳入（首批不承诺）。
//! - **收藏**：文件夹路径 + 收藏时记录的卷序列号。卷序列号用来识别“盘符被另一块盘占用”：
//!   序列号对不上时该收藏标为失效，**不会**自动指向现在占用该盘符的另一块磁盘。
//!   收藏只接受本机盘符路径（不接受 UNC／未确认的网络位置）。
//! - **持久化**：`<数据目录>\favorites.json`（与会话文件同一数据目录），UTF-8 JSON，带整数 `version`；
//!   无法解析、结构不符、版本不符或过大时不使用也不覆盖，移为 `favorites.backup-<时间>-<原因>.json` 并给出说明。
//! - 本模块的判定函数都不访问磁盘（目录是否存在由调用方传入判断函数），便于无头测试。

use serde_json::{Value, json};
use std::fs;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

/// 收藏文件格式版本。
pub const FAV_VERSION: i64 = 1;
/// 收藏文件名。
pub const FAV_FILE: &str = "favorites.json";
/// 收藏文件大小上限。
pub const FAV_MAX_BYTES: u64 = 1024 * 1024;
/// 收藏数量上限。
pub const MAX_FAVORITES: usize = 200;
const KEEP_BACKUPS: usize = 5;

// ---------- 磁盘 ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveKind {
    /// 固定磁盘（含多数“移动硬盘”：系统把它们报告为固定磁盘）。
    Fixed,
    /// 可移动磁盘（U 盘、读卡器）。
    Removable,
}

/// 一个带盘符的本机磁盘。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drive {
    /// 根路径，如 `C:\`。
    pub root: PathBuf,
    pub letter: char,
    /// 卷标，可能为空。
    pub label: String,
    pub kind: DriveKind,
    /// 卷序列号；介质未就绪（如空读卡器）时为 `None`。
    pub serial: Option<u32>,
    /// 可弹出（可移动磁盘或 USB 总线上的磁盘）。对这类磁盘上的目录不持有打开的句柄，避免“安全删除硬件”被占用。
    pub ejectable: bool,
}

impl Drive {
    pub fn new(letter: char, label: &str, kind: DriveKind, serial: Option<u32>) -> Drive {
        let letter = letter.to_ascii_uppercase();
        Drive {
            root: PathBuf::from(format!("{letter}:\\")),
            letter,
            label: label.to_string(),
            kind,
            serial,
            ejectable: kind == DriveKind::Removable,
        }
    }

    pub fn with_ejectable(mut self, e: bool) -> Drive {
        self.ejectable = e;
        self
    }

    /// 介质是否就绪（可读卷信息）。
    pub fn ready(&self) -> bool {
        self.serial.is_some()
    }

    /// 侧栏显示名：卷标（没有时按类型给默认名）+ 盘符，如 `U 盘 (E:)`。
    pub fn display(&self) -> String {
        let name = if self.label.is_empty() {
            match self.kind {
                DriveKind::Fixed => "本地磁盘",
                DriveKind::Removable => "可移动磁盘",
            }
        } else {
            self.label.as_str()
        };
        format!("{name} ({}:)", self.letter)
    }
}

/// 路径所在的盘符（大写）；只认 `X:\…` 形式，UNC 等返回 `None`。
pub fn drive_letter(p: &Path) -> Option<char> {
    match p.components().next() {
        Some(Component::Prefix(pre)) => {
            let s = pre.as_os_str().to_string_lossy();
            let b = s.as_bytes();
            (b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
                .then(|| (b[0] as char).to_ascii_uppercase())
        }
        _ => None,
    }
}

/// 路径所在盘符的根（`C:\`）。
pub fn root_of(p: &Path) -> Option<PathBuf> {
    drive_letter(p).map(|l| PathBuf::from(format!("{l}:\\")))
}

/// 两次磁盘列表的差异：新增与移除的盘符（盘符相同但序列号变化视为先移除再新增）。
pub fn diff_drives(old: &[Drive], new: &[Drive]) -> (Vec<Drive>, Vec<Drive>) {
    let added = new
        .iter()
        .filter(|d| !old.contains(d))
        .cloned()
        .collect::<Vec<_>>();
    let removed = old
        .iter()
        .filter(|d| !new.contains(d))
        .cloned()
        .collect::<Vec<_>>();
    (added, removed)
}

use windows_sys::Win32::Storage::FileSystem::{
    GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};
use windows_sys::Win32::System::Diagnostics::Debug::{SEM_FAILCRITICALERRORS, SetErrorMode};

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// 读取卷标与序列号；介质未就绪或读取失败返回 `None`。
fn volume_info(root: &Path) -> Option<(String, u32)> {
    let w = wide(root.as_os_str());
    let mut label = [0u16; 261];
    let mut serial = 0u32;
    // SAFETY: 缓冲区长度如实传入；不需要的输出参数传空指针。
    let ok = unsafe {
        GetVolumeInformationW(
            w.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    let n = label.iter().position(|&c| c == 0).unwrap_or(label.len());
    Some((
        std::ffi::OsString::from_wide(&label[..n])
            .to_string_lossy()
            .into_owned(),
        serial,
    ))
}

/// 路径所在卷的序列号（读取失败返回 `None`）。
pub fn volume_serial(p: &Path) -> Option<u32> {
    volume_info(&root_of(p)?).map(|(_, s)| s)
}

/// 当前盘符位图（第 0 位为 A:）。非常便宜，供轮询判断是否需要重新枚举。
pub fn drive_mask() -> u32 {
    // SAFETY: 无参数的简单查询。
    unsafe { GetLogicalDrives() }
}

/// 该盘符所在设备是否挂在 USB 总线上（移动硬盘通常被报告为“固定磁盘”，靠总线类型识别）。
/// 以 0 访问权限短暂打开卷再立即关闭；任何一步失败都返回 `false`。
fn is_usb_bus(letter: char) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
        STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
    };
    const BUS_TYPE_USB: i32 = 7;
    let path = wide(std::ffi::OsStr::new(&format!(r"\\.\{letter}:")));
    // SAFETY: 路径以 NUL 结尾；缓冲区大小如实传入；句柄用后关闭。
    unsafe {
        let h = CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut q: STORAGE_PROPERTY_QUERY = std::mem::zeroed();
        q.PropertyId = StorageDeviceProperty;
        q.QueryType = PropertyStandardQuery;
        let mut buf = [0u8; 1024];
        let mut ret = 0u32;
        let ok = DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&q as *const STORAGE_PROPERTY_QUERY).cast(),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            &mut ret,
            std::ptr::null_mut(),
        );
        CloseHandle(h);
        if ok == 0 || (ret as usize) < std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
            return false;
        }
        let d = &*(buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR);
        d.BusType == BUS_TYPE_USB
    }
}

/// 枚举本机固定与可移动磁盘，按盘符排序。网络、光驱、RAM 盘不纳入。
pub fn enumerate_drives() -> Vec<Drive> {
    // 找不到介质时不弹系统“请插入磁盘”对话框
    // SAFETY: 设置进程错误模式，无内存安全问题。
    unsafe { SetErrorMode(SEM_FAILCRITICALERRORS) };
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    let mask = drive_mask();
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = PathBuf::from(format!("{letter}:\\"));
        let w = wide(root.as_os_str());
        // SAFETY: 以 NUL 结尾的宽字符串。
        let ty = unsafe { GetDriveTypeW(w.as_ptr()) };
        let kind = match ty {
            DRIVE_FIXED => DriveKind::Fixed,
            DRIVE_REMOVABLE => DriveKind::Removable,
            _ => continue,
        };
        let (label, serial) = match volume_info(&root) {
            Some((l, s)) => (l, Some(s)),
            None => (String::new(), None),
        };
        let ejectable = kind == DriveKind::Removable || is_usb_bus(letter);
        out.push(Drive::new(letter, &label, kind, serial).with_ejectable(ejectable));
    }
    out
}

// ---------- 收藏 ----------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Favorite {
    pub path: PathBuf,
    /// 显示名（收藏时的文件夹名）。
    pub name: String,
    /// 收藏时所在卷的序列号；未能读取时为 `None`（此时无法识别盘符被换盘）。
    pub volume: Option<u32>,
}

/// 收藏的可用状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FavState {
    Ok,
    /// 所在磁盘当前未连接。
    Offline,
    /// 盘符现在对应另一块磁盘（卷序列号不同）。
    OtherVolume,
    /// 磁盘在，但文件夹不存在。
    Missing,
}

impl FavState {
    pub fn is_ok(self) -> bool {
        self == FavState::Ok
    }

    /// 给用户看的原因（有效时为空）。
    pub fn reason(self) -> &'static str {
        match self {
            FavState::Ok => "",
            FavState::Offline => "所在磁盘未连接",
            FavState::OtherVolume => "该盘符现在是另一块磁盘",
            FavState::Missing => "文件夹不存在",
        }
    }
}

/// 判定收藏状态。`exists` 判断目录是否存在（只在磁盘在线且卷一致时才会调用）。
pub fn favorite_state(f: &Favorite, drives: &[Drive], exists: impl Fn(&Path) -> bool) -> FavState {
    let Some(letter) = drive_letter(&f.path) else {
        return FavState::Missing;
    };
    let Some(d) = drives.iter().find(|d| d.letter == letter && d.ready()) else {
        return FavState::Offline;
    };
    if let (Some(want), Some(have)) = (f.volume, d.serial)
        && want != have
    {
        return FavState::OtherVolume;
    }
    if exists(&f.path) {
        FavState::Ok
    } else {
        FavState::Missing
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AddOutcome {
    Added,
    AlreadyThere,
    Rejected(&'static str),
}

/// 收藏列表（保持添加顺序）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Favorites {
    items: Vec<Favorite>,
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .trim_end_matches('\\')
        .eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches('\\'))
}

impl Favorites {
    pub fn items(&self) -> &[Favorite] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn contains(&self, p: &Path) -> bool {
        self.items.iter().any(|f| same_path(&f.path, p))
    }

    pub fn index_of(&self, p: &Path) -> Option<usize> {
        self.items.iter().position(|f| same_path(&f.path, p))
    }

    /// 添加收藏。只接受本机盘符下的绝对路径；`volume` 为该路径所在卷的序列号。
    pub fn add(&mut self, path: &Path, volume: Option<u32>) -> AddOutcome {
        if drive_letter(path).is_none() {
            return AddOutcome::Rejected("只能收藏本机磁盘上的文件夹");
        }
        if self.contains(path) {
            return AddOutcome::AlreadyThere;
        }
        if self.items.len() >= MAX_FAVORITES {
            return AddOutcome::Rejected("收藏数量已达上限");
        }
        self.items.push(Favorite {
            path: path.to_path_buf(),
            name: super::location::leaf_label(path),
            volume,
        });
        AddOutcome::Added
    }

    /// 取消收藏；返回是否有变化。
    pub fn remove(&mut self, p: &Path) -> bool {
        match self.index_of(p) {
            Some(i) => {
                self.items.remove(i);
                true
            }
            None => false,
        }
    }

    pub fn remove_at(&mut self, i: usize) -> Option<Favorite> {
        (i < self.items.len()).then(|| self.items.remove(i))
    }
}

// ---------- JSON ----------

#[derive(Debug, PartialEq)]
pub enum FavParseError {
    Syntax(String),
    Version(i64),
    Invalid(String),
}

pub fn favorites_to_json(f: &Favorites) -> String {
    let v = json!({
        "version": FAV_VERSION,
        "saved_at": chrono::Local::now().to_rfc3339(),
        "items": f.items.iter().map(|i| json!({
            "path": i.path.to_string_lossy(),
            "name": i.name,
            "volume": i.volume,
        })).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
}

pub fn favorites_from_json(bytes: &[u8]) -> Result<Favorites, FavParseError> {
    let v: Value =
        serde_json::from_slice(bytes).map_err(|e| FavParseError::Syntax(e.to_string()))?;
    let o = v
        .as_object()
        .ok_or_else(|| FavParseError::Invalid("顶层应为对象".into()))?;
    let version = o
        .get("version")
        .and_then(Value::as_i64)
        .ok_or_else(|| FavParseError::Invalid("缺少整数字段 version".into()))?;
    if version != FAV_VERSION {
        return Err(FavParseError::Version(version));
    }
    let items = o
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| FavParseError::Invalid("缺少数组字段 items".into()))?;
    if items.len() > MAX_FAVORITES {
        return Err(FavParseError::Invalid(format!(
            "收藏数量 {} 超过上限",
            items.len()
        )));
    }
    let mut out = Favorites::default();
    for (n, it) in items.iter().enumerate() {
        let io = it
            .as_object()
            .ok_or_else(|| FavParseError::Invalid(format!("第 {} 项应为对象", n + 1)))?;
        let path = io
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .filter(|p| drive_letter(p).is_some() && p.is_absolute())
            .ok_or_else(|| FavParseError::Invalid(format!("第 {} 项路径无效", n + 1)))?;
        let volume = match io.get("volume") {
            None | Some(Value::Null) => None,
            Some(x) => Some(
                x.as_u64()
                    .and_then(|x| u32::try_from(x).ok())
                    .ok_or_else(|| FavParseError::Invalid(format!("第 {} 项卷号无效", n + 1)))?,
            ),
        };
        let name = io
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| super::location::leaf_label(&path));
        if out.contains(&path) {
            continue;
        }
        out.items.push(Favorite { path, name, volume });
    }
    Ok(out)
}

// ---------- 存取 ----------

/// 加载结果。
#[derive(Debug)]
pub enum FavLoad {
    Missing,
    Restored(Favorites),
    /// 文件不可用：已移为备份（或无法备份）；`message` 给用户看。
    Degraded {
        message: String,
    },
}

pub struct FavoritesStore {
    dir: PathBuf,
    writable: std::cell::Cell<bool>,
}

fn is_backup_name(n: &str) -> bool {
    n.starts_with("favorites.backup-") && n.ends_with(".json")
}

impl FavoritesStore {
    pub fn at(dir: impl Into<PathBuf>) -> FavoritesStore {
        FavoritesStore {
            dir: dir.into(),
            writable: std::cell::Cell::new(true),
        }
    }

    /// 与会话相同的默认数据目录。
    pub fn default_location() -> Option<FavoritesStore> {
        super::session::SessionStore::default_location().map(|s| FavoritesStore::at(s.dir()))
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FAV_FILE)
    }

    pub fn load(&self) -> FavLoad {
        let path = self.path();
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return FavLoad::Missing,
            Err(e) => {
                self.writable.set(false);
                return FavLoad::Degraded {
                    message: format!("无法读取收藏（{e}），本次不使用也不会保存收藏。"),
                };
            }
        };
        let parsed = if meta.len() > FAV_MAX_BYTES {
            Err(FavParseError::Invalid(format!(
                "文件大小 {} 字节超过上限",
                meta.len()
            )))
        } else {
            match fs::read(&path) {
                Ok(b) => favorites_from_json(&b),
                Err(e) => {
                    self.writable.set(false);
                    return FavLoad::Degraded {
                        message: format!("无法读取收藏（{e}），本次不使用也不会保存收藏。"),
                    };
                }
            }
        };
        match parsed {
            Ok(f) => FavLoad::Restored(f),
            Err(err) => {
                let (tag, why) = match &err {
                    FavParseError::Version(v) => (
                        format!("v{v}"),
                        format!("收藏文件版本为 {v}，当前只支持版本 {FAV_VERSION}"),
                    ),
                    FavParseError::Syntax(m) => {
                        ("corrupt".to_string(), format!("收藏文件已损坏（{m}）"))
                    }
                    FavParseError::Invalid(m) => {
                        ("corrupt".to_string(), format!("收藏文件格式不正确（{m}）"))
                    }
                };
                FavLoad::Degraded {
                    message: self.set_aside(&tag, &why),
                }
            }
        }
    }

    fn set_aside(&self, tag: &str, why: &str) -> String {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let mut target = self.dir.join(format!("favorites.backup-{ts}-{tag}.json"));
        let mut n = 1;
        while target.exists() {
            target = self
                .dir
                .join(format!("favorites.backup-{ts}-{tag}-{n}.json"));
            n += 1;
        }
        match fs::rename(self.path(), &target) {
            Ok(()) => {
                self.prune_backups();
                format!(
                    "{why}，原文件已备份为 {}，本次收藏为空。",
                    target
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                )
            }
            Err(e) => {
                self.writable.set(false);
                format!("{why}，且无法备份（{e}）；本次收藏为空且不会保存，以免覆盖原文件。")
            }
        }
    }

    fn prune_backups(&self) {
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| is_backup_name(n))
            .collect();
        names.sort();
        if names.len() > KEEP_BACKUPS {
            for n in &names[..names.len() - KEEP_BACKUPS] {
                let _ = fs::remove_file(self.dir.join(n));
            }
        }
    }

    /// 原子写入（临时文件 + 重命名）。
    pub fn save(&self, f: &Favorites) -> io::Result<()> {
        if !self.writable.get() {
            return Err(io::Error::other("收藏文件不可覆盖，已停止保存"));
        }
        fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{FAV_FILE}.tmp"));
        {
            use std::io::Write;
            let mut file = fs::File::create(&tmp)?;
            file.write_all(favorites_to_json(f).as_bytes())?;
            file.sync_data()?;
        }
        fs::rename(&tmp, self.path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sfm-fav-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn drive_display_and_letters() {
        assert_eq!(
            Drive::new('e', "", DriveKind::Removable, Some(1)).display(),
            "可移动磁盘 (E:)"
        );
        assert_eq!(
            Drive::new('C', "系统", DriveKind::Fixed, Some(2)).display(),
            "系统 (C:)"
        );
        assert_eq!(drive_letter(Path::new(r"d:\a\b")), Some('D'));
        assert_eq!(drive_letter(Path::new(r"\\srv\share\a")), None);
        assert_eq!(root_of(Path::new(r"e:\x")), Some(PathBuf::from(r"E:\")));
    }

    #[test]
    fn diff_detects_add_remove_and_swap() {
        let c = Drive::new('C', "", DriveKind::Fixed, Some(1));
        let e1 = Drive::new('E', "U", DriveKind::Removable, Some(10));
        let e2 = Drive::new('E', "V", DriveKind::Removable, Some(20));
        let (a, r) = diff_drives(std::slice::from_ref(&c), &[c.clone(), e1.clone()]);
        assert_eq!((a, r), (vec![e1.clone()], vec![]));
        let (a, r) = diff_drives(&[c.clone(), e1.clone()], std::slice::from_ref(&c));
        assert_eq!((a, r), (vec![], vec![e1.clone()]));
        let (a, r) = diff_drives(&[c.clone(), e1.clone()], &[c, e2.clone()]);
        assert_eq!((a, r), (vec![e2], vec![e1]));
    }

    #[test]
    fn favorite_state_never_points_to_other_disk() {
        let e1 = Drive::new('E', "U", DriveKind::Removable, Some(10));
        let e2 = Drive::new('E', "V", DriveKind::Removable, Some(20));
        let f = Favorite {
            path: PathBuf::from(r"E:\资料"),
            name: "资料".into(),
            volume: Some(10),
        };
        let yes = |_: &Path| true;
        assert_eq!(
            favorite_state(&f, std::slice::from_ref(&e1), yes),
            FavState::Ok
        );
        assert_eq!(favorite_state(&f, &[], yes), FavState::Offline);
        assert_eq!(
            favorite_state(&f, std::slice::from_ref(&e2), yes),
            FavState::OtherVolume,
            "同盘符另一块盘：失效而不是指过去"
        );
        assert_eq!(
            favorite_state(&f, std::slice::from_ref(&e1), |_| false),
            FavState::Missing
        );
        // 介质未就绪的读卡器视为离线
        let e0 = Drive::new('E', "", DriveKind::Removable, None);
        assert_eq!(favorite_state(&f, &[e0], yes), FavState::Offline);
    }

    #[test]
    fn add_remove_and_reject() {
        let mut f = Favorites::default();
        assert_eq!(f.add(Path::new(r"C:\a\b"), Some(1)), AddOutcome::Added);
        assert_eq!(
            f.add(Path::new(r"c:\A\B\"), Some(1)),
            AddOutcome::AlreadyThere
        );
        assert!(matches!(
            f.add(Path::new(r"\\srv\share\x"), None),
            AddOutcome::Rejected(_)
        ));
        assert_eq!(f.items()[0].name, "b");
        assert!(f.remove(Path::new(r"C:\a\b")));
        assert!(!f.remove(Path::new(r"C:\a\b")));
    }

    #[test]
    fn json_roundtrip_and_validation() {
        let mut f = Favorites::default();
        f.add(Path::new(r"D:\照片"), Some(77));
        f.add(Path::new(r"C:\"), None);
        assert_eq!(favorites_from_json(favorites_to_json(&f).as_bytes()), Ok(f));
        assert!(matches!(
            favorites_from_json(b"{ x"),
            Err(FavParseError::Syntax(_))
        ));
        assert_eq!(
            favorites_from_json(br#"{"version":9,"items":[]}"#),
            Err(FavParseError::Version(9))
        );
        assert!(matches!(
            favorites_from_json(r#"{"version":1,"items":[{"path":"相对"}]}"#.as_bytes()),
            Err(FavParseError::Invalid(_))
        ));
        assert!(matches!(
            favorites_from_json(br#"{"version":1,"items":[{"path":"\\\\s\\h\\x"}]}"#),
            Err(FavParseError::Invalid(_))
        ));
    }

    #[test]
    fn store_roundtrip_and_degrade() {
        let d = tmp("store");
        let s = FavoritesStore::at(&d);
        assert!(matches!(s.load(), FavLoad::Missing));
        let mut f = Favorites::default();
        f.add(Path::new(r"D:\照片"), Some(5));
        s.save(&f).unwrap();
        let FavLoad::Restored(back) = FavoritesStore::at(&d).load() else {
            panic!("应能恢复");
        };
        assert_eq!(back, f);

        fs::write(s.path(), b"{ broken").unwrap();
        let FavLoad::Degraded { message } = FavoritesStore::at(&d).load() else {
            panic!("应降级");
        };
        assert!(message.contains("已备份"), "{message}");
        assert!(!s.path().exists(), "损坏文件被移走而不是覆盖");
        let backups = fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| is_backup_name(&e.file_name().to_string_lossy()))
            .count();
        assert_eq!(backups, 1);

        fs::write(s.path(), br#"{"version":2,"items":[]}"#).unwrap();
        assert!(matches!(
            FavoritesStore::at(&d).load(),
            FavLoad::Degraded { .. }
        ));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn enumerates_system_drive() {
        let v = enumerate_drives();
        assert!(v.iter().any(|d| d.kind == DriveKind::Fixed), "{v:?}");
        assert!(v.windows(2).all(|w| w[0].letter < w[1].letter));
    }
}
