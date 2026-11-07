//! 系统缩略图回退（Issue #13）：本应用解不了的格式（MP4／MKV 等视频、TIFF、HEIC 等）通过 Windows Shell 的
//! `IShellItemImageFactory` 取缩略图，与资源管理器同源（同一套提取器与 thumbcache），能否出图取决于本机安装的解码器。
//!
//! - 云端占位文件只能带 `SIIGBF_INCACHEONLY`：只取系统缓存里已有的缩略图，不触发提取、不触发下载（ADR 0004、研究 0003）。
//!   拿不到就回退图标，不重试、不读取内容。
//! - 一律带 `SIIGBF_THUMBNAILONLY`：取不到缩略图就失败，由本应用显示自己的类型图标，不接受系统给的图标当缩略图。
//! - 系统侧提取较慢且串行（研究 0005：约 113 ms／张，多线程不增吞吐），所以调度上单独一条车道（见 [`super::lane`]）。
//! - [`ShellProvider`] 是接缝：产品用 [`SystemShell`]，测试与 harness 注入确定性的替身（可模拟慢、卡死、云端）。

use super::{Fail, THUMB_SIZE, Thumb};
use std::path::Path;

/// 缩略图来源路线。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    /// 本应用自己解码（WebP／JPEG／PNG／GIF／BMP）
    Native,
    /// Windows Shell
    Shell,
}

/// 交给 Shell 的视频扩展名。容器支持不等于任意编码支持：实际有没有缩略图取决于本机解码器，见 docs/thumbnails.md 的编码兼容表。
pub const SHELL_VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "wmv", "asf", "mpg", "mpeg", "ts", "m2ts", "mts",
    "flv", "3gp", "3g2", "ogv",
];

/// 交给 Shell 的图片扩展名（本应用不解码的格式）。RAW 需要系统安装对应编解码器，没有就回退图标。
pub const SHELL_IMAGE_EXTS: &[&str] = &[
    "tif", "tiff", "heic", "heif", "hif", "avif", "jxl", "dng", "cr2", "cr3", "nef", "arw", "orf",
    "rw2", "raf",
];

/// 按扩展名决定走哪条路线；`None` 表示不生成缩略图（显示类型图标）。
pub fn route(ext: &str) -> Option<Route> {
    if super::wants_thumbnail(ext) {
        Some(Route::Native)
    } else if wants_shell(ext) {
        Some(Route::Shell)
    } else {
        None
    }
}

pub fn wants_shell(ext: &str) -> bool {
    SHELL_VIDEO_EXTS.contains(&ext) || SHELL_IMAGE_EXTS.contains(&ext)
}

/// 某个路径的扩展名（小写）走 Shell 路线。
pub fn path_is_shell(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| wants_shell(&e.to_ascii_lowercase()))
}

/// 系统缩略图提供者。实现必须是线程安全的；调用可能很慢甚至卡死（调度器有硬超时与替换线程）。
pub trait ShellProvider: Send + Sync {
    /// 取缩略图（最长边不超过 [`THUMB_SIZE`]，非预乘 RGBA）。
    /// `cache_only` 为真时只允许读系统缓存，不得触发提取或下载（云端占位文件）。
    fn thumbnail(&self, path: &Path, cache_only: bool) -> Result<Thumb, Fail>;
}

/// 产品实现：`IShellItemImageFactory::GetImage`。
pub struct SystemShell;

/// 诊断信息：失败的 HRESULT（只输出十六进制码，不含路径）。
#[derive(Clone, Copy, Debug)]
pub struct ShellError {
    pub fail: Fail,
    pub hresult: i32,
}

impl ShellProvider for SystemShell {
    fn thumbnail(&self, path: &Path, cache_only: bool) -> Result<Thumb, Fail> {
        thumbnail_diag(path, cache_only).map_err(|e| e.fail)
    }
}

mod sys {
    use super::*;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC,
        GetDIBits, GetObjectW, HBITMAP, HGDIOBJ, ReleaseDC,
    };
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF, SIIGBF_INCACHEONLY,
        SIIGBF_RESIZETOFIT, SIIGBF_THUMBNAILONLY,
    };
    use windows::core::PCWSTR;

    thread_local! {
        static COM: () = {
            // SAFETY: 每个线程只初始化一次；线程结束时由系统回收，不显式反初始化。
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        };
    }

    // HRESULT 的常见值：STG_E_FILENOTFOUND（INCACHEONLY 未命中）、WTS_E_FAILEDEXTRACTION、WTS_E_EXTRACTIONPENDING
    const E_FILE_NOT_FOUND: i32 = 0x8007_0002_u32 as i32;
    const E_PATH_NOT_FOUND: i32 = 0x8007_0003_u32 as i32;
    const E_ACCESS_DENIED: i32 = 0x8007_0005_u32 as i32;

    fn classify(hr: i32) -> Fail {
        match hr {
            E_FILE_NOT_FOUND | E_PATH_NOT_FOUND | E_ACCESS_DENIED => Fail::Io,
            // 缓存未命中（STG_E_FILENOTFOUND）、提取失败（无解码器、文件损坏）、提取挂起：都是“没有缩略图”
            _ => Fail::Unsupported,
        }
    }

    pub fn thumbnail_diag(path: &Path, cache_only: bool) -> Result<Thumb, ShellError> {
        COM.with(|_| ());
        // SHCreateItemFromParsingName 不接受正斜杠，先规范成反斜杠
        let wide: Vec<u16> = path
            .to_string_lossy()
            .replace('/', "\\")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: wide 以 NUL 结尾并在调用期间存活；返回的 COM 对象与 HBITMAP 在本函数内释放。
        unsafe {
            let f: IShellItemImageFactory =
                SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).map_err(|e| {
                    ShellError {
                        fail: classify(e.code().0),
                        hresult: e.code().0,
                    }
                })?;
            let mut flags = SIIGBF_RESIZETOFIT.0 | SIIGBF_THUMBNAILONLY.0;
            if cache_only {
                flags |= SIIGBF_INCACHEONLY.0;
            }
            let hb = f
                .GetImage(
                    SIZE {
                        cx: THUMB_SIZE as i32,
                        cy: THUMB_SIZE as i32,
                    },
                    SIIGBF(flags),
                )
                .map_err(|e| ShellError {
                    fail: classify(e.code().0),
                    hresult: e.code().0,
                })?;
            let t = hbitmap_to_thumb(hb);
            let _ = DeleteObject(HGDIOBJ(hb.0));
            t.ok_or(ShellError {
                fail: Fail::Corrupt,
                hresult: 0,
            })
        }
    }

    /// HBITMAP（32 位）→ 非预乘 RGBA。
    ///
    /// # Safety
    /// `hb` 必须是有效的位图句柄。
    unsafe fn hbitmap_to_thumb(hb: HBITMAP) -> Option<Thumb> {
        // SAFETY: 句柄有效；缓冲长度等于 宽×高×4，GetDIBits 只写入这么多。
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
            let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
            if w <= 0 || h <= 0 || w > 4096 || h > 4096 {
                return None;
            }
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
            Some(super::bgra_to_thumb(w as u32, h as u32, buf))
        }
    }
}

/// 取缩略图并返回失败的 HRESULT（诊断用）。
pub fn thumbnail_diag(path: &Path, cache_only: bool) -> Result<Thumb, ShellError> {
    sys::thumbnail_diag(path, cache_only).map(fit_thumb)
}

/// BGRA（Shell 给的是预乘 alpha）→ 非预乘 RGBA。所有像素 alpha 都是 0 的位图按不透明处理
/// （部分提取器返回 BGRX，alpha 通道没有填）。
pub fn bgra_to_thumb(w: u32, h: u32, mut buf: Vec<u8>) -> Thumb {
    let all_zero = buf.as_chunks::<4>().0.iter().all(|p| p[3] == 0);
    for p in buf.as_chunks_mut::<4>().0 {
        let (b, g, r) = (p[0], p[1], p[2]);
        let a = if all_zero { 255 } else { p[3] };
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
        p[3] = a;
    }
    Thumb { w, h, rgba: buf }
}

/// 最长边超过 [`THUMB_SIZE`] 时缩小（Shell 偶尔返回比请求大的图），保证能写入磁盘缓存。
pub fn fit_thumb(t: Thumb) -> Thumb {
    if t.w <= THUMB_SIZE && t.h <= THUMB_SIZE {
        return t;
    }
    use fast_image_resize as fr;
    let (tw, th) = super::webp::fit(t.w, t.h, THUMB_SIZE);
    let Ok(src) = fr::images::ImageRef::new(t.w, t.h, &t.rgba, fr::PixelType::U8x4) else {
        return t;
    };
    let mut dst = fr::images::Image::new(tw, th, fr::PixelType::U8x4);
    let opts = fr::ResizeOptions::new()
        .resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Bilinear))
        .use_alpha(true);
    if fr::Resizer::new().resize(&src, &mut dst, &opts).is_err() {
        return t;
    }
    Thumb {
        w: tw,
        h: th,
        rgba: dst.into_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        assert_eq!(route("jpg"), Some(Route::Native));
        assert_eq!(route("webp"), Some(Route::Native));
        assert_eq!(route("mp4"), Some(Route::Shell));
        assert_eq!(route("mkv"), Some(Route::Shell));
        assert_eq!(route("tiff"), Some(Route::Shell));
        assert_eq!(route("heic"), Some(Route::Shell));
        for e in ["", "txt", "pdf", "docx", "exe", "mp3"] {
            assert_eq!(route(e), None, "{e}");
        }
        assert!(path_is_shell(Path::new(r"C:\x\a.MP4")));
        assert!(!path_is_shell(Path::new(r"C:\x\a.png")));
    }

    #[test]
    fn bgra_unpremultiplies_and_fixes_missing_alpha() {
        // 半透明红：预乘后 (b,g,r,a) = (0,0,128,128)
        let t = bgra_to_thumb(1, 1, vec![0, 0, 128, 128]);
        assert_eq!(t.rgba, vec![255, 0, 0, 128]);
        // 全部 alpha 为 0：按不透明处理，颜色保留
        let t = bgra_to_thumb(2, 1, vec![10, 20, 30, 0, 40, 50, 60, 0]);
        assert_eq!(t.rgba, vec![30, 20, 10, 255, 60, 50, 40, 255]);
        // 局部透明（有不为 0 的 alpha）：alpha 为 0 的像素保持透明
        let t = bgra_to_thumb(2, 1, vec![1, 2, 3, 0, 4, 5, 6, 255]);
        assert_eq!(t.rgba[3], 0);
        assert_eq!(&t.rgba[4..], &[6, 5, 4, 255]);
    }

    #[test]
    fn oversized_shell_images_are_shrunk() {
        let t = Thumb {
            w: 512,
            h: 288,
            rgba: vec![200; 512 * 288 * 4],
        };
        let s = fit_thumb(t);
        assert_eq!((s.w, s.h), (256, 144));
        let small = fit_thumb(Thumb {
            w: 10,
            h: 10,
            rgba: vec![0; 400],
        });
        assert_eq!((small.w, small.h), (10, 10));
    }
}
