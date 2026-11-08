//! 系统入口的界面接线（Issue #17）：外部打开请求与“用本应用打开文件夹”设置。
//!
//! - **外部请求**（[`Controller::handle_request`]）：来自启动参数或第二个进程的转发（见 [`crate::sysentry::instance`]）。
//!   打开文件夹时按 docs/panes-tabs-session.md 的外部打开规则复用或新建标签，然后把窗口还原并切到前台；
//!   目标是文件时打开其所在文件夹并选中它；路径不可用时弹出说明（已有对话框时改在状态栏提示），不新建标签。
//! - **设置面板**：开关与状态行读取 [`manage::inspect`]；启用前列出将被替换的现有设置并确认；
//!   停用直接恢复原设置，恢复不完整时如实说明；关联失效／冲突时提供修复与恢复。
//! - **启动自检**（[`Controller::entry_self_check`]）：关联指向别的程序路径时弹出“修复／恢复原设置／以后再说”，
//!   部分设置被别的程序改动时在状态栏说明。
//!
//! 注册表根与备份位置由 [`EntryEnv`] 注入：产品用真实的 `HKCU\Software\Classes`（只有用户在设置里启用时才写入），
//! harness 用内存实现或测试根。

use super::ops::Dialog;
use super::*;
use crate::sysentry::manage::{self, State};
use crate::sysentry::request::{Reply, Request};
use crate::sysentry::takeover::RegStore;
use crate::{EntryState, EntryView};
use std::path::Path;
use std::sync::mpsc;

/// 等待界面线程答复转发请求的上限（超过则先答复“已转交”，请求仍会被处理）。
const REPLY_WAIT: Duration = Duration::from_millis(3000);

/// 接管设置所需的外部环境。
pub struct EntryEnv {
    /// 注册表根；`Err` 为接管功能不可用的原因。
    pub store: Result<Box<dyn RegStore>, String>,
    /// 是否真实的 `HKCU\Software\Classes`（只有真实根才通知外壳刷新关联）。
    pub real: bool,
    /// 写进命令行的程序路径（当前 exe）。
    pub exe: PathBuf,
    /// 备份文件；`None`（没有数据目录）时接管功能不可用。
    pub backup: Option<PathBuf>,
}

impl EntryEnv {
    /// 产品：按环境变量选择注册表根（见 [`manage::Root::from_env`]），备份放在数据目录。
    pub fn production(data_dir: Option<&Path>) -> EntryEnv {
        let exe = std::env::current_exe().unwrap_or_default();
        let backup = data_dir.map(manage::backup_path);
        match manage::Root::from_env() {
            Ok(r) => EntryEnv {
                store: Ok(Box::new(r.store)),
                real: r.real,
                exe,
                backup,
            },
            Err(e) => EntryEnv {
                store: Err(e),
                real: false,
                exe,
                backup,
            },
        }
    }

    /// 接管功能不可用（harness 默认、没有数据目录）。
    pub fn unavailable(reason: &str) -> EntryEnv {
        EntryEnv {
            store: Err(reason.to_string()),
            real: false,
            exe: PathBuf::new(),
            backup: None,
        }
    }

    /// 测试与演示：注入注册表根、程序路径与备份文件。
    pub fn with_store(store: Box<dyn RegStore>, exe: PathBuf, backup: PathBuf) -> EntryEnv {
        EntryEnv {
            store: Ok(store),
            real: false,
            exe,
            backup: Some(backup),
        }
    }
}

/// 界面上的状态（含“不可用”）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryStatus {
    Unavailable(String),
    Known(State),
}

fn io_reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "位置不存在（可能已被删除、移动，或磁盘未连接）".into(),
        std::io::ErrorKind::PermissionDenied => "没有访问权限".into(),
        _ => e.to_string(),
    }
}

/// 给界面线程转发请求并等待答复的处理函数（在管道连接线程上调用）。
pub fn external_handler(c: &Controller) -> impl Fn(Request) -> Reply + Send + Sync + 'static {
    let id = c.id;
    move |req| {
        let (tx, rx) = mpsc::channel();
        post(id, Msg::External(req, tx));
        rx.recv_timeout(REPLY_WAIT)
            .unwrap_or_else(|_| Reply::ok("已转交，界面稍后处理"))
    }
}

impl Controller {
    // ---------- 外部打开请求 ----------

    /// 处理外部请求：激活窗口，或打开文件夹（复用已打开该路径的标签，否则在当前面板新开标签）。
    pub fn handle_request(&self, req: Request) -> Reply {
        self.raise_window();
        match req {
            Request::Activate => Reply::ok("已激活窗口"),
            Request::Open { folder, select } => self.open_external(folder, select),
        }
    }

    fn open_external(&self, folder: PathBuf, select: Option<PathBuf>) -> Reply {
        let shown = location::display(&folder);
        if location::is_recycle_bin(&folder) {
            self.leave_overlays();
            self.open_directory(folder);
            return Reply::ok("已打开回收站");
        }
        let (dir, item) = match std::fs::metadata(&folder) {
            Ok(m) if m.is_dir() => (folder, select),
            Ok(_) => match folder.parent() {
                // 目标是文件：打开所在文件夹并选中它
                Some(p) if !p.as_os_str().is_empty() => (p.to_path_buf(), Some(folder.clone())),
                _ => (folder, None),
            },
            Err(e) => {
                let msg = format!("无法打开“{shown}”：{}", io_reason(&e));
                self.report_unavailable(&msg);
                return Reply::fail(msg);
            }
        };
        self.leave_overlays();
        self.open_directory(dir.clone());
        if let Some(name) = item.as_deref().and_then(Path::file_name) {
            let tab = {
                let ws = self.ws.borrow();
                ws.active_tab(ws.active_pane()).id()
            };
            self.locate_in(tab, name.to_os_string());
        }
        self.focus_active();
        Reply::ok(format!("已打开“{}”", location::display(&dir)))
    }

    /// 打开外部请求前关闭搜索、预览与设置面板（它们盖在面板上，用户看不到新标签）。
    fn leave_overlays(&self) {
        self.search_dismiss();
        self.preview_close();
        self.hide_settings();
    }

    fn report_unavailable(&self, msg: &str) {
        if self.dialog_open() {
            self.set_message(msg);
        } else {
            self.show_notice("无法打开文件夹", msg);
        }
    }

    /// 还原并把主窗口切到前台。
    fn raise_window(&self) {
        if let Some(w) = self.window.upgrade() {
            w.window().set_minimized(false);
        }
        let h = self.hwnd.get();
        if h != 0 {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
            };
            // SAFETY: 句柄来自本进程的主窗口；窗口已销毁时这些调用只是失败。
            unsafe {
                let hwnd = h as windows_sys::Win32::Foundation::HWND;
                if IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                SetForegroundWindow(hwnd);
            }
        }
    }

    /// 在状态栏常驻一条说明（与会话降级说明并列）。
    pub fn add_notice(&self, m: &str) {
        {
            let mut n = self.notice.borrow_mut();
            if !n.is_empty() {
                n.push('\n');
            }
            n.push_str(m);
        }
        self.sync_global();
    }

    // ---------- 接管状态 ----------

    /// 读取当前接管状态。
    pub fn entry_status(&self) -> EntryStatus {
        let env = self.entry.borrow();
        let store = match &env.store {
            Ok(s) => s,
            Err(e) => return EntryStatus::Unavailable(e.clone()),
        };
        if env.backup.is_none() {
            return EntryStatus::Unavailable(
                "没有数据目录（--no-session 或无法确定 %LOCALAPPDATA%），无法保存备份".into(),
            );
        }
        match manage::inspect(&**store, &env.exe) {
            Ok(s) => EntryStatus::Known(s),
            Err(e) => EntryStatus::Unavailable(format!("读取注册表失败：{e}")),
        }
    }

    fn entry_view(&self) -> EntryView {
        let (state, title, detail) = match self.entry_status() {
            EntryStatus::Unavailable(why) => (EntryState::Unavailable, "不可用".to_string(), why),
            EntryStatus::Known(State::NotTaken) => (
                EntryState::NotTaken,
                "未接管".into(),
                "文件夹、驱动器和 Win+E 仍由资源管理器打开。".into(),
            ),
            EntryStatus::Known(State::Taken) => (
                EntryState::Taken,
                "已接管".into(),
                format!(
                    "双击文件夹、驱动器和 Win+E 会打开：{}",
                    self.entry.borrow().exe.display()
                ),
            ),
            EntryStatus::Known(State::Conflict(items)) => (
                EntryState::Conflict,
                "冲突".into(),
                format!(
                    "部分设置已被别的程序改动：{}。可以重新接管，或恢复原设置（不会覆盖别的程序的改动）。",
                    items.join("、")
                ),
            ),
            EntryStatus::Known(State::Stale { old_exe, missing }) => (
                EntryState::Stale,
                "关联失效".into(),
                if missing {
                    format!(
                        "登记的程序 {} 已不存在，双击文件夹可能打不开。点“修复”改为当前程序，或恢复原设置。",
                        old_exe.display()
                    )
                } else {
                    format!(
                        "登记的是另一个位置的程序 {}，当前运行的是 {}。点“修复”改为当前程序，或恢复原设置。",
                        old_exe.display(),
                        self.entry.borrow().exe.display()
                    )
                },
            ),
        };
        EntryView {
            state,
            title: title.into(),
            detail: detail.into(),
        }
    }

    fn sync_entry(&self) {
        if let Some(w) = self.window.upgrade() {
            w.set_entry_view(self.entry_view());
        }
    }

    // ---------- 设置面板 ----------

    pub fn show_settings(&self) {
        self.search_dismiss();
        self.sync_entry();
        if let Some(w) = self.window.upgrade() {
            w.set_settings_visible(true);
            w.set_settings_token(w.get_settings_token() + 1);
        }
    }

    pub fn hide_settings(&self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        if w.get_settings_visible() {
            w.set_settings_visible(false);
            let p = self.ws.borrow().active_pane();
            self.focus_pane(p);
        }
    }

    pub fn settings_open(&self) -> bool {
        self.window
            .upgrade()
            .is_some_and(|w| w.get_settings_visible())
    }

    /// 对话框关闭后把焦点还给设置面板（面板打开时）。返回是否处理了焦点。
    pub(super) fn refocus_settings(&self) -> bool {
        let Some(w) = self.window.upgrade() else {
            return false;
        };
        if !w.get_settings_visible() {
            return false;
        }
        w.set_settings_token(w.get_settings_token() + 1);
        true
    }

    /// 开关：`on` 为请求的新状态。
    pub fn entry_toggle(&self, on: bool) {
        if on {
            self.entry_ask_enable();
        } else {
            self.entry_restore();
        }
    }

    /// 启用前确认：说明影响，并列出将被替换的现有设置。
    fn entry_ask_enable(&self) {
        let pre = {
            let env = self.entry.borrow();
            match &env.store {
                Ok(s) => manage::preflight(&**s, &env.exe).unwrap_or_default(),
                Err(_) => return,
            }
        };
        let mut body = String::from(
            "启用后，双击文件夹、驱动器以及按 Win+E 都会打开本应用；在资源管理器窗口里双击文件夹也会改在本应用中打开。\n\n只写当前用户的设置，写入前先备份到数据目录；随时可以在设置里恢复原设置。",
        );
        if !pre.is_empty() {
            body.push_str("\n\n将替换以下现有设置（已备份，恢复时还原）：");
            for (k, v) in &pre {
                body.push_str(&format!("\n• {k} = {v}"));
            }
        }
        self.open_dialog(
            Dialog::EntryEnable,
            "用本应用打开文件夹？",
            &body,
            "启用",
            "取消",
            false,
        );
    }

    /// 启用、修复或重新接管。
    pub fn entry_take(&self) {
        let r = {
            let mut env = self.entry.borrow_mut();
            let env = &mut *env;
            let (Ok(store), Some(backup)) = (&mut env.store, &env.backup) else {
                return;
            };
            manage::take(&mut **store, &env.exe, backup)
        };
        self.after_entry_change();
        match r {
            Ok(rep) => {
                let mut m = "已启用：双击文件夹、驱动器和 Win+E 将打开本应用".to_string();
                if let Some(p) = rep.set_aside {
                    m += &format!("（原备份文件无法解析，已移到 {}）", p.display());
                }
                self.set_info(&m);
            }
            Err(e) => self.show_notice("启用失败", &e),
        }
    }

    /// 停用：恢复原设置。
    pub fn entry_restore(&self) {
        let r = {
            let mut env = self.entry.borrow_mut();
            let env = &mut *env;
            let (Ok(store), Some(backup)) = (&mut env.store, &env.backup) else {
                return;
            };
            manage::disable(&mut **store, backup)
        };
        self.after_entry_change();
        match r {
            Ok(rep) if rep.is_clean() => self.set_info("已恢复原设置：文件夹将由资源管理器打开"),
            Ok(rep) => {
                let mut body = String::new();
                if rep.without_backup {
                    body.push_str("没有找到可用的备份，只删除了本应用写入的设置；原来的默认打开方式无从得知，已回到系统默认。");
                }
                let d = manage::describe(&rep.restore);
                if !d.is_empty() {
                    if !body.is_empty() {
                        body.push_str("\n\n");
                    }
                    body.push_str(&format!("以下项目没有恢复：{d}。"));
                }
                if let Some(p) = rep.set_aside {
                    body.push_str(&format!("\n\n无法解析的备份已移到 {}。", p.display()));
                }
                body.push_str("\n\n手工检查方法见 system-entry.md。");
                self.show_notice("恢复不完整", &body);
            }
            Err(e) => self.show_notice("恢复失败", &e),
        }
    }

    fn after_entry_change(&self) {
        if self.entry.borrow().real {
            manage::notify_shell();
        }
        self.sync_entry();
    }

    /// 启动自检：关联失效时询问修复或恢复；部分设置被改动时在状态栏说明。
    pub fn entry_self_check(&self) {
        match self.entry_status() {
            EntryStatus::Known(State::Stale { old_exe, missing }) => {
                let why = if missing {
                    format!("登记的程序 {} 已不存在", old_exe.display())
                } else {
                    format!(
                        "登记的是另一个位置的程序 {}（当前运行的是 {}）",
                        old_exe.display(),
                        self.entry.borrow().exe.display()
                    )
                };
                self.open_dialog(
                    Dialog::EntryStale,
                    "文件夹打开方式指向了旧位置",
                    &format!(
                        "{why}，双击文件夹时可能打不开或打开旧版本。\n\n修复：改为当前程序。恢复原设置：撤销接管，回到资源管理器。"
                    ),
                    "修复",
                    "以后再说",
                    false,
                );
                if let Some(w) = self.window.upgrade() {
                    w.set_dialog_secondary_label("恢复原设置".into());
                }
            }
            EntryStatus::Known(State::Conflict(_)) => self.add_notice(
                "文件夹打开方式被别的程序改动了一部分；可在“更多 → 设置…”里重新接管或恢复原设置",
            ),
            _ => {}
        }
    }
}
