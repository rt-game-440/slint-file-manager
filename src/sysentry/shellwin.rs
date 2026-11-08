//! 把本应用登记为“外壳窗口”（`IShellWindows`），让 `SHOpenFolderAndSelectItems` 找到并选中项目（Issue #18）。
//!
//! 浏览器下载、VS Code、Firefox 等的“在文件夹中显示”调用 `SHOpenFolderAndSelectItems`。系统先在 `IShellWindows`
//! 里按文件夹查找已登记的窗口，找到就把它切到前台并要求它选中项目；找不到才新开资源管理器。实测（见
//! `docs/research/0016-system-entry-takeover.md` 第 11 节、`src/bin/reveal-probe`）系统走的接口链是：
//!
//! 1. `IShellWindows::FindWindowSW`（类型必须是 `SWC_BROWSER`）按文件夹 PIDL 找到登记的 `IDispatch`；
//! 2. 对它 `QueryInterface(IWebBrowserApp)`，取 `get_HWND`（切前台）与 `get_Document`；
//! 3. 对文档 `QueryInterface(IServiceProvider)`，`QueryService(IID_IFolderView, IID_IShellView)`；
//! 4. `IShellView::SelectItem(子项 PIDL, SVSI_*)`。
//!
//! 每个“槽”（产品里是一个标签）各登记一次（同一个窗口句柄、不同 cookie），所以文件夹开在非活动标签里也能找到；
//! 选中时由 [`Selection::slot`] 告诉应用切到哪个标签。登记只在进程存活期间有效，进程退出或 [`ShellWindows::revoke`]
//! 即注销，不写任何持久配置。只登记文件系统文件夹，虚拟位置不登记。
//!
//! 线程：必须在已初始化 COM（STA）的界面线程上创建和调用；外部请求经 COM 的消息循环回到同一线程。

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use windows::Win32::Foundation::{
    DISP_E_MEMBERNOTFOUND, DISP_E_UNKNOWNNAME, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_POINTER,
    HWND, LPARAM, RECT, SHANDLE_PTR, VARIANT_BOOL,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, DISPATCH_FLAGS, DISPPARAMS, EXCEPINFO, IDispatch,
    IDispatch_Impl, IServiceProvider, IServiceProvider_Impl, ITypeInfo,
};
use windows::Win32::System::Ole::{DispGetIDsOfNames, DispInvoke, IOleWindow_Impl, LoadRegTypeLib};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::Variant::{InitVariantFromBuffer, VARIANT};
use windows::Win32::UI::Controls::LPFNSVADDPROPSHEETPAGE;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    _SVGIO, FOLDERSETTINGS, IFolderView, ILCombine, ILFree, ILGetSize, IShellBrowser, IShellView,
    IShellView_Impl, IShellWindows, IWebBrowser_Impl, IWebBrowserApp, IWebBrowserApp_Impl,
    SHGetNameFromIDList, SHParseDisplayName, SIGDN_FILESYSPATH, SWC_BROWSER, ShellWindows,
    UrlCreateFromPathW,
};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, MSG};
use windows::core::{
    BOOL, BSTR, GUID, HRESULT, Interface, InterfaceRef, PCWSTR, PWSTR, Ref, Result, implement,
};
use windows_core::ComObjectInterface;

/// `SVSI_SELECT`：选中。
pub const SVSI_SELECT: u32 = 0x1;
/// `SVSI_EDIT`：选中并进入重命名（`OFASI_EDIT`）。
pub const SVSI_EDIT: u32 = 0x3;
/// `SVSI_DESELECTOTHERS`：取消其他项目的选中。
pub const SVSI_DESELECTOTHERS: u32 = 0x4;

/// 外壳要求选中的项目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// 登记时给的槽号（产品里是标签 id）。
    pub slot: u64,
    /// 槽当前登记的文件夹。
    pub folder: PathBuf,
    /// 要选中的项目的完整路径；子项 PIDL 解析不出文件系统路径时为 `None`。
    pub item: Option<PathBuf>,
    /// `SVSI_*` 标志位。
    pub flags: u32,
}

/// 选中请求的接收者（在界面线程上、COM 调用内部被调用；不要在里面做阻塞操作）。
pub type Sink = Rc<dyn Fn(Selection)>;
/// 诊断记录（每次外壳调用一行）。
pub type Trace = Rc<dyn Fn(&str)>;

fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 外壳分配的 PIDL，离开作用域时释放。
pub struct Pidl(*mut ITEMIDLIST);

impl Pidl {
    /// 解析文件系统路径（不存在的路径会失败）。
    pub fn parse(path: &Path) -> Result<Pidl> {
        let w = wide(path);
        let mut p: *mut ITEMIDLIST = std::ptr::null_mut();
        // SAFETY: w 以 0 结尾；成功时 p 由外壳分配，由 Drop 释放。
        unsafe { SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut p, 0, None)? };
        Ok(Pidl(p))
    }

    pub fn as_ptr(&self) -> *const ITEMIDLIST {
        self.0
    }

    /// PIDL 字节（含结尾的 0 长度项）。
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: self.0 是有效 PIDL；ILGetSize 返回含终止项的总长度。
        unsafe {
            let n = ILGetSize(Some(self.0)) as usize;
            std::slice::from_raw_parts(self.0 as *const u8, n)
        }
    }

    fn variant(&self) -> Result<VARIANT> {
        let b = self.bytes();
        // SAFETY: 缓冲区在调用期间有效；VARIANT 复制了内容（VT_ARRAY|VT_UI1）。
        unsafe { InitVariantFromBuffer(b.as_ptr() as _, b.len() as u32) }
    }
}

impl Drop for Pidl {
    fn drop(&mut self) {
        // SAFETY: 由 SHParseDisplayName／ILCombine 分配。
        unsafe { ILFree(Some(self.0)) };
    }
}

/// 子项 PIDL（相对 `folder`）→ 完整文件系统路径。
fn child_path(folder: &Path, child: *const ITEMIDLIST) -> Option<PathBuf> {
    if child.is_null() {
        return None;
    }
    let parent = Pidl::parse(folder).ok()?;
    // SAFETY: 两个 PIDL 都有效；ILCombine 返回新分配的 PIDL，交给 Pidl 释放。
    let full = unsafe { ILCombine(Some(parent.as_ptr()), Some(child)) };
    if full.is_null() {
        return None;
    }
    let full = Pidl(full);
    // SAFETY: SHGetNameFromIDList 返回 CoTaskMem 字符串，读取后释放。
    unsafe {
        let s = SHGetNameFromIDList(full.as_ptr(), SIGDN_FILESYSPATH).ok()?;
        let path = PathBuf::from(s.to_string().ok()?);
        CoTaskMemFree(Some(s.0 as _));
        Some(path)
    }
}

fn guid_name(g: &GUID) -> String {
    let known: &[(GUID, &str)] = &[
        (IShellView::IID, "IShellView"),
        (IFolderView::IID, "IFolderView"),
        (IShellBrowser::IID, "IShellBrowser"),
        (IServiceProvider::IID, "IServiceProvider"),
        (IDispatch::IID, "IDispatch"),
        (IWebBrowserApp::IID, "IWebBrowserApp"),
        (
            windows::Win32::UI::Shell::SID_STopLevelBrowser,
            "SID_STopLevelBrowser",
        ),
    ];
    known
        .iter()
        .find(|(k, _)| k == g)
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| format!("{g:?}"))
}

// ---------- 槽的共享状态 ----------

struct SlotState {
    /// 脚本（PowerShell 的 COM 适配器只认类型信息）用来发现成员的类型库信息
    infos: Rc<TypeInfos>,
    slot: u64,
    folder: RefCell<PathBuf>,
    hwnd: isize,
    sink: Sink,
    trace: Trace,
}

// ---------- IShellView：只实现 SelectItem 与 GetWindow ----------

#[implement(IShellView)]
struct View(Rc<SlotState>);

#[allow(non_snake_case)]
impl IOleWindow_Impl for View_Impl {
    fn GetWindow(&self) -> Result<HWND> {
        (self.0.trace)(&format!("IShellView::GetWindow 槽 {}", self.0.slot));
        Ok(HWND(self.0.hwnd as _))
    }
    fn ContextSensitiveHelp(&self, _f: BOOL) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
}

#[allow(non_snake_case)]
impl IShellView_Impl for View_Impl {
    fn TranslateAccelerator(&self, _pmsg: *const MSG) -> HRESULT {
        E_NOTIMPL
    }
    fn EnableModeless(&self, _f: BOOL) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn UIActivate(&self, ustate: u32) -> Result<()> {
        (self.0.trace)(&format!("IShellView::UIActivate {ustate}"));
        Err(E_NOTIMPL.into())
    }
    fn Refresh(&self) -> Result<()> {
        (self.0.trace)("IShellView::Refresh");
        Err(E_NOTIMPL.into())
    }
    fn CreateViewWindow(
        &self,
        _prev: Ref<IShellView>,
        _pfs: *const FOLDERSETTINGS,
        _psb: Ref<IShellBrowser>,
        _rc: *const RECT,
    ) -> Result<HWND> {
        Err(E_NOTIMPL.into())
    }
    fn DestroyViewWindow(&self) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetCurrentInfo(&self) -> Result<FOLDERSETTINGS> {
        (self.0.trace)("IShellView::GetCurrentInfo");
        Err(E_NOTIMPL.into())
    }
    fn AddPropertySheetPages(
        &self,
        _r: u32,
        _pfn: LPFNSVADDPROPSHEETPAGE,
        _l: LPARAM,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn SaveViewState(&self) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn SelectItem(&self, pidlitem: *const ITEMIDLIST, uflags: u32) -> Result<()> {
        let s = &self.0;
        let folder = s.folder.borrow().clone();
        let item = child_path(&folder, pidlitem);
        (s.trace)(&format!(
            "IShellView::SelectItem 槽 {} 标志 0x{uflags:x} 项目 {}",
            s.slot,
            item.as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "（无法解析）".into())
        ));
        (s.sink)(Selection {
            slot: s.slot,
            folder,
            item,
            flags: uflags,
        });
        Ok(())
    }
    fn GetItemObject(
        &self,
        _u: &_SVGIO,
        riid: *const GUID,
        _ppv: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        // SAFETY: riid 由调用方提供。
        let n = unsafe { riid.as_ref().map(guid_name).unwrap_or_default() };
        (self.0.trace)(&format!("IShellView::GetItemObject {n}"));
        Err(E_NOTIMPL.into())
    }
}

// ---------- 后期绑定：脚本经 IDispatch 读到的只读属性（Issue #51） ----------

const DISPATCH_METHOD: u16 = 0x1;
const DISPATCH_PROPERTYGET: u16 = 0x2;

/// 本对象树用到的类型库信息（Shell32 与 SHDocVw 类型库；任何一个取不到就退回手写名称表）。
#[derive(Default)]
struct TypeInfos {
    app: Option<ITypeInfo>,
    doc: Option<ITypeInfo>,
    folder: Option<ITypeInfo>,
    item: Option<ITypeInfo>,
}

fn typeinfo_of(lib: &windows::Win32::System::Com::ITypeLib, iid: &GUID) -> Option<ITypeInfo> {
    // SAFETY: lib 有效，iid 是有效 GUID。
    unsafe { lib.GetTypeInfoOfGuid(iid).ok() }
}

fn load_typeinfos() -> TypeInfos {
    // SAFETY: 参数都是有效值；LoadRegTypeLib 只读注册表里的类型库登记。
    let load = |libid: u128, major: u16, minor: u16| unsafe {
        LoadRegTypeLib(&GUID::from_u128(libid), major, minor, 0).ok()
    };
    let mut t = TypeInfos::default();
    if let Some(lib) = load(0xEAB22AC0_30C1_11CF_A7EB_0000C05BAE0B, 1, 1) {
        t.app = typeinfo_of(&lib, &IWebBrowserApp::IID);
    }
    if let Some(lib) = load(0x50A7E9B0_70EF_11D1_B75A_00A0C90564FE, 1, 0) {
        // IShellFolderViewDual（Document）、Folder3（Document.Folder）、FolderItem2（…Self）
        t.doc = typeinfo_of(
            &lib,
            &GUID::from_u128(0xE7A1AF80_4D96_11CF_960C_0080C7F4EE85),
        );
        t.folder = typeinfo_of(
            &lib,
            &GUID::from_u128(0xA7AE5F64_C4D7_4D7F_9307_4D24EE54B841),
        );
        t.item = typeinfo_of(
            &lib,
            &GUID::from_u128(0xEDC817AA_92B8_11D1_B075_00C04FC33AA5),
        );
    }
    t
}

/// 名称 → DISPID：有类型信息时用类型库的编号（与真实资源管理器一致），否则用手写表。
fn resolve_ids(
    ti: &Option<ITypeInfo>,
    table: &[(&str, i32)],
    names: *const PCWSTR,
    count: u32,
    ids: *mut i32,
) -> Result<()> {
    match ti {
        // SAFETY: 参数由调用方按 IDispatch 约定提供。
        Some(t) => unsafe { DispGetIDsOfNames(t, names, count, ids) },
        None => lookup_ids(table, names, count, ids),
    }
}

/// DISPID → 手写表里的成员序号（用名称在类型信息里反查编号，保证与 `resolve_ids` 一致）。
fn member_of(ti: &Option<ITypeInfo>, table: &[(&str, i32)], id: i32) -> Option<i32> {
    table.iter().find_map(|(name, fallback)| {
        let got = match ti {
            Some(t) => {
                let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
                let p = PCWSTR(w.as_ptr());
                let mut out = -1i32;
                // SAFETY: p 以 0 结尾；out 为单个元素的输出。
                unsafe { DispGetIDsOfNames(t, &p, 1, &mut out) }
                    .ok()
                    .map(|_| out)
            }
            None => Some(*fallback),
        };
        (got == Some(id)).then_some(*fallback)
    })
}

/// `GetIDsOfNames` 的手写实现：名称不区分大小写，未知名称返回 `DISP_E_UNKNOWNNAME`。
fn lookup_ids(
    table: &[(&str, i32)],
    names: *const PCWSTR,
    count: u32,
    ids: *mut i32,
) -> Result<()> {
    if names.is_null() || ids.is_null() || count == 0 {
        return Err(E_INVALIDARG.into());
    }
    let mut result = Ok(());
    for i in 0..count as usize {
        // SAFETY: 调用方保证 names／ids 各有 count 个元素。
        let (name, slot) = unsafe { ((*names.add(i)).to_string().unwrap_or_default(), ids.add(i)) };
        // 命名参数（i > 0）：本对象的属性都没有参数，一律未知
        let hit = if i == 0 {
            table
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(&name))
                .map(|(_, id)| *id)
        } else {
            None
        };
        // SAFETY: slot 指向调用方提供的 DISPID 数组元素。
        unsafe { *slot = hit.unwrap_or(-1) };
        if hit.is_none() {
            result = Err(DISP_E_UNKNOWNNAME.into());
        }
    }
    result
}

/// 只读属性的 `Invoke`：属性读取（和无参方法）按 `get` 求值写入 `result`；写属性等一律返回成员不存在。
fn invoke_get(
    flags: DISPATCH_FLAGS,
    result: *mut VARIANT,
    get: impl FnOnce() -> Result<VARIANT>,
) -> Result<()> {
    if flags.0 & (DISPATCH_PROPERTYGET | DISPATCH_METHOD) == 0 {
        return Err(DISP_E_MEMBERNOTFOUND.into());
    }
    let v = get()?;
    if !result.is_null() {
        // SAFETY: result 由调用方提供且按约定未初始化；ptr::write 不析构旧值。
        unsafe { std::ptr::write(result, v) };
    }
    Ok(())
}

fn folder_url(folder: &Path) -> Result<BSTR> {
    let w = wide(folder);
    let mut buf = vec![0u16; 4096];
    let mut n = buf.len() as u32;
    // SAFETY: 输入以 0 结尾；输出缓冲区长度为 n。
    unsafe { UrlCreateFromPathW(PCWSTR(w.as_ptr()), PWSTR(buf.as_mut_ptr()), &mut n, 0)? };
    Ok(BSTR::from_wide(&buf[..n as usize]))
}

fn folder_name(f: &Path) -> String {
    f.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| f.display().to_string())
}

/// `Document.Folder`（标题与自身）、`Document.Folder.Self`（路径与名称）上的只读对象。
#[derive(Clone, Copy, Debug)]
enum PropKind {
    Folder,
    FolderItem,
}

const FOLDER_NAMES: &[(&str, i32)] = &[("Title", 0), ("Self", 1)];
const ITEM_NAMES: &[(&str, i32)] = &[
    ("Path", 0),
    ("Name", 1),
    ("IsFolder", 2),
    ("IsFileSystem", 3),
    ("IsBrowsable", 4),
];
const DOC_NAMES: &[(&str, i32)] = &[("Folder", 0)];

#[implement(IDispatch)]
struct PropObj {
    kind: PropKind,
    state: Rc<SlotState>,
}

impl PropObj_Impl {
    fn info(&self) -> &Option<ITypeInfo> {
        match self.kind {
            PropKind::Folder => &self.state.infos.folder,
            PropKind::FolderItem => &self.state.infos.item,
        }
    }
}

#[allow(non_snake_case)]
impl IDispatch_Impl for PropObj_Impl {
    fn GetTypeInfoCount(&self) -> Result<u32> {
        Ok(self.info().is_some() as u32)
    }
    fn GetTypeInfo(&self, i: u32, _l: u32) -> Result<ITypeInfo> {
        match (self.info(), i) {
            (Some(t), 0) => Ok(t.clone()),
            _ => Err(E_NOTIMPL.into()),
        }
    }
    fn GetIDsOfNames(
        &self,
        _riid: *const GUID,
        names: *const PCWSTR,
        c: u32,
        _l: u32,
        ids: *mut i32,
    ) -> Result<()> {
        let (table, ti) = match self.kind {
            PropKind::Folder => (FOLDER_NAMES, &self.state.infos.folder),
            PropKind::FolderItem => (ITEM_NAMES, &self.state.infos.item),
        };
        resolve_ids(ti, table, names, c, ids)
    }
    fn Invoke(
        &self,
        id: i32,
        _riid: *const GUID,
        _l: u32,
        f: DISPATCH_FLAGS,
        _p: *const DISPPARAMS,
        r: *mut VARIANT,
        _e: *mut EXCEPINFO,
        _a: *mut u32,
    ) -> Result<()> {
        let folder = self.state.folder.borrow().clone();
        (self.state.trace)(&format!("{:?}::Invoke {id}", self.kind));
        let (table, ti) = match self.kind {
            PropKind::Folder => (FOLDER_NAMES, &self.state.infos.folder),
            PropKind::FolderItem => (ITEM_NAMES, &self.state.infos.item),
        };
        let id = member_of(ti, table, id).unwrap_or(-1000);
        invoke_get(f, r, || match (self.kind, id) {
            (PropKind::Folder, 0) => Ok(VARIANT::from(folder_name(&folder).as_str())),
            (PropKind::Folder, 1) => Ok(VARIANT::from(IDispatch::from(PropObj {
                kind: PropKind::FolderItem,
                state: self.state.clone(),
            }))),
            (PropKind::FolderItem, 0) => Ok(VARIANT::from(folder.display().to_string().as_str())),
            (PropKind::FolderItem, 1) => Ok(VARIANT::from(folder_name(&folder).as_str())),
            (PropKind::FolderItem, 2..=4) => Ok(VARIANT::from(true)),
            _ => Err(DISP_E_MEMBERNOTFOUND.into()),
        })
    }
}

// ---------- 文档：IDispatch + IServiceProvider ----------

#[implement(IDispatch, IServiceProvider)]
struct Document {
    view: IShellView,
    state: Rc<SlotState>,
    trace: Trace,
}

#[allow(non_snake_case)]
impl IDispatch_Impl for Document_Impl {
    fn GetTypeInfoCount(&self) -> Result<u32> {
        Ok(self.state.infos.doc.is_some() as u32)
    }
    fn GetTypeInfo(&self, i: u32, _l: u32) -> Result<ITypeInfo> {
        match (&self.state.infos.doc, i) {
            (Some(t), 0) => Ok(t.clone()),
            _ => Err(E_NOTIMPL.into()),
        }
    }
    fn GetIDsOfNames(
        &self,
        _riid: *const GUID,
        names: *const PCWSTR,
        c: u32,
        _l: u32,
        ids: *mut i32,
    ) -> Result<()> {
        resolve_ids(&self.state.infos.doc, DOC_NAMES, names, c, ids)
    }
    fn Invoke(
        &self,
        id: i32,
        _riid: *const GUID,
        _l: u32,
        f: DISPATCH_FLAGS,
        _p: *const DISPPARAMS,
        r: *mut VARIANT,
        _e: *mut EXCEPINFO,
        _a: *mut u32,
    ) -> Result<()> {
        (self.trace)(&format!("Document::Invoke {id}"));
        let id = member_of(&self.state.infos.doc, DOC_NAMES, id).unwrap_or(-1000);
        invoke_get(f, r, || match id {
            0 => Ok(VARIANT::from(IDispatch::from(PropObj {
                kind: PropKind::Folder,
                state: self.state.clone(),
            }))),
            _ => Err(DISP_E_MEMBERNOTFOUND.into()),
        })
    }
}

#[allow(non_snake_case)]
impl IServiceProvider_Impl for Document_Impl {
    fn QueryService(
        &self,
        guidservice: *const GUID,
        riid: *const GUID,
        ppv: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        if ppv.is_null() || guidservice.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: 指针已检查非空，由调用方提供。
        let (svc, iid) = unsafe { (*guidservice, *riid) };
        (self.trace)(&format!(
            "IServiceProvider::QueryService {} → {}",
            guid_name(&svc),
            guid_name(&iid)
        ));
        // SAFETY: ppv 非空；失败时置空。
        unsafe { *ppv = std::ptr::null_mut() };
        if svc == IFolderView::IID || svc == IShellView::IID {
            // SAFETY: query 按 iid 写入 ppv 并增加引用计数。
            return unsafe { self.view.query(&iid, ppv) }.ok();
        }
        Err(E_NOINTERFACE.into())
    }
}

// ---------- IWebBrowserApp：只实现 get_HWND 与 get_Document ----------

#[implement(IWebBrowserApp)]
struct BrowserApp {
    /// SHDocVw 类型库里 `IWebBrowserApp` 的类型信息：后期绑定的名称解析与分发都交给它（Issue #51）；
    /// 类型库不可用时为 `None`，此时后期绑定读取返回 `E_NOTIMPL`。
    typeinfo: Option<ITypeInfo>,
    hwnd: isize,
    doc: IDispatch,
    state: Rc<SlotState>,
    trace: Trace,
}

const APP_NAME: &str = "Slint File Manager";

impl BrowserApp_Impl {
    fn window_rect(&self) -> RECT {
        let mut r = RECT::default();
        // SAFETY: r 为有效输出；窗口句柄失效时调用失败，保持全 0。
        let _ = unsafe { GetWindowRect(HWND(self.hwnd as _), &mut r) };
        r
    }
}

macro_rules! notimpl {
    ($( fn $name:ident(&self $(, $a:ident : $t:ty)*) -> $r:ty; )*) => {
        $( fn $name(&self $(, $a: $t)*) -> $r { $( let _ = $a; )* Err(E_NOTIMPL.into()) } )*
    };
}

#[allow(non_snake_case)]
impl IDispatch_Impl for BrowserApp_Impl {
    fn GetTypeInfoCount(&self) -> Result<u32> {
        Ok(self.typeinfo.is_some() as u32)
    }
    fn GetTypeInfo(&self, i: u32, _l: u32) -> Result<ITypeInfo> {
        match (&self.typeinfo, i) {
            (Some(t), 0) => Ok(t.clone()),
            _ => Err(E_NOTIMPL.into()),
        }
    }
    fn GetIDsOfNames(
        &self,
        _riid: *const GUID,
        names: *const PCWSTR,
        c: u32,
        _l: u32,
        ids: *mut i32,
    ) -> Result<()> {
        let Some(t) = &self.typeinfo else {
            return Err(E_NOTIMPL.into());
        };
        // SAFETY: 参数由调用方按 IDispatch 约定提供。
        unsafe { DispGetIDsOfNames(t, names, c, ids) }
    }
    fn Invoke(
        &self,
        id: i32,
        _riid: *const GUID,
        _l: u32,
        f: DISPATCH_FLAGS,
        p: *const DISPPARAMS,
        r: *mut VARIANT,
        e: *mut EXCEPINFO,
        a: *mut u32,
    ) -> Result<()> {
        (self.trace)(&format!("IWebBrowserApp::Invoke {id}"));
        let Some(t) = &self.typeinfo else {
            return Err(E_NOTIMPL.into());
        };
        // 只开放读取（和无参方法）；写属性一律“成员不存在”
        if f.0 & (DISPATCH_PROPERTYGET | DISPATCH_METHOD) == 0 {
            return Err(DISP_E_MEMBERNOTFOUND.into());
        }
        let this: InterfaceRef<'_, IWebBrowserApp> = self.as_interface_ref();
        // SAFETY: this 是本对象的 IWebBrowserApp 接口指针，与类型信息的布局一致（双接口）；
        // 其余参数由调用方按 IDispatch::Invoke 约定提供。DispInvoke 经 vtable 调用下面的早期绑定实现。
        unsafe { DispInvoke(this.as_raw(), t, id, f.0, p as *mut DISPPARAMS, r, e, a) }
    }
}

#[allow(non_snake_case)]
impl IWebBrowser_Impl for BrowserApp_Impl {
    notimpl! {
        fn GoBack(&self) -> Result<()>;
        fn GoForward(&self) -> Result<()>;
        fn GoHome(&self) -> Result<()>;
        fn GoSearch(&self) -> Result<()>;
        fn Refresh(&self) -> Result<()>;
        fn Refresh2(&self, level: *const VARIANT) -> Result<()>;
        fn Stop(&self) -> Result<()>;
        fn Application(&self) -> Result<IDispatch>;
        fn Parent(&self) -> Result<IDispatch>;
        fn Container(&self) -> Result<IDispatch>;
        fn SetLeft(&self, v: i32) -> Result<()>;
        fn SetTop(&self, v: i32) -> Result<()>;
        fn SetWidth(&self, v: i32) -> Result<()>;
        fn SetHeight(&self, v: i32) -> Result<()>;
    }
    fn TopLevelContainer(&self) -> Result<VARIANT_BOOL> {
        Ok(VARIANT_BOOL(-1))
    }
    fn Type(&self) -> Result<BSTR> {
        Ok(BSTR::from("Document"))
    }
    fn Left(&self) -> Result<i32> {
        Ok(self.window_rect().left)
    }
    fn Top(&self) -> Result<i32> {
        Ok(self.window_rect().top)
    }
    fn Width(&self) -> Result<i32> {
        let r = self.window_rect();
        Ok(r.right - r.left)
    }
    fn Height(&self) -> Result<i32> {
        let r = self.window_rect();
        Ok(r.bottom - r.top)
    }
    fn Busy(&self) -> Result<VARIANT_BOOL> {
        Ok(VARIANT_BOOL(0))
    }
    // 位置名与 URL：给枚举外壳窗口读取“当前文件夹”的工具（与资源管理器窗口的格式一致）
    fn LocationName(&self) -> Result<BSTR> {
        Ok(BSTR::from(folder_name(&self.state.folder.borrow())))
    }
    fn LocationURL(&self) -> Result<BSTR> {
        folder_url(&self.state.folder.borrow())
    }
    fn Navigate(
        &self,
        _url: &BSTR,
        _f: *const VARIANT,
        _t: *const VARIANT,
        _p: *const VARIANT,
        _h: *const VARIANT,
    ) -> Result<()> {
        (self.trace)("IWebBrowserApp::Navigate");
        Err(E_NOTIMPL.into())
    }
    fn Document(&self) -> Result<IDispatch> {
        (self.trace)("IWebBrowserApp::get_Document");
        Ok(self.doc.clone())
    }
}

#[allow(non_snake_case)]
impl IWebBrowserApp_Impl for BrowserApp_Impl {
    notimpl! {
        fn Quit(&self) -> Result<()>;
        fn ClientToWindow(&self, x: *mut i32, y: *mut i32) -> Result<()>;
        fn PutProperty(&self, p: &BSTR, v: &VARIANT) -> Result<()>;
        fn GetProperty(&self, p: &BSTR) -> Result<VARIANT>;
        fn StatusBar(&self) -> Result<VARIANT_BOOL>;
        fn SetStatusBar(&self, v: VARIANT_BOOL) -> Result<()>;
        fn StatusText(&self) -> Result<BSTR>;
        fn SetStatusText(&self, v: &BSTR) -> Result<()>;
        fn ToolBar(&self) -> Result<i32>;
        fn SetToolBar(&self, v: i32) -> Result<()>;
        fn MenuBar(&self) -> Result<VARIANT_BOOL>;
        fn SetMenuBar(&self, v: VARIANT_BOOL) -> Result<()>;
        fn FullScreen(&self) -> Result<VARIANT_BOOL>;
        fn SetFullScreen(&self, v: VARIANT_BOOL) -> Result<()>;
    }
    fn Name(&self) -> Result<BSTR> {
        Ok(BSTR::from(APP_NAME))
    }
    fn FullName(&self) -> Result<BSTR> {
        Ok(BSTR::from(
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        ))
    }
    fn Path(&self) -> Result<BSTR> {
        Ok(BSTR::from(
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.display().to_string()))
                .unwrap_or_default(),
        ))
    }
    fn HWND(&self) -> Result<SHANDLE_PTR> {
        (self.trace)("IWebBrowserApp::get_HWND");
        Ok(SHANDLE_PTR(self.hwnd))
    }
    fn Visible(&self) -> Result<VARIANT_BOOL> {
        (self.trace)("IWebBrowserApp::get_Visible");
        Ok(VARIANT_BOOL(-1))
    }
    fn SetVisible(&self, v: VARIANT_BOOL) -> Result<()> {
        (self.trace)(&format!("IWebBrowserApp::put_Visible {}", v.0));
        Ok(())
    }
}

// ---------- 登记 ----------

struct Entry {
    cookie: i32,
    state: Rc<SlotState>,
}

/// 本窗口在 `IShellWindows` 里的登记（每个槽一个 cookie）。丢弃时全部注销。
pub struct ShellWindows {
    sw: IShellWindows,
    infos: Rc<TypeInfos>,
    hwnd: isize,
    sink: Sink,
    trace: Trace,
    entries: BTreeMap<u64, Entry>,
    /// 登记失败的槽与文件夹：文件夹不变时不再重试（避免每次刷新界面都去解析不可用的路径）
    failed: BTreeMap<u64, PathBuf>,
}

impl ShellWindows {
    /// 连接资源管理器的 `ShellWindows` 对象（跨进程；资源管理器没有运行时失败）。
    pub fn new(hwnd: isize, sink: Sink, trace: Trace) -> Result<ShellWindows> {
        // SAFETY: 调用线程已初始化 COM。
        let sw: IShellWindows = unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)? };
        let infos = Rc::new(load_typeinfos());
        trace(&format!(
            "后期绑定类型信息：浏览器 {} 文档 {} 文件夹 {} 项目 {}",
            infos.app.is_some(),
            infos.doc.is_some(),
            infos.folder.is_some(),
            infos.item.is_some()
        ));
        Ok(ShellWindows {
            sw,
            infos,
            hwnd,
            sink,
            trace,
            entries: BTreeMap::new(),
            failed: BTreeMap::new(),
        })
    }

    /// 已登记的槽与文件夹。
    pub fn registered(&self) -> Vec<(u64, PathBuf)> {
        self.entries
            .iter()
            .map(|(k, e)| (*k, e.state.folder.borrow().clone()))
            .collect()
    }

    /// 登记或更新一个槽的文件夹。文件夹必须存在（解析成 PIDL）。
    pub fn set(&mut self, slot: u64, folder: &Path) -> Result<()> {
        if let Some(e) = self.entries.get(&slot) {
            if *e.state.folder.borrow() == folder {
                return Ok(());
            }
            let v = Pidl::parse(folder)?.variant()?;
            // SAFETY: cookie 来自本对象的 Register；v 在调用期间有效。
            unsafe { self.sw.OnNavigate(e.cookie, &v)? };
            *e.state.folder.borrow_mut() = folder.to_path_buf();
            (self.trace)(&format!(
                "OnNavigate 槽 {slot} cookie {} → {}",
                e.cookie,
                folder.display()
            ));
            return Ok(());
        }
        let v = Pidl::parse(folder)?.variant()?;
        let empty = VARIANT::default();
        let state = Rc::new(SlotState {
            infos: self.infos.clone(),
            slot,
            folder: RefCell::new(folder.to_path_buf()),
            hwnd: self.hwnd,
            sink: self.sink.clone(),
            trace: self.trace.clone(),
        });
        let view: IShellView = View(state.clone()).into();
        let doc: IDispatch = Document {
            view,
            state: state.clone(),
            trace: self.trace.clone(),
        }
        .into();
        let app: IWebBrowserApp = BrowserApp {
            typeinfo: self.infos.app.clone(),
            hwnd: self.hwnd,
            doc,
            state: state.clone(),
            trace: self.trace.clone(),
        }
        .into();
        let disp: IDispatch = app.cast()?;
        // SAFETY: 同一线程上先 RegisterPending 再 Register（系统按线程 id 把两者对应起来，cookie 相同）。
        // 没有 RegisterPending 时，冷启动路径上的选中请求不会到来（Explorer++ 的实测结论）。
        let cookie = unsafe {
            let tid = GetCurrentThreadId() as i32;
            let pending = self.sw.RegisterPending(tid, &v, &empty, SWC_BROWSER)?;
            let cookie = match self.sw.Register(&disp, self.hwnd as i32, SWC_BROWSER) {
                Ok(c) => c,
                Err(e) => {
                    let _ = self.sw.Revoke(pending);
                    return Err(e);
                }
            };
            if cookie != pending {
                (self.trace)(&format!(
                    "注意：RegisterPending cookie {pending} 与 Register cookie {cookie} 不同"
                ));
                let _ = self.sw.Revoke(pending);
            }
            cookie
        };
        (self.trace)(&format!(
            "Register 槽 {slot} cookie {cookie} → {}",
            folder.display()
        ));
        self.entries.insert(slot, Entry { cookie, state });
        Ok(())
    }

    /// 注销一个槽。
    pub fn revoke(&mut self, slot: u64) {
        if let Some(e) = self.entries.remove(&slot) {
            // SAFETY: cookie 来自本对象的 Register。
            let r = unsafe { self.sw.Revoke(e.cookie) };
            (self.trace)(&format!(
                "Revoke 槽 {slot} cookie {}：{}",
                e.cookie,
                if r.is_ok() { "成功" } else { "失败" }
            ));
        }
    }

    /// 让登记与 `want`（槽 → 文件夹）一致：多余的注销，新的登记，变化的 OnNavigate。返回失败的槽与原因。
    pub fn sync(&mut self, want: &[(u64, PathBuf)]) -> Vec<(u64, String)> {
        let keep: Vec<u64> = want.iter().map(|(s, _)| *s).collect();
        let stale: Vec<u64> = self
            .entries
            .keys()
            .filter(|k| !keep.contains(k))
            .copied()
            .collect();
        for s in stale {
            self.revoke(s);
        }
        self.failed.retain(|k, _| keep.contains(k));
        let mut failed = Vec::new();
        for (slot, folder) in want {
            if self.failed.get(slot) == Some(folder) {
                continue;
            }
            match self.set(*slot, folder) {
                Ok(()) => {
                    self.failed.remove(slot);
                }
                Err(e) => {
                    // 登记失败（文件夹已不存在等）：注销旧登记，免得外壳把请求发到错误的文件夹
                    self.revoke(*slot);
                    self.failed.insert(*slot, folder.clone());
                    failed.push((*slot, e.message()));
                }
            }
        }
        failed
    }

    /// 全部注销。
    pub fn revoke_all(&mut self) {
        let all: Vec<u64> = self.entries.keys().copied().collect();
        for s in all {
            self.revoke(s);
        }
    }
}

impl Drop for ShellWindows {
    fn drop(&mut self) {
        self.revoke_all();
    }
}
