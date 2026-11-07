//! 原生右键菜单（IContextMenu / IContextMenu2 / IContextMenu3）承载验证。
//!
//! 流程与主程序计划复用的方式一致：
//! 1. 路径 -> SHParseDisplayName 得到绝对 PIDL；
//! 2. SHCreateShellItemArrayFromIDLists -> BindToHandler(BHID_SFUIObject) 得到 IContextMenu（支持多选）；
//! 3. QueryContextMenu 填充 HMENU，宿主窗口把 WM_INITMENUPOPUP / WM_DRAWITEM / WM_MEASUREITEM /
//!    WM_MENUCHAR 转发给 IContextMenu3::HandleMenuMsg2（否则延迟填充的子菜单为空）；
//! 4. 选中后用 CMINVOKECOMMANDINFOEX 调用命令偏移；取消（返回 0）时只销毁菜单并释放对象。
//!
//! 键盘自动化：通过向本线程的菜单窗口（类名 #32768）投递 WM_KEYDOWN 模拟方向键/回车/Esc，
//! 并依据 WM_MENUSELECT 回报的高亮项决定下一步，避免依赖坐标。

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetCurrentThreadId, GetGuiResources,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

use crate::{flag, from_wide, opt, wide};

const ID_FIRST: u32 = 1;
const ID_LAST: u32 = 0x7FFF;
const VK_RETURN: usize = 0x0D;
// 与 SEE_MASK_UNICODE / SEE_MASK_NOASYNC 同值；windows crate 未导出 CMIC_ 前缀常量。
const CMIC_MASK_UNICODE: u32 = 0x4000;
const CMIC_MASK_NOASYNC: u32 = 0x100;
const VK_ESCAPE: usize = 0x1B;
const VK_RIGHT: usize = 0x27;
const VK_DOWN: usize = 0x28;

#[derive(Clone, Debug)]
struct Item {
    depth: usize,
    menu: HMENU,
    pos: u32,
    id: u32,
    text: String,
    verb: String,
    submenu: Option<HMENU>,
    separator: bool,
    disabled: bool,
    ownerdraw: bool,
}

#[derive(Default)]
struct MsgStats {
    initmenupopup: u32,
    menuselect: u32,
    drawitem: u32,
    measureitem: u32,
    menuchar: u32,
    forwarded_ok: u32,
}

/// 键盘自动化：沿 path 逐层高亮，最后执行 final_key（回车选择或 Esc 取消）。
struct Auto {
    path: Vec<(HMENU, u32, bool)>, // (所在菜单, 位置, 是否子菜单入口)
    level: usize,
    final_key: usize,
    keys_sent: u32,
    done_nav: bool,
    escapes_sent: u32,
}

thread_local! {
    static CM2: RefCell<Option<IContextMenu2>> = const { RefCell::new(None) };
    static CM3: RefCell<Option<IContextMenu3>> = const { RefCell::new(None) };
    static STATS: RefCell<MsgStats> = RefCell::new(MsgStats::default());
    static SELECTED: Cell<(isize, u32, u32)> = const { Cell::new((0, 0, 0)) }; // (hmenu, item, flags)
    static AUTO: RefCell<Option<Auto>> = const { RefCell::new(None) };
    static BITMAPS: Cell<u32> = const { Cell::new(0) };
    static MANUAL: Cell<bool> = const { Cell::new(false) };
}

pub fn cmd_menu(args: &[String]) -> i32 {
    match run(args) {
        Ok(code) => code,
        Err(e) => {
            println!("错误：{e:?}");
            1
        }
    }
}

fn run(args: &[String]) -> Result<i32> {
    let items: Vec<String> = opt(args, "--items")
        .map(|s| s.split('|').map(|p| p.to_string()).collect())
        .unwrap_or_default();
    if items.is_empty() {
        println!("需要 --items <路径>[|<路径>...]");
        return Ok(2);
    }
    let mode = opt(args, "--mode").unwrap_or("list");
    let target = opt(args, "--match");
    let repeat: u32 = opt(args, "--repeat")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let wait_file = opt(args, "--wait-file");
    let wait_secs: u64 = opt(args, "--wait")
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let extended = flag(args, "--extended");
    // --manual：不投递模拟按键，由真人用键盘/鼠标操作弹出的菜单。
    MANUAL.with(|m| m.set(flag(args, "--manual")));

    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    let hwnd = create_host_window()?;
    let gui0 = gui_resources();
    println!(
        "宿主窗口已创建；初始 GUI 资源 GDI={} USER={}",
        gui0.0, gui0.1
    );

    let mut last_code = 0;
    for round in 1..=repeat {
        println!(
            "== 第 {round}/{repeat} 轮，模式 {mode}，选中 {} 项",
            items.len()
        );
        last_code = one_round(hwnd, &items, mode, target, extended, wait_file, wait_secs)?;
        pump_for(Duration::from_millis(300));
        let g = gui_resources();
        println!("第 {round} 轮结束：GUI 资源 GDI={} USER={}", g.0, g.1);
    }
    // 取消若干轮后在同一进程里再执行一次命令，确认取消不留下无效状态。
    if let Some(m) = opt(args, "--after-invoke") {
        println!("== 取消轮次之后，同一进程直接调用：{m}");
        last_code = one_round(
            hwnd,
            &items,
            "invoke",
            Some(m),
            extended,
            wait_file,
            wait_secs,
        )?;
    }
    let gui1 = gui_resources();
    println!(
        "全部轮次结束，进程仍在运行；GUI 资源变化 GDI {:+} USER {:+}",
        gui1.0 as i64 - gui0.0 as i64,
        gui1.1 as i64 - gui0.1 as i64
    );
    unsafe {
        let _ = DestroyWindow(hwnd);
        CoUninitialize();
    }
    Ok(last_code)
}

fn one_round(
    hwnd: HWND,
    items: &[String],
    mode: &str,
    target: Option<&str>,
    extended: bool,
    wait_file: Option<&str>,
    wait_secs: u64,
) -> Result<i32> {
    let cm = context_menu_for(items)?;
    let cm2: Option<IContextMenu2> = cm.cast().ok();
    let cm3: Option<IContextMenu3> = cm.cast().ok();
    println!(
        "IContextMenu 获取成功；IContextMenu2={} IContextMenu3={}",
        cm2.is_some(),
        cm3.is_some()
    );
    CM2.with(|c| *c.borrow_mut() = cm2.clone());
    CM3.with(|c| *c.borrow_mut() = cm3.clone());
    STATS.with(|s| *s.borrow_mut() = MsgStats::default());

    let hmenu = unsafe { CreatePopupMenu()? };
    let mut flags = CMF_NORMAL | CMF_CANRENAME;
    if extended {
        flags |= CMF_EXTENDEDVERBS;
    }
    let qr = unsafe { cm.QueryContextMenu(hmenu, 0, ID_FIRST, ID_LAST, flags) };
    println!("QueryContextMenu: {qr:?}");

    let mut tree = Vec::new();
    BITMAPS.with(|b| b.set(0));
    if std::env::var_os("PROBE_NO_WALK").is_none() {
        walk(&cm, hmenu, 0, &mut tree);
    }
    println!("带位图的菜单项：{}", BITMAPS.with(|b| b.get()));
    let code = match mode {
        "list" => {
            for it in &tree {
                print_item(it);
            }
            println!("共 {} 项（含子菜单项）", tree.len());
            0
        }
        "invoke" => {
            let t = target.expect("invoke 需要 --match");
            match find_path(&tree, t) {
                Some(path) => {
                    let leaf = &tree[*path.last().unwrap()];
                    println!(
                        "直接调用：{}（id={} verb={}）",
                        leaf.text, leaf.id, leaf.verb
                    );
                    invoke(&cm, hwnd, leaf.id)?;
                    wait_result(wait_file, wait_secs)
                }
                None => {
                    println!("未找到匹配项：{t}");
                    3
                }
            }
        }
        "track-cancel" | "track-select" => {
            let path_idx = match target {
                Some(t) => match find_path(&tree, t) {
                    Some(p) => p,
                    None => {
                        println!("未找到匹配项：{t}");
                        unsafe { DestroyMenu(hmenu)? };
                        release(cm, cm2, cm3);
                        return Ok(3);
                    }
                },
                None => vec![],
            };
            let path: Vec<(HMENU, u32, bool)> = path_idx
                .iter()
                .map(|&i| (tree[i].menu, tree[i].pos, tree[i].submenu.is_some()))
                .collect();
            let final_key = if mode == "track-select" {
                VK_RETURN
            } else {
                VK_ESCAPE
            };
            if !MANUAL.with(|m| m.get()) {
                AUTO.with(|a| {
                    *a.borrow_mut() = Some(Auto {
                        path,
                        level: 0,
                        final_key,
                        keys_sent: 0,
                        done_nav: false,
                        escapes_sent: 0,
                    })
                });
            }
            let cmd = track(hwnd, hmenu);
            let keys = AUTO.with(|a| a.borrow_mut().take().map(|a| a.keys_sent).unwrap_or(0));
            STATS.with(|s| {
                let s = s.borrow();
                println!(
                    "菜单消息：INITMENUPOPUP={} MENUSELECT={} DRAWITEM={} MEASUREITEM={} MENUCHAR={} 转发成功={}；投递按键 {keys} 次",
                    s.initmenupopup, s.menuselect, s.drawitem, s.measureitem, s.menuchar, s.forwarded_ok
                )
            });
            if cmd == 0 {
                println!("TrackPopupMenuEx 返回 0：菜单已取消，未调用任何命令");
                if mode == "track-select" { 4 } else { 0 }
            } else {
                let name = tree
                    .iter()
                    .find(|i| i.id == cmd)
                    .map(|i| i.text.clone())
                    .unwrap_or_default();
                println!("TrackPopupMenuEx 返回命令 id={cmd}（{name}）");
                if mode == "track-cancel" {
                    println!("预期取消却选中了命令，不调用");
                    5
                } else {
                    invoke(&cm, hwnd, cmd)?;
                    wait_result(wait_file, wait_secs)
                }
            }
        }
        _ => {
            println!("未知模式 {mode}");
            2
        }
    };
    unsafe { DestroyMenu(hmenu)? };
    release(cm, cm2, cm3);
    Ok(code)
}

fn release(cm: IContextMenu, cm2: Option<IContextMenu2>, cm3: Option<IContextMenu3>) {
    CM2.with(|c| c.borrow_mut().take());
    CM3.with(|c| c.borrow_mut().take());
    drop(cm3);
    drop(cm2);
    drop(cm);
    println!("菜单已销毁，IContextMenu 系列接口已释放");
}

fn context_menu_for(items: &[String]) -> Result<IContextMenu> {
    let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
    for p in items {
        let w = wide(p);
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        unsafe { SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None)? };
        pidls.push(pidl);
    }
    let consts: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| *p as *const _).collect();
    let arr = unsafe { SHCreateShellItemArrayFromIDLists(&consts) };
    for p in pidls {
        unsafe { CoTaskMemFree(Some(p as *const _)) };
    }
    let arr = arr?;
    unsafe { arr.BindToHandler(None, &BHID_SFUIObject) }
}

fn walk(cm: &IContextMenu, hmenu: HMENU, depth: usize, out: &mut Vec<Item>) {
    let count = unsafe { GetMenuItemCount(Some(hmenu)) };
    for pos in 0..count.max(0) as u32 {
        let mut mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_ID | MIIM_SUBMENU | MIIM_STATE | MIIM_STRING | MIIM_BITMAP,
            ..Default::default()
        };
        if unsafe { GetMenuItemInfoW(hmenu, pos, true, &mut mii) }.is_err() {
            continue;
        }
        let mut text = String::new();
        if mii.cch > 0 {
            let mut buf = vec![0u16; mii.cch as usize + 1];
            mii.dwTypeData = PWSTR(buf.as_mut_ptr());
            mii.cch += 1;
            if unsafe { GetMenuItemInfoW(hmenu, pos, true, &mut mii) }.is_ok() {
                text = from_wide(&buf).replace('&', "");
            }
        }
        let separator = mii.fType.0 & MFT_SEPARATOR.0 != 0;
        let ownerdraw = mii.fType.0 & MFT_OWNERDRAW.0 != 0;
        let disabled = mii.fState.0 & (MFS_DISABLED.0 | MFS_GRAYED.0) != 0;
        let mut verb = String::new();
        if !separator && mii.wID >= ID_FIRST && mii.wID <= ID_LAST && mii.hSubMenu.is_invalid() {
            let mut buf = [0u16; 260];
            let r = unsafe {
                cm.GetCommandString(
                    (mii.wID - ID_FIRST) as usize,
                    GCS_VERBW,
                    None,
                    PSTR(buf.as_mut_ptr() as *mut u8),
                    buf.len() as u32,
                )
            };
            if r.is_ok() {
                verb = from_wide(&buf);
            }
        }
        // 记录处理程序设置的菜单项位图（HBMMENU_* 预定义值 <= 11 除外），用于诊断 GDI 增长。
        if (mii.hbmpItem.0 as isize) > 11 {
            BITMAPS.with(|b| b.set(b.get() + 1));
        }
        let sub = if mii.hSubMenu.is_invalid() {
            None
        } else {
            Some(mii.hSubMenu)
        };
        out.push(Item {
            depth,
            menu: hmenu,
            pos,
            id: mii.wID,
            text,
            verb,
            submenu: sub,
            separator,
            disabled,
            ownerdraw,
        });
        if let Some(h) = sub {
            // 延迟填充的子菜单（发送到、NanaZip 等）需要先收到 WM_INITMENUPOPUP。
            forward(WM_INITMENUPOPUP, WPARAM(h.0 as usize), LPARAM(pos as isize));
            walk(cm, h, depth + 1, out);
        }
    }
}

fn print_item(it: &Item) {
    let pad = "  ".repeat(it.depth);
    if it.separator {
        println!("{pad}----");
        return;
    }
    let mut tags = Vec::new();
    if it.submenu.is_some() {
        tags.push("子菜单".to_string());
    }
    if it.disabled {
        tags.push("禁用".to_string());
    }
    if it.ownerdraw {
        tags.push("自绘".to_string());
    }
    if !it.verb.is_empty() {
        tags.push(format!("verb={}", it.verb));
    }
    println!("{pad}[{}] {} {}", it.id, it.text, tags.join(" "));
}

/// `--match "A>B"`：逐层匹配（忽略大小写的子串），返回 tree 下标路径。
fn find_path(tree: &[Item], spec: &str) -> Option<Vec<usize>> {
    let parts: Vec<String> = spec.split('>').map(|s| s.trim().to_lowercase()).collect();
    let mut path = Vec::new();
    let mut parent_menu: Option<HMENU> = None;
    for (lvl, part) in parts.iter().enumerate() {
        let idx = tree.iter().position(|it| {
            it.depth == lvl
                && !it.separator
                && parent_menu.is_none_or(|m| it.menu == m)
                && (it.text.to_lowercase().contains(part) || it.verb.to_lowercase() == *part)
        })?;
        path.push(idx);
        parent_menu = tree[idx].submenu;
    }
    Some(path)
}

fn invoke(cm: &IContextMenu, hwnd: HWND, id: u32) -> Result<()> {
    let offset = (id - ID_FIRST) as usize;
    let dir = wide(&std::env::current_dir().unwrap().to_string_lossy());
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE | CMIC_MASK_NOASYNC,
        hwnd,
        lpVerb: PCSTR(offset as *const u8),
        lpVerbW: PCWSTR(offset as *const u16),
        lpDirectoryW: PCWSTR(dir.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    let r = unsafe { cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO) };
    println!("InvokeCommand: {r:?}");
    r
}

fn wait_result(wait_file: Option<&str>, secs: u64) -> i32 {
    let Some(f) = wait_file else {
        pump_for(Duration::from_secs(secs.min(3)));
        return 0;
    };
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        pump_for(Duration::from_millis(200));
        if let Ok(m) = std::fs::metadata(f)
            && m.len() > 0
        {
            // 再等一会儿让写入完成
            pump_for(Duration::from_millis(1500));
            let len = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
            println!(
                "目标文件已出现：{f}（{len} 字节，用时 {:?}）",
                start.elapsed()
            );
            return 0;
        }
    }
    println!("等待 {secs}s 未出现目标文件：{f}");
    6
}

fn track(hwnd: HWND, hmenu: HMENU) -> u32 {
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let fg = SetForegroundWindow(hwnd).as_bool();
        println!("SetForegroundWindow={fg}；开始 TrackPopupMenuEx");
        SetTimer(Some(hwnd), 1, 120, None);
        let r = TrackPopupMenuEx(
            hmenu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            pt.x,
            pt.y,
            hwnd,
            None,
        );
        let _ = KillTimer(Some(hwnd), 1);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        r.0 as u32
    }
}

fn forward(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    let r3 = CM3.with(|c| {
        c.borrow().as_ref().map(|cm| {
            let mut lr = LRESULT(0);
            let ok = unsafe { cm.HandleMenuMsg2(msg, wp, lp, Some(&mut lr)) }.is_ok();
            (ok, lr)
        })
    });
    if let Some((ok, lr)) = r3 {
        if ok {
            STATS.with(|s| s.borrow_mut().forwarded_ok += 1);
            return Some(lr);
        }
        return None;
    }
    let ok2 = CM2.with(|c| {
        c.borrow()
            .as_ref()
            .map(|cm| unsafe { cm.HandleMenuMsg(msg, wp, lp) }.is_ok())
            .unwrap_or(false)
    });
    if ok2 {
        STATS.with(|s| s.borrow_mut().forwarded_ok += 1);
        Some(LRESULT(0))
    } else {
        None
    }
}

fn menu_window() -> Option<HWND> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let mut buf = [0u16; 32];
        let n = unsafe { GetClassNameW(h, &mut buf) };
        if n > 0
            && from_wide(&buf[..n as usize]) == "#32768"
            && unsafe { IsWindowVisible(h) }.as_bool()
        {
            unsafe { *(lp.0 as *mut HWND) = h };
        }
        TRUE
    }
    let mut found = HWND::default();
    unsafe {
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(cb),
            LPARAM(&mut found as *mut _ as isize),
        );
    }
    if found.is_invalid() {
        None
    } else {
        Some(found)
    }
}

fn on_timer() {
    let Some(mw) = menu_window() else { return };
    let (sel_menu, sel_item, sel_flags) = SELECTED.with(|s| s.get());
    let key = AUTO.with(|a| {
        let mut a = a.borrow_mut();
        let a = a.as_mut()?;
        if a.keys_sent > 300 {
            return Some(VK_ESCAPE);
        }
        if !a.done_nav && a.level < a.path.len() {
            let (m, pos, is_sub) = a.path[a.level];
            let hit = sel_menu == m.0 as isize
                && if sel_flags & MF_POPUP.0 != 0 {
                    sel_item == pos
                } else {
                    sel_item == unsafe { GetMenuItemID(m, pos as i32) }
                };
            if !hit {
                return Some(VK_DOWN);
            }
            a.level += 1;
            if a.level < a.path.len() {
                // 打开子菜单进入下一层
                return Some(if is_sub { VK_RIGHT } else { VK_DOWN });
            }
            a.done_nav = true;
            if a.final_key == VK_ESCAPE {
                // 取消路径：目标若是子菜单则先打开，再逐层 Esc 退出
                if is_sub {
                    return Some(VK_RIGHT);
                }
            }
        }
        if a.path.is_empty() && !a.done_nav {
            a.done_nav = true;
            return Some(VK_DOWN);
        }
        if a.final_key == VK_ESCAPE {
            a.escapes_sent += 1;
        }
        Some(a.final_key)
    });
    if let Some(k) = key {
        AUTO.with(|a| {
            if let Some(a) = a.borrow_mut().as_mut() {
                a.keys_sent += 1;
            }
        });
        unsafe {
            let _ = PostMessageW(Some(mw), WM_KEYDOWN, WPARAM(k), LPARAM(0));
            let _ = PostMessageW(
                Some(mw),
                WM_KEYUP,
                WPARAM(k),
                LPARAM(0xC000_0001u32 as isize),
            );
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR | WM_MENUSELECT => {
            STATS.with(|s| {
                let mut s = s.borrow_mut();
                match msg {
                    WM_INITMENUPOPUP => s.initmenupopup += 1,
                    WM_DRAWITEM => s.drawitem += 1,
                    WM_MEASUREITEM => s.measureitem += 1,
                    WM_MENUCHAR => s.menuchar += 1,
                    _ => s.menuselect += 1,
                }
            });
            if msg == WM_MENUSELECT {
                let item = (wp.0 & 0xFFFF) as u32;
                let flags = ((wp.0 >> 16) & 0xFFFF) as u32;
                SELECTED.with(|s| s.set((lp.0, item, flags)));
            }
            if let Some(lr) = forward(msg, wp, lp) {
                if msg == WM_MENUCHAR {
                    return lr;
                }
                if msg != WM_MENUSELECT {
                    return LRESULT(0);
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_TIMER => {
            on_timer();
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn create_host_window() -> Result<HWND> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("ShellCloudProbeHost");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("shell-cloud-probe"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            200,
            100,
            None,
            None,
            Some(hinst),
            None,
        )
    }
}

pub fn pump_for(d: Duration) {
    let start = Instant::now();
    unsafe {
        let mut msg = MSG::default();
        while start.elapsed() < d {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

fn gui_resources() -> (u32, u32) {
    unsafe {
        let p = GetCurrentProcess();
        (
            GetGuiResources(p, GR_GDIOBJECTS),
            GetGuiResources(p, GR_USEROBJECTS),
        )
    }
}
