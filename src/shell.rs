//! 与 Windows 外壳的少量交互：交给默认应用打开、读取系统文字缩放、用户主目录。

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// 用默认应用打开文件（等同资源管理器双击）。会阻塞到外壳完成分派，应在工作线程调用。
/// 没有关联应用时由系统显示“选择打开方式”。失败返回系统错误说明。
pub fn open_with_default(path: &Path) -> Result<(), String> {
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
    };
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let file = wide(path.as_os_str());
    let dir = path.parent().map(|p| wide(p.as_os_str()));
    // SAFETY: 按 ShellExecuteExW 文档初始化结构体；字符串缓冲区在调用期间有效。
    unsafe {
        // 外壳扩展可能依赖 COM；同一线程重复初始化返回 S_FALSE，不影响结果。
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        );
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOASYNC;
        info.lpFile = file.as_ptr();
        info.lpDirectory = dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
        info.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut info) != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().to_string())
        }
    }
}

/// 系统“文字大小”设置（设置 → 辅助功能 → 文字大小），返回倍数 1.0–2.25；读取失败为 1.0。
pub fn text_scale_factor() -> f32 {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let key = wide("SOFTWARE\\Microsoft\\Accessibility".as_ref());
    let name = wide("TextScaleFactor".as_ref());
    let mut v: u32 = 0;
    let mut len = std::mem::size_of::<u32>() as u32;
    // SAFETY: 输出缓冲区为 4 字节 DWORD，长度如实传入。
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut v as *mut u32).cast(),
            &mut len,
        )
    };
    if r == 0 && (100..=225).contains(&v) {
        v as f32 / 100.0
    } else {
        1.0
    }
}

/// 用户主目录（`%USERPROFILE%`）。
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}
