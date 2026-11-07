//! 图片缩略图：解码、持久缓存与调度（Issue #10）。与界面无关，界面接线在 `crate::app::thumbs`。
//!
//! 路线依据 `docs/research/0005-thumbnail-route.md`：
//! - WebP 用 libwebp 解码时缩放（有损不透明的 WebP 不需要整幅缓冲）；无损／带透明 WebP 同样走 libwebp，但受像素上限约束。
//! - JPEG／PNG／GIF／BMP 用 `image` 整幅解码 + `fast_image_resize` 缩放，受像素上限约束；GIF 取第一帧。
//! - 按内容识别格式（文件头），不看扩展名；扩展名只决定“要不要尝试生成”。
//! - 云端占位文件（脱机／访问时召回属性）一律不读取内容，避免隐式下载（ADR 0004、研究 0003）；
//!   只用 Shell 的 `SIIGBF_INCACHEONLY` 取系统缓存里已有的缩略图，拿不到回退图标（Issue #13）。
//! - 视频（MP4／MKV 等）与 TIFF、HEIC 等本应用解不了的格式走系统缩略图回退（[`shell`]、[`lane`]）。
//!
//! 模块：[`decode`]（生成）、[`webp`]（libwebp 封装）、[`cache`]（磁盘缓存）、[`queue`]（优先队列，纯逻辑）、
//! [`sched`]（工作线程池、慢文件增援与硬超时）、[`shell`]（系统缩略图提供者）、[`lane`]（系统缩略图车道：
//! 单线程、让路、硬超时与替换线程）。

pub mod cache;
pub mod decode;
pub mod lane;
pub mod queue;
pub mod sched;
pub mod shell;
pub mod webp;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 缩略图最长边（像素，不放大）。内存与磁盘缓存都按这个尺寸保存。
pub const THUMB_SIZE: u32 = 256;

/// 缓存键算法版本：改变生成方式（尺寸、方向处理、格式）时递增，旧缓存自然失效。
pub const KEY_VERSION: u32 = 1;

/// 缩略图：RGBA8，非预乘 alpha（与 Slint `Rgba8Pixel` 一致）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thumb {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

impl Thumb {
    pub fn bytes(&self) -> usize {
        self.rgba.len()
    }
}

/// 失败类别：只暴露类别，不带系统错误文本（日志不含路径）。全部回退到类型图标，不重试、不弹提示。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Fail {
    /// 读取失败（不存在、无权限、I/O 错误）
    Io,
    /// 内容损坏或被截断
    Corrupt,
    /// 格式不支持（含动图 WebP、未知内容）
    Unsupported,
    /// 像素或文件体积超过上限，未解码
    TooLarge,
    /// 云端占位文件：读取内容会触发下载，不读取
    Cloud,
    /// 超过硬超时（之后若成功仍会补上）
    Timeout,
}

impl Fail {
    pub fn as_str(self) -> &'static str {
        match self {
            Fail::Io => "io",
            Fail::Corrupt => "corrupt",
            Fail::Unsupported => "unsupported",
            Fail::TooLarge => "too_large",
            Fail::Cloud => "cloud",
            Fail::Timeout => "timeout",
        }
    }
}

/// 生成缩略图所需的源文件身份：路径 + 大小 + 修改时间（来自目录枚举，不另读文件）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Source {
    pub path: PathBuf,
    pub size: u64,
    /// 修改时间（自 UNIX 纪元的纳秒）；未知时为 `i64::MIN`
    pub mtime_ns: i64,
}

impl Source {
    pub fn new(path: PathBuf, size: u64, modified: Option<SystemTime>) -> Source {
        Source {
            path,
            size,
            mtime_ns: mtime_ns(modified),
        }
    }

    /// 缓存键：FNV-1a(版本｜缩略图尺寸｜路径（小写 UTF-16）｜大小｜修改时间)。
    /// 重命名（路径变）、内容变化（大小或修改时间变）都会得到新键，旧缩略图不会被当作新文件的结果。
    pub fn key(&self) -> u64 {
        let mut h = Fnv::new();
        h.write(&KEY_VERSION.to_le_bytes());
        h.write(&THUMB_SIZE.to_le_bytes());
        use std::os::windows::ffi::OsStrExt;
        for u in self.path.as_os_str().encode_wide() {
            // Windows 路径不区分大小写：ASCII 部分按小写参与哈希
            let u = if (b'A' as u16..=b'Z' as u16).contains(&u) {
                u + 32
            } else {
                u
            };
            h.write(&u.to_le_bytes());
        }
        h.write(&self.size.to_le_bytes());
        h.write(&self.mtime_ns.to_le_bytes());
        h.finish()
    }

    /// 写入缓存文件、用于命中时核对的身份字符串（防哈希碰撞）。
    pub fn identity(&self) -> String {
        format!(
            "{}|{}|{}",
            self.path.to_string_lossy().to_lowercase(),
            self.size,
            self.mtime_ns
        )
    }
}

pub fn mtime_ns(t: Option<SystemTime>) -> i64 {
    match t.map(|t| t.duration_since(UNIX_EPOCH)) {
        Some(Ok(d)) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Some(Err(e)) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
        None => i64::MIN,
    }
}

/// 64 位 FNV-1a。
pub struct Fnv(u64);

impl Fnv {
    pub fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    pub fn write(&mut self, b: &[u8]) {
        for &x in b {
            self.0 ^= x as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for Fnv {
    fn default() -> Self {
        Fnv::new()
    }
}

/// 按扩展名判断是否值得尝试生成缩略图（只是“尝试”，是否成功由内容决定）。
pub fn wants_thumbnail(ext: &str) -> bool {
    matches!(
        ext,
        "webp" | "jpg" | "jpeg" | "jpe" | "jfif" | "png" | "gif" | "bmp" | "dib"
    )
}

/// 这个扩展名是否值得请求缩略图：本应用自己能解码的格式，或交给系统缩略图的格式（视频、TIFF、HEIC……）。
pub fn wants_any_thumbnail(ext: &str) -> bool {
    shell::route(ext).is_some()
}

/// 工作线程数：`min(逻辑核数 - 2, 6)`，至少 2（研究 0005 第 1 节第 6 条）。
pub fn default_workers() -> usize {
    let n = std::thread::available_parallelism().map_or(4, |n| n.get());
    n.saturating_sub(2).clamp(2, 6)
}

/// 缩略图缓存目录：`SFM_THUMB_CACHE_DIR` 优先，否则 `数据目录\thumbs`。都没有时 `None`（只用内存缓存）。
pub fn cache_dir(data_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("SFM_THUMB_CACHE_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    data_dir.map(|d| d.join("thumbs"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn key_changes_with_identity() {
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let a = Source::new(PathBuf::from(r"C:\图\a.webp"), 10, Some(t));
        let same_case = Source::new(PathBuf::from(r"C:\图\A.WEBP"), 10, Some(t));
        assert_eq!(a.key(), same_case.key(), "路径大小写不影响");
        let renamed = Source::new(PathBuf::from(r"C:\图\b.webp"), 10, Some(t));
        let resized = Source::new(PathBuf::from(r"C:\图\a.webp"), 11, Some(t));
        let touched = Source::new(
            PathBuf::from(r"C:\图\a.webp"),
            10,
            Some(t + Duration::from_nanos(100)),
        );
        for o in [&renamed, &resized, &touched] {
            assert_ne!(a.key(), o.key());
            assert_ne!(a.identity(), o.identity());
        }
    }

    #[test]
    fn workers_in_range() {
        let n = default_workers();
        assert!((2..=6).contains(&n));
    }

    #[test]
    fn extension_gate() {
        for e in ["webp", "jpg", "jpeg", "png", "gif", "bmp"] {
            assert!(wants_thumbnail(e));
        }
        for e in ["", "txt", "mp4", "pdf", "heic"] {
            assert!(!wants_thumbnail(e));
        }
    }
}
