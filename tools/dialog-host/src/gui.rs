//! 带按钮的简单窗口（人工操作）与命令行 `--owner=window` 用的所有者窗口。

use crate::catalog::{Preset, presets};
use crate::cli::Request;
use crate::runner::execute;
use crate::util::wide;
use std::cell::RefCell;
use std::path::PathBuf;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, DEFAULT_GUI_FONT, GetStockObject, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

struct State {
    edit: HWND,
    presets: Vec<Preset>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

const ID_BASE: usize = 100;
const BTN_W: i32 = 330;
const BTN_H: i32 = 30;
const MARGIN: i32 = 10;

fn register(class: &str, proc: WNDPROC) -> Vec<u16> {
    let name = wide(class);
    unsafe {
        let hi = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: proc,
            hInstance: hi.into(),
            lpszClassName: PCWSTR(name.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as isize as *mut _),
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
    name
}

fn set_font(h: HWND) {
    unsafe {
        let f = GetStockObject(DEFAULT_GUI_FONT);
        SendMessageW(h, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));
    }
}

// ---------- 所有者窗口 ----------

unsafe extern "system" fn owner_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, m, w, l) }
}

/// 创建一个可见的普通顶层窗口，作为对话框的所有者（多数真实软件都会传所有者窗口）。
pub fn create_owner() -> HWND {
    let class = register("DialogHostOwner", Some(owner_proc));
    let title = wide("dialog-host 所有者窗口");
    unsafe {
        let hi = GetModuleHandleW(None).unwrap_or_default();
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_VISIBLE,
            200,
            200,
            360,
            120,
            None,
            None,
            Some(hi.into()),
            None,
        )
        .unwrap_or_default()
    }
}

pub fn destroy(h: HWND) {
    unsafe {
        let _ = DestroyWindow(h);
    }
}

// ---------- 带按钮的窗口 ----------

fn run_preset(owner: HWND, idx: usize) {
    let (req, edit) = STATE.with(|s| {
        let s = s.borrow();
        let st = s.as_ref().unwrap();
        let p = &st.presets[idx];
        (Request::parse(p.api, p.scenario, &p.opts), st.edit)
    });
    let text = match req {
        Ok((r, _)) => {
            let (rec, _) = execute(&r, Some(owner));
            serde_json::to_string_pretty(&rec).unwrap_or_default()
        }
        Err(e) => format!("参数错误：{e}"),
    };
    let w = wide(&text.replace('\n', "\r\n"));
    unsafe {
        let _ = SetWindowTextW(edit, PCWSTR(w.as_ptr()));
    }
}

unsafe extern "system" fn main_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match m {
            WM_COMMAND => {
                let id = w.0 & 0xFFFF;
                if id >= ID_BASE {
                    run_preset(h, id - ID_BASE);
                }
                LRESULT(0)
            }
            WM_SIZE => {
                let (cw, ch) = ((l.0 & 0xFFFF) as i32, ((l.0 >> 16) & 0xFFFF) as i32);
                let edit =
                    STATE.with(|s| s.borrow().as_ref().map(|st| (st.edit, st.presets.len())));
                if let Some((e, n)) = edit {
                    let cols = columns(cw);
                    let rows = (n as i32 + cols - 1) / cols;
                    let top = MARGIN + rows * (BTN_H + 6) + 4;
                    let _ = MoveWindow(
                        e,
                        MARGIN,
                        top,
                        cw - 2 * MARGIN,
                        (ch - top - MARGIN).max(40),
                        true,
                    );
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(h, m, w, l),
        }
    }
}

fn columns(client_w: i32) -> i32 {
    ((client_w - MARGIN) / (BTN_W + 6)).max(1)
}

pub fn run_gui(fixture: PathBuf) {
    let ps = presets(&fixture);
    let class = register("DialogHostMain", Some(main_proc));
    let title = wide(&format!(
        "dialog-host（{} 位）：文件选择对话框测试宿主 — 夹具 {}",
        crate::util::bitness(),
        fixture.display()
    ));
    unsafe {
        let hi = GetModuleHandleW(None).unwrap_or_default();
        let width = MARGIN * 2 + 2 * (BTN_W + 6) + 20;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            120,
            80,
            width,
            720,
            None,
            None,
            Some(hi.into()),
            None,
        )
        .unwrap_or_default();
        let cols = 2;
        for (i, p) in ps.iter().enumerate() {
            let (c, r) = ((i as i32) % cols, (i as i32) / cols);
            let label = wide(p.label);
            let b = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(wide("BUTTON").as_ptr()),
                PCWSTR(label.as_ptr()),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                MARGIN + c * (BTN_W + 6),
                MARGIN + r * (BTN_H + 6),
                BTN_W,
                BTN_H,
                Some(hwnd),
                Some(HMENU((ID_BASE + i) as *mut _)),
                Some(hi.into()),
                None,
            )
            .unwrap_or_default();
            set_font(b);
        }
        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            PCWSTR(wide("EDIT").as_ptr()),
            PCWSTR(wide("点击按钮弹出对话框，结果 JSON 显示在这里（同时写入日志）。").as_ptr()),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | WINDOW_STYLE((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32),
            0,
            0,
            100,
            100,
            Some(hwnd),
            None,
            Some(hi.into()),
            None,
        )
        .unwrap_or_default();
        set_font(edit);
        STATE.with(|s| *s.borrow_mut() = Some(State { edit, presets: ps }));
        let mut rc = windows::Win32::Foundation::RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        SendMessageW(
            hwnd,
            WM_SIZE,
            None,
            Some(LPARAM((rc.right as isize) | ((rc.bottom as isize) << 16))),
        );
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
