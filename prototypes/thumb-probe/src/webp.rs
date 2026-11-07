//! libwebp 的最小封装：读头、解码（可选解码时缩放）、编码（夹具与缓存用）。

use crate::common::{Fail, Thumb};
use libwebp_sys as w;

/// 是否是 RIFF/WEBP 容器（按内容判断，不看扩展名）。
pub fn is_webp(b: &[u8]) -> bool {
    b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP"
}

pub struct Features {
    pub w: u32,
    pub h: u32,
    pub has_alpha: bool,
    pub animated: bool,
}

pub fn features(b: &[u8]) -> Result<Features, Fail> {
    unsafe {
        let mut f = std::mem::zeroed::<w::WebPBitstreamFeatures>();
        match w::WebPGetFeatures(b.as_ptr(), b.len(), &mut f) {
            w::VP8StatusCode::VP8_STATUS_OK => Ok(Features {
                w: f.width as u32,
                h: f.height as u32,
                has_alpha: f.has_alpha != 0,
                animated: f.has_animation != 0,
            }),
            w::VP8StatusCode::VP8_STATUS_UNSUPPORTED_FEATURE => Err(Fail::Unsupported),
            _ => Err(Fail::Corrupt),
        }
    }
}

/// 解码选项。`scaled` 为目标尺寸时由 libwebp 在解码过程中缩放，不分配整幅 RGBA。
#[derive(Clone, Copy, Default)]
pub struct DecodeOpts {
    pub scaled: Option<(u32, u32)>,
    /// 跳过环路滤波与精细色度上采样：缩略图场景画质影响很小，换取解码速度
    pub fast: bool,
}

pub fn decode(b: &[u8], out_w: u32, out_h: u32, o: DecodeOpts) -> Result<Thumb, Fail> {
    unsafe {
        let mut cfg = w::WebPDecoderConfig::new().map_err(|_| Fail::Unsupported)?;
        let mut rgba = vec![0u8; out_w as usize * out_h as usize * 4];
        cfg.output.colorspace = w::WEBP_CSP_MODE::MODE_RGBA;
        cfg.output.is_external_memory = 1;
        cfg.output.width = out_w as i32;
        cfg.output.height = out_h as i32;
        cfg.output.u.RGBA = w::WebPRGBABuffer {
            rgba: rgba.as_mut_ptr(),
            stride: (out_w * 4) as i32,
            size: rgba.len(),
        };
        if let Some((sw, sh)) = o.scaled {
            cfg.options.use_scaling = 1;
            cfg.options.scaled_width = sw as i32;
            cfg.options.scaled_height = sh as i32;
        }
        if o.fast {
            cfg.options.no_fancy_upsampling = 1;
            cfg.options.bypass_filtering = 1;
        }
        match w::WebPDecode(b.as_ptr(), b.len(), &mut cfg) {
            w::VP8StatusCode::VP8_STATUS_OK => Ok(Thumb {
                w: out_w,
                h: out_h,
                rgba,
            }),
            w::VP8StatusCode::VP8_STATUS_UNSUPPORTED_FEATURE => Err(Fail::Unsupported),
            _ => Err(Fail::Corrupt),
        }
    }
}

/// 有损编码 RGBA。
pub fn encode(rgba: &[u8], w_: u32, h_: u32, quality: f32) -> Vec<u8> {
    unsafe {
        let mut out: *mut u8 = std::ptr::null_mut();
        let n = w::WebPEncodeRGBA(
            rgba.as_ptr(),
            w_ as i32,
            h_ as i32,
            (w_ * 4) as i32,
            quality,
            &mut out,
        );
        assert!(n > 0 && !out.is_null(), "WebP 编码失败");
        let v = std::slice::from_raw_parts(out, n).to_vec();
        w::WebPFree(out as *mut _);
        v
    }
}

/// 无损编码 RGBA（VP8L，用来验证“解码时缩放”对无损 WebP 是否同样省内存）。
pub fn encode_lossless(rgba: &[u8], w_: u32, h_: u32) -> Vec<u8> {
    unsafe {
        let mut out: *mut u8 = std::ptr::null_mut();
        let n = w::WebPEncodeLosslessRGBA(
            rgba.as_ptr(),
            w_ as i32,
            h_ as i32,
            (w_ * 4) as i32,
            &mut out,
        );
        assert!(n > 0 && !out.is_null(), "WebP 无损编码失败");
        let v = std::slice::from_raw_parts(out, n).to_vec();
        w::WebPFree(out as *mut _);
        v
    }
}

/// 把最长边缩到 `size`（不放大），保持宽高比。
pub fn fit(w_: u32, h_: u32, size: u32) -> (u32, u32) {
    let longest = w_.max(h_);
    if longest <= size {
        return (w_, h_);
    }
    let s = size as f64 / longest as f64;
    (
        ((w_ as f64 * s).round() as u32).max(1),
        ((h_ as f64 * s).round() as u32).max(1),
    )
}
