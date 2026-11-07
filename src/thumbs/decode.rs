//! 生成一张缩略图：读属性与体积检查 → 读文件 → 按内容识别格式 → 读头与像素上限检查 → 解码（缩放）→ 方向校正。
//!
//! 内存保护（研究 0005 第 6.3 节）：
//! - 有损不透明 WebP：libwebp 解码时缩放，峰值内存与原图像素基本无关，不设像素上限（WebP 自身上限 16383²）。
//! - 无损／带透明 WebP、JPEG／PNG／GIF／BMP：需要整幅缓冲，超过 [`Limits::max_full_pixels`] 不解码（`too_large`）；
//!   超过 [`Limits::heavy_pixels`] 的大图同一时刻只解一张（[`HEAVY_SLOTS`]），避免多个线程同时分配数百 MiB。
//! - 文件体积超过 [`Limits::max_file_bytes`] 不读取。
//!
//! GIF 的代表帧：第一帧（不播放动画，不挑“最有代表性”的帧）。动图 WebP 暂不支持，显示类型图标。
//! JPEG 按 EXIF 方向旋转／翻转后再显示；PNG／GIF／WebP 的透明区域保留 alpha。

use super::webp;
use super::{Fail, Source, THUMB_SIZE, Thumb, mtime_ns};
use std::io::Cursor;
use std::sync::{Condvar, Mutex};

/// 同时进行的大图整幅解码数。
pub const HEAVY_SLOTS: usize = 1;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// 文件体积上限（字节），超过不读取
    pub max_file_bytes: u64,
    /// 需要整幅缓冲的路线的像素上限（宽×高）
    pub max_full_pixels: u64,
    /// 超过该像素数的整幅解码视为大图，受 [`HEAVY_SLOTS`] 限制并发
    pub heavy_pixels: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_file_bytes: 256 * 1024 * 1024,
            // 约 5～7 MiB／百万像素（研究 0005 实测）→ 单张最坏约 250～350 MiB，同一时刻只有一张
            max_full_pixels: 50_000_000,
            heavy_pixels: 16_000_000,
        }
    }
}

/// 生成结果。`identity_ok` 为 false 表示文件在目录枚举之后被改过（大小或修改时间不同），
/// 这时缩略图照常显示，但不写入磁盘缓存（缓存键对应的是旧身份）。
pub struct Generated {
    pub thumb: Thumb,
    pub identity_ok: bool,
}

/// 带云端占位属性（脱机／打开时召回／访问数据时召回）：读取内容会触发同步客户端下载。
pub fn is_cloud_placeholder(attrs: u32) -> bool {
    crate::cloud::is_recall(attrs)
}

/// 读取并生成缩略图。
pub fn generate(src: &Source, lim: &Limits) -> Result<Generated, Fail> {
    // 云端占位：读取内容会触发同步客户端下载原文件（ADR 0004），本应用不读。
    // 守卫先查属性再打开、打开后在句柄上再查一次（文件在目录枚举之后被“释放空间”的竞态）。
    let (mut file, meta) = crate::cloud::open_local_only(&src.path).map_err(|g| match g {
        crate::cloud::Guard::Cloud => Fail::Cloud,
        crate::cloud::Guard::NotFile => Fail::Unsupported,
        crate::cloud::Guard::Io => Fail::Io,
    })?;
    if meta.len() > lim.max_file_bytes {
        return Err(Fail::TooLarge);
    }
    let identity_ok = meta.len() == src.size && mtime_ns(meta.modified().ok()) == src.mtime_ns;
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    std::io::Read::read_to_end(&mut file, &mut bytes).map_err(|_| Fail::Io)?;
    let thumb = from_bytes(&bytes, lim)?;
    Ok(Generated { thumb, identity_ok })
}

/// 从内存中的文件内容生成缩略图（按内容识别格式）。
pub fn from_bytes(bytes: &[u8], lim: &Limits) -> Result<Thumb, Fail> {
    if bytes.is_empty() {
        return Err(Fail::Corrupt);
    }
    if webp::is_webp(bytes) {
        return from_webp(bytes, lim);
    }
    from_image(bytes, lim)
}

fn from_webp(bytes: &[u8], lim: &Limits) -> Result<Thumb, Fail> {
    let f = webp::features(bytes)?;
    if f.animated {
        return Err(Fail::Unsupported);
    }
    if f.w == 0 || f.h == 0 {
        return Err(Fail::Corrupt);
    }
    let px = f.w as u64 * f.h as u64;
    let full_buffer = !f.lossy || f.has_alpha;
    if full_buffer && px > lim.max_full_pixels {
        return Err(Fail::TooLarge);
    }
    let _g = (full_buffer && px > lim.heavy_pixels).then(heavy);
    let (tw, th) = webp::fit(f.w, f.h, THUMB_SIZE);
    webp::decode(bytes, &f, tw, th)
}

fn from_image(bytes: &[u8], lim: &Limits) -> Result<Thumb, Fail> {
    use image::{ImageDecoder, ImageFormat};
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| Fail::Io)?;
    match reader.format() {
        Some(ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::Gif | ImageFormat::Bmp) => {}
        _ => return Err(Fail::Unsupported),
    }
    let mut il = image::Limits::default();
    il.max_alloc = Some(lim.max_full_pixels * 8);
    reader.limits(il);
    let mut dec = reader.into_decoder().map_err(map_err)?;
    let (w, h) = dec.dimensions();
    if w == 0 || h == 0 {
        return Err(Fail::Corrupt);
    }
    let px = w as u64 * h as u64;
    if px > lim.max_full_pixels {
        return Err(Fail::TooLarge);
    }
    let orientation = dec
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let _g = (px > lim.heavy_pixels).then(heavy);
    let img = image::DynamicImage::from_decoder(dec).map_err(map_err)?;
    let (tw, th) = webp::fit(w, h, THUMB_SIZE);
    let t = resize(img, tw, th)?;
    Ok(orient(t, orientation))
}

fn map_err(e: image::ImageError) -> Fail {
    match e {
        image::ImageError::Unsupported(_) => Fail::Unsupported,
        image::ImageError::Limits(_) => Fail::TooLarge,
        _ => Fail::Corrupt,
    }
}

/// 缩放到目标尺寸并转成 RGBA。RGB 源直接按 3 通道缩放，避免整幅转 RGBA 的额外一份内存。
fn resize(img: image::DynamicImage, tw: u32, th: u32) -> Result<Thumb, Fail> {
    use fast_image_resize as fr;
    use image::DynamicImage as D;
    let (sw, sh) = (img.width(), img.height());
    let alpha = img.color().has_alpha();
    let (buf, pt) = match img {
        D::ImageRgb8(b) => (b.into_raw(), fr::PixelType::U8x3),
        D::ImageRgba8(b) => (b.into_raw(), fr::PixelType::U8x4),
        other if alpha => (other.into_rgba8().into_raw(), fr::PixelType::U8x4),
        other => (other.into_rgb8().into_raw(), fr::PixelType::U8x3),
    };
    let out = if (sw, sh) == (tw, th) {
        buf
    } else {
        let src = fr::images::ImageRef::new(sw, sh, &buf, pt).map_err(|_| Fail::Corrupt)?;
        let mut dst = fr::images::Image::new(tw, th, pt);
        let opts = fr::ResizeOptions::new()
            .resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Bilinear))
            .use_alpha(alpha);
        fr::Resizer::new()
            .resize(&src, &mut dst, &opts)
            .map_err(|_| Fail::Corrupt)?;
        dst.into_vec()
    };
    let rgba = if pt == fr::PixelType::U8x3 {
        let mut v = Vec::with_capacity(tw as usize * th as usize * 4);
        for p in out.as_chunks::<3>().0 {
            v.extend_from_slice(&[p[0], p[1], p[2], 255]);
        }
        v
    } else {
        out
    };
    Ok(Thumb { w: tw, h: th, rgba })
}

/// 按 EXIF 方向校正（在缩略图上做，代价很小）。
fn orient(t: Thumb, o: image::metadata::Orientation) -> Thumb {
    if o == image::metadata::Orientation::NoTransforms {
        return t;
    }
    let Some(buf) = image::RgbaImage::from_raw(t.w, t.h, t.rgba.clone()) else {
        return t;
    };
    let mut img = image::DynamicImage::ImageRgba8(buf);
    img.apply_orientation(o);
    let b = img.into_rgba8();
    Thumb {
        w: b.width(),
        h: b.height(),
        rgba: b.into_raw(),
    }
}

static HEAVY: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

/// 大图解码许可（Drop 时归还）。
pub struct HeavyGuard(());

fn heavy() -> HeavyGuard {
    let (m, cv) = &HEAVY;
    let mut n = m.lock().unwrap_or_else(|e| e.into_inner());
    while *n >= HEAVY_SLOTS {
        n = cv.wait(n).unwrap_or_else(|e| e.into_inner());
    }
    *n += 1;
    HeavyGuard(())
}

impl Drop for HeavyGuard {
    fn drop(&mut self) {
        let (m, cv) = &HEAVY;
        let mut n = m.lock().unwrap_or_else(|e| e.into_inner());
        *n -= 1;
        cv.notify_one();
    }
}

/// 确定性的样例图像（单元测试与 harness 夹具共用）：内容可辨认、可核对方向与透明区域。
pub mod samples {
    use super::super::{Thumb, webp};
    use image::ImageEncoder;

    /// 渐变底色 + 左上角色块（用来核对方向）：宽 `w`、高 `h`。左上 1/4 区域为纯红。
    pub fn pattern(w: u32, h: u32, seed: u8) -> Thumb {
        let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
        for y in 0..h {
            for x in 0..w {
                if x < w / 4 && y < h / 4 {
                    rgba.extend_from_slice(&[230, 30, 30, 255]);
                } else {
                    let r = (x * 255 / w.max(1)) as u8;
                    let g = (y * 255 / h.max(1)) as u8;
                    rgba.extend_from_slice(&[r / 2 + 40, g / 2 + 60, seed, 255]);
                }
            }
        }
        Thumb { w, h, rgba }
    }

    /// 中间是透明圆、四周不透明的图（核对透明区域）。
    pub fn with_hole(w: u32, h: u32) -> Thumb {
        let mut t = pattern(w, h, 200);
        let (cx, cy, r) = (w as f32 / 2.0, h as f32 / 2.0, w.min(h) as f32 / 3.0);
        for y in 0..h {
            for x in 0..w {
                let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
                if d < r {
                    t.rgba[((y * w + x) * 4 + 3) as usize] = 0;
                }
            }
        }
        t
    }

    pub fn webp_lossy(t: &Thumb) -> Vec<u8> {
        webp::encode(t, 85.0).expect("WebP 编码")
    }

    pub fn webp_lossless(t: &Thumb) -> Vec<u8> {
        webp::encode_lossless(t).expect("WebP 无损编码")
    }

    pub fn png(t: &Thumb) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(&t.rgba, t.w, t.h, image::ExtendedColorType::Rgba8)
            .expect("PNG 编码");
        out
    }

    pub fn bmp(t: &Thumb) -> Vec<u8> {
        let rgb: Vec<u8> = t
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        let mut out = Vec::new();
        image::codecs::bmp::BmpEncoder::new(&mut out)
            .write_image(&rgb, t.w, t.h, image::ExtendedColorType::Rgb8)
            .expect("BMP 编码");
        out
    }

    /// JPEG；`orientation` 为 EXIF 方向值（1～8），1 表示不写 EXIF。
    pub fn jpeg(t: &Thumb, orientation: u16) -> Vec<u8> {
        let rgb: Vec<u8> = t
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect();
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
            .write_image(&rgb, t.w, t.h, image::ExtendedColorType::Rgb8)
            .expect("JPEG 编码");
        if orientation <= 1 {
            return out;
        }
        // 在 SOI 之后插入只含方向标签的 APP1/EXIF 段（小端 TIFF，IFD0 一个条目 0x0112）
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II*\0");
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0u32.to_le_bytes());
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);
        let len = (app1.len() + 2) as u16;
        let mut v = out[..2].to_vec();
        v.extend_from_slice(&[0xFF, 0xE1]);
        v.extend_from_slice(&len.to_be_bytes());
        v.extend_from_slice(&app1);
        v.extend_from_slice(&out[2..]);
        v
    }

    /// 两帧 GIF：第一帧 `first`，第二帧纯蓝（核对“取第一帧”）。
    pub fn gif2(first: &Thumb) -> Vec<u8> {
        use image::codecs::gif::GifEncoder;
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut out);
            let f1 = image::RgbaImage::from_raw(first.w, first.h, first.rgba.clone()).unwrap();
            let f2 =
                image::RgbaImage::from_pixel(first.w, first.h, image::Rgba([20, 40, 230, 255]));
            enc.encode_frames([image::Frame::new(f1), image::Frame::new(f2)])
                .expect("GIF 编码");
        }
        out
    }

    /// 只有文件头声明超大尺寸的 PNG（不含像素数据），用于核对“超限不解码”。
    pub fn png_huge_header(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(&ihdr);
        v.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        // 一个很短的 IDAT 与 IEND，让解码器能读完文件头
        for (name, data) in [
            (&b"IDAT"[..], &[0x78u8, 0x9c, 0x03, 0x00][..]),
            (b"IEND", &[]),
        ] {
            let mut c = name.to_vec();
            c.extend_from_slice(data);
            v.extend_from_slice(&(data.len() as u32).to_be_bytes());
            v.extend_from_slice(&c);
            v.extend_from_slice(&crc32(&c).to_be_bytes());
        }
        v
    }

    fn crc32(b: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &x in b {
            c ^= x as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }
}

#[cfg(test)]
mod tests {
    use super::samples::*;
    use super::*;

    fn px(t: &Thumb, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * t.w + x) * 4) as usize;
        [t.rgba[i], t.rgba[i + 1], t.rgba[i + 2], t.rgba[i + 3]]
    }

    fn reddish(p: [u8; 4]) -> bool {
        p[0] > 180 && p[1] < 90 && p[2] < 90
    }

    #[test]
    fn webp_scaled_keeps_aspect() {
        let src = pattern(1200, 800, 90);
        let t = from_bytes(&webp_lossy(&src), &Limits::default()).unwrap();
        assert_eq!((t.w, t.h), (256, 171));
        assert!(reddish(px(&t, 10, 10)), "左上角应为红色块");
        assert!(!reddish(px(&t, 250, 160)));
    }

    #[test]
    fn small_images_are_not_upscaled() {
        let t = from_bytes(&png(&pattern(40, 30, 1)), &Limits::default()).unwrap();
        assert_eq!((t.w, t.h), (40, 30));
    }

    #[test]
    fn png_alpha_preserved() {
        let t = from_bytes(&png(&with_hole(600, 400)), &Limits::default()).unwrap();
        assert_eq!(px(&t, t.w / 2, t.h / 2)[3], 0, "中心透明");
        assert_eq!(px(&t, 2, 2)[3], 255, "角落不透明");
        let w = from_bytes(&webp_lossless(&with_hole(600, 400)), &Limits::default()).unwrap();
        assert_eq!(px(&w, w.w / 2, w.h / 2)[3], 0, "WebP 透明保留");
    }

    #[test]
    fn jpeg_exif_orientation_applied() {
        // 方向 6：显示时顺时针旋转 90°，原图左上角的红块应出现在右上角，宽高互换
        let src = pattern(800, 400, 10);
        let t = from_bytes(&jpeg(&src, 6), &Limits::default()).unwrap();
        assert_eq!((t.w, t.h), (128, 256));
        assert!(reddish(px(&t, t.w - 6, 6)), "红块应在右上角");
        assert!(!reddish(px(&t, 6, 6)));
        let plain = from_bytes(&jpeg(&src, 1), &Limits::default()).unwrap();
        assert_eq!((plain.w, plain.h), (256, 128));
        assert!(reddish(px(&plain, 6, 6)));
    }

    #[test]
    fn gif_uses_first_frame_and_bmp_decodes() {
        let g = from_bytes(&gif2(&pattern(300, 200, 50)), &Limits::default()).unwrap();
        assert!(reddish(px(&g, 5, 5)), "GIF 取第一帧（第二帧是纯蓝）");
        let b = from_bytes(&bmp(&pattern(300, 200, 50)), &Limits::default()).unwrap();
        assert_eq!((b.w, b.h), (256, 171));
        assert!(reddish(px(&b, 5, 5)));
    }

    #[test]
    fn content_not_extension_decides() {
        // PNG 内容（无论扩展名）按 PNG 解码；文本内容无法识别
        assert!(from_bytes(&png(&pattern(64, 64, 3)), &Limits::default()).is_ok());
        assert_eq!(
            from_bytes(b"hello, not an image", &Limits::default()),
            Err(Fail::Unsupported)
        );
        assert_eq!(from_bytes(b"", &Limits::default()), Err(Fail::Corrupt));
    }

    #[test]
    fn corrupt_and_truncated_fail_cleanly() {
        let full = webp_lossy(&pattern(800, 600, 7));
        let cut = &full[..full.len() / 3];
        assert!(matches!(
            from_bytes(cut, &Limits::default()),
            Err(Fail::Corrupt)
        ));
        let mut junk = b"RIFF\x10\0\0\0WEBPVP8 ".to_vec();
        junk.extend_from_slice(&[0xAB; 40]);
        assert!(from_bytes(&junk, &Limits::default()).is_err());
        let j = jpeg(&pattern(400, 300, 9), 1);
        assert!(
            from_bytes(&j[..60], &Limits::default()).is_err(),
            "只有文件头"
        );
        // 截断到 1/4：解码器可能补灰后返回部分图像（与系统行为相近），只要求不崩溃、尺寸正确
        if let Ok(t) = from_bytes(&j[..j.len() / 4], &Limits::default()) {
            assert_eq!((t.w, t.h), (256, 192));
        }
    }

    #[test]
    fn huge_header_rejected_without_decoding() {
        let t = std::time::Instant::now();
        let r = from_bytes(&png_huge_header(40_000, 40_000), &Limits::default());
        assert_eq!(r, Err(Fail::TooLarge));
        assert!(t.elapsed().as_millis() < 500, "只读头即拒绝");
    }

    #[test]
    fn generate_reports_io_and_identity() {
        let dir = std::env::temp_dir().join(format!("sfm-thumb-decode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("图.webp");
        std::fs::write(&p, webp_lossy(&pattern(500, 300, 1))).unwrap();
        let m = std::fs::metadata(&p).unwrap();
        let src = Source::new(p.clone(), m.len(), m.modified().ok());
        let g = generate(&src, &Limits::default()).unwrap();
        assert!(g.identity_ok);
        let stale = Source::new(p.clone(), m.len() + 1, m.modified().ok());
        assert!(!generate(&stale, &Limits::default()).unwrap().identity_ok);
        let missing = Source::new(dir.join("没有.webp"), 1, None);
        assert_eq!(generate(&missing, &Limits::default()).err(), Some(Fail::Io));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
