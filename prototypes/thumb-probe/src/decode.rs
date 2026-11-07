//! 生成缩略图的各条路线。每条路线：读文件 → 读头与限额检查 → 解码 → 缩放，并分段计时。
//!
//! - `libwebp-scaled`：WebP 用 libwebp 解码时直接缩放（不分配整幅 RGBA）。
//! - `libwebp-fast`：同上，并跳过环路滤波与精细上采样。
//! - `libwebp-full`：libwebp 整幅解码为 RGBA，再用 fast_image_resize 缩放（对照“解码时缩放”的收益）。
//! - `rust-image`：纯 Rust（image-webp / zune-jpeg / png 等，经 image crate）整幅解码，再缩放。
//! - `shell`：Windows Shell `IShellItemImageFactory`（见 shell.rs）。
//! - `auto`：WebP 走 `libwebp-fast`，其余格式按内容走 `rust-image`（按内容而非扩展名判断）。

use crate::common::{Fail, Limits, SrcInfo, Thumb, Timing};
use crate::webp::{self, DecodeOpts};
use std::io::Cursor;
use std::path::Path;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    LibwebpScaled,
    LibwebpFast,
    LibwebpFull,
    RustImage,
    Shell,
    Auto,
}

impl Route {
    pub fn parse(s: &str) -> Option<Route> {
        Some(match s {
            "libwebp-scaled" => Route::LibwebpScaled,
            "libwebp-fast" => Route::LibwebpFast,
            "libwebp-full" => Route::LibwebpFull,
            "rust-image" => Route::RustImage,
            "shell" => Route::Shell,
            "auto" => Route::Auto,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Route::LibwebpScaled => "libwebp-scaled",
            Route::LibwebpFast => "libwebp-fast",
            Route::LibwebpFull => "libwebp-full",
            Route::RustImage => "rust-image",
            Route::Shell => "shell",
            Route::Auto => "auto",
        }
    }
}

fn us(t: Instant) -> u64 {
    t.elapsed().as_micros() as u64
}

/// 读取并生成缩略图。返回的耗时不含调用方的排队时间。
pub fn generate(
    route: Route,
    path: &Path,
    size: u32,
    lim: &Limits,
) -> Result<(Thumb, Timing, SrcInfo), Fail> {
    let mut tm = Timing::default();

    if route == Route::Shell {
        let t = Instant::now();
        let th = crate::shell::thumbnail(path, size)?;
        tm.decode_us = us(t);
        return Ok((th, tm, SrcInfo::default()));
    }

    // 读文件：先看体积，超限不读
    let t = Instant::now();
    let meta = std::fs::metadata(path).map_err(|_| Fail::Io)?;
    if meta.len() > lim.max_file_bytes {
        return Err(Fail::TooLarge);
    }
    let bytes = std::fs::read(path).map_err(|_| Fail::Io)?;
    tm.read_us = us(t);
    let mut info = SrcInfo {
        bytes: bytes.len() as u64,
        ..Default::default()
    };
    if bytes.is_empty() {
        return Err(Fail::Corrupt);
    }

    let is_webp = webp::is_webp(&bytes);
    let route = match route {
        Route::Auto if is_webp => Route::LibwebpFast,
        Route::Auto => Route::RustImage,
        r => r,
    };

    let th = match route {
        Route::LibwebpScaled | Route::LibwebpFast | Route::LibwebpFull => {
            if !is_webp {
                return Err(Fail::Unsupported);
            }
            let f = webp::features(&bytes)?;
            info.w = f.w;
            info.h = f.h;
            if f.animated {
                return Err(Fail::Unsupported);
            }
            if f.w as u64 * f.h as u64 > lim.max_pixels {
                return Err(Fail::TooLarge);
            }
            let (tw, th_) = webp::fit(f.w, f.h, size);
            if route == Route::LibwebpFull {
                let t = Instant::now();
                let full = webp::decode(&bytes, f.w, f.h, DecodeOpts::default())?;
                tm.decode_us = us(t);
                let t = Instant::now();
                let out = resize(full, tw, th_, f.has_alpha)?;
                tm.scale_us = us(t);
                out
            } else {
                let t = Instant::now();
                let o = DecodeOpts {
                    scaled: (tw != f.w || th_ != f.h).then_some((tw, th_)),
                    fast: route == Route::LibwebpFast,
                };
                let out = webp::decode(&bytes, tw, th_, o)?;
                tm.decode_us = us(t);
                out
            }
        }
        Route::RustImage => {
            let reader = || {
                image::ImageReader::new(Cursor::new(&bytes))
                    .with_guessed_format()
                    .map_err(|_| Fail::Io)
            };
            let (iw, ih) = reader()?.into_dimensions().map_err(map_img_err)?;
            info.w = iw;
            info.h = ih;
            if iw as u64 * ih as u64 > lim.max_pixels {
                return Err(Fail::TooLarge);
            }
            let t = Instant::now();
            let img = reader()?.decode().map_err(map_img_err)?;
            let has_alpha = img.color().has_alpha();
            let rgba = img.into_rgba8();
            let full = Thumb {
                w: rgba.width(),
                h: rgba.height(),
                rgba: rgba.into_raw(),
            };
            tm.decode_us = us(t);
            let (tw, th_) = webp::fit(iw, ih, size);
            let t = Instant::now();
            let out = resize(full, tw, th_, has_alpha)?;
            tm.scale_us = us(t);
            out
        }
        Route::Shell | Route::Auto => unreachable!(),
    };
    Ok((th, tm, info))
}

fn map_img_err(e: image::ImageError) -> Fail {
    match e {
        image::ImageError::Unsupported(_) => Fail::Unsupported,
        image::ImageError::Limits(_) => Fail::TooLarge,
        image::ImageError::IoError(_) => Fail::Corrupt, // 内存里读到 EOF：内容被截断
        _ => Fail::Corrupt,
    }
}

/// SIMD 缩放（卷积，双线性核；缩小倍数大时核随之放大，等价于区域平均）。
pub fn resize(src: Thumb, tw: u32, th: u32, has_alpha: bool) -> Result<Thumb, Fail> {
    use fast_image_resize as fr;
    if src.w == tw && src.h == th {
        return Ok(src);
    }
    let (sw, sh) = (src.w, src.h);
    let s = fr::images::Image::from_vec_u8(sw, sh, src.rgba, fr::PixelType::U8x4)
        .map_err(|_| Fail::Corrupt)?;
    let mut d = fr::images::Image::new(tw, th, fr::PixelType::U8x4);
    let opts = fr::ResizeOptions::new()
        .resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Bilinear))
        .use_alpha(has_alpha);
    fr::Resizer::new()
        .resize(&s, &mut d, &opts)
        .map_err(|_| Fail::Corrupt)?;
    Ok(Thumb {
        w: tw,
        h: th,
        rgba: d.into_vec(),
    })
}
