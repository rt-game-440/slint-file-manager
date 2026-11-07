//! Windows Shell 缩略图路线：`IShellItemImageFactory::GetImage`。
//!
//! 与资源管理器同源（同一套缩略图提取器与系统缩略图缓存 thumbcache）。能否出图取决于系统是否安装了对应格式的
//! 解码器（例如 WebP 需要“WebP 图像扩展”），所以结果随机器而异。

use crate::common::{Fail, Thumb};
use std::path::Path;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::*;
use windows::core::PCWSTR;

thread_local! {
    static COM: () = unsafe {
        // 多线程套间；线程退出时不显式 CoUninitialize（原型进程很快退出）
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// 失败时的 HRESULT 也有诊断价值（例如 WebP 解码器缺失），用 last_hresult 读取；只输出十六进制码，不含路径
thread_local! {
    static LAST: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

pub fn last_hresult() -> i32 {
    LAST.with(|l| l.get())
}

/// 取缩略图。`flags`：允许提取（不是只查缓存）、只要缩略图不要图标回退。
pub fn thumbnail(path: &Path, size: u32) -> Result<Thumb, Fail> {
    COM.with(|_| ());
    // SHCreateItemFromParsingName 不接受正斜杠（会返回 E_INVALIDARG），先规范成反斜杠
    let w = wide(&path.to_string_lossy().replace('/', "\\"));
    unsafe {
        let f: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None)
            .map_err(|e| {
                LAST.with(|l| l.set(e.code().0));
                Fail::Io
            })?;
        let hb = f
            .GetImage(
                SIZE {
                    cx: size as i32,
                    cy: size as i32,
                },
                SIIGBF_RESIZETOFIT | SIIGBF_THUMBNAILONLY,
            )
            .map_err(|e| {
                LAST.with(|l| l.set(e.code().0));
                Fail::Unsupported
            })?;
        let th = hbitmap_to_rgba(hb);
        let _ = DeleteObject(HGDIOBJ(hb.0));
        th.ok_or(Fail::Corrupt)
    }
}

unsafe fn hbitmap_to_rgba(hb: HBITMAP) -> Option<Thumb> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(
            HGDIOBJ(hb.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        ) == 0
        {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h; // 自上而下
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        let dc = GetDC(None);
        let n = GetDIBits(
            dc,
            hb,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, dc);
        if n == 0 {
            return None;
        }
        // BGRA（Shell 给的是预乘 alpha）→ RGBA 非预乘
        for p in buf.chunks_exact_mut(4) {
            let (b, g, r, a) = (p[0], p[1], p[2], p[3]);
            if a != 0 && a != 255 {
                let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                p[0] = un(r);
                p[1] = un(g);
                p[2] = un(b);
            } else {
                p[0] = r;
                p[1] = g;
                p[2] = b;
            }
        }
        Some(Thumb {
            w: w as u32,
            h: h as u32,
            rgba: buf,
        })
    }
}
