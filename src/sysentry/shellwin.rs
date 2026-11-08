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
    E_NOINTERFACE, E_NOTIMPL, E_POINTER, HWND, LPARAM, RECT, SHANDLE_PTR, VARIANT_BOOL,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, DISPATCH_FLAGS, DISPPARAMS, EXCEPINFO, IDispatch,
    IDispatch_Impl, IServiceProvider, IServiceProvider_Impl, ITypeInfo,
};
use windows::Win32::System::Ole::IOleWindow_Impl;
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
use windows::Win32::UI::WindowsAndMessaging::MSG;
use windows::core::{BOOL, BSTR, GUID, HRESULT, Interface, PCWSTR, PWSTR, Ref, Result, implement};

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

// ---------- 文档：IDispatch + IServiceProvider ----------

#[implement(IDispatch, IServiceProvider)]
struct Document {
    view: IShellView,
    trace: Trace,
}

#[allow(non_snake_case)]
impl IDispatch_Impl for Document_Impl {
    fn GetTypeInfoCount(&self) -> Result<u32> {
        Err(E_NOTIMPL.into())
    }
    fn GetTypeInfo(&self, _i: u32, _l: u32) -> Result<ITypeInfo> {
        Err(E_NOTIMPL.into())
    }
    fn GetIDsOfNames(
        &self,
        _riid: *const GUID,
        _names: *const PCWSTR,
        _c: u32,
        _l: u32,
        _ids: *mut i32,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Invoke(
        &self,
        id: i32,
        _riid: *const GUID,
        _l: u32,
        _f: DISPATCH_FLAGS,
        _p: *const DISPPARAMS,
        _r: *mut VARIANT,
        _e: *mut EXCEPINFO,
        _a: *mut u32,
    ) -> Result<()> {
        (self.trace)(&format!("Document::Invoke {id}"));
        Err(E_NOTIMPL.into())
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
    hwnd: isize,
    doc: IDispatch,
    state: Rc<SlotState>,
    trace: Trace,
}

macro_rules! notimpl {
    ($( fn $name:ident(&self $(, $a:ident : $t:ty)*) -> $r:ty; )*) => {
        $( fn $name(&self $(, $a: $t)*) -> $r { $( let _ = $a; )* Err(E_NOTIMPL.into()) } )*
    };
}

#[allow(non_snake_case)]
impl IDispatch_Impl for BrowserApp_Impl {
    fn GetTypeInfoCount(&self) -> Result<u32> {
        Err(E_NOTIMPL.into())
    }
    fn GetTypeInfo(&self, _i: u32, _l: u32) -> Result<ITypeInfo> {
        Err(E_NOTIMPL.into())
    }
    fn GetIDsOfNames(
        &self,
        _riid: *const GUID,
        _names: *const PCWSTR,
        _c: u32,
        _l: u32,
        _ids: *mut i32,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Invoke(
        &self,
        id: i32,
        _riid: *const GUID,
        _l: u32,
        _f: DISPATCH_FLAGS,
        _p: *const DISPPARAMS,
        _r: *mut VARIANT,
        _e: *mut EXCEPINFO,
        _a: *mut u32,
    ) -> Result<()> {
        (self.trace)(&format!("IWebBrowserApp::Invoke {id}"));
        Err(E_NOTIMPL.into())
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
        fn TopLevelContainer(&self) -> Result<VARIANT_BOOL>;
        fn Type(&self) -> Result<BSTR>;
        fn Left(&self) -> Result<i32>;
        fn SetLeft(&self, v: i32) -> Result<()>;
        fn Top(&self) -> Result<i32>;
        fn SetTop(&self, v: i32) -> Result<()>;
        fn Width(&self) -> Result<i32>;
        fn SetWidth(&self, v: i32) -> Result<()>;
        fn Height(&self) -> Result<i32>;
        fn SetHeight(&self, v: i32) -> Result<()>;
        fn Busy(&self) -> Result<VARIANT_BOOL>;
    }
    // 位置名与 URL：给枚举外壳窗口读取“当前文件夹”的工具（与资源管理器窗口的格式一致）
    fn LocationName(&self) -> Result<BSTR> {
        let f = self.state.folder.borrow();
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| f.display().to_string());
        Ok(BSTR::from(name))
    }
    fn LocationURL(&self) -> Result<BSTR> {
        let w = wide(&self.state.folder.borrow());
        let mut buf = vec![0u16; 4096];
        let mut n = buf.len() as u32;
        // SAFETY: 输入以 0 结尾；输出缓冲区长度为 n。
        unsafe { UrlCreateFromPathW(PCWSTR(w.as_ptr()), PWSTR(buf.as_mut_ptr()), &mut n, 0)? };
        Ok(BSTR::from_wide(&buf[..n as usize]))
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
        fn Name(&self) -> Result<BSTR>;
        fn FullName(&self) -> Result<BSTR>;
        fn Path(&self) -> Result<BSTR>;
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
        Ok(ShellWindows {
            sw,
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
            slot,
            folder: RefCell::new(folder.to_path_buf()),
            hwnd: self.hwnd,
            sink: self.sink.clone(),
            trace: self.trace.clone(),
        });
        let view: IShellView = View(state.clone()).into();
        let doc: IDispatch = Document {
            view,
            trace: self.trace.clone(),
        }
        .into();
        let app: IWebBrowserApp = BrowserApp {
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
