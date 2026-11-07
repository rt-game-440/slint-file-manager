//! libwebp 的最小封装：读头、解码（可选解码时缩放）、有损编码（磁盘缓存用）。移植自 `prototypes/thumb-probe`。

use super::{Fail, Thumb};
use libwebp_sys as w;

/// 是否是 RIFF/WEBP 容器（按内容判断，不看扩展名）。
pub fn is_webp(b: &[u8]) -> bool {
    b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP"
}

#[derive(Clone, Copy, Debug)]
pub struct Features {
    pub w: u32,
    pub h: u32,
    pub has_alpha: bool,
    pub animated: bool,
    /// 有损（VP8）。无损（VP8L）或混合／未知为 false
    pub lossy: bool,
}

pub fn features(b: &[u8]) -> Result<Features, Fail> {
    // SAFETY: WebPGetFeatures 只读取 b 的前 len 字节，写入 f。
    unsafe {
        let mut f = std::mem::zeroed::<w::WebPBitstreamFeatures>();
        match w::WebPGetFeatures(b.as_ptr(), b.len(), &mut f) {
            w::VP8StatusCode::VP8_STATUS_OK => Ok(Features {
                w: f.width as u32,
                h: f.height as u32,
                has_alpha: f.has_alpha != 0,
                animated: f.has_animation != 0,
                lossy: f.format == 1,
            }),
            w::VP8StatusCode::VP8_STATUS_UNSUPPORTED_FEATURE => Err(Fail::Unsupported),
            _ => Err(Fail::Corrupt),
        }
    }
}

/// 解码为 `out_w × out_h` 的 RGBA。尺寸与原图不同时由 libwebp 在解码过程中缩放（不分配整幅 RGBA，
/// 仅对有损不透明成立；无损或带透明时 libwebp 内部仍会分配整幅缓冲，调用方须先做像素上限检查）。
pub fn decode(b: &[u8], f: &Features, out_w: u32, out_h: u32) -> Result<Thumb, Fail> {
    decode_with(b, f, out_w, out_h, true)
}

/// 原尺寸、完整画质解码（大图预览用：不跳过环路滤波与精细上采样）。调用方须先做像素上限检查。
pub fn decode_original(b: &[u8], f: &Features) -> Result<Thumb, Fail> {
    decode_with(b, f, f.w, f.h, false)
}

fn decode_with(b: &[u8], f: &Features, out_w: u32, out_h: u32, fast: bool) -> Result<Thumb, Fail> {
    // SAFETY: 输出缓冲由我们分配并以 external memory 交给 libwebp，长度与 stride 一致；解码结束后不再被引用。
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
        if out_w != f.w || out_h != f.h {
            cfg.options.use_scaling = 1;
            cfg.options.scaled_width = out_w as i32;
            cfg.options.scaled_height = out_h as i32;
        }
        // 跳过环路滤波与精细上采样：缩略图画质差异很小（研究 0005 第 3 节），换取速度
        if fast {
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

/// 整幅解码（磁盘缓存里的小图用）。
pub fn decode_full(b: &[u8]) -> Result<Thumb, Fail> {
    let f = features(b)?;
    if f.animated || f.w == 0 || f.h == 0 || f.w > super::THUMB_SIZE || f.h > super::THUMB_SIZE {
        return Err(Fail::Corrupt);
    }
    decode(b, &f, f.w, f.h)
}

/// 有损编码 RGBA（保留 alpha）。失败返回 `None`。
pub fn encode(t: &Thumb, quality: f32) -> Option<Vec<u8>> {
    if t.w == 0 || t.h == 0 || t.rgba.len() != t.w as usize * t.h as usize * 4 {
        return None;
    }
    // SAFETY: 输入长度已核对；输出由 libwebp 分配，复制后用 WebPFree 释放。
    unsafe {
        let mut out: *mut u8 = std::ptr::null_mut();
        let n = w::WebPEncodeRGBA(
            t.rgba.as_ptr(),
            t.w as i32,
            t.h as i32,
            (t.w * 4) as i32,
            quality,
            &mut out,
        );
        if n == 0 || out.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(out, n).to_vec();
        w::WebPFree(out as *mut _);
        Some(v)
    }
}

/// 无损编码（测试夹具用）。
pub fn encode_lossless(t: &Thumb) -> Option<Vec<u8>> {
    // SAFETY: 同 encode。
    unsafe {
        let mut out: *mut u8 = std::ptr::null_mut();
        let n = w::WebPEncodeLosslessRGBA(
            t.rgba.as_ptr(),
            t.w as i32,
            t.h as i32,
            (t.w * 4) as i32,
            &mut out,
        );
        if n == 0 || out.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(out, n).to_vec();
        w::WebPFree(out as *mut _);
        Some(v)
    }
}

/// 把最长边缩到 `size`（不放大），保持宽高比。
pub fn fit(w_: u32, h_: u32, size: u32) -> (u32, u32) {
    let longest = w_.max(h_);
    if longest <= size {
        return (w_.max(1), h_.max(1));
    }
    let s = size as f64 / longest as f64;
    (
        ((w_ as f64 * s).round() as u32).max(1),
        ((h_ as f64 * s).round() as u32).max(1),
    )
}
