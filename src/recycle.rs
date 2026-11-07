//! Windows 回收站：用外壳命名空间枚举与恢复，用 `SHFileOperationW` 移入回收站。
//!
//! - **不把回收站当普通路径**：枚举走 `FOLDERID_RecycleBinFolder` 的 `IShellFolder2`，名称、原位置、删除时间、
//!   大小来自外壳属性；项目标识是外壳的相对 PIDL 字节，恢复时原样交回。
//! - **恢复**走外壳自己的 `undelete` 命令（与资源管理器“还原”同一路径，同时清理回收站元数据），
//!   并带 `CMIC_MASK_FLAG_NO_UI`：同名冲突、缺目录由本模块先行判断，不弹系统对话框、不覆盖、不建目录。
//!   命令返回后再核对原位置确实出现了该项，核对不过按失败处理。
//! - **移入回收站**：`FO_DELETE + FOF_ALLOWUNDO`，并带 `FOF_WANTNUKEWARNING`——系统若发现无法回收而要永久删除，
//!   必须先询问用户，绝不静默永久删除；另外 [`can_recycle`] 对明显不支持的位置（可移动磁盘、网络、路径过长）
//!   直接拒绝，不调用系统。结果以“原路径是否已消失”核对。
//! - 所有 COM 调用都在调用线程内初始化（单线程套间）并在返回前释放，可从任意工作线程调用。

use crate::fileops::{BinItem, FailKind, OpError, OpResult};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{HWND, PROPERTYKEY};
use windows::Win32::Storage::FileSystem::GetDriveTypeW;
use windows::Win32::System::Com::IBindCtx;
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::Common::{ITEMIDLIST, STRRET};
use windows::Win32::UI::Shell::{
    CMF_NORMAL, CMINVOKECOMMANDINFO, FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI,
    FOF_SILENT, FOF_WANTNUKEWARNING, FOLDERID_RecycleBinFolder, IContextMenu, IEnumIDList,
    ILGetSize, IShellFolder, IShellFolder2, KF_FLAG_DEFAULT, SHBindToObject, SHCONTF_FOLDERS,
    SHCONTF_INCLUDEHIDDEN, SHCONTF_NONFOLDERS, SHFILEOPSTRUCTW, SHFileOperationW, SHGDN_FORPARSING,
    SHGDN_INFOLDER, SHGDNF, SHGetKnownFolderIDList, StrRetToStrW,
};
use windows::Win32::UI::WindowsAndMessaging::{CreatePopupMenu, DestroyMenu};
use windows::core::{GUID, PCSTR, PCWSTR};

/// `CMIC_MASK_FLAG_NO_UI`：命令执行时不显示任何界面。
const CMIC_MASK_FLAG_NO_UI: u32 = 0x400;
/// `SFGAO_FOLDER`。
const SFGAO_FOLDER: u32 = 0x2000_0000;
/// `GetDriveTypeW` 对本地固定磁盘返回 3（`DRIVE_FIXED`）。
const DRIVE_FIXED: u32 = 3;

/// 本线程的 COM 初始化；已用其他套间模型初始化时直接沿用，不重复卸载。
pub(crate) struct Com(bool);

impl Com {
    pub(crate) fn init() -> Com {
        // SAFETY: 标准 COM 初始化；成功（含 S_FALSE）才在 Drop 时配对卸载。
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        Com(hr.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: 与成功的 CoInitializeEx 配对。
            unsafe { CoUninitialize() };
        }
    }
}

/// `PKEY_Displaced_FromPath`：删除前所在文件夹。
const PKEY_FROM: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9B174B33_40FF_11D2_A27E_00C04FC30871),
    pid: 2,
};
/// `PKEY_Displaced_DateDisplaced`：删除时间（本地时间的 VT_DATE）。
const PKEY_DELETED: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9B174B33_40FF_11D2_A27E_00C04FC30871),
    pid: 3,
};
/// `PKEY_Size`。
const PKEY_SIZE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xB725F130_47EF_101A_A5F1_02608C9EEBAC),
    pid: 12,
};
/// `PKEY_ItemNameDisplay`。
const PKEY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xB725F130_47EF_101A_A5F1_02608C9EEBAC),
    pid: 10,
};

fn bin_folder() -> windows::core::Result<IShellFolder2> {
    // SAFETY: 取已知文件夹的 PIDL，绑定后立即释放 PIDL。
    unsafe {
        let pidl =
            SHGetKnownFolderIDList(&FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT.0 as u32, None)?;
        let r = SHBindToObject::<Option<&IShellFolder>, Option<&IBindCtx>, IShellFolder2>(
            None, pidl, None,
        );
        CoTaskMemFree(Some(pidl as *const _));
        r
    }
}

fn pidl_bytes(pidl: *const ITEMIDLIST) -> Vec<u8> {
    // SAFETY: pidl 来自外壳枚举，ILGetSize 给出含结尾 0 终止符的总长度。
    unsafe {
        let n = ILGetSize(Some(pidl)) as usize;
        std::slice::from_raw_parts(pidl as *const u8, n).to_vec()
    }
}

fn variant_string(v: &VARIANT) -> Option<String> {
    windows::core::BSTR::try_from(v).ok().map(|b| b.to_string())
}

fn variant_f64(v: &VARIANT) -> Option<f64> {
    use windows::Win32::System::Variant::VT_DATE;
    if v.vt() == VT_DATE {
        // SAFETY: vt 为 VT_DATE 时联合体里的 date 字段有效。
        return Some(unsafe { v.Anonymous.Anonymous.Anonymous.date });
    }
    f64::try_from(v).ok()
}

/// VT_DATE（自 1899-12-30 起的天数）转系统时间。外壳给出的“删除时间”是 UTC（由 FILETIME 直接换算，
/// 不含时区），实测与删除发生的时刻一致（见 tests.rs 的“删除时间应是刚才”断言）。
fn ole_date_to_time(d: f64) -> Option<SystemTime> {
    use chrono::NaiveDate;
    let base = NaiveDate::from_ymd_opt(1899, 12, 30)?.and_hms_opt(0, 0, 0)?;
    let ms = (d * 86_400_000.0).round() as i64;
    let naive = base.checked_add_signed(chrono::Duration::milliseconds(ms))?;
    let secs = naive.and_utc().timestamp();
    let t = if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs((-secs) as u64)
    };
    Some(t)
}

fn detail(folder: &IShellFolder2, pidl: *const ITEMIDLIST, key: &PROPERTYKEY) -> Option<VARIANT> {
    // SAFETY: pidl 有效；key 为常量。
    unsafe { folder.GetDetailsEx(pidl, key).ok() }
}

fn display_name(folder: &IShellFolder2, pidl: *const ITEMIDLIST) -> String {
    name_of(folder, pidl, SHGDN_INFOLDER)
}

/// 项目在回收站里的实际文件路径（`$Recycle.Bin\<SID>\$R…`）；用来确认标识还对应着真实存在的项目。
fn parsing_path(folder: &IShellFolder2, pidl: *const ITEMIDLIST) -> Option<String> {
    let s = name_of(folder, pidl, SHGDN_FORPARSING);
    (!s.is_empty()).then_some(s)
}

fn name_of(folder: &IShellFolder2, pidl: *const ITEMIDLIST, flags: SHGDNF) -> String {
    // SAFETY: 外壳返回 STRRET，用 StrRetToStrW 转换并释放。
    unsafe {
        let mut sr = STRRET::default();
        if folder.GetDisplayNameOf(pidl, flags, &mut sr).is_err() {
            return String::new();
        }
        let mut out = windows::core::PWSTR::null();
        match StrRetToStrW(&mut sr, Some(pidl), &mut out) {
            Ok(()) => {
                let s = out.to_string().unwrap_or_default();
                CoTaskMemFree(Some(out.0 as *const _));
                s
            }
            Err(_) => String::new(),
        }
    }
}

/// 枚举回收站（所有磁盘的合并视图，与资源管理器一致）。
pub fn list() -> Result<Vec<BinItem>, String> {
    let _com = Com::init();
    let folder = bin_folder().map_err(|e| format!("无法打开回收站：{e}"))?;
    let mut out = Vec::new();
    // SAFETY: 标准 IEnumIDList 遍历；每个 PIDL 用后以 CoTaskMemFree 释放。
    unsafe {
        let mut en: Option<IEnumIDList> = None;
        let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0 | SHCONTF_INCLUDEHIDDEN.0) as u32;
        let hr = folder.EnumObjects(HWND::default(), flags, &mut en);
        if hr.is_err() {
            return Err(format!(
                "无法枚举回收站：{}",
                windows::core::Error::from(hr)
            ));
        }
        let Some(en) = en else {
            return Ok(out); // S_FALSE：没有任何项目
        };
        loop {
            let mut slot = [std::ptr::null_mut::<ITEMIDLIST>()];
            let mut fetched = 0u32;
            let hr = en.Next(&mut slot, Some(&mut fetched));
            if hr.0 != 0 || fetched == 0 || slot[0].is_null() {
                break;
            }
            let pidl = slot[0] as *const ITEMIDLIST;
            let mut name = detail(&folder, pidl, &PKEY_NAME)
                .and_then(|v| variant_string(&v))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| display_name(&folder, pidl));
            let original_dir = detail(&folder, pidl, &PKEY_FROM)
                .and_then(|v| variant_string(&v))
                .unwrap_or_default();
            let deleted = detail(&folder, pidl, &PKEY_DELETED)
                .and_then(|v| variant_f64(&v))
                .and_then(ole_date_to_time);
            let size = detail(&folder, pidl, &PKEY_SIZE)
                .and_then(|v| u64::try_from(&v).ok())
                .unwrap_or(0);
            let mut attrs = SFGAO_FOLDER;
            let _ = folder.GetAttributesOf(&[pidl], &mut attrs);
            let is_dir = attrs & SFGAO_FOLDER != 0;
            if name.is_empty() {
                name = "（未命名）".into();
            }
            out.push(BinItem {
                id: pidl_bytes(pidl),
                name,
                original_dir,
                deleted,
                size: if is_dir { 0 } else { size },
                is_dir,
            });
            CoTaskMemFree(Some(pidl as *const _));
        }
    }
    Ok(out)
}

/// 恢复一项到原位置。
pub fn restore(item: &BinItem) -> OpResult {
    let dest = item.original_path();
    let dir = Path::new(&item.original_dir);
    if item.original_dir.is_empty() {
        return Err(OpError::new(FailKind::Other, "回收站没有记录原位置"));
    }
    if !dir.is_dir() {
        return Err(OpError::new(FailKind::DestMissing, ""));
    }
    if std::fs::symlink_metadata(&dest).is_ok() {
        return Err(OpError::new(FailKind::Conflict, ""));
    }
    // 没有写入权限时外壳会弹“目标文件夹访问被拒绝”并等待用户，这里先探测，直接如实失败
    if let Err(e) = probe_add_access(dir, item.is_dir) {
        return Err(OpError::from_io(&e));
    }
    let _com = Com::init();
    let folder = bin_folder().map_err(|e| OpError::new(FailKind::Other, e.to_string()))?;
    if item.id.is_empty() {
        return Err(OpError::new(FailKind::NotFound, ""));
    }
    // 过期的标识（项目已被别处还原或清除）交给外壳会弹“没有找到项目”并等待用户，先自行确认
    if let Some(p) = parsing_path(&folder, item.id.as_ptr() as *const ITEMIDLIST)
        && std::fs::symlink_metadata(&p).is_err()
    {
        return Err(OpError::new(FailKind::NotFound, ""));
    }
    let r = invoke_verb(&folder, &item.id, b"undelete\0");
    // 无论系统怎么说，以原位置是否真的出现为准
    let exists = std::fs::symlink_metadata(&dest).is_ok();
    match (r, exists) {
        (Ok(()), true) => Ok(()),
        (Ok(()), false) => Err(OpError::new(
            FailKind::Other,
            "系统报告成功，但原位置没有出现该项目",
        )),
        // 命令报错：如实当作失败（即使项目已回到原位置也说明系统状态异常，不掩盖）
        (Err(e), true) => Err(OpError::new(FailKind::Other, format!("系统返回错误：{e}"))),
        (Err(e), false) => Err(map_hresult(&e)),
    }
}

/// 探测对目录是否有“新建文件／子文件夹”的权限：只打开目录并申请该访问权，不创建任何东西。
pub(crate) fn probe_add_access(dir: &Path, is_dir: bool) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_ADD_FILE: u32 = 0x2;
    const FILE_ADD_SUBDIRECTORY: u32 = 0x4;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .access_mode(if is_dir {
            FILE_ADD_SUBDIRECTORY
        } else {
            FILE_ADD_FILE
        })
        .share_mode(7)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .map(|_| ())
}

pub(crate) fn map_hresult(e: &windows::core::Error) -> OpError {
    // HRESULT_FROM_WIN32 的低 16 位是 Win32 错误码
    let code = (e.code().0 as u32) & 0xFFFF;
    let kind = match code {
        5 | 19 => FailKind::AccessDenied,
        2 | 3 => FailKind::NotFound,
        32 | 33 => FailKind::InUse,
        80 | 183 => FailKind::Exists,
        39 | 112 => FailKind::DiskFull,
        21 | 31 | 53 | 55 | 64 | 67 | 1117 | 1167 => FailKind::DeviceGone,
        1223 => FailKind::Aborted,
        _ => FailKind::Other,
    };
    OpError::new(
        kind,
        if kind == FailKind::Other {
            e.to_string()
        } else {
            String::new()
        },
    )
}

fn invoke_verb(
    folder: &IShellFolder2,
    id: &[u8],
    verb: &'static [u8],
) -> windows::core::Result<()> {
    // SAFETY: id 是此前从外壳复制的完整 PIDL（含终止符）；菜单句柄在调用后销毁。
    unsafe {
        let pidl = id.as_ptr() as *const ITEMIDLIST;
        let cm: IContextMenu = folder.GetUIObjectOf(HWND::default(), &[pidl], None)?;
        let menu = CreatePopupMenu()?;
        let r = (|| {
            cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL).ok()?;
            let info = CMINVOKECOMMANDINFO {
                cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                fMask: CMIC_MASK_FLAG_NO_UI,
                lpVerb: PCSTR(verb.as_ptr()),
                nShow: 0,
                ..Default::default()
            };
            cm.InvokeCommand(&info)
        })();
        let _ = DestroyMenu(menu);
        r
    }
}

/// 该路径所在位置能否使用回收站。只做静态判断，不改动任何东西。
/// 不支持：路径过长（回收站不处理超过 MAX_PATH 的路径）、非固定磁盘（可移动磁盘、网络位置、光驱）。
pub fn can_recycle(path: &Path) -> Result<(), String> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.len() >= 260 {
        return Err("路径超过 260 个字符，回收站无法处理".into());
    }
    let s = path.display().to_string();
    if s.starts_with(r"\\") && !s.starts_with(r"\\?\") {
        return Err("网络位置没有回收站".into());
    }
    let root: Vec<u16> = match path.components().next() {
        Some(std::path::Component::Prefix(p)) => {
            let mut r: Vec<u16> = p.as_os_str().encode_wide().collect();
            r.push(b'\\' as u16);
            r.push(0);
            r
        }
        _ => return Err("无法确定所在磁盘".into()),
    };
    // SAFETY: root 以 NUL 结尾。
    let t = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
    if t == DRIVE_FIXED {
        Ok(())
    } else {
        Err("可移动磁盘、网络位置和光盘没有回收站".into())
    }
}

/// 移入回收站。成功当且仅当原路径已消失；系统要求永久删除时会先询问用户（`FOF_WANTNUKEWARNING`），
/// 被拒绝则返回 [`FailKind::Aborted`]，项目原样保留。
pub fn recycle(path: &Path) -> OpResult {
    if std::fs::symlink_metadata(path).is_err() {
        return Err(OpError::new(FailKind::NotFound, ""));
    }
    if can_recycle(path).is_err() {
        return Err(OpError::new(FailKind::Unsupported, ""));
    }
    let mut from: Vec<u16> = path.as_os_str().encode_wide().collect();
    from.extend([0, 0]); // 双 NUL 结尾
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO
            | FOF_NOCONFIRMATION
            | FOF_NOERRORUI
            | FOF_SILENT
            | FOF_WANTNUKEWARNING)
            .0 as u16,
        ..Default::default()
    };
    // SAFETY: pFrom 指向双 NUL 结尾缓冲区，调用期间有效。
    let code = unsafe { SHFileOperationW(&mut op) };
    let gone = std::fs::symlink_metadata(path).is_err();
    if code == 0 && !op.fAnyOperationsAborted.as_bool() && gone {
        return Ok(());
    }
    if op.fAnyOperationsAborted.as_bool() {
        return Err(OpError::new(FailKind::Aborted, ""));
    }
    if code == 0 {
        return Err(OpError::new(
            FailKind::Other,
            "系统报告成功，但项目仍在原位置",
        ));
    }
    Err(map_shfileop(code))
}

/// `SHFileOperation` 的返回码（旧式 DE_* 与 Win32 错误码混用）。
fn map_shfileop(code: i32) -> OpError {
    match code {
        0x78 | 0x74 | 0x75 | 5 | 19 => OpError::new(FailKind::AccessDenied, ""),
        0x7C | 0x71 | 0x72 | 2 | 3 | 0x402 => OpError::new(FailKind::NotFound, ""),
        32 | 33 => OpError::new(FailKind::InUse, ""),
        _ => OpError::new(FailKind::Other, format!("系统错误码 0x{code:X}")),
    }
}

/// 永久删除回收站里的指定项目（仅供测试清理自己放进去的夹具；产品不提供清空回收站的入口）。
/// 用 `IFileOperation` 并关闭所有确认与界面——外壳的 `delete` 命令会弹“删除文件”确认框，无人值守的测试会卡住。
#[cfg(test)]
pub(crate) fn purge(items: &[BinItem]) -> OpResult {
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        FileOperation, IFileOperation, IShellItem, SHCreateItemWithParent,
    };
    use windows::core::Interface;
    let _com = Com::init();
    let folder = bin_folder().map_err(|e| OpError::new(FailKind::Other, e.to_string()))?;
    let r = (|| -> windows::core::Result<()> {
        // SAFETY: 项目标识是此前从外壳复制的完整相对 PIDL；所有接口在本闭包内使用并释放。
        unsafe {
            let parent: IShellFolder = folder.cast()?;
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            op.SetOperationFlags(FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT)?;
            for it in items {
                let pidl = it.id.as_ptr() as *const ITEMIDLIST;
                let item: IShellItem = SHCreateItemWithParent(None, &parent, pidl)?;
                op.DeleteItem(&item, None)?;
            }
            op.PerformOperations()
        }
    })();
    r.map_err(|e| map_hresult(&e))
}

#[cfg(test)]
mod tests;
