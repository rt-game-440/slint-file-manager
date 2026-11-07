//! 菜单自动化（仅 `harness` 特性）：枚举菜单树、按文字路径自动导航到某一项并回车选择或 Esc 取消。
//!
//! 做法沿用 Issue #3 原型：用线程定时器向本线程的菜单窗口（类名 `#32768`）投递 `WM_KEYDOWN`，
//! 并依据 `WM_MENUSELECT` 报告的高亮项决定下一键，不依赖屏幕坐标。这是程序模拟的按键，
//! 不等同真人键盘／鼠标操作（真人操作列入验收清单）。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Shell::{IContextMenu, IContextMenu2, IContextMenu3};
use windows::Win32::UI::WindowsAndMessaging::{
    DeleteMenu, EnumThreadWindows, GetClassNameW, GetMenuItemCount, GetMenuItemID,
    GetMenuItemInfoW, HMENU, IsWindowVisible, KillTimer, MENUITEMINFOW, MF_BYPOSITION, MF_POPUP,
    MFS_DISABLED, MFS_GRAYED, MFT_SEPARATOR, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING,
    MIIM_SUBMENU, PostMessageW, SetTimer, WM_INITMENUPOPUP, WM_KEYDOWN, WM_KEYUP,
};
use windows::core::Interface;

use super::{
    ID_FIRST, MenuError, Target, build_context_menu, command_verb, find_owner, from_wide,
    top_level_position,
};

const VK_RETURN: usize = 0x0D;
const VK_ESCAPE: usize = 0x1B;
const VK_RIGHT: usize = 0x27;
const VK_DOWN: usize = 0x28;

/// 自动操作计划。
pub struct Plan {
    /// 逐层匹配的菜单文字，层间用 `>`（如 `NanaZip>添加到 "a.7z"`）；每层可用 `|` 列出多个候选
    /// （忽略大小写的子串匹配，或与 verb 相等）。空表示只弹出后取消。
    pub path: String,
    /// `true`：导航到目标后回车选择；`false`：导航到目标（若是子菜单则展开）后用 Esc 逐层取消。
    pub select: bool,
    /// 目标已被高亮、尚未按最后一键时调用一次（用于截图等观察）。
    pub on_ready: Option<Rc<dyn Fn()>>,
    /// 目标高亮后额外保持菜单打开的时间（观察事件循环、界面状态用）。
    pub hold: Duration,
}

impl Plan {
    pub fn select(path: &str) -> Plan {
        Plan {
            path: path.into(),
            select: true,
            on_ready: None,
            hold: Duration::ZERO,
        }
    }
    pub fn cancel(path: &str) -> Plan {
        Plan {
            path: path.into(),
            select: false,
            on_ready: None,
            hold: Duration::ZERO,
        }
    }
    pub fn ready(mut self, f: impl Fn() + 'static) -> Plan {
        self.on_ready = Some(Rc::new(f));
        self
    }
    pub fn hold(mut self, d: Duration) -> Plan {
        self.hold = d;
        self
    }
}

#[derive(Clone, Debug)]
pub struct TreeItem {
    pub depth: usize,
    pub id: u32,
    pub text: String,
    pub verb: String,
    pub submenu: bool,
    pub separator: bool,
    pub disabled: bool,
}

struct Step {
    menu: HMENU,
    pos: u32,
    is_sub: bool,
}

struct Auto {
    steps: Vec<Step>,
    level: usize,
    final_key: usize,
    keys_sent: u32,
    done_nav: bool,
    ready_fired: bool,
    ready_at: Option<Instant>,
    hold: Duration,
    on_ready: Option<Rc<dyn Fn()>>,
}

thread_local! {
    static LAST_TREE: RefCell<Vec<TreeItem>> = const { RefCell::new(Vec::new()) };
    static AUTO: RefCell<Option<Auto>> = const { RefCell::new(None) };
    static SELECTED: Cell<(isize, u32, u32)> = const { Cell::new((0, 0, 0)) };
}

/// 取走最近一次自动化菜单的完整菜单树（含延迟填充的子菜单）。
pub fn take_tree() -> Vec<TreeItem> {
    LAST_TREE.with(|t| std::mem::take(&mut *t.borrow_mut()))
}

pub fn on_menu_select(menu: isize, item: u32, flags: u32) {
    SELECTED.with(|s| s.set((menu, item, flags)));
}

/// 删掉顶层的“NanaZip”子菜单，模拟未安装。
pub fn remove_nanazip(hmenu: HMENU) {
    if let Some(pos) = top_level_position(hmenu, "nanazip") {
        // SAFETY: 删除自己构建的菜单里的一项（连同其子菜单）。
        unsafe {
            let _ = DeleteMenu(hmenu, pos, MF_BYPOSITION);
        }
    }
}

fn forward_init(cm: &IContextMenu, hmenu: HMENU, pos: u32) {
    let (wp, lp) = (WPARAM(hmenu.0 as usize), LPARAM(pos as isize));
    // SAFETY: 与产品里的转发一致，向外壳扩展报告子菜单即将显示。
    unsafe {
        if let Ok(cm3) = cm.cast::<IContextMenu3>() {
            let mut lr = windows::Win32::Foundation::LRESULT(0);
            let _ = cm3.HandleMenuMsg2(WM_INITMENUPOPUP, wp, lp, Some(&mut lr));
        } else if let Ok(cm2) = cm.cast::<IContextMenu2>() {
            let _ = cm2.HandleMenuMsg(WM_INITMENUPOPUP, wp, lp);
        }
    }
}

struct Node {
    item: TreeItem,
    menu: HMENU,
    pos: u32,
    sub: Option<HMENU>,
}

fn walk(cm: &IContextMenu, hmenu: HMENU, depth: usize, out: &mut Vec<Node>) {
    // SAFETY: 只读枚举菜单项。
    let count = unsafe { GetMenuItemCount(Some(hmenu)) };
    for pos in 0..count.max(0) as u32 {
        let mut mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_ID | MIIM_SUBMENU | MIIM_STATE | MIIM_STRING,
            ..Default::default()
        };
        // SAFETY: 同上；两次调用取文字。
        if unsafe { GetMenuItemInfoW(hmenu, pos, true, &mut mii) }.is_err() {
            continue;
        }
        let mut text = String::new();
        if mii.cch > 0 {
            let mut buf = vec![0u16; mii.cch as usize + 1];
            mii.dwTypeData = windows::core::PWSTR(buf.as_mut_ptr());
            mii.cch += 1;
            if unsafe { GetMenuItemInfoW(hmenu, pos, true, &mut mii) }.is_ok() {
                text = super::clean_label(&from_wide(&buf));
            }
        }
        let separator = mii.fType.0 & MFT_SEPARATOR.0 != 0;
        let disabled = mii.fState.0 & (MFS_DISABLED.0 | MFS_GRAYED.0) != 0;
        let sub = (!mii.hSubMenu.is_invalid()).then_some(mii.hSubMenu);
        let verb = if !separator && sub.is_none() && mii.wID >= ID_FIRST && mii.wID <= 0x7EFF {
            command_verb(cm, mii.wID - ID_FIRST)
        } else {
            String::new()
        };
        out.push(Node {
            item: TreeItem {
                depth,
                id: mii.wID,
                text,
                verb,
                submenu: sub.is_some(),
                separator,
                disabled,
            },
            menu: hmenu,
            pos,
            sub,
        });
        if let Some(h) = sub {
            forward_init(cm, h, pos); // 延迟填充的子菜单需要先收到 WM_INITMENUPOPUP
            walk(cm, h, depth + 1, out);
        }
    }
}

fn find_path(nodes: &[Node], spec: &str) -> Option<Vec<usize>> {
    let mut path = Vec::new();
    let mut parent: Option<HMENU> = None;
    for (lvl, part) in spec.split('>').enumerate() {
        let alts: Vec<String> = part.split('|').map(|s| s.trim().to_lowercase()).collect();
        let idx = nodes.iter().position(|n| {
            n.item.depth == lvl
                && !n.item.separator
                && parent.is_none_or(|m| n.menu == m)
                && alts.iter().any(|a| {
                    n.item.text.to_lowercase().contains(a.as_str())
                        || n.item.verb.to_lowercase() == *a
                })
        })?;
        path.push(idx);
        parent = nodes[idx].sub;
    }
    Some(path)
}

/// 枚举某个对象的完整菜单树（含延迟填充的子菜单），不弹出菜单、不执行命令。
pub fn snapshot(target: &Target) -> Result<Vec<TreeItem>, MenuError> {
    use windows::Win32::UI::Shell::CMF_NORMAL;
    use windows::Win32::UI::WindowsAndMessaging::CreatePopupMenu;
    let owner = find_owner().ok_or(MenuError::NoWindow)?;
    super::init_com()?;
    let built = build_context_menu(target, owner)?;
    // SAFETY: 句柄由 MenuGuard 销毁。
    let menu = super::MenuGuard(unsafe { CreatePopupMenu()? });
    // SAFETY: menu.0 有效。
    unsafe {
        built
            .cm
            .QueryContextMenu(menu.0, 0, ID_FIRST, super::ID_LAST, CMF_NORMAL)
            .ok()?;
    }
    let nanazip = top_level_position(menu.0, "nanazip").is_some();
    super::add_custom_items(menu.0, target, nanazip);
    let mut nodes = Vec::new();
    walk(&built.cm, menu.0, 0, &mut nodes);
    Ok(nodes.into_iter().map(|n| n.item).collect())
}

/// 已准备好的自动操作。
pub struct Driver {
    steps: Vec<Step>,
    select: bool,
    on_ready: Option<Rc<dyn Fn()>>,
    hold: Duration,
}

impl Driver {
    pub fn prepare(plan: &Plan, cm: &IContextMenu, hmenu: HMENU) -> Result<Driver, MenuError> {
        let mut nodes = Vec::new();
        walk(cm, hmenu, 0, &mut nodes);
        LAST_TREE.with(|t| *t.borrow_mut() = nodes.iter().map(|n| n.item.clone()).collect());
        let steps = if plan.path.is_empty() {
            vec![]
        } else {
            let idx = find_path(&nodes, &plan.path)
                .ok_or_else(|| MenuError::Shell(format!("未找到匹配项：{}", plan.path)))?;
            idx.iter()
                .map(|&i| Step {
                    menu: nodes[i].menu,
                    pos: nodes[i].pos,
                    is_sub: nodes[i].sub.is_some(),
                })
                .collect()
        };
        Ok(Driver {
            steps,
            select: plan.select,
            on_ready: plan.on_ready.clone(),
            hold: plan.hold,
        })
    }

    /// 开始定时发送按键，返回定时器编号。
    pub fn start(&mut self) -> usize {
        let steps = std::mem::take(&mut self.steps);
        AUTO.with(|a| {
            *a.borrow_mut() = Some(Auto {
                steps,
                level: 0,
                final_key: if self.select { VK_RETURN } else { VK_ESCAPE },
                keys_sent: 0,
                done_nav: false,
                ready_fired: false,
                ready_at: None,
                hold: self.hold,
                on_ready: self.on_ready.take(),
            })
        });
        SELECTED.with(|s| s.set((0, 0, 0)));
        // SAFETY: 线程定时器，回调为静态函数；stop 时 KillTimer。
        unsafe { SetTimer(None, 0, 120, Some(timer_proc)) }
    }
}

pub fn stop(timer: usize) {
    // SAFETY: 与 start 配对。
    unsafe {
        let _ = KillTimer(None, timer);
    }
    AUTO.with(|a| a.borrow_mut().take());
}

/// 本线程所有可见的菜单窗口（顶层菜单与已展开的子菜单），用于截图。
pub fn menu_windows() -> Vec<HWND> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> windows::core::BOOL {
        let mut buf = [0u16; 32];
        // SAFETY: 只读窗口查询；lp 指向调用者的 Vec。
        unsafe {
            let n = GetClassNameW(h, &mut buf);
            if n > 0 && from_wide(&buf[..n as usize]) == "#32768" && IsWindowVisible(h).as_bool() {
                (*(lp.0 as *mut Vec<HWND>)).push(h);
            }
        }
        true.into()
    }
    let mut v: Vec<HWND> = Vec::new();
    // SAFETY: 回调同步执行。
    unsafe {
        let _ = EnumThreadWindows(
            windows::Win32::System::Threading::GetCurrentThreadId(),
            Some(cb),
            LPARAM(&mut v as *mut _ as isize),
        );
    }
    v
}

unsafe extern "system" fn timer_proc(_hwnd: HWND, _msg: u32, _id: usize, _time: u32) {
    let Some(&mw) = menu_windows().first() else {
        return;
    };
    let (sel_menu, sel_item, sel_flags) = SELECTED.with(Cell::get);
    // on_ready 里可能做耗时操作，先把决策算出来，闭包放在锁外调用。
    let mut fire: Option<Rc<dyn Fn()>> = None;
    let key = AUTO.with(|a| {
        let mut a = a.borrow_mut();
        let a = a.as_mut()?;
        if a.keys_sent > 300 {
            return Some(VK_ESCAPE);
        }
        if !a.done_nav && a.level < a.steps.len() {
            let Step { menu, pos, is_sub } = {
                let s = &a.steps[a.level];
                Step {
                    menu: s.menu,
                    pos: s.pos,
                    is_sub: s.is_sub,
                }
            };
            let hit = sel_menu == menu.0 as isize
                && if sel_flags & MF_POPUP.0 != 0 {
                    sel_item == pos
                } else {
                    // SAFETY: 只读。
                    sel_item == unsafe { GetMenuItemID(menu, pos as i32) }
                };
            if !hit {
                return Some(VK_DOWN);
            }
            a.level += 1;
            if a.level < a.steps.len() {
                return Some(if is_sub { VK_RIGHT } else { VK_DOWN });
            }
            a.done_nav = true;
            if a.final_key == VK_ESCAPE && is_sub {
                return Some(VK_RIGHT); // 取消路径：目标是子菜单时先展开，再逐层 Esc
            }
        }
        if a.steps.is_empty() && !a.done_nav {
            a.done_nav = true;
            return Some(VK_DOWN);
        }
        if !a.ready_fired {
            // 目标已高亮：本拍只做观察（截图等），之后至少再等 hold 才按最后一键
            a.ready_fired = true;
            a.ready_at = Some(Instant::now());
            fire = a.on_ready.take();
            return None;
        }
        if a.ready_at.is_some_and(|t| t.elapsed() < a.hold) {
            return None;
        }
        Some(a.final_key)
    });
    if let Some(f) = fire {
        f();
    }
    if let Some(k) = key {
        AUTO.with(|a| {
            if let Some(a) = a.borrow_mut().as_mut() {
                a.keys_sent += 1;
            }
        });
        // SAFETY: 向本线程的菜单窗口投递按键消息。
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
