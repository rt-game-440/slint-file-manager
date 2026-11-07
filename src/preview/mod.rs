//! 图片大图预览的解码与加载（Issue #13）。与界面无关，界面接线在 `crate::app::preview`。
//!
//! - **格式**：与缩略图的原生路线一致——WebP（静态）、JPEG、PNG、GIF、BMP，按内容识别（文件头），不看扩展名。
//!   TIFF／HEIC／视频等只有系统缩略图的类型不在应用内预览（见 docs/preview.md）。
//! - **原尺寸解码**：预览要能 1:1 查看，所以不缩小；JPEG 按 EXIF 方向校正。受 [`Limits`] 约束：
//!   文件体积、总像素、单边像素（避免超过 GPU 纹理上限）；超限给出明确的 [`Failure::TooLarge`]，不尝试解码。
//! - **云端占位文件**（脱机／访问时召回）不读取内容，避免隐式下载：[`Failure::Cloud`]。
//! - **GIF**：解码为多帧并带帧延迟，是否播放由界面决定（默认值待用户确认，见 docs/preview.md）；
//!   解码后体积或帧数超过上限时只保留第一帧并附说明（[`Decoded::note`]），不静默截断动画。
//!   APNG 与动图 WebP 不播放（APNG 显示默认图，动图 WebP 判不支持）。
//! - **加载器**（[`loader::Loader`]）：单个后台线程、只处理“最新的一个请求”，快速切换时中间的请求被跳过，
//!   同一时刻至多一次解码在进行，资源占用有界。

pub mod loader;

use crate::thumbs::decode::is_cloud_placeholder;
use crate::thumbs::webp;
use std::io::Cursor;
use std::os::windows::fs::MetadataExt;
use std::path::Path;

/// 预览资源上限。
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// 文件体积上限（字节），超过不读取
    pub max_file_bytes: u64,
    /// 总像素上限（宽×高）。RGBA 约 4 字节／像素：5 千万像素约 200 MiB
    pub max_pixels: u64,
    /// 单边像素上限（常见 GPU 纹理上限 16384）
    pub max_side: u32,
    /// 动图解码后总字节数上限，超过只显示第一帧
    pub max_anim_bytes: u64,
    /// 动图帧数上限，超过只显示第一帧
    pub max_frames: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_file_bytes: 256 * 1024 * 1024,
            max_pixels: 50_000_000,
            max_side: 16_384,
            max_anim_bytes: 192 * 1024 * 1024,
            max_frames: 1000,
        }
    }
}

/// 一帧：RGBA8（非预乘）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// 解码结果。`frames` 至少一帧；多帧时 `delays_ms` 与之等长（毫秒，已按浏览器惯例修正过短的延迟）。
#[derive(Clone, Debug)]
pub struct Decoded {
    pub frames: Vec<Frame>,
    pub delays_ms: Vec<u32>,
    /// 需要告诉用户的说明（例如动图过大只显示第一帧）
    pub note: Option<String>,
}

impl Decoded {
    pub fn width(&self) -> u32 {
        self.frames[0].w
    }
    pub fn height(&self) -> u32 {
        self.frames[0].h
    }
    pub fn animated(&self) -> bool {
        self.frames.len() > 1
    }
    fn still(f: Frame) -> Decoded {
        Decoded {
            frames: vec![f],
            delays_ms: Vec::new(),
            note: None,
        }
    }
}

/// 预览失败的类别：界面按类别给出可理解的说明，都不弹对话框、不阻塞切换。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// 文件不存在或不再可读（已被删除、移走、无权限）
    Missing,
    /// 云端占位文件，读取会触发下载，不读取
    Cloud,
    /// 内容损坏或被截断
    Corrupt,
    /// 超过资源上限，未解码
    TooLarge { w: u32, h: u32, bytes: u64 },
    /// 本应用解不了的格式（动图 WebP、改了扩展名的其他类型……）
    Unsupported,
}

impl Failure {
    /// 面向用户的说明（简体中文）。
    pub fn message(&self) -> String {
        match self {
            Failure::Missing => "文件已不存在或无法读取（可能已被删除、移动，或没有权限）".into(),
            Failure::Cloud => {
                "这是尚未下载到本机的云端文件，预览不会触发下载。用默认应用打开会开始下载。".into()
            }
            Failure::Corrupt => "图片内容已损坏或不完整，无法显示".into(),
            Failure::TooLarge { w, h, bytes } => {
                if *w > 0 {
                    format!(
                        "图片过大（{w} × {h} 像素），超出预览的内存上限，没有加载。可用默认应用打开。"
                    )
                } else {
                    format!(
                        "文件过大（{}），超出预览上限，没有加载。可用默认应用打开。",
                        crate::app::thumbs::bytes_text(*bytes)
                    )
                }
            }
            Failure::Unsupported => {
                "这种图片格式暂不支持预览（例如动图 WebP）。可用默认应用打开。".into()
            }
        }
    }

    /// 稳定的短代号（harness 与日志用，不含路径）。
    pub fn code(&self) -> &'static str {
        match self {
            Failure::Missing => "missing",
            Failure::Cloud => "cloud",
            Failure::Corrupt => "corrupt",
            Failure::TooLarge { .. } => "too_large",
            Failure::Unsupported => "unsupported",
        }
    }
}

/// 按扩展名判断是否进入预览列表（只是“尝试”，是否成功由内容决定）。
pub fn is_previewable(ext: &str) -> bool {
    crate::thumbs::wants_thumbnail(ext)
}

/// 视频扩展名：Space 时给出“不提供内置播放，用默认应用打开”的提示。
pub fn is_video(ext: &str) -> bool {
    crate::thumbs::shell::SHELL_VIDEO_EXTS.contains(&ext)
}

/// 读取并解码一个文件。
pub fn decode_file(path: &Path, lim: &Limits) -> Result<Decoded, Failure> {
    let meta = std::fs::metadata(path).map_err(|_| Failure::Missing)?;
    if is_cloud_placeholder(meta.file_attributes()) {
        return Err(Failure::Cloud);
    }
    if !meta.is_file() {
        return Err(Failure::Unsupported);
    }
    if meta.len() > lim.max_file_bytes {
        return Err(Failure::TooLarge {
            w: 0,
            h: 0,
            bytes: meta.len(),
        });
    }
    let bytes = std::fs::read(path).map_err(|_| Failure::Missing)?;
    decode_bytes(&bytes, lim)
}

fn check_dims(w: u32, h: u32, bytes: u64, lim: &Limits) -> Result<(), Failure> {
    if w == 0 || h == 0 {
        return Err(Failure::Corrupt);
    }
    if w > lim.max_side || h > lim.max_side || w as u64 * h as u64 > lim.max_pixels {
        return Err(Failure::TooLarge { w, h, bytes });
    }
    Ok(())
}

/// 从内存中的文件内容解码（按内容识别格式）。
pub fn decode_bytes(bytes: &[u8], lim: &Limits) -> Result<Decoded, Failure> {
    if bytes.is_empty() {
        return Err(Failure::Corrupt);
    }
    if webp::is_webp(bytes) {
        return decode_webp(bytes, lim);
    }
    use image::{ImageDecoder, ImageFormat};
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| Failure::Corrupt)?;
    let format = reader.format();
    if !matches!(
        format,
        Some(ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::Gif | ImageFormat::Bmp)
    ) {
        return Err(Failure::Unsupported);
    }
    let mut il = image::Limits::default();
    il.max_alloc = Some(lim.max_pixels * 8);
    reader.limits(il);
    if format == Some(ImageFormat::Gif) {
        return decode_gif(bytes, lim);
    }
    let mut dec = reader.into_decoder().map_err(map_err)?;
    let (w, h) = dec.dimensions();
    check_dims(w, h, bytes.len() as u64, lim)?;
    let orientation = dec
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(dec).map_err(map_err)?;
    img.apply_orientation(orientation);
    let rgba = img.into_rgba8();
    Ok(Decoded::still(Frame {
        w: rgba.width(),
        h: rgba.height(),
        rgba: rgba.into_raw(),
    }))
}

fn decode_webp(bytes: &[u8], lim: &Limits) -> Result<Decoded, Failure> {
    let f = webp::features(bytes).map_err(|e| match e {
        crate::thumbs::Fail::Unsupported => Failure::Unsupported,
        _ => Failure::Corrupt,
    })?;
    if f.animated {
        return Err(Failure::Unsupported);
    }
    check_dims(f.w, f.h, bytes.len() as u64, lim)?;
    let t = webp::decode_original(bytes, &f).map_err(|e| match e {
        crate::thumbs::Fail::Unsupported => Failure::Unsupported,
        _ => Failure::Corrupt,
    })?;
    Ok(Decoded::still(Frame {
        w: t.w,
        h: t.h,
        rgba: t.rgba,
    }))
}

fn map_err(e: image::ImageError) -> Failure {
    match e {
        image::ImageError::Unsupported(_) => Failure::Unsupported,
        image::ImageError::Limits(_) => Failure::TooLarge {
            w: 0,
            h: 0,
            bytes: 0,
        },
        _ => Failure::Corrupt,
    }
}

/// 过短的帧延迟（0～10 ms）按浏览器惯例当作 100 ms；其余下限 20 ms，避免界面被高频刷新拖慢。
pub fn normalize_delay_ms(ms: u32) -> u32 {
    if ms <= 10 { 100 } else { ms.max(20) }
}

fn decode_gif(bytes: &[u8], lim: &Limits) -> Result<Decoded, Failure> {
    use image::AnimationDecoder;
    use image::ImageDecoder;
    use image::codecs::gif::GifDecoder;
    let dec = GifDecoder::new(Cursor::new(bytes)).map_err(map_err)?;
    let (w, h) = dec.dimensions();
    check_dims(w, h, bytes.len() as u64, lim)?;
    let frame_bytes = w as u64 * h as u64 * 4;
    let mut frames: Vec<Frame> = Vec::new();
    let mut delays: Vec<u32> = Vec::new();
    let mut note = None;
    let mut total = 0u64;
    for fr in dec.into_frames() {
        // 第一帧必须成功；之后的帧出错说明动图被截断，保留已解出的帧
        let fr = match fr {
            Ok(f) => f,
            Err(e) if frames.is_empty() => return Err(map_err(e)),
            Err(_) => break,
        };
        let (n, d) = fr.delay().numer_denom_ms();
        let ms = n.checked_div(d).unwrap_or(100);
        total += frame_bytes;
        if total > lim.max_anim_bytes || frames.len() >= lim.max_frames {
            // 太大：不播放，只显示第一帧，并说明原因
            let first = frames.swap_remove(0);
            note = Some(format!(
                "动图过大（超过 {} MB 或 {} 帧），为控制内存只显示第一帧。",
                lim.max_anim_bytes / (1024 * 1024),
                lim.max_frames
            ));
            return Ok(Decoded {
                frames: vec![first],
                delays_ms: Vec::new(),
                note,
            });
        }
        let buf = fr.into_buffer();
        frames.push(Frame {
            w: buf.width(),
            h: buf.height(),
            rgba: buf.into_raw(),
        });
        delays.push(normalize_delay_ms(ms));
    }
    if frames.is_empty() {
        return Err(Failure::Corrupt);
    }
    if frames.len() == 1 {
        delays.clear();
    }
    Ok(Decoded {
        frames,
        delays_ms: delays,
        note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thumbs::decode::samples as s;

    fn px(f: &Frame, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * f.w + x) * 4) as usize;
        [f.rgba[i], f.rgba[i + 1], f.rgba[i + 2], f.rgba[i + 3]]
    }

    fn reddish(p: [u8; 4]) -> bool {
        p[0] > 180 && p[1] < 90 && p[2] < 90
    }

    #[test]
    fn five_formats_decode_at_original_size() {
        let lim = Limits::default();
        let t = s::pattern(640, 480, 90);
        for (name, bytes) in [
            ("webp-lossy", s::webp_lossy(&t)),
            ("webp-lossless", s::webp_lossless(&t)),
            ("png", s::png(&t)),
            ("bmp", s::bmp(&t)),
            ("jpeg", s::jpeg(&t, 1)),
            ("gif", s::gif2(&t)),
        ] {
            let d = decode_bytes(&bytes, &lim).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert_eq!((d.width(), d.height()), (640, 480), "{name} 不缩小");
            assert!(reddish(px(&d.frames[0], 8, 8)), "{name} 左上角是红块");
        }
    }

    #[test]
    fn exif_orientation_is_applied_to_full_image() {
        let t = s::pattern(300, 200, 90);
        let d = decode_bytes(&s::jpeg(&t, 6), &Limits::default()).unwrap();
        assert_eq!((d.width(), d.height()), (200, 300));
        // 方向 6：顺时针 90°，红块从左上到右上
        assert!(reddish(px(&d.frames[0], 190, 8)));
        assert!(!reddish(px(&d.frames[0], 8, 8)));
    }

    #[test]
    fn png_alpha_is_kept() {
        let d = decode_bytes(&s::png(&s::with_hole(200, 200)), &Limits::default()).unwrap();
        assert_eq!(px(&d.frames[0], 100, 100)[3], 0);
        assert_eq!(px(&d.frames[0], 2, 2)[3], 255);
    }

    #[test]
    fn gif_decodes_all_frames_with_delays() {
        let d = decode_bytes(&s::gif2(&s::pattern(64, 48, 90)), &Limits::default()).unwrap();
        assert!(d.animated());
        assert_eq!(d.frames.len(), 2);
        assert_eq!(d.delays_ms.len(), 2);
        assert!(d.delays_ms.iter().all(|&m| m >= 20));
        assert!(reddish(px(&d.frames[0], 2, 2)));
        // 第二帧纯蓝
        let p = px(&d.frames[1], 2, 2);
        assert!(p[2] > 180 && p[0] < 80);
    }

    #[test]
    fn oversized_gif_falls_back_to_first_frame_with_note() {
        let lim = Limits {
            max_anim_bytes: 64 * 48 * 4, // 只够一帧
            ..Limits::default()
        };
        let d = decode_bytes(&s::gif2(&s::pattern(64, 48, 90)), &lim).unwrap();
        assert!(!d.animated());
        assert!(
            d.note
                .as_deref()
                .is_some_and(|n| n.contains("只显示第一帧"))
        );
        assert!(reddish(px(&d.frames[0], 2, 2)));
        let lim = Limits {
            max_frames: 1,
            ..Limits::default()
        };
        assert!(
            !decode_bytes(&s::gif2(&s::pattern(64, 48, 90)), &lim)
                .unwrap()
                .animated()
        );
    }

    #[test]
    fn too_large_is_reported_without_decoding() {
        let lim = Limits::default();
        // 声明 30000×30000 的 PNG 文件头
        let e = decode_bytes(&s::png_huge_header(30_000, 30_000), &lim).unwrap_err();
        assert!(
            matches!(
                e,
                Failure::TooLarge {
                    w: 30_000,
                    h: 30_000,
                    ..
                }
            ),
            "{e:?}"
        );
        assert!(e.message().contains("30000"));
        // 单边超限（总像素不超）
        let narrow = Limits {
            max_side: 100,
            ..Limits::default()
        };
        let e = decode_bytes(&s::png(&s::pattern(200, 10, 1)), &narrow).unwrap_err();
        assert!(matches!(e, Failure::TooLarge { .. }));
        // 总像素超限
        let few = Limits {
            max_pixels: 1000,
            ..Limits::default()
        };
        assert!(matches!(
            decode_bytes(&s::webp_lossy(&s::pattern(100, 100, 1)), &few).unwrap_err(),
            Failure::TooLarge { .. }
        ));
    }

    #[test]
    fn corrupt_unsupported_and_empty() {
        let lim = Limits::default();
        let t = s::pattern(200, 200, 90);
        let mut png = s::png(&t);
        png.truncate(png.len() / 2);
        assert_eq!(decode_bytes(&png, &lim).unwrap_err(), Failure::Corrupt);
        let jpg = s::jpeg(&t, 1);
        assert!(decode_bytes(&jpg[..60], &lim).is_err(), "只有文件头");
        // 截断的 JPEG：解码器补灰后返回部分图像（浏览器与系统查看器同样如此），只要求不崩溃、尺寸正确
        if let Ok(d) = decode_bytes(&jpg[..jpg.len() / 3], &lim) {
            assert_eq!((d.width(), d.height()), (200, 200));
        }
        assert_eq!(decode_bytes(&[], &lim).unwrap_err(), Failure::Corrupt);
        assert_eq!(
            decode_bytes(b"II*\x00not-a-real-tiff", &lim).unwrap_err(),
            Failure::Unsupported
        );
        assert_eq!(
            decode_bytes(b"RIFF\x04\x00\x00\x00WEBPxxxx", &lim).unwrap_err(),
            Failure::Corrupt
        );
        for f in [
            Failure::Missing,
            Failure::Cloud,
            Failure::Corrupt,
            Failure::Unsupported,
            Failure::TooLarge {
                w: 1,
                h: 1,
                bytes: 0,
            },
        ] {
            assert!(!f.message().is_empty());
        }
    }

    #[test]
    fn decode_file_missing_and_content_not_extension() {
        let dir = std::env::temp_dir().join(format!("sfm-preview-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lim = Limits::default();
        assert_eq!(
            decode_file(&dir.join("none.png"), &lim).unwrap_err(),
            Failure::Missing
        );
        // 扩展名是 .jpg 的 PNG：按内容识别
        let p = dir.join("x.jpg");
        std::fs::write(&p, s::png(&s::pattern(50, 40, 3))).unwrap();
        let d = decode_file(&p, &lim).unwrap();
        assert_eq!((d.width(), d.height()), (50, 40));
        // 文件体积上限
        let small = Limits {
            max_file_bytes: 10,
            ..Limits::default()
        };
        assert!(matches!(
            decode_file(&p, &small).unwrap_err(),
            Failure::TooLarge { w: 0, .. }
        ));
        // 目录不是文件
        assert_eq!(decode_file(&dir, &lim).unwrap_err(), Failure::Unsupported);
        // 云占位：脱机属性
        let c = dir.join("cloud.png");
        std::fs::write(&c, s::png(&s::pattern(50, 40, 3))).unwrap();
        let ok = std::process::Command::new("attrib")
            .arg("+O")
            .arg(&c)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok);
        assert_eq!(decode_file(&c, &lim).unwrap_err(), Failure::Cloud);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delay_normalization() {
        assert_eq!(normalize_delay_ms(0), 100);
        assert_eq!(normalize_delay_ms(10), 100);
        assert_eq!(normalize_delay_ms(11), 20);
        assert_eq!(normalize_delay_ms(80), 80);
    }

    #[test]
    fn extension_gate() {
        for e in ["webp", "jpg", "jpeg", "png", "gif", "bmp"] {
            assert!(is_previewable(e));
        }
        for e in ["mp4", "mkv", "tif", "heic", "txt", ""] {
            assert!(!is_previewable(e));
        }
        assert!(is_video("mp4") && is_video("mkv") && !is_video("png"));
    }
}
