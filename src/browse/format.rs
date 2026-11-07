//! 条目的显示文本：大小、修改日期、类型与图标类别。全部来自真实元数据，不使用占位值。

use super::entry::Entry;
use std::time::SystemTime;

/// 文件大小：小于 1 KB 显示字节数，其余按 1024 进位保留至多一位小数。
pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} 字节");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v < 100.0 {
        let s = format!("{v:.1}");
        let s = s.strip_suffix(".0").unwrap_or(&s);
        format!("{s} {}", UNITS[u])
    } else {
        format!("{v:.0} {}", UNITS[u])
    }
}

/// 本地时间 `yyyy/MM/dd HH:mm`；取不到时间时为空。
pub fn date_text(t: Option<SystemTime>) -> String {
    match t {
        Some(t) => chrono::DateTime::<chrono::Local>::from(t)
            .format("%Y/%m/%d %H:%M")
            .to_string(),
        None => String::new(),
    }
}

/// 类型文字：文件夹、`扩展名大写 文件` 或 `文件`。
pub fn kind_text(e: &Entry) -> String {
    if e.is_dir {
        "文件夹".into()
    } else if e.ext.is_empty() {
        "文件".into()
    } else {
        format!("{} 文件", e.ext.to_uppercase())
    }
}

/// 图标类别名（与 ui/icons.slint 中的图标名一致）。仅按扩展名粗分，不代表能生成缩略图。
pub fn icon_name(e: &Entry) -> &'static str {
    if e.is_dir {
        return "folder";
    }
    match e.ext.as_str() {
        "webp" | "jpg" | "jpeg" | "png" | "gif" | "bmp" | "tif" | "tiff" | "heic" | "svg" => {
            "file-image"
        }
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "flv" | "m4v" => "file-video",
        "mp3" | "flac" | "wav" | "aac" | "ogg" | "m4a" | "wma" => "file-audio",
        "zip" | "7z" | "rar" | "tar" | "gz" | "xz" | "bz2" | "zst" => "file-archive",
        "exe" | "msi" | "bat" | "cmd" | "ps1" | "lnk" => "file-app",
        "txt" | "md" | "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "csv" | "rtf"
        | "json" | "xml" | "log" => "file-text",
        _ => "file",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size_text(0), "0 字节");
        assert_eq!(size_text(1023), "1023 字节");
        assert_eq!(size_text(1024), "1 KB");
        assert_eq!(size_text(1536), "1.5 KB");
        assert_eq!(size_text(12_504_596), "11.9 MB");
        assert_eq!(size_text(500 * 1024 * 1024), "500 MB");
    }

    #[test]
    fn kinds_and_icons() {
        let f = Entry::new("照片.WebP".into(), false, 1, None);
        assert_eq!(kind_text(&f), "WEBP 文件");
        assert_eq!(icon_name(&f), "file-image");
        let d = Entry::new("文件夹.zip".into(), true, 0, None);
        assert_eq!(kind_text(&d), "文件夹");
        assert_eq!(icon_name(&d), "folder");
        let n = Entry::new("README".into(), false, 1, None);
        assert_eq!(kind_text(&n), "文件");
        assert_eq!(date_text(None), "");
    }
}
