//! 原生右键菜单：把 Windows 外壳的 `IContextMenu` 菜单（含 NanaZip 等第三方扩展）弹在本应用的文件列表上。
//!
//! 做法来自 Issue #3 的原型（docs/research/0003-shell-menu-and-cloud-compat.md），在产品窗口里的调整：
//!
//! - 菜单的 owner 直接用**本应用主窗口**（前台窗口），不另建隐藏窗口：主窗口保持激活，菜单关闭后键盘焦点原样不动。
//!   菜单期间临时给主窗口装一个窗口子类（`SetWindowSubclass`），把 `WM_INITMENUPOPUP`、`WM_DRAWITEM`、
//!   `WM_MEASUREITEM`、`WM_MENUCHAR`、`WM_MENUSELECT` 转给 `IContextMenu3`／`IContextMenu2`
//!   （不转发的话，NanaZip、“发送到”“新建”这类延迟填充的子菜单是空的），菜单结束立即拆除。
//! - 单选／多选：`SHCreateShellItemArrayFromIDLists` → `BHID_SFUIObject`；空白处：文件夹自身的
//!   `IShellFolder::CreateViewObject(IID_IContextMenu)`（“新建”“粘贴”“属性”等）。
//! - `TrackPopupMenuEx(TPM_RETURNCMD)` 的返回值为准：0 为取消，只销毁菜单与接口；非 0 才 `InvokeCommand`。
//!   不使用 `CMIC_MASK_NOASYNC`：该标志会让属性页等命令阻塞界面线程；本线程持续泵消息，异步命令可以正常完成。
//! - 命令执行、取消、失败都有明确结果（[`Outcome`]），任何路径都销毁菜单、释放接口、拆除子类、清空会话状态。
//! - NanaZip：菜单里没有“NanaZip”子菜单时，追加一条灰色提示，不伪造压缩／解压能力。
//!
//! 线程：必须在创建主窗口的 UI 线程（STA）上调用，不能重入（菜单期间再次请求直接返回 [`MenuError::Busy`]）。
//! `TrackPopupMenuEx` 是模态循环，期间 Slint 事件循环不运行；见 docs/shell-context-menu.md 的实测。

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, IBindCtx,
};
use windows::Win32::System::Threading::{
    GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetCurrentThreadId, GetGuiResources,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_SHIFT};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_SFUIObject, CMF_EXTENDEDVERBS, CMF_NORMAL, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX,
    DefSubclassProc, GCS_VERBW, IContextMenu, IContextMenu2, IContextMenu3, IShellFolder,
    RemoveWindowSubclass, SHBindToObject, SHCreateShellItemArrayFromIDLists, SHParseDisplayName,
    SetWindowSubclass,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, EnumThreadWindows, GW_OWNER, GetClassNameW,
    GetCursorPos, GetForegroundWindow, GetMenuItemCount, GetMenuItemInfoW, GetWindow,
    GetWindowThreadProcessId, HMENU, IsWindowVisible, MENUITEMINFOW, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, MIIM_ID, MIIM_STRING, MIIM_SUBMENU, PostMessageW, SW_SHOWNORMAL,
    SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx, WM_DRAWITEM,
    WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR, WM_MENUSELECT, WM_NCDESTROY, WM_NULL,
};
use windows::core::{Interface, PCSTR, PCWSTR, PSTR, w};

#[cfg(feature = "harness")]
pub mod drive;

/// 外壳命令偏移的起止（`QueryContextMenu` 的 idCmdFirst／idCmdLast）。
const ID_FIRST: u32 = 1;
const ID_LAST: u32 = 0x7EFF;
/// 本应用自己追加的菜单项编号（在外壳范围之外）。
const ID_CUSTOM_REFRESH: u32 = 0x7F00;
const ID_CUSTOM_HINT: u32 = 0x7F01;
/// 与 `SEE_MASK_UNICODE` 同值；windows crate 未导出 `CMIC_` 前缀常量。
const CMIC_MASK_UNICODE: u32 = 0x4000;
const CMIC_MASK_PTINVOKE: u32 = 0x2000_0000;
/// 用户取消（例如关闭了确认框）：`HRESULT_FROM_WIN32(ERROR_CANCELLED)`。
const HR_CANCELLED: i32 = 0x800704C7u32 as i32;
/// 窗口子类编号（任意常量，同一窗口上不重复即可）。
const SUBCLASS_ID: usize = 0x5346_4D39;
/// 菜单构建超过此耗时记入日志（第三方扩展慢的信号）。
pub const SLOW_BUILD: Duration = Duration::from_millis(500);

/// 菜单作用的对象。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// 已选的文件／文件夹（同一目录下，一个或多个）。
    Items(Vec<PathBuf>),
    /// 文件夹空白处：作用于该文件夹本身（新建、粘贴、属性……）。
    Background(PathBuf),
}

#[derive(Default)]
pub struct Options {
    /// 单选文件夹时由调用方接管“打开”（在本应用内进入该文件夹，而不是让外壳另开资源管理器窗口）：
    /// 选中 verb 为 `open` 时不调用外壳，返回 [`Outcome::Intercepted`]。
    pub intercept_open: bool,
    /// 自动化测试：自动导航与取消（见 [`drive`]）。
    #[cfg(feature = "harness")]
    pub drive: Option<drive::Plan>,
    /// 自动化测试：弹出前先删掉“NanaZip”子菜单，模拟未安装。
    #[cfg(feature = "harness")]
    pub hide_nanazip: bool,
}

/// 菜单弹出位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// 当前鼠标位置（鼠标右键）。
    Cursor,
    /// 主窗口客户区内的位置（物理像素；键盘呼出时用光标项附近）。
    Client(i32, i32),
}

pub struct Request {
    pub target: Target,
    pub anchor: Anchor,
    pub options: Options,
}

/// 本应用自己追加的菜单命令。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Custom {
    Refresh,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 菜单被取消（Esc、点击菜单外、命令执行时用户取消）。
    Cancelled,
    /// `InvokeCommand` 成功返回。异步命令（NanaZip 压缩等）的最终结果由各自进程完成。
    Invoked { label: String, verb: String },
    /// `InvokeCommand` 返回失败，`error` 为可读说明。
    InvokeFailed {
        label: String,
        verb: String,
        error: String,
    },
    /// 调用方要求接管该命令（见 [`Options::intercept_open`]），未调用外壳。
    Intercepted { verb: String },
    /// 本应用自己的命令。
    Custom(Custom),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub initmenupopup: u32,
    pub menuselect: u32,
    pub drawitem: u32,
    pub measureitem: u32,
    pub menuchar: u32,
    /// 成功转给 IContextMenu2／3 的消息数
    pub forwarded_ok: u32,
}

#[derive(Debug)]
pub struct Report {
    pub outcome: Outcome,
    pub stats: Stats,
    /// 构建菜单（含 `QueryContextMenu`）耗时
    pub build: Duration,
    /// 菜单从弹出到关闭的耗时（含用户思考时间）
    pub track: Duration,
    /// 菜单里是否检测到 NanaZip 子菜单
    pub nanazip: bool,
}

#[derive(Debug)]
pub enum MenuError {
    /// 菜单正在显示（不允许重入）。
    Busy,
    /// 找不到本线程的可见主窗口作为菜单 owner。
    NoWindow,
    /// 所选路径无法解析（已被删除、移动或无权访问）。
    Missing(PathBuf),
    /// 其他系统错误（附说明）。
    Shell(String),
}

impl std::fmt::Display for MenuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MenuError::Busy => write!(f, "右键菜单正在显示"),
            MenuError::NoWindow => write!(f, "找不到可作为菜单所有者的窗口"),
            MenuError::Missing(p) => write!(
                f,
                "“{}”已不存在或无法访问",
                p.file_name().unwrap_or_default().to_string_lossy()
            ),
            MenuError::Shell(s) => write!(f, "无法生成系统菜单：{s}"),
        }
    }
}

impl From<windows::core::Error> for MenuError {
    fn from(e: windows::core::Error) -> Self {
        MenuError::Shell(e.message())
    }
}

// ---------------------------------------------------------------------------
// 会话状态（线程本地；子类过程通过它找到当前菜单的 IContextMenu2/3）
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Session {
    cm2: Option<IContextMenu2>,
    cm3: Option<IContextMenu3>,
    stats: Stats,
    /// 最近一次 WM_MENUSELECT 报告的 (菜单句柄, 项编号／位置, 标志)；用于得到所选项的文字。
    last_select: (isize, u32, u32),
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

/// 当前是否有菜单正在显示（用于测试与调用方判断）。
pub fn is_active() -> bool {
    BUSY.with(Cell::get)
}

struct BusyGuard;
impl BusyGuard {
    fn enter() -> Result<BusyGuard, MenuError> {
        if BUSY.with(|b| b.replace(true)) {
            return Err(MenuError::Busy);
        }
        Ok(BusyGuard)
    }
}
impl Drop for BusyGuard {
    fn drop(&mut self) {
        SESSION.with(|s| s.borrow_mut().take());
        BUSY.with(|b| b.set(false));
    }
}

struct MenuGuard(HMENU);
impl Drop for MenuGuard {
    fn drop(&mut self) {
        // SAFETY: 句柄由 CreatePopupMenu 创建，只在这里销毁一次（连同外壳扩展加入的子菜单）。
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}

struct SubclassGuard(HWND);
impl SubclassGuard {
    fn install(hwnd: HWND) -> SubclassGuard {
        // SAFETY: hwnd 属于当前线程；过程与引用数据均为静态，卸载在 Drop 中完成。
        unsafe {
            let _ = SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0);
        }
        SubclassGuard(hwnd)
    }
}
impl Drop for SubclassGuard {
    fn drop(&mut self) {
        // SAFETY: 与 install 配对。窗口已销毁时调用失败，忽略。
        unsafe {
            let _ = RemoveWindowSubclass(self.0, Some(subclass_proc), SUBCLASS_ID);
        }
    }
}

struct PidlGuard(*mut ITEMIDLIST);
impl Drop for PidlGuard {
    fn drop(&mut self) {
        // SAFETY: 由 SHParseDisplayName 分配，须用 CoTaskMemFree 释放。
        unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// 当前进程的 GDI／USER 对象数（观察右键菜单的 GDI 增长）。
pub fn gui_resources() -> (u32, u32) {
    // SAFETY: 只读查询当前进程。
    unsafe {
        let p = GetCurrentProcess();
        (
            GetGuiResources(p, GR_GDIOBJECTS),
            GetGuiResources(p, GR_USEROBJECTS),
        )
    }
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------

/// 弹出右键菜单并执行所选命令。阻塞到菜单关闭（以及命令的 `InvokeCommand` 返回）。
pub fn show(req: &Request) -> Result<Report, MenuError> {
    let _busy = BusyGuard::enter()?;
    init_com()?;
    let owner = find_owner().ok_or(MenuError::NoWindow)?;

    let t0 = Instant::now();
    let built = build_context_menu(&req.target, owner)?;
    // SAFETY: 弹出菜单句柄由 MenuGuard 负责销毁。
    let menu = MenuGuard(unsafe { CreatePopupMenu()? });
    let mut flags = CMF_NORMAL;
    // SAFETY: 只读取键盘状态。
    if unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0 {
        flags |= CMF_EXTENDEDVERBS; // Shift+右键：扩展菜单（与资源管理器一致）
    }
    // 不带 CMF_CANRENAME：本应用文件列表暂不支持就地重命名，不显示无效的“重命名”。
    // SAFETY: menu.0 有效；返回值的低 16 位是最大命令偏移 + 1，负值为失败。
    unsafe {
        built
            .cm
            .QueryContextMenu(menu.0, 0, ID_FIRST, ID_LAST, flags)
            .ok()?;
    }
    #[cfg(feature = "harness")]
    if req.options.hide_nanazip {
        drive::remove_nanazip(menu.0);
    }
    let nanazip = top_level_position(menu.0, "nanazip").is_some();
    add_custom_items(menu.0, &req.target, nanazip);
    let build = t0.elapsed();

    #[cfg(feature = "harness")]
    let mut driver = match req.options.drive.as_ref() {
        Some(plan) => Some(drive::Driver::prepare(plan, &built.cm, menu.0)?),
        None => None,
    };

    SESSION.with(|s| {
        *s.borrow_mut() = Some(Session {
            cm2: built.cm2.clone(),
            cm3: built.cm3.clone(),
            ..Session::default()
        })
    });
    let _subclass = SubclassGuard::install(owner);
    #[cfg(feature = "harness")]
    let timer = driver.as_mut().map(|d| d.start());

    let pos = match req.anchor {
        Anchor::Cursor => cursor_pos(),
        Anchor::Client(x, y) => client_to_screen(owner, x, y),
    };
    // SAFETY: owner 是本线程的窗口；menu.0 有效。TPM_RETURNCMD 时返回所选命令编号（0 为取消）。
    let t1 = Instant::now();
    let cmd = unsafe {
        let _ = SetForegroundWindow(owner);
        let r = TrackPopupMenuEx(
            menu.0,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            pos.0,
            pos.1,
            owner,
            None,
        );
        // MSDN：弹出菜单后向 owner 投递一个空消息，避免菜单关闭后消息循环短暂“卡住”。
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        r.0 as u32
    };
    let track = t1.elapsed();
    #[cfg(feature = "harness")]
    if let Some(t) = timer {
        drive::stop(t);
    }

    let (stats, last_select) = SESSION.with(|s| {
        s.borrow()
            .as_ref()
            .map(|s| (s.stats.clone(), s.last_select))
            .unwrap_or_default()
    });
    // 菜单已经结束：先拆子类、清会话，再执行命令，命令执行期间不再转发菜单消息。
    drop(_subclass);
    SESSION.with(|s| s.borrow_mut().take());

    let outcome = if cmd == 0 {
        Outcome::Cancelled
    } else if cmd == ID_CUSTOM_REFRESH {
        Outcome::Custom(Custom::Refresh)
    } else if !(ID_FIRST..=ID_LAST).contains(&cmd) {
        Outcome::Cancelled // 灰色提示项不会被选中；其他未知编号按取消处理
    } else {
        let label = selected_label(menu.0, last_select, cmd);
        let verb = command_verb(&built.cm, cmd - ID_FIRST);
        if req.options.intercept_open && verb.eq_ignore_ascii_case("open") {
            Outcome::Intercepted { verb }
        } else {
            match invoke(&built.cm, owner, cmd - ID_FIRST, &req.target, pos) {
                Ok(()) => Outcome::Invoked { label, verb },
                Err(e) if e.code().0 == HR_CANCELLED => Outcome::Cancelled,
                Err(e) => Outcome::InvokeFailed {
                    label,
                    verb,
                    error: e.message(),
                },
            }
        }
    };
    // menu（DestroyMenu）、built（Release）在这里按声明的逆序释放。
    Ok(Report {
        outcome,
        stats,
        build,
        track,
        nanazip,
    })
}

fn init_com() -> Result<(), MenuError> {
    // SAFETY: 外壳扩展要求 STA。winit 已 OleInitialize 过本线程时返回 S_FALSE，同样可用。
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        return Err(MenuError::Shell(format!(
            "本线程不是单线程 COM 单元（{:#010x}）",
            hr.0
        )));
    }
    Ok(())
}

struct Built {
    cm: IContextMenu,
    cm2: Option<IContextMenu2>,
    cm3: Option<IContextMenu3>,
}

fn parse_pidl(path: &Path) -> Result<PidlGuard, MenuError> {
    let w = wide(&path.to_string_lossy());
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    // SAFETY: w 以 NUL 结尾并在调用期间有效；成功时 pidl 由外壳分配。
    unsafe { SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None) }
        .map_err(|_| MenuError::Missing(path.to_path_buf()))?;
    Ok(PidlGuard(pidl))
}

fn build_context_menu(target: &Target, owner: HWND) -> Result<Built, MenuError> {
    let cm: IContextMenu = match target {
        Target::Items(paths) => {
            if paths.is_empty() {
                return Err(MenuError::Shell("没有选中项目".into()));
            }
            let pidls: Vec<PidlGuard> = paths
                .iter()
                .map(|p| parse_pidl(p))
                .collect::<Result<_, _>>()?;
            let raw: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| p.0 as *const _).collect();
            // SAFETY: raw 中的 PIDL 在本函数结束前一直有效（pidls 持有）。
            let arr = unsafe { SHCreateShellItemArrayFromIDLists(&raw) }?;
            // SAFETY: BHID_SFUIObject 取得所选项的 IContextMenu（可多选）。
            unsafe { arr.BindToHandler(None::<&IBindCtx>, &BHID_SFUIObject) }?
        }
        Target::Background(dir) => {
            let pidl = parse_pidl(dir)?;
            // SAFETY: pidl 有效；绑定到文件夹自身，再取它的背景菜单。
            let folder: IShellFolder =
                unsafe { SHBindToObject(None::<&IShellFolder>, pidl.0, None::<&IBindCtx>) }?;
            unsafe { folder.CreateViewObject(owner) }?
        }
    };
    let cm2 = cm.cast::<IContextMenu2>().ok();
    let cm3 = cm.cast::<IContextMenu3>().ok();
    Ok(Built { cm, cm2, cm3 })
}

/// 优先作为前台窗口的本线程主窗口；否则取本线程第一个可见的顶层窗口。
fn find_owner() -> Option<HWND> {
    // SAFETY: 只读窗口查询；回调只写入传入的 HWND。
    unsafe {
        let me = GetCurrentThreadId();
        let fg = GetForegroundWindow();
        if !fg.is_invalid() && GetWindowThreadProcessId(fg, None) == me && is_top_level(fg) {
            return Some(fg);
        }
        unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> windows::core::BOOL {
            // SAFETY: lp 指向调用者栈上的 HWND。
            unsafe {
                if is_top_level(h) {
                    *(lp.0 as *mut HWND) = h;
                    return false.into();
                }
            }
            true.into()
        }
        let mut found = HWND::default();
        let _ = EnumThreadWindows(me, Some(cb), LPARAM(&mut found as *mut _ as isize));
        (!found.is_invalid()).then_some(found)
    }
}

/// 可见、无 owner、且不是菜单窗口的顶层窗口。
unsafe fn is_top_level(h: HWND) -> bool {
    // SAFETY: 只读窗口查询。
    unsafe {
        if !IsWindowVisible(h).as_bool() || GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid()) {
            return false;
        }
        let mut buf = [0u16; 32];
        let n = GetClassNameW(h, &mut buf);
        // 排除菜单窗口，以及 winit 的线程事件目标窗口（26×26 的辅助窗口，也报告为可见）
        n > 0 && {
            let class = from_wide(&buf[..n as usize]);
            class != "#32768" && !class.starts_with("Winit")
        }
    }
}

// ---------------------------------------------------------------------------
// 菜单读取与追加
// ---------------------------------------------------------------------------

/// 读取菜单项文字（去掉 `&` 与制表符后的快捷键说明）。
fn item_text(hmenu: HMENU, id_or_pos: u32, by_position: bool) -> Option<String> {
    let mut mii = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STRING | MIIM_ID | MIIM_SUBMENU,
        ..Default::default()
    };
    // SAFETY: 两次调用：先取长度，再用足够大的缓冲区取文字。
    unsafe {
        GetMenuItemInfoW(hmenu, id_or_pos, by_position, &mut mii).ok()?;
        if mii.cch == 0 {
            return Some(String::new());
        }
        let mut buf = vec![0u16; mii.cch as usize + 1];
        mii.dwTypeData = windows::core::PWSTR(buf.as_mut_ptr());
        mii.cch += 1;
        GetMenuItemInfoW(hmenu, id_or_pos, by_position, &mut mii).ok()?;
        Some(clean_label(&from_wide(&buf)))
    }
}

/// 去掉助记符 `&` 与 `\t` 之后的快捷键提示。
pub fn clean_label(raw: &str) -> String {
    raw.split('\t').next().unwrap_or("").replace('&', "")
}

/// 顶层项里文字包含 `needle`（忽略大小写）的第一项位置。
fn top_level_position(hmenu: HMENU, needle: &str) -> Option<u32> {
    // SAFETY: 只读枚举。
    let count = unsafe { GetMenuItemCount(Some(hmenu)) };
    (0..count.max(0) as u32)
        .find(|&pos| item_text(hmenu, pos, true).is_some_and(|t| t.to_lowercase().contains(needle)))
}

fn add_custom_items(hmenu: HMENU, target: &Target, nanazip: bool) {
    // SAFETY: 向自己创建的弹出菜单追加项；字符串为静态宽字面量。
    unsafe {
        match target {
            Target::Background(_) => {
                let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
                let _ = AppendMenuW(hmenu, MF_STRING, ID_CUSTOM_REFRESH as usize, w!("刷新"));
            }
            Target::Items(paths) if !nanazip => {
                let archive = paths.iter().any(|p| {
                    p.extension()
                        .is_some_and(|e| is_archive_ext(&e.to_string_lossy()))
                });
                let text = if archive {
                    w!("未检测到 NanaZip（压缩包可用“打开”交给默认应用）")
                } else {
                    w!("未检测到 NanaZip（压缩与解压不可用）")
                };
                let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
                let _ = AppendMenuW(hmenu, MF_STRING | MF_GRAYED, ID_CUSTOM_HINT as usize, text);
            }
            Target::Items(_) => {}
        }
    }
}

fn selected_label(root: HMENU, last: (isize, u32, u32), cmd: u32) -> String {
    // WM_MENUSELECT 的 lParam 是所选项所在菜单；先在那里按命令编号找，找不到再在根菜单找。
    let from_select = HMENU(last.0 as *mut _);
    if last.0 != 0
        && let Some(t) = item_text(from_select, cmd, false)
    {
        return t;
    }
    item_text(root, cmd, false).unwrap_or_default()
}

fn command_verb(cm: &IContextMenu, offset: u32) -> String {
    let mut buf = [0u16; 260];
    // SAFETY: 缓冲区为 260 个 u16；GCS_VERBW 要求以宽字符写入。
    let r = unsafe {
        cm.GetCommandString(
            offset as usize,
            GCS_VERBW,
            None,
            PSTR(buf.as_mut_ptr() as *mut u8),
            buf.len() as u32,
        )
    };
    if r.is_ok() {
        from_wide(&buf)
    } else {
        String::new()
    }
}

fn invoke(
    cm: &IContextMenu,
    owner: HWND,
    offset: u32,
    target: &Target,
    pos: (i32, i32),
) -> windows::core::Result<()> {
    let dir_path = match target {
        Target::Background(d) => Some(d.clone()),
        Target::Items(p) => p.first().and_then(|f| f.parent()).map(Path::to_path_buf),
    };
    let dir = wide(&dir_path.unwrap_or_default().to_string_lossy());
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
        hwnd: owner,
        lpVerb: PCSTR(offset as usize as *const u8),
        lpVerbW: PCWSTR(offset as usize as *const u16),
        lpDirectoryW: PCWSTR(dir.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: POINT { x: pos.0, y: pos.1 },
        ..Default::default()
    };
    // SAFETY: info 按 CMINVOKECOMMANDINFOEX 填写，字符串在调用期间有效；cbSize 指明扩展结构。
    unsafe { cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO) }
}

// ---------------------------------------------------------------------------
// 窗口子类：把菜单消息转给 IContextMenu2/3
// ---------------------------------------------------------------------------

fn forward(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    // 先把接口取出来（AddRef）再调用，不在持有 RefCell 借用时进入第三方代码（它可能再向窗口发消息）。
    let (cm2, cm3) = SESSION.with(|s| {
        s.try_borrow()
            .ok()
            .and_then(|s| s.as_ref().map(|s| (s.cm2.clone(), s.cm3.clone())))
    })?;
    let result = if let Some(cm3) = cm3 {
        let mut lr = LRESULT(0);
        // SAFETY: lr 在调用期间有效；消息参数原样转交。
        unsafe { cm3.HandleMenuMsg2(msg, wp, lp, Some(&mut lr)) }
            .is_ok()
            .then_some(lr)
    } else if let Some(cm2) = cm2 {
        // SAFETY: 同上。
        unsafe { cm2.HandleMenuMsg(msg, wp, lp) }
            .is_ok()
            .then_some(LRESULT(0))
    } else {
        None
    };
    if result.is_some() {
        SESSION.with(|s| {
            if let Ok(mut s) = s.try_borrow_mut()
                && let Some(s) = s.as_mut()
            {
                s.stats.forwarded_ok += 1;
            }
        });
    }
    result
}

fn note(msg: u32, wp: WPARAM, lp: LPARAM) {
    SESSION.with(|s| {
        let Ok(mut s) = s.try_borrow_mut() else {
            return;
        };
        let Some(s) = s.as_mut() else { return };
        match msg {
            WM_INITMENUPOPUP => s.stats.initmenupopup += 1,
            WM_DRAWITEM => s.stats.drawitem += 1,
            WM_MEASUREITEM => s.stats.measureitem += 1,
            WM_MENUCHAR => s.stats.menuchar += 1,
            WM_MENUSELECT => {
                s.stats.menuselect += 1;
                let item = (wp.0 & 0xFFFF) as u32;
                let flags = ((wp.0 >> 16) & 0xFFFF) as u32;
                s.last_select = (lp.0, item, flags);
                #[cfg(feature = "harness")]
                drive::on_menu_select(lp.0, item, flags);
            }
            _ => {}
        }
    });
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR | WM_MENUSELECT => {
            // 窗口过程不能 panic（会跨 FFI 终止进程）：任何异常都退回默认处理。
            let handled = std::panic::catch_unwind(|| {
                note(msg, wp, lp);
                forward(msg, wp, lp)
            })
            .ok()
            .flatten();
            if let Some(lr) = handled {
                match msg {
                    WM_MENUCHAR => return lr,
                    WM_DRAWITEM | WM_MEASUREITEM => return LRESULT(1),
                    WM_INITMENUPOPUP => return LRESULT(0),
                    _ => {}
                }
            }
            // SAFETY: 按子类约定交给下一个处理者。
            unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
        }
        WM_NCDESTROY => {
            // SAFETY: 窗口销毁时一并拆除子类，避免悬挂。
            unsafe {
                let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
                DefSubclassProc(hwnd, msg, wp, lp)
            }
        }
        // SAFETY: 同上。
        _ => unsafe { DefSubclassProc(hwnd, msg, wp, lp) },
    }
}

fn client_to_screen(owner: HWND, x: i32, y: i32) -> (i32, i32) {
    let mut pt = POINT { x, y };
    // SAFETY: pt 为有效缓冲区。
    unsafe {
        let _ = ClientToScreen(owner, &mut pt);
    }
    (pt.x, pt.y)
}

/// 当前鼠标位置（屏幕坐标）。
pub fn cursor_pos() -> (i32, i32) {
    let mut pt = POINT::default();
    // SAFETY: pt 为有效输出缓冲区。
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    (pt.x, pt.y)
}

/// 自动化测试：本线程的主窗口（菜单的 owner）。
#[cfg(feature = "harness")]
pub fn owner_window() -> Option<HWND> {
    find_owner()
}

/// 自动化测试：菜单结束后是否留有残余状态（空表示干净）。
#[cfg(feature = "harness")]
pub fn leftovers(owner: HWND) -> Vec<&'static str> {
    let mut v = Vec::new();
    if is_active() {
        v.push("菜单仍标记为活动");
    }
    if SESSION.with(|s| s.borrow().is_some()) {
        v.push("会话状态未清空");
    }
    // RemoveWindowSubclass 在子类仍装着时才返回成功（并顺手拆掉）：借它判断是否有残留。
    // SAFETY: 与 SubclassGuard 相同的调用；没有残留时返回失败，无副作用。
    if unsafe { RemoveWindowSubclass(owner, Some(subclass_proc), SUBCLASS_ID) }.as_bool() {
        v.push("窗口子类未拆除");
    }
    if !drive::menu_windows().is_empty() {
        v.push("仍有菜单窗口");
    }
    v
}

/// 压缩包扩展名（不区分大小写，不含点）：只用于选择提示文案，能否解压仍以 NanaZip 菜单为准。
pub fn is_archive_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "cab" | "iso" | "wim"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{DeleteMenu, MF_BYPOSITION};

    #[test]
    fn clean_label_strips_mnemonic_and_accelerator() {
        assert_eq!(clean_label("打开(&O)\tEnter"), "打开(O)");
        assert_eq!(clean_label("&NanaZip"), "NanaZip");
        assert_eq!(clean_label(""), "");
    }

    #[test]
    fn archive_extensions() {
        assert!(is_archive_ext("ZIP"));
        assert!(is_archive_ext("7z"));
        assert!(!is_archive_ext("txt"));
        assert!(!is_archive_ext(""));
    }

    /// 用合成菜单验证 NanaZip 检测与“未安装”提示项（不依赖外壳与桌面窗口）。
    #[test]
    fn nanazip_detection_and_hint_item() {
        // SAFETY: 只创建并销毁本测试自己的弹出菜单。
        unsafe {
            let with = MenuGuard(CreatePopupMenu().unwrap());
            let sub = CreatePopupMenu().unwrap();
            let _ = AppendMenuW(with.0, MF_STRING, 1, w!("打开(&O)"));
            let _ = AppendMenuW(
                with.0,
                windows::Win32::UI::WindowsAndMessaging::MF_POPUP,
                sub.0 as usize,
                w!("&NanaZip"),
            );
            assert_eq!(top_level_position(with.0, "nanazip"), Some(1));

            let without = MenuGuard(CreatePopupMenu().unwrap());
            let _ = AppendMenuW(without.0, MF_STRING, 1, w!("打开(&O)"));
            assert_eq!(top_level_position(without.0, "nanazip"), None);
            add_custom_items(
                without.0,
                &Target::Items(vec![PathBuf::from("C:\\a.txt")]),
                false,
            );
            let n = GetMenuItemCount(Some(without.0));
            assert_eq!(n, 3, "打开 + 分隔线 + 灰色提示");
            let hint = item_text(without.0, ID_CUSTOM_HINT, false).unwrap();
            assert!(hint.contains("NanaZip"));
            // 已安装时不追加任何东西；背景菜单只追加“刷新”
            add_custom_items(with.0, &Target::Items(vec![]), true);
            assert_eq!(GetMenuItemCount(Some(with.0)), 2);
            add_custom_items(with.0, &Target::Background(PathBuf::from("C:\\")), true);
            assert_eq!(GetMenuItemCount(Some(with.0)), 4);
            let _ = DeleteMenu(with.0, 0, MF_BYPOSITION);
        }
    }

    #[test]
    fn busy_guard_blocks_reentry_and_clears() {
        let g = BusyGuard::enter().unwrap();
        assert!(is_active());
        assert!(matches!(BusyGuard::enter(), Err(MenuError::Busy)));
        drop(g);
        assert!(!is_active());
        drop(BusyGuard::enter().unwrap());
    }
}
