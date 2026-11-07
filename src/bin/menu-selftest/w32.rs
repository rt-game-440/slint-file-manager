//! 自测用的 Win32 辅助：窗口枚举、真实鼠标／键盘消息、抓屏。全部只作用于本进程的窗口或只读查询。

use std::ffi::c_void;
use std::path::Path;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, ClientToScreen, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC,
    SelectObject,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, HWND_TOPMOST, IsWindowVisible, PostMessageW, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SetCursorPos, SetForegroundWindow, SetWindowPos, WM_CLOSE, WM_KEYDOWN,
    WM_KEYUP, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_RBUTTONUP,
};

pub fn hwnd(v: isize) -> HWND {
    HWND(v as *mut c_void)
}

pub fn pid() -> u32 {
    // SAFETY: 无参数的只读查询。
    unsafe { GetCurrentProcessId() }
}

#[derive(Clone, Debug)]
pub struct Win {
    pub hwnd: isize,
    pub title: String,
    pub class: String,
    pub pid: u32,
    pub visible: bool,
}

pub fn all_windows() -> Vec<Win> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> windows::core::BOOL {
        let mut t = [0u16; 512];
        let mut c = [0u16; 64];
        let mut pid = 0u32;
        // SAFETY: 只读窗口查询；lp 指向调用者的 Vec。
        unsafe {
            let n = GetWindowTextW(h, &mut t).max(0) as usize;
            let m = GetClassNameW(h, &mut c).max(0) as usize;
            GetWindowThreadProcessId(h, Some(&mut pid));
            (*(lp.0 as *mut Vec<Win>)).push(Win {
                hwnd: h.0 as isize,
                title: String::from_utf16_lossy(&t[..n]),
                class: String::from_utf16_lossy(&c[..m]),
                pid,
                visible: IsWindowVisible(h).as_bool(),
            });
        }
        true.into()
    }
    let mut v: Vec<Win> = Vec::new();
    // SAFETY: 回调同步执行。
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

/// 标题包含 `needle` 的可见顶层窗口。
pub fn find_by_title(needle: &str) -> Option<Win> {
    all_windows()
        .into_iter()
        .find(|w| w.visible && w.title.contains(needle))
}

/// 本进程里标题含 `name` 的对话框（`#32770`），用于找属性对话框。
pub fn find_props_dialog(name: &str) -> Option<isize> {
    let me = pid();
    all_windows()
        .into_iter()
        .find(|w| w.pid == me && w.class == "#32770" && w.visible && w.title.contains(name))
        .map(|w| w.hwnd)
}

/// 资源管理器窗口数（用来确认“打开文件夹”没有另开窗口）。
pub fn explorer_windows() -> usize {
    all_windows()
        .iter()
        .filter(|w| w.class == "CabinetWClass" && w.visible)
        .count()
}

pub fn close(h: HWND) {
    // SAFETY: 向指定窗口投递关闭请求。
    unsafe {
        let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

pub fn focus() -> isize {
    // SAFETY: 只读查询。
    unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus().0 as isize }
}

pub fn foreground() -> isize {
    // SAFETY: 只读查询。
    unsafe { GetForegroundWindow().0 as isize }
}

/// 尽力把窗口提到前台（先试直接设置，不行再借前台线程的输入队列）。返回是否成功。
pub fn force_foreground(h: HWND) -> bool {
    // SAFETY: 只涉及本进程窗口与前台线程的输入附着，随即解除。
    unsafe {
        let _ = SetForegroundWindow(h);
        if GetForegroundWindow() == h {
            return true;
        }
        let fg = GetForegroundWindow();
        let fg_thread = GetWindowThreadProcessId(fg, None);
        let me = GetCurrentThreadId();
        if fg_thread != 0 && fg_thread != me {
            let _ = AttachThreadInput(me, fg_thread, true);
            let _ = SetForegroundWindow(h);
            let _ = AttachThreadInput(me, fg_thread, false);
        }
        GetForegroundWindow() == h
    }
}

pub fn set_topmost(h: HWND) {
    // SAFETY: 只改本进程窗口的 Z 序。
    unsafe {
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

pub fn client_to_screen(h: HWND, x: i32, y: i32) -> (i32, i32) {
    let mut p = POINT { x, y };
    // SAFETY: p 为有效缓冲区。
    unsafe {
        let _ = ClientToScreen(h, &mut p);
    }
    (p.x, p.y)
}

fn lparam_xy(x: i32, y: i32) -> LPARAM {
    LPARAM((((y & 0xFFFF) as isize) << 16) | (x & 0xFFFF) as isize)
}

/// 真实右键：光标移到客户区 (x, y)（物理像素），再向窗口投递移动、右键按下、右键松开。
/// 由 winit 的窗口过程接收并转成 Slint 事件，与用户操作走同一条路径。
pub fn right_click(h: HWND, x: i32, y: i32) {
    let (sx, sy) = client_to_screen(h, x, y);
    let lp = lparam_xy(x, y);
    // SAFETY: 移动光标并向本进程窗口投递消息。
    unsafe {
        let _ = SetCursorPos(sx, sy);
        let _ = PostMessageW(Some(h), WM_MOUSEMOVE, WPARAM(0), lp);
        let _ = PostMessageW(Some(h), WM_RBUTTONDOWN, WPARAM(0x0002), lp);
        let _ = PostMessageW(Some(h), WM_RBUTTONUP, WPARAM(0), lp);
    }
}

/// 诊断：可见性、是否最小化、类名、DPI 感知。
pub fn describe(h: HWND) -> String {
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::IsIconic;
    let mut c = [0u16; 64];
    // SAFETY: 只读查询。
    unsafe {
        let m = GetClassNameW(h, &mut c).max(0) as usize;
        format!(
            "类名 {}，可见 {}，最小化 {}，DPI {}",
            String::from_utf16_lossy(&c[..m]),
            IsWindowVisible(h).as_bool(),
            IsIconic(h).as_bool(),
            GetDpiForWindow(h)
        )
    }
}

/// 诊断：真实左键（同 right_click 的路径）。
pub fn left_click(h: HWND, x: i32, y: i32) {
    use windows::Win32::UI::WindowsAndMessaging::{WM_LBUTTONDOWN, WM_LBUTTONUP};
    let (sx, sy) = client_to_screen(h, x, y);
    let lp = lparam_xy(x, y);
    // SAFETY: 同 right_click。
    unsafe {
        let _ = SetCursorPos(sx, sy);
        let _ = PostMessageW(Some(h), WM_MOUSEMOVE, WPARAM(0), lp);
        let _ = PostMessageW(Some(h), WM_LBUTTONDOWN, WPARAM(0x0001), lp);
        let _ = PostMessageW(Some(h), WM_LBUTTONUP, WPARAM(0), lp);
    }
}

pub fn set_cursor(x: i32, y: i32) {
    // SAFETY: 只移动光标。
    unsafe {
        let _ = SetCursorPos(x, y);
    }
}

/// 键盘菜单键（VK_APPS）。
pub fn menu_key(h: HWND) {
    const VK_APPS: usize = 0x5D;
    let down = LPARAM(1 | (0x5D << 16) | (1 << 24));
    let up = LPARAM(1 | (0x5D << 16) | (1 << 24) | (1 << 30) | (1 << 31));
    // SAFETY: 向本进程窗口投递按键消息。
    unsafe {
        let _ = PostMessageW(Some(h), WM_KEYDOWN, WPARAM(VK_APPS), down);
        let _ = PostMessageW(Some(h), WM_KEYUP, WPARAM(VK_APPS), up);
    }
}

pub fn window_rect(h: HWND) -> RECT {
    let mut r = RECT::default();
    // SAFETY: r 为有效缓冲区。
    unsafe {
        let _ = GetWindowRect(h, &mut r);
    }
    r
}

pub fn union(rects: &[RECT]) -> RECT {
    let mut r = rects[0];
    for x in &rects[1..] {
        if x.right <= x.left || x.bottom <= x.top {
            continue;
        }
        r.left = r.left.min(x.left);
        r.top = r.top.min(x.top);
        r.right = r.right.max(x.right);
        r.bottom = r.bottom.max(x.bottom);
    }
    r
}

#[link(name = "user32")]
unsafe extern "system" {
    fn PrintWindow(hwnd: *mut c_void, hdc: *mut c_void, flags: u32) -> i32;
}

/// 用 `PrintWindow` 逐个把**本进程的窗口**（应用主窗口、菜单窗口、属性对话框）画进各自的位图，
/// 再按屏幕相对位置合成到一张图里。不读取屏幕像素，所以不会带进桌面上其他应用的内容
/// （这样得到的图里，菜单与应用重叠的地方按 `windows` 的先后顺序绘制，后者在上）。
pub fn capture_windows(
    hs: &[HWND],
    layer: Option<(HWND, u32, u32, Vec<u8>)>,
) -> Option<(u32, u32, Vec<u8>)> {
    let rects: Vec<RECT> = hs.iter().map(|&h| window_rect(h)).collect();
    let u = union(&rects);
    let (uw, uh) = (u.right - u.left, u.bottom - u.top);
    if uw <= 0 || uh <= 0 {
        return None;
    }
    // 底色：中性灰，便于一眼分辨“没画上的区域”
    let mut canvas = vec![0u8; (uw * uh * 4) as usize];
    for p in canvas.as_chunks_mut::<4>().0 {
        *p = [0x60, 0x60, 0x60, 0xFF];
    }
    for (&hw, r) in hs.iter().zip(&rects) {
        let (mut w, mut h) = (r.right - r.left, r.bottom - r.top);
        let mut r = *r;
        if w <= 0 || h <= 0 {
            continue;
        }
        // OpenGL 渲染的主窗口 PrintWindow 画不出来：用 Slint 自己的快照（只含客户区）放在客户区原点
        let px = match &layer {
            Some((lh, lw, lht, data)) if *lh == hw => {
                let (cx, cy) = client_to_screen(hw, 0, 0);
                r = RECT {
                    left: cx,
                    top: cy,
                    right: cx + *lw as i32,
                    bottom: cy + *lht as i32,
                };
                (w, h) = (*lw as i32, *lht as i32);
                Some(data.clone())
            }
            _ => print_window(hw, w, h),
        };
        let Some(px) = px else {
            continue;
        };
        for y in 0..h {
            for x in 0..w {
                let s = ((y * w + x) * 4) as usize;
                let (cx, cy) = (x + r.left - u.left, y + r.top - u.top);
                let d = ((cy * uw + cx) * 4) as usize;
                canvas[d..d + 4].copy_from_slice(&px[s..s + 4]);
            }
        }
    }
    Some((uw as u32, uh as u32, canvas))
}

/// 单个窗口的 RGBA 内容（尺寸 w×h，物理像素）。
fn print_window(h: HWND, w: i32, ht: i32) -> Option<Vec<u8>> {
    // SAFETY: 创建的 DC 与位图都在返回前释放；PrintWindow 只让窗口把自己画进我们的位图。
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, ht);
        let old = SelectObject(mem, bmp.into());
        let ok = PrintWindow(h.0, mem.0, 2) != 0; // PW_RENDERFULLCONTENT
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -ht, // 自上而下
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * ht * 4) as usize];
        let lines = if ok {
            GetDIBits(
                mem,
                bmp,
                0,
                ht as u32,
                Some(buf.as_mut_ptr().cast()),
                &mut bmi,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        if lines == 0 {
            return None;
        }
        for p in buf.as_chunks_mut::<4>().0 {
            p.swap(0, 2); // BGRA -> RGBA
            p[3] = 255;
        }
        Some(buf)
    }
}

pub fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    let f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .and_then(|mut w| w.write_image_data(rgba))
        .map_err(|e| e.to_string())
}
