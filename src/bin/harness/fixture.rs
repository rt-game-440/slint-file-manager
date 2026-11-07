//! 确定性样例目录：每次 harness 启动时在 `%TEMP%\sfm-harness\样例目录` 重新生成。
//!
//! - 固定位置让同一台机器上不同工作树的地址栏、截图一致；路径含用户名，因此写入截图指纹，
//!   换机器时比较结果为“环境不同”而不是回归。
//! - 名称、大小、修改时间全部固定；内容只是占位字节，不会被执行（harness 的打开动作只记录路径）。
//! - “无权限”文件夹用 icacls 拒绝 Everyone 列目录，只作用于本夹具；下次生成前先 `/reset` 还原。
//! - 缩略图样例（`%TEMP%\sfm-harness\缩略图样例`，在样例目录之外，不改变其条目数）：内容由
//!   `thumbs::decode::samples` 确定性生成的真实图片，以及损坏、扩展名不符、超大尺寸、“慢”文件与不支持的格式。

use crate::ctx::Res;
use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const ROOT_NAME: &str = "样例目录";
pub const PHOTOS: &str = "照片";
pub const EMPTY: &str = "空文件夹";
pub const DENIED: &str = "无权限";
pub const DEEP: [&str; 4] = [
    "一个名字很长的项目资料文件夹（用于检查路径栏溢出）",
    "第二层同样很长的子文件夹名称",
    "2026 年度汇总",
    "最终版",
];
pub const LONG_PDF: &str = "一份标题非常非常长的中文年度总结报告（最终修订版，含附录与图表）.pdf";
pub const LONG_TXT: &str =
    "A very long English file name that should wrap onto two lines and then elide.txt";

/// (名称, 大小字节)。顺序只影响修改时间：第 i 个文件的时间为基准 + i 小时。
const FILES: &[(&str, u64)] = &[
    ("报告2.txt", 1_200),
    ("报告10.txt", 860),
    ("说明.md", 3_400),
    ("数据表.xlsx", 48_213),
    ("视频片段.mp4", 15_728_640),
    ("压缩包.zip", 2_621_440),
    ("程序.exe", 524_288),
    ("音乐.mp3", 4_404_019),
    (LONG_TXT, 512),
    (LONG_PDF, 1_887_436),
];

/// 缩略图样例目录名。
pub const THUMBS: &str = "缩略图样例";

/// 缩略图样例的文件（按显示顺序）。名称含 [`crate::ctx::SLOW_MARK`] 的被当作慢文件。
pub const THUMB_FILES: [&str; 12] = [
    "01 风景.webp",
    "02 横拍竖看.jpg",
    "03 透明.png",
    "04 动图.gif",
    "05 位图.bmp",
    "06 无损透明.webp",
    "07 内容是PNG.jpg",
    "08 损坏.webp",
    "09 超大尺寸.png",
    "10 慢.webp",
    "11 说明.txt",
    "12 不支持.tif",
];

/// 2026-03-14 01:30:00 UTC
const BASE_SECS: u64 = 1_773_451_800;

pub fn time(hours: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(BASE_SECS + hours * 3600)
}

/// 样例目录的父目录。默认 `%TEMP%\sfm-harness`；环境变量 `SFM_HARNESS_DIR` 可改为独立目录，
/// 供多个工作树并行运行 harness 时互不覆盖（每次运行都会重建该目录下的样例目录，不要指向有用数据的位置）。
pub fn parent() -> PathBuf {
    match std::env::var_os("SFM_HARNESS_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => std::env::temp_dir().join("sfm-harness"),
    }
}

pub fn root() -> PathBuf {
    parent().join(ROOT_NAME)
}

pub fn path(rel: &str) -> PathBuf {
    root().join(rel)
}

pub fn thumbs_root() -> PathBuf {
    parent().join(THUMBS)
}

pub fn deep_path(levels: usize) -> PathBuf {
    let mut p = root();
    for d in &DEEP[..levels] {
        p.push(d);
    }
    p
}

/// 会话测试用的全新目录（`%TEMP%\sfm-harness\sessions\<name>`）。绝不使用用户真实的数据目录。
pub fn session_dir(name: &str) -> Res<PathBuf> {
    fresh_dir(&parent().join("sessions").join(name))
}

/// 路径失效测试用的全新草稿目录（`%TEMP%\sfm-harness\scratch\<name>`，位于样例目录之外，不影响其条目数）。
pub fn scratch_dir(name: &str) -> PathBuf {
    parent().join("scratch").join(name)
}

pub fn fresh_dir(d: &Path) -> Res<PathBuf> {
    if d.exists() {
        fs::remove_dir_all(d)?;
    }
    fs::create_dir_all(d)?;
    Ok(d.to_path_buf())
}

pub fn file(dir: &Path, name: &str, size: u64, t: SystemTime) -> Res<()> {
    let f = File::create(dir.join(name))?;
    f.set_len(size)?;
    f.set_modified(t)?;
    Ok(())
}

/// 目录的修改时间需在其子项创建完成后设置；打开目录需要 FILE_FLAG_BACKUP_SEMANTICS。
pub fn dir_time(dir: &Path, t: SystemTime) -> Res<()> {
    const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let f = OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?;
    f.set_modified(t)?;
    Ok(())
}

pub fn icacls(args: &[&str]) -> Res<()> {
    let out = Command::new("icacls").args(args).output()?;
    if !out.status.success() {
        return Err(format!("icacls {args:?} 失败").into());
    }
    Ok(())
}

/// 删除旧夹具并重新生成。
pub fn ensure() -> Res<PathBuf> {
    let root = root();
    // 上次运行遗留的会话与草稿目录
    for d in ["sessions", "scratch", "stash"] {
        let _ = fs::remove_dir_all(parent().join(d));
    }
    let denied = root.join(DENIED);
    if denied.exists() {
        let d = denied.to_string_lossy().to_string();
        icacls(&[&d, "/reset", "/T", "/Q"])?;
    }
    if root.exists() {
        fs::remove_dir_all(&root)?;
    }
    fs::create_dir_all(&root)?;

    // 照片：12 张 WebP 占位，大小各不相同
    let photos = root.join(PHOTOS);
    fs::create_dir(&photos)?;
    for i in 1..=12u64 {
        file(
            &photos,
            &format!("海边{i:03}.webp"),
            180_000 + i * 7_919,
            time(i),
        )?;
    }
    dir_time(&photos, time(20))?;

    for i in 1..=12u64 {
        file(
            &root,
            &format!("图片{i:03}.webp"),
            95_000 + i * 3_571,
            time(30 + i),
        )?;
    }
    for (i, (name, size)) in FILES.iter().enumerate() {
        file(&root, name, *size, time(50 + i as u64))?;
    }

    // 隐藏文件：不应出现在列表中
    file(&root, "隐藏的配置.ini", 64, time(70))?;
    let status = Command::new("attrib")
        .arg("+h")
        .arg(root.join("隐藏的配置.ini"))
        .status()?;
    if !status.success() {
        return Err("attrib +h 失败".into());
    }

    let empty = root.join(EMPTY);
    fs::create_dir(&empty)?;
    dir_time(&empty, time(21))?;

    let mut deep = root.clone();
    for d in DEEP {
        deep.push(d);
        fs::create_dir(&deep)?;
    }
    file(&deep, "结论.txt", 2_048, time(80))?;
    let mut p = deep.clone();
    for i in (0..DEEP.len()).rev() {
        dir_time(&p, time(22 + i as u64))?;
        p.pop();
    }

    fs::create_dir(&denied)?;
    file(&denied, "秘密.txt", 10, time(90))?;
    dir_time(&denied, time(26))?;
    let d = denied.to_string_lossy().to_string();
    // *S-1-1-0 = Everyone；RD = 列出目录／读取数据
    icacls(&[&d, "/deny", "*S-1-1-0:(OI)(CI)(RD)", "/Q"])?;

    ensure_thumbs()?;
    Ok(root)
}

/// 生成缩略图样例目录（内容、修改时间固定）。
fn ensure_thumbs() -> Res<()> {
    use slint_file_manager::thumbs::decode::samples as s;
    let dir = fresh_dir(&thumbs_root())?;
    let png_as_jpg = s::png(&s::pattern(360, 240, 30));
    let contents: [Vec<u8>; 12] = [
        s::webp_lossy(&s::pattern(1200, 800, 120)),
        // EXIF 方向 6：存储为横图，显示应为竖图（红块在右上）
        s::jpeg(&s::pattern(960, 640, 90), 6),
        s::png(&s::with_hole(512, 512)),
        s::gif2(&s::pattern(400, 300, 160)),
        s::bmp(&s::pattern(480, 360, 60)),
        s::webp_lossless(&s::with_hole(400, 400)),
        png_as_jpg,
        b"RIFF\x10\x00\x00\x00WEBPVP8 garbage-garbage-garbage".to_vec(),
        s::png_huge_header(30_000, 30_000),
        s::webp_lossy(&s::pattern(800, 800, 220)),
        "缩略图样例里的普通文本文件".as_bytes().to_vec(),
        b"II*\x00not-a-real-tiff".to_vec(),
    ];
    for (i, (name, data)) in THUMB_FILES.iter().zip(contents).enumerate() {
        let path = dir.join(name);
        fs::write(&path, data)?;
        OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_modified(time(100 + i as u64))?;
    }
    dir_time(&dir, time(120))?;
    Ok(())
}

/// 根目录可见条目数（不含隐藏文件）：照片、空文件夹、无权限、长名称文件夹 + 12 张图片 + FILES。
pub fn root_visible_count() -> usize {
    4 + 12 + FILES.len()
}
