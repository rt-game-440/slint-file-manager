//! “在文件夹中显示”（`SHOpenFolderAndSelectItems`）的原型与测试调用器（Issue #18）。
//!
//! 子命令：
//!
//! - `host --folder=<目录> [--folder=<目录2> ...] [--seconds=N] [--minimize] [--navigate-to=<目录> --navigate-after-ms=N]
//!   [--trigger=<文件> --late-folder=<目录>] [--entry <目录>] [--log=<文件>]`：建一个普通顶层窗口，
//!   用 [`slint_file_manager::sysentry::shellwin`]（产品同一份代码）按文件夹登记进 `IShellWindows`
//!   （每个文件夹一个槽），记录外壳的每次调用；到时注销并退出。输出 `host: ready`、`select: 槽 … 项目 …`、
//!   `foreground: yes|no` 等行。
//! - `call --file=<路径> [--file=<路径2>] [--style=chromium|single] [--wait-ms=N] [--keep] [--delete-before-call]`：像 Chromium／Electron
//!   一样调用 `SHOpenFolderAndSelectItems`，报告返回值、耗时、调用后新出现的资源管理器窗口；默认关闭这些新窗口
//!   （只关调用后新出现的、且标题是目标文件夹名的窗口）。
//! - `exec --folder=<目录> [--verb=open|default]`：用 `ShellExecuteExW` 打开文件夹（检查已登记窗口是否被复用）。
//! - `list`：列出 `IShellWindows` 里所有窗口（类型、句柄、位置）。
//! - `test-root take|disable|delete`：在 `SFM_TEST_REG_ROOT` 测试根上启停接管（让真实程序进入“已接管”状态）。
//!
//! 只用调用方给的（临时）路径；注册表只碰 `SFM_TEST_REG_ROOT` 测试根；登记只在 host 进程存活期间有效。真实程序验证见 `scripts/verify-reveal.ps1`。

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint_file_manager::sysentry::shellwin::{Pidl, Selection, ShellWindows};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IDispatch,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::{IShellWindows, IWebBrowserApp, SHOpenFolderAndSelectItems};
use windows::core::Interface;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging as wm;

/// `--log=<文件>`：输出同时追加到文件（被外壳启动、没有标准输出时用）。
static LOG: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn out(s: &str) {
    use std::io::Write;
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{s}");
    let _ = o.flush();
    if let Some(p) = LOG.get()
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
    {
        let _ = writeln!(f, "{s}");
    }
}

fn values<'a>(args: &'a [String], key: &str) -> Vec<&'a str> {
    args.iter().filter_map(|a| a.strip_prefix(key)).collect()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn com_init() {
    // SAFETY: 本线程首次初始化 COM（STA）。
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(p) = values(&args, "--log=").first() {
        let _ = LOG.set(PathBuf::from(p));
    }
    let code = match args.get(1).map(String::as_str) {
        Some("host") => host(&args[2..]),
        Some("call") => call(&args[2..]),
        Some("exec") => exec(&args[2..]),
        Some("test-root") => test_root(&args[2..]),
        Some("list") => {
            com_init();
            for l in list_shell_windows() {
                out(&format!("list: {l}"));
            }
            0
        }
        _ => {
            eprintln!("用法：reveal-probe host|call|exec|list …（见源码注释）");
            2
        }
    };
    std::process::exit(code);
}

// ---------- 测试根上的接管状态 ----------

/// `test-root take|disable|delete --exe=<程序> --backup=<文件>`：在 `SFM_TEST_REG_ROOT` 指定的测试根上
/// 用产品同一份逻辑（`manage::take`／`disable`）启用或停用接管，让真实程序进入“已接管”状态；`delete` 整棵删除测试根。
/// 环境变量未设置（即真实的 `HKCU\Software\Classes`）时拒绝。
fn test_root(args: &[String]) -> i32 {
    use slint_file_manager::sysentry::{manage, winreg};
    let Ok(v) = std::env::var(manage::REG_ROOT_ENV) else {
        out("test-root: 必须设置 SFM_TEST_REG_ROOT（拒绝操作真实注册表）");
        return 2;
    };
    let mut store = match manage::test_root(&v) {
        Ok(s) => s,
        Err(e) => {
            out(&format!("test-root: {e}"));
            return 2;
        }
    };
    let exe = values(args, "--exe=").first().map(PathBuf::from);
    let backup = values(args, "--backup=").first().map(PathBuf::from);
    match (args.first().map(String::as_str), exe, backup) {
        (Some("take"), Some(exe), Some(b)) => match manage::take(&mut store, &exe, &b) {
            Ok(_) => {
                let st = manage::inspect(&store, &exe);
                out(&format!("test-root: 已在测试根启用接管，状态 {st:?}"));
                0
            }
            Err(e) => {
                out(&format!("test-root: 启用失败：{e}"));
                1
            }
        },
        (Some("disable"), _, Some(b)) => match manage::disable(&mut store, &b) {
            Ok(r) => {
                out(&format!("test-root: 已停用，干净={}", r.is_clean()));
                0
            }
            Err(e) => {
                out(&format!("test-root: 停用失败：{e}"));
                1
            }
        },
        (Some("delete"), _, _) => match winreg::delete_test_tree(store.base()) {
            Ok(()) => {
                out("test-root: 已删除测试根");
                0
            }
            Err(e) => {
                out(&format!("test-root: 删除失败：{e}"));
                1
            }
        },
        _ => {
            out("用法：reveal-probe test-root take|disable|delete --exe=<程序> --backup=<文件>");
            2
        }
    }
}

// ---------- host ----------

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == wm::WM_DESTROY {
        // SAFETY: 本线程的消息循环。
        unsafe { wm::PostQuitMessage(0) };
        return 0;
    }
    // SAFETY: 默认处理。
    unsafe { wm::DefWindowProcW(h, m, w, l) }
}

fn create_window(title: &str) -> HWND {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    let class = wide("SfmRevealProbeHost");
    let title = wide(title);
    // SAFETY: 注册窗口类并创建普通顶层窗口；字符串在调用期间有效。
    unsafe {
        let inst = GetModuleHandleW(std::ptr::null());
        let wc = wm::WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst,
            lpszClassName: class.as_ptr(),
            hCursor: wm::LoadCursorW(std::ptr::null_mut(), wm::IDC_ARROW),
            hbrBackground: (5 + 1) as _, // COLOR_WINDOW + 1
            ..std::mem::zeroed()
        };
        wm::RegisterClassW(&wc);
        let h = wm::CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            wm::WS_OVERLAPPEDWINDOW,
            200,
            200,
            520,
            240,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            inst,
            std::ptr::null(),
        );
        wm::ShowWindow(h, wm::SW_SHOWNORMAL);
        h
    }
}

fn host(args: &[String]) -> i32 {
    com_init();
    let mut folders: Vec<PathBuf> = values(args, "--folder=")
        .into_iter()
        .map(PathBuf::from)
        .collect();
    // 模拟接管动词 `"<exe>" … --entry "%1"`：被外壳以默认动词启动时，登记 %1
    if let Some(i) = args.iter().position(|a| a == "--entry")
        && let Some(f) = args.get(i + 1)
    {
        out(&format!("host: 由 --entry 启动：{f}"));
        folders.push(PathBuf::from(f));
    }
    let secs: u64 = values(args, "--seconds=")
        .first()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let hwnd = create_window("reveal-probe 宿主（Issue #18 原型）");
    if args.iter().any(|a| a == "--minimize") {
        // SAFETY: 本进程窗口。
        unsafe { wm::ShowWindow(hwnd, wm::SW_MINIMIZE) };
    }
    let started = Instant::now();
    let pending_fg: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
    let pf = pending_fg.clone();
    let sink = Rc::new(move |s: Selection| {
        out(&format!(
            "select: 槽 {} 标志 0x{:x} 文件夹 {} 项目 {}",
            s.slot,
            s.flags,
            s.folder.display(),
            s.item
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "（无）".into())
        ));
        pf.set(Some(Instant::now()));
    });
    let t0 = started;
    let trace = Rc::new(move |m: &str| {
        out(&format!("trace: +{}ms {m}", t0.elapsed().as_millis()));
    });
    let mut sw = match ShellWindows::new(hwnd as isize, sink, trace) {
        Ok(s) => s,
        Err(e) => {
            out(&format!("host: 无法连接 ShellWindows：{e}"));
            return 1;
        }
    };
    let want: Vec<(u64, PathBuf)> = folders
        .iter()
        .enumerate()
        .map(|(i, f)| (i as u64 + 1, f.clone()))
        .collect();
    for (slot, why) in sw.sync(&want) {
        out(&format!("host: 槽 {slot} 登记失败：{why}"));
    }
    out(&format!("host: ready hwnd=0x{:x}", hwnd as usize));
    // 消息循环（COM 的跨进程调用经它回到本线程），带截止时间
    let deadline = started + Duration::from_secs(secs);
    // --trigger=<文件> --late-folder=<目录>：触发文件出现时才登记 late-folder（模拟“外壳找不到窗口后新实例才登记”）
    let trigger = values(args, "--trigger=").first().map(PathBuf::from);
    let late = values(args, "--late-folder=").first().map(PathBuf::from);
    let mut late_done = false;
    // --navigate-to=<目录> --navigate-after-ms=N：槽 1 在 N 毫秒后导航到另一个文件夹（OnNavigate）
    let nav_to = values(args, "--navigate-to=").first().map(PathBuf::from);
    let nav_after = Duration::from_millis(
        values(args, "--navigate-after-ms=")
            .first()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3000),
    );
    let mut nav_done = false;
    let mut was_fg = false;
    // SAFETY: 标准消息循环；MSG 为局部变量。
    unsafe {
        wm::SetTimer(hwnd, 1, 20, None);
        let mut msg: wm::MSG = std::mem::zeroed();
        while wm::GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if msg.message == wm::WM_TIMER {
                let fg = wm::GetForegroundWindow() == hwnd;
                if fg != was_fg {
                    was_fg = fg;
                    out(&format!(
                        "trace: +{}ms 前台变化：{}",
                        started.elapsed().as_millis(),
                        if fg {
                            "本窗口到前台"
                        } else {
                            "本窗口离开前台"
                        }
                    ));
                }
                if !nav_done
                    && let Some(n) = &nav_to
                    && started.elapsed() > nav_after
                {
                    nav_done = true;
                    match sw.set(1, n) {
                        Ok(()) => out(&format!("host: 槽 1 已导航到 {}", n.display())),
                        Err(e) => out(&format!("host: 槽 1 导航失败：{e}")),
                    }
                }
                if !late_done
                    && let (Some(t), Some(l)) = (&trigger, &late)
                    && t.exists()
                {
                    late_done = true;
                    let mut w = want.clone();
                    w.push((99, l.clone()));
                    for (slot, why) in sw.sync(&w) {
                        out(&format!("host: 槽 {slot} 登记失败：{why}"));
                    }
                    out(&format!(
                        "trace: +{}ms 触发后登记 {}",
                        started.elapsed().as_millis(),
                        l.display()
                    ));
                }
                if let Some(t) = pending_fg.get()
                    && t.elapsed() > Duration::from_millis(400)
                {
                    pending_fg.set(None);
                    let fg = wm::GetForegroundWindow() == hwnd;
                    let iconic = wm::IsIconic(hwnd) != 0;
                    out(&format!(
                        "foreground: {} iconic: {}",
                        if fg { "yes" } else { "no" },
                        if iconic { "yes" } else { "no" }
                    ));
                }
                if Instant::now() > deadline {
                    break;
                }
            }
            wm::TranslateMessage(&msg);
            wm::DispatchMessageW(&msg);
        }
    }
    sw.revoke_all();
    drop(sw);
    out("host: revoked");
    // SAFETY: 本进程窗口。
    unsafe { wm::DestroyWindow(hwnd) };
    0
}

// ---------- 资源管理器窗口快照 ----------

#[derive(Clone, Debug)]
struct TopWin {
    hwnd: usize,
    title: String,
}

fn explorer_windows() -> Vec<TopWin> {
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> i32 {
        // SAFETY: l 是 Vec 的指针；h 来自 EnumWindows。
        unsafe {
            let v = &mut *(l as *mut Vec<TopWin>);
            let mut cls = [0u16; 64];
            let n = wm::GetClassNameW(h, cls.as_mut_ptr(), 64);
            let c = String::from_utf16_lossy(&cls[..n.max(0) as usize]);
            if c == "CabinetWClass" || c == "ExploreWClass" {
                let mut t = [0u16; 512];
                let n = wm::GetWindowTextW(h, t.as_mut_ptr(), 512);
                v.push(TopWin {
                    hwnd: h as usize,
                    title: String::from_utf16_lossy(&t[..n.max(0) as usize]),
                });
            }
        }
        1
    }
    let mut v: Vec<TopWin> = Vec::new();
    // SAFETY: 回调只在调用期间使用 v。
    unsafe { wm::EnumWindows(Some(cb), &mut v as *mut _ as LPARAM) };
    v
}

fn new_windows(before: &[TopWin]) -> Vec<TopWin> {
    explorer_windows()
        .into_iter()
        .filter(|w| !before.iter().any(|b| b.hwnd == w.hwnd))
        .collect()
}

/// 关闭调用后新出现、且标题等于目标文件夹名的资源管理器窗口（不碰用户原有窗口）。
fn close_new(new: &[TopWin], folder_name: &str) {
    for w in new {
        if w.title == folder_name {
            // SAFETY: 向窗口投递关闭消息。
            unsafe { wm::PostMessageW(w.hwnd as HWND, wm::WM_CLOSE, 0, 0) };
            out(&format!("cleanup: 已关闭新开的资源管理器窗口“{}”", w.title));
        } else {
            out(&format!(
                "cleanup: 新出现的窗口“{}”标题不是目标文件夹，保留不动",
                w.title
            ));
        }
    }
}

fn wait_new(before: &[TopWin], ms: u64) -> Vec<TopWin> {
    let t = Instant::now();
    loop {
        let n = new_windows(before);
        if !n.is_empty() || t.elapsed() > Duration::from_millis(ms) {
            // 给窗口一点时间设置标题
            std::thread::sleep(Duration::from_millis(500));
            return new_windows(before);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn folder_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ---------- call ----------

fn call(args: &[String]) -> i32 {
    com_init();
    let files: Vec<PathBuf> = values(args, "--file=")
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let Some(first) = files.first() else {
        eprintln!("call 需要 --file=");
        return 2;
    };
    let style = values(args, "--style=")
        .first()
        .copied()
        .unwrap_or("chromium");
    let wait_ms: u64 = values(args, "--wait-ms=")
        .first()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let dir = first.parent().unwrap_or(first).to_path_buf();
    // 与 Chromium 一样先把路径解析成 PIDL（文件不存在时 Chromium 不会调用 SHOpenFolderAndSelectItems）
    let pidls_first: Vec<Pidl> = files.iter().filter_map(|f| Pidl::parse(f).ok()).collect();
    let dir_pidl = Pidl::parse(&dir).ok();
    // --delete-before-call：先取得 PIDL 再删掉文件（模拟“调用方拿到路径后文件被移走”）
    if args.iter().any(|a| a == "--delete-before-call") {
        for f in &files {
            let r = std::fs::remove_file(f);
            out(&format!("call: 调用前删除 {}：{}", f.display(), r.is_ok()));
        }
    }
    let before = explorer_windows();
    let t = Instant::now();
    let hr = match style {
        // Chromium／Electron（platform_util_win.cc）：父目录 PIDL + 一个文件的完整 PIDL
        "chromium" => {
            let Some(d) = dir_pidl
                .as_ref()
                .filter(|_| pidls_first.len() == files.len())
            else {
                out(
                    "call: 解析 PIDL 失败（文件不存在？），Chromium 此时不调用 SHOpenFolderAndSelectItems",
                );
                return 3;
            };
            let ptrs: Vec<_> = pidls_first.iter().map(|p| p.as_ptr()).collect();
            // SAFETY: PIDL 在调用期间有效。
            unsafe { SHOpenFolderAndSelectItems(d.as_ptr(), Some(&ptrs), 0) }
        }
        // 只传文件 PIDL、cidl=0：选中该项（部分程序这样调用）
        _ => {
            let Some(f) = pidls_first.first() else {
                out("call: 解析 PIDL 失败（文件不存在？）");
                return 3;
            };
            // SAFETY: PIDL 在调用期间有效。
            unsafe { SHOpenFolderAndSelectItems(f.as_ptr(), None, 0) }
        }
    };
    let elapsed = t.elapsed().as_millis();
    let hr_text = match &hr {
        Ok(()) => "0x0".to_string(),
        Err(e) => format!("0x{:08x}", e.code().0 as u32),
    };
    let new = wait_new(&before, wait_ms);
    out(&format!(
        "call: style={style} hr={hr_text} elapsed={elapsed}ms new_explorer={}",
        new.len()
    ));
    for w in &new {
        out(&format!("call: 新资源管理器窗口“{}”", w.title));
    }
    if !args.iter().any(|a| a == "--keep") {
        close_new(&new, &folder_name(&dir));
    }
    0
}

// ---------- exec ----------

fn exec(args: &[String]) -> i32 {
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    com_init();
    let Some(folder) = values(args, "--folder=").first().map(PathBuf::from) else {
        eprintln!("exec 需要 --folder=");
        return 2;
    };
    let verb = values(args, "--verb=").first().copied().unwrap_or("open");
    let before = explorer_windows();
    let file = wide(&folder.to_string_lossy());
    let verb_w = wide(verb);
    // SAFETY: 结构体字段在调用期间有效。
    let ok = unsafe {
        let mut sei: SHELLEXECUTEINFOW = std::mem::zeroed();
        sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOCLOSEPROCESS;
        sei.lpVerb = if verb == "default" {
            std::ptr::null()
        } else {
            verb_w.as_ptr()
        };
        sei.lpFile = file.as_ptr();
        sei.nShow = wm::SW_SHOWNORMAL;
        let ok = ShellExecuteExW(&mut sei) != 0;
        if !sei.hProcess.is_null() {
            windows_sys::Win32::Foundation::CloseHandle(sei.hProcess);
        }
        ok
    };
    let new = wait_new(&before, 3000);
    out(&format!(
        "exec: verb={verb} ok={ok} new_explorer={}",
        new.len()
    ));
    if !args.iter().any(|a| a == "--keep") {
        close_new(&new, &folder_name(&folder));
    }
    0
}

// ---------- list ----------

fn list_shell_windows() -> Vec<String> {
    let mut v = Vec::new();
    // SAFETY: COM 已初始化；只读枚举。
    unsafe {
        let Ok(sw) = CoCreateInstance::<_, IShellWindows>(
            &windows::Win32::UI::Shell::ShellWindows,
            None,
            CLSCTX_ALL,
        ) else {
            v.push("无法连接 ShellWindows".into());
            return v;
        };
        let n = sw.Count().unwrap_or(0);
        for i in 0..n {
            let idx = VARIANT::from(i);
            let Ok(d): windows::core::Result<IDispatch> = sw.Item(&idx) else {
                v.push(format!("#{i} Item 失败"));
                continue;
            };
            match d.cast::<IWebBrowserApp>() {
                Ok(app) => {
                    let h = app.HWND().map(|h| h.0).unwrap_or(0);
                    let url = app
                        .LocationURL()
                        .map(|b| b.to_string())
                        .unwrap_or_else(|e| format!("（LocationURL 失败 0x{:08x}）", e.code().0));
                    v.push(format!("#{i} hwnd=0x{h:x} {url}"));
                }
                Err(_) => v.push(format!("#{i} 不支持 IWebBrowserApp")),
            }
        }
    }
    v
}
