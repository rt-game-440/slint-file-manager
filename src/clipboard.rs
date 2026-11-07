//! 文件剪贴板：与资源管理器互通的复制／剪切。
//!
//! - 剪贴板上放两种格式：`CF_HDROP`（路径列表，`DROPFILES` 结构 + UTF-16 路径，双 NUL 结尾）与
//!   `Preferred DropEffect`（1 = 复制，2 = 剪切／移动）。资源管理器的“剪切”也是这两样，所以两边可以互相粘贴。
//! - 本应用的“剪切”只是在剪贴板上标记“粘贴时移动”，**不会**立刻动任何文件；源文件在粘贴并确认目标完整后才被移走。
//! - 写入剪贴板需要一个属主窗口：用一个只存在于本线程的消息窗口（不显示），只在界面线程使用。
//! - [`Clipboard`] 是可注入的：产品用 [`SystemClipboard`]，harness 与测试用 [`MemClipboard`]（不碰用户真实剪贴板）。
//! - [`Clipboard::sequence`] 返回剪贴板内容的变更序号，用来判断“剪切标记”是否已被别处的复制覆盖。

use std::cell::{Cell, RefCell};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

/// 粘贴时的动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Copy,
    Move,
}

/// 剪贴板里的文件。
#[derive(Clone, Debug, PartialEq)]
pub struct ClipFiles {
    pub paths: Vec<PathBuf>,
    pub effect: Effect,
}

pub trait Clipboard {
    /// 读取剪贴板上的文件；没有文件时为 `None`。
    fn read(&self) -> Option<ClipFiles>;
    /// 把文件放上剪贴板（替换原有内容）。
    fn write(&self, paths: &[PathBuf], effect: Effect) -> Result<(), String>;
    /// 清空剪贴板（只在剪贴板仍是本应用放上去的内容时调用）。
    fn clear(&self);
    /// 内容变更序号；每次剪贴板内容变化都会增加。
    fn sequence(&self) -> u32;
}

// ---------- DROPFILES 编解码（纯函数，可单测） ----------

const DROPFILES_HEADER: usize = 20;

/// 构造 `DROPFILES` 内存块：头（`pFiles = 20`，宽字符）+ 以 NUL 分隔、双 NUL 结尾的路径。
pub fn build_hdrop(paths: &[PathBuf]) -> Vec<u8> {
    let mut out = vec![0u8; DROPFILES_HEADER];
    out[0..4].copy_from_slice(&(DROPFILES_HEADER as u32).to_le_bytes());
    out[16..20].copy_from_slice(&1u32.to_le_bytes()); // fWide
    for p in paths {
        for u in p.as_os_str().encode_wide() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&[0, 0]);
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// 解析 `DROPFILES` 内存块。格式不对时返回空列表。
pub fn parse_hdrop(data: &[u8]) -> Vec<PathBuf> {
    if data.len() < DROPFILES_HEADER {
        return Vec::new();
    }
    let off = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let wide = u32::from_le_bytes([data[16], data[17], data[18], data[19]]) != 0;
    if off < DROPFILES_HEADER || off > data.len() {
        return Vec::new();
    }
    let body = &data[off..];
    let mut out = Vec::new();
    if wide {
        let units: Vec<u16> = body
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        for part in units.split(|&u| u == 0) {
            if part.is_empty() {
                break; // 双 NUL：列表结束
            }
            out.push(PathBuf::from(std::ffi::OsString::from_wide(part)));
        }
    } else {
        // 旧式 ANSI：很少见，按 UTF-8 宽松解码
        for part in body.split(|&b| b == 0) {
            if part.is_empty() {
                break;
            }
            out.push(PathBuf::from(String::from_utf8_lossy(part).into_owned()));
        }
    }
    out
}

fn effect_value(e: Effect) -> u32 {
    match e {
        Effect::Copy => 1,
        Effect::Move => 2,
    }
}

fn effect_from(v: u32) -> Effect {
    if v & 2 != 0 {
        Effect::Move
    } else {
        Effect::Copy
    }
}

// ---------- 系统剪贴板 ----------

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, HWND_MESSAGE};

const CF_HDROP: u32 = 15;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn drop_effect_format() -> u32 {
    // SAFETY: 参数是以 NUL 结尾的宽字符串。
    unsafe { RegisterClipboardFormatW(wide("Preferred DropEffect").as_ptr()) }
}

thread_local! {
    static OWNER: Cell<isize> = const { Cell::new(0) };
}

/// 本线程的剪贴板属主窗口（消息窗口，用预定义的 STATIC 类，不显示）。
fn owner() -> HWND {
    OWNER.with(|o| {
        if o.get() == 0 {
            // SAFETY: 创建只接收消息的隐藏窗口；类名与标题都是以 NUL 结尾的宽字符串。
            let h = unsafe {
                CreateWindowExW(
                    0,
                    wide("STATIC").as_ptr(),
                    wide("sfm-clipboard").as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            o.set(h as isize);
        }
        o.get() as HWND
    })
}

/// 打开剪贴板，被其他程序占用时短暂重试。
struct Open;

impl Open {
    fn new(with_owner: bool) -> Option<Open> {
        let hwnd = if with_owner {
            owner()
        } else {
            std::ptr::null_mut()
        };
        for _ in 0..20 {
            // SAFETY: 标准调用；成功才在 Drop 里关闭。
            if unsafe { OpenClipboard(hwnd) } != 0 {
                return Some(Open);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: 与成功的 OpenClipboard 配对。
        unsafe { CloseClipboard() };
    }
}

/// 把字节放进新分配的全局内存（可移动），失败返回空指针。
fn alloc_global(bytes: &[u8]) -> HANDLE {
    // SAFETY: 分配、加锁、复制、解锁；失败时释放。
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
        if h.is_null() {
            return h;
        }
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            GlobalFree(h);
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        GlobalUnlock(h);
        h
    }
}

fn read_global(h: HANDLE) -> Vec<u8> {
    if h.is_null() {
        return Vec::new();
    }
    // SAFETY: h 是剪贴板返回的全局内存句柄；复制出内容后立即解锁。
    unsafe {
        let size = GlobalSize(h);
        let p = GlobalLock(h) as *const u8;
        if p.is_null() {
            return Vec::new();
        }
        let v = std::slice::from_raw_parts(p, size).to_vec();
        GlobalUnlock(h);
        v
    }
}

pub struct SystemClipboard;

impl Clipboard for SystemClipboard {
    fn read(&self) -> Option<ClipFiles> {
        // SAFETY: 只读查询。
        if unsafe { IsClipboardFormatAvailable(CF_HDROP) } == 0 {
            return None;
        }
        let _open = Open::new(false)?;
        // SAFETY: 剪贴板已打开；返回的句柄归剪贴板所有，只读不释放。
        let paths = parse_hdrop(&read_global(unsafe { GetClipboardData(CF_HDROP) }));
        if paths.is_empty() {
            return None;
        }
        let fmt = drop_effect_format();
        // SAFETY: 同上。
        let effect = if unsafe { IsClipboardFormatAvailable(fmt) } != 0 {
            let d = read_global(unsafe { GetClipboardData(fmt) });
            if d.len() >= 4 {
                effect_from(u32::from_le_bytes([d[0], d[1], d[2], d[3]]))
            } else {
                Effect::Copy
            }
        } else {
            Effect::Copy
        };
        Some(ClipFiles { paths, effect })
    }

    fn write(&self, paths: &[PathBuf], effect: Effect) -> Result<(), String> {
        let open = Open::new(true).ok_or("剪贴板正被其他程序占用，请稍后再试")?;
        let hdrop = alloc_global(&build_hdrop(paths));
        let eff = alloc_global(&effect_value(effect).to_le_bytes());
        if hdrop.is_null() || eff.is_null() {
            return Err("内存不足，无法写入剪贴板".into());
        }
        // SAFETY: 剪贴板已打开且有属主；SetClipboardData 成功后内存归系统所有，失败时由我们释放。
        unsafe {
            EmptyClipboard();
            if SetClipboardData(CF_HDROP, hdrop).is_null() {
                GlobalFree(hdrop);
                GlobalFree(eff);
                return Err("写入剪贴板失败".into());
            }
            if SetClipboardData(drop_effect_format(), eff).is_null() {
                GlobalFree(eff);
            }
        }
        drop(open);
        Ok(())
    }

    fn clear(&self) {
        if let Some(_o) = Open::new(true) {
            // SAFETY: 剪贴板已打开且有属主。
            unsafe { EmptyClipboard() };
        }
    }

    fn sequence(&self) -> u32 {
        // SAFETY: 无参数查询。
        unsafe { GetClipboardSequenceNumber() }
    }
}

// ---------- 内存剪贴板（harness 与测试） ----------

/// 只存在于内存里的剪贴板，用于无头测试：不碰用户真实剪贴板。
#[derive(Default)]
pub struct MemClipboard {
    data: RefCell<Option<ClipFiles>>,
    seq: Cell<u32>,
}

impl MemClipboard {
    pub fn new() -> MemClipboard {
        MemClipboard::default()
    }

    /// 模拟别的程序往剪贴板放了文件。
    pub fn put_external(&self, paths: &[PathBuf], effect: Effect) {
        let _ = self.write(paths, effect);
    }

    /// 模拟别的程序放了文本，覆盖掉文件。
    pub fn put_text(&self) {
        *self.data.borrow_mut() = None;
        self.seq.set(self.seq.get() + 1);
    }
}

impl Clipboard for MemClipboard {
    fn read(&self) -> Option<ClipFiles> {
        self.data.borrow().clone()
    }
    fn write(&self, paths: &[PathBuf], effect: Effect) -> Result<(), String> {
        *self.data.borrow_mut() = Some(ClipFiles {
            paths: paths.to_vec(),
            effect,
        });
        self.seq.set(self.seq.get() + 1);
        Ok(())
    }
    fn clear(&self) {
        *self.data.borrow_mut() = None;
        self.seq.set(self.seq.get() + 1);
    }
    fn sequence(&self) -> u32 {
        self.seq.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdrop_roundtrip_with_chinese_and_spaces() {
        let paths = vec![
            PathBuf::from(r"C:\临时 目录\报告.txt"),
            PathBuf::from(r"D:\a\b c\文件夹"),
        ];
        let blob = build_hdrop(&paths);
        assert_eq!(u32::from_le_bytes(blob[0..4].try_into().unwrap()), 20);
        assert_eq!(&blob[blob.len() - 4..], &[0, 0, 0, 0], "双 NUL 结尾");
        assert_eq!(parse_hdrop(&blob), paths);
    }

    #[test]
    fn hdrop_garbage_is_empty() {
        assert!(parse_hdrop(&[]).is_empty());
        assert!(parse_hdrop(&[0; 10]).is_empty());
        let mut bad = build_hdrop(&[PathBuf::from("C:\\x")]);
        bad[0..4].copy_from_slice(&9999u32.to_le_bytes());
        assert!(parse_hdrop(&bad).is_empty());
    }

    #[test]
    fn effect_codes() {
        assert_eq!(effect_from(1), Effect::Copy);
        assert_eq!(effect_from(2), Effect::Move);
        assert_eq!(effect_from(5), Effect::Copy);
        assert_eq!(effect_from(2 | 4), Effect::Move);
    }

    #[test]
    fn mem_clipboard_tracks_sequence() {
        let c = MemClipboard::new();
        let s0 = c.sequence();
        c.write(&[PathBuf::from("C:\\a")], Effect::Move).unwrap();
        assert_ne!(c.sequence(), s0);
        assert_eq!(c.read().unwrap().effect, Effect::Move);
        c.put_text();
        assert!(c.read().is_none());
    }

    /// 真实系统剪贴板往返。会短暂改动用户剪贴板，所以保存并尽量恢复文本无法保证——默认忽略，需要时手动运行。
    #[test]
    #[ignore = "会改动真实系统剪贴板"]
    fn system_clipboard_roundtrip() {
        let c = SystemClipboard;
        let paths = vec![PathBuf::from(r"C:\Windows\notepad.exe")];
        let s0 = c.sequence();
        c.write(&paths, Effect::Move).unwrap();
        assert_ne!(c.sequence(), s0);
        let got = c.read().expect("应能读回");
        assert_eq!(got.paths, paths);
        assert_eq!(got.effect, Effect::Move);
        c.clear();
        assert!(c.read().is_none());
    }
}
