//! 云占位文件识别与“只取已有缩略图”边界验证。
//!
//! 只读原则：
//! - 枚举使用 FindFirstFileExW，只读目录元数据；
//! - 占位信息用 FILE_READ_ATTRIBUTES + FILE_FLAG_OPEN_REPARSE_POINT 打开句柄后调用 CfGetPlaceholderInfo，
//!   不读取数据流；
//! - 是否发生“水合”（下载原文件内容）以 CF_PLACEHOLDER_STANDARD_INFO.OnDiskDataSize 和
//!   FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS / OFFLINE 的前后变化判断。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{BITMAP, DeleteObject, GetObjectW, HGDIOBJ};
use windows::Win32::Storage::CloudFilters::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;
use windows::core::*;

use crate::menu::pump_for;
use crate::{from_wide, opt, wide};

const A_DIRECTORY: u32 = 0x10;
const A_REPARSE: u32 = 0x400;
const A_OFFLINE: u32 = 0x1000;
const A_RECALL_ON_OPEN: u32 = 0x4_0000;
const A_PINNED: u32 = 0x8_0000;
const A_UNPINNED: u32 = 0x10_0000;
const A_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
enum Class {
    Plain,
    OnlineOnly,
    Local,
    Pinned,
}

impl Class {
    fn name(self) -> &'static str {
        match self {
            Class::Plain => "普通文件",
            Class::OnlineOnly => "仅在线",
            Class::Local => "本地可用",
            Class::Pinned => "始终保留",
        }
    }
}

fn is_cloud_tag(tag: u32) -> bool {
    (tag & 0xFFFF_0FFF) == 0x9000_001A
}

fn classify(attrs: u32, tag: u32) -> Class {
    let cloud = (attrs & A_REPARSE != 0 && is_cloud_tag(tag))
        || attrs & (A_RECALL_ON_DATA_ACCESS | A_RECALL_ON_OPEN | A_OFFLINE | A_PINNED | A_UNPINNED)
            != 0;
    if !cloud {
        Class::Plain
    } else if attrs & (A_RECALL_ON_DATA_ACCESS | A_OFFLINE) != 0 {
        Class::OnlineOnly
    } else if attrs & A_PINNED != 0 && attrs & A_UNPINNED == 0 {
        Class::Pinned
    } else {
        Class::Local
    }
}

fn attr_str(a: u32) -> String {
    let mut v = Vec::new();
    for (bit, n) in [
        (A_REPARSE, "REPARSE"),
        (A_OFFLINE, "OFFLINE"),
        (A_RECALL_ON_OPEN, "RECALL_ON_OPEN"),
        (A_RECALL_ON_DATA_ACCESS, "RECALL_ON_DATA_ACCESS"),
        (A_PINNED, "PINNED"),
        (A_UNPINNED, "UNPINNED"),
    ] {
        if a & bit != 0 {
            v.push(n);
        }
    }
    format!("0x{a:08X}[{}]", v.join("|"))
}

#[derive(Clone, Debug)]
struct Entry {
    path: PathBuf,
    attrs: u32,
    tag: u32,
    size: u64,
}

fn enumerate(root: &Path, max: usize, out: &mut Vec<Entry>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= max {
            break;
        }
        let pat = wide(&dir.join("*").to_string_lossy());
        let mut fd = WIN32_FIND_DATAW::default();
        let h = unsafe {
            FindFirstFileExW(
                PCWSTR(pat.as_ptr()),
                FindExInfoBasic,
                &mut fd as *mut _ as *mut _,
                FindExSearchNameMatch,
                None,
                FIND_FIRST_EX_LARGE_FETCH,
            )
        };
        let Ok(h) = h else { continue };
        loop {
            let name = from_wide(&fd.cFileName);
            if name != "." && name != ".." {
                let p = dir.join(&name);
                let attrs = fd.dwFileAttributes;
                if attrs & A_DIRECTORY != 0 {
                    stack.push(p);
                } else {
                    out.push(Entry {
                        path: p,
                        attrs,
                        tag: if attrs & A_REPARSE != 0 {
                            fd.dwReserved0
                        } else {
                            0
                        },
                        size: ((fd.nFileSizeHigh as u64) << 32) | fd.nFileSizeLow as u64,
                    });
                }
            }
            if unsafe { FindNextFileW(h, &mut fd) }.is_err() {
                break;
            }
        }
        unsafe {
            let _ = FindClose(h);
        }
    }
}

fn ext_of(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Ph {
    attrs: u32,
    on_disk: i64,
    validated: i64,
    pin: i32,
    in_sync: i32,
    ok: bool,
}

/// 只读取占位信息，不打开数据流。
fn placeholder(p: &Path) -> Ph {
    let w = wide(&p.to_string_lossy());
    let attrs = unsafe { GetFileAttributesW(PCWSTR(w.as_ptr())) };
    let mut ph = Ph {
        attrs,
        ..Default::default()
    };
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    };
    let Ok(h) = h else { return ph };
    let mut buf = vec![0u8; 4096];
    let mut ret = 0u32;
    let r = unsafe {
        CfGetPlaceholderInfo(
            h,
            CF_PLACEHOLDER_INFO_STANDARD,
            buf.as_mut_ptr() as *mut _,
            buf.len() as u32,
            Some(&mut ret),
        )
    };
    if r.is_ok() {
        let info = unsafe { &*(buf.as_ptr() as *const CF_PLACEHOLDER_STANDARD_INFO) };
        ph.on_disk = info.OnDiskDataSize;
        ph.validated = info.ValidatedDataSize;
        ph.pin = info.PinState.0;
        ph.in_sync = info.InSyncState.0;
        ph.ok = true;
    }
    unsafe {
        let _ = CloseHandle(h);
    }
    ph
}

fn ph_str(p: &Ph) -> String {
    if p.ok {
        format!(
            "attrs={} on_disk={} validated={} pin={} in_sync={}",
            attr_str(p.attrs),
            p.on_disk,
            p.validated,
            p.pin,
            p.in_sync
        )
    } else {
        format!("attrs={} (非占位或无法读取占位信息)", attr_str(p.attrs))
    }
}

fn hydrated(before: &Ph, after: &Ph) -> bool {
    after.on_disk > before.on_disk
        || (before.attrs & (A_RECALL_ON_DATA_ACCESS | A_OFFLINE) != 0
            && after.attrs & (A_RECALL_ON_DATA_ACCESS | A_OFFLINE) == 0)
}

// ---------------------------------------------------------------- env

pub fn cmd_env() -> i32 {
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).unwrap();
        #[repr(C)]
        struct OsVer {
            size: u32,
            major: u32,
            minor: u32,
            build: u32,
            platform: u32,
            csd: [u16; 128],
        }
        if let Some(f) = GetProcAddress(ntdll, s!("RtlGetVersion")) {
            let f: extern "system" fn(*mut OsVer) -> i32 = std::mem::transmute(f);
            let mut v = OsVer {
                size: std::mem::size_of::<OsVer>() as u32,
                major: 0,
                minor: 0,
                build: 0,
                platform: 0,
                csd: [0; 128],
            };
            f(&mut v);
            println!("Windows {}.{}.{}", v.major, v.minor, v.build);
        }
        if let Some(f) = GetProcAddress(ntdll, s!("RtlQueryProcessPlaceholderCompatibilityMode")) {
            let f: extern "system" fn() -> i8 = std::mem::transmute(f);
            let m = f();
            let name = match m {
                0 => "PHCM_APPLICATION_DEFAULT",
                1 => "PHCM_DISGUISE_PLACEHOLDER",
                2 => "PHCM_EXPOSE_PLACEHOLDERS",
                _ => "其他/错误",
            };
            println!("进程占位符兼容模式：{m}（{name}）");
        }
    }
    0
}

/// 让本进程看到真实的占位符属性（REPARSE 位与云重解析标记），默认模式下会被伪装隐藏。
fn set_expose_placeholders() {
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).unwrap();
        if let Some(f) = GetProcAddress(ntdll, s!("RtlSetProcessPlaceholderCompatibilityMode")) {
            let f: extern "system" fn(i8) -> i8 = std::mem::transmute(f);
            let prev = f(2);
            println!("已设置 PHCM_EXPOSE_PLACEHOLDERS（之前模式 {prev}）");
        }
    }
}

// ---------------------------------------------------------------- cloud-info

pub fn cmd_cloud_info(args: &[String]) -> i32 {
    let Some(root) = args.first() else {
        println!("需要同步根路径");
        return 2;
    };
    let w = wide(root);
    let mut buf = vec![0u8; 8192];
    let mut ret = 0u32;
    let r = unsafe {
        CfGetSyncRootInfoByPath(
            PCWSTR(w.as_ptr()),
            CF_SYNC_ROOT_INFO_STANDARD,
            buf.as_mut_ptr() as *mut _,
            buf.len() as u32,
            Some(&mut ret),
        )
    };
    match r {
        Ok(()) => {
            let i = unsafe { &*(buf.as_ptr() as *const CF_SYNC_ROOT_STANDARD_INFO) };
            println!("提供方名称：{}", from_wide(&i.ProviderName));
            println!("提供方版本：{}", from_wide(&i.ProviderVersion));
            println!("提供方状态：{:?}", i.ProviderStatus);
            println!(
                "水合策略：Primary={} Modifier=0x{:X}",
                i.HydrationPolicy.Primary.0, i.HydrationPolicy.Modifier.0
            );
            println!(
                "填充策略：Primary={} Modifier=0x{:X}",
                i.PopulationPolicy.Primary.0, i.PopulationPolicy.Modifier.0
            );
            println!("InSync 策略：{:?}", i.InSyncPolicy);
            0
        }
        Err(e) => {
            println!("CfGetSyncRootInfoByPath 失败：{e:?}");
            1
        }
    }
}

// ---------------------------------------------------------------- cloud-scan

pub fn cmd_cloud_scan(args: &[String]) -> i32 {
    let Some(root) = args.first() else {
        println!("需要根目录");
        return 2;
    };
    let max: usize = opt(args, "--max")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);
    if crate::flag(args, "--expose") {
        set_expose_placeholders();
    }
    let t = Instant::now();
    let mut v = Vec::new();
    enumerate(Path::new(root), max, &mut v);
    println!(
        "枚举 {} 个文件，用时 {:?}（上限 {max}）",
        v.len(),
        t.elapsed()
    );
    let mut by_class: HashMap<Class, usize> = HashMap::new();
    let mut by_ext: HashMap<(String, Class), usize> = HashMap::new();
    let mut attr_combos: HashMap<u32, usize> = HashMap::new();
    let mut tags: HashMap<u32, usize> = HashMap::new();
    for e in &v {
        let c = classify(e.attrs, e.tag);
        *by_class.entry(c).or_default() += 1;
        *attr_combos.entry(e.attrs & !0x20).or_default() += 1; // 去掉 ARCHIVE 方便合并
        if e.tag != 0 {
            *tags.entry(e.tag).or_default() += 1;
        }
        let ext = ext_of(&e.path);
        if is_media(&ext) {
            *by_ext.entry((ext, c)).or_default() += 1;
        }
    }
    for c in [Class::OnlineOnly, Class::Local, Class::Pinned, Class::Plain] {
        println!("  {}：{}", c.name(), by_class.get(&c).copied().unwrap_or(0));
    }
    println!("属性组合（去掉 ARCHIVE）：");
    let mut combos: Vec<_> = attr_combos.into_iter().collect();
    combos.sort_by_key(|c| std::cmp::Reverse(c.1));
    for (a, n) in combos.iter().take(12) {
        println!("  {} × {n}", attr_str(*a));
    }
    println!("重解析标记：");
    for (t, n) in &tags {
        println!("  0x{t:08X} × {n}（云标记={}）", is_cloud_tag(*t));
    }
    println!("媒体文件按扩展名/状态：");
    let mut exts: Vec<_> = by_ext.into_iter().collect();
    exts.sort_by(|a, b| a.0.cmp(&b.0));
    for ((ext, c), n) in exts {
        println!("  .{ext} {}：{n}", c.name());
    }
    0
}

fn is_media(ext: &str) -> bool {
    matches!(
        ext,
        "jpg"
            | "jpeg"
            | "png"
            | "gif"
            | "bmp"
            | "webp"
            | "heic"
            | "heif"
            | "tif"
            | "tiff"
            | "mp4"
            | "mov"
            | "mkv"
            | "avi"
            | "pdf"
            | "docx"
            | "xlsx"
            | "pptx"
    )
}

// ---------------------------------------------------------------- thumb-scan

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    InCache,
    ThumbOnly,
    Default,
    IconOnly,
    ExtIcon,
}

pub fn cmd_thumb_scan(args: &[String]) -> i32 {
    let Some(root) = args.first() else {
        println!("需要根目录");
        return 2;
    };
    let mode = match opt(args, "--mode").unwrap_or("incache") {
        "incache" => Mode::InCache,
        "thumbonly" => Mode::ThumbOnly,
        "default" => Mode::Default,
        "icononly" => Mode::IconOnly,
        "exticon" => Mode::ExtIcon,
        m => {
            println!("未知模式 {m}");
            return 2;
        }
    };
    let want_class = opt(args, "--class").unwrap_or("online");
    let exts: Option<HashSet<String>> =
        opt(args, "--ext").map(|s| s.split(',').map(|x| x.to_lowercase()).collect());
    let limit: usize = opt(args, "--limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let settle: u64 = opt(args, "--settle")
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let smallest = crate::flag(args, "--smallest");
    let max: usize = opt(args, "--max")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);
    let only: Option<String> = opt(args, "--only").map(|s| s.to_string());

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
    }

    let mut all = Vec::new();
    enumerate(Path::new(root), max, &mut all);
    let before_tree: HashMap<PathBuf, u32> =
        all.iter().map(|e| (e.path.clone(), e.attrs)).collect();

    let mut cands: Vec<Entry> = all
        .iter()
        .filter(|e| {
            let c = classify(e.attrs, e.tag);
            let class_ok = match want_class {
                "online" => c == Class::OnlineOnly,
                "local" => c == Class::Local || c == Class::Pinned,
                _ => true,
            };
            let ext = ext_of(&e.path);
            let ext_ok = exts
                .as_ref()
                .map(|s| s.contains(&ext))
                .unwrap_or_else(|| is_media(&ext));
            let only_ok = only
                .as_ref()
                .map(|o| e.path.to_string_lossy().contains(o.as_str()))
                .unwrap_or(true);
            class_ok && ext_ok && only_ok
        })
        .cloned()
        .collect();
    if smallest {
        cands.sort_by_key(|e| e.size);
    }
    cands.truncate(limit);
    println!(
        "模式 {}，类别 {want_class}，候选 {} 个（全树 {} 个文件）",
        mode_name(mode),
        cands.len(),
        all.len()
    );

    let mut hits = 0;
    let mut hydr_now = 0;
    let mut befores = Vec::new();
    let t_all = Instant::now();
    for (i, e) in cands.iter().enumerate() {
        let b = placeholder(&e.path);
        let t = Instant::now();
        let res = get_image(&e.path, mode);
        let dt = t.elapsed();
        let a = placeholder(&e.path);
        let h = hydrated(&b, &a);
        if h {
            hydr_now += 1;
        }
        if res.is_ok() {
            hits += 1;
        }
        let rel = e
            .path
            .strip_prefix(root)
            .unwrap_or(&e.path)
            .to_string_lossy();
        println!(
            "#{i:03} .{} {}B {} -> {} | {:?} | 水合={} | 前 on_disk={} 后 on_disk={} 后 attrs={}",
            ext_of(&e.path),
            e.size,
            classify(e.attrs, e.tag).name(),
            match &res {
                Ok(s) => format!("命中 {s}"),
                Err(err) => format!("未取得 {:?}", err.code()),
            },
            dt,
            h,
            b.on_disk,
            a.on_disk,
            attr_str(a.attrs)
        );
        println!("      路径（仅本地日志）：{rel}");
        befores.push((e.path.clone(), b));
    }
    println!(
        "调用阶段：取得 {hits}/{}，调用后立即观测到水合 {hydr_now}，用时 {:?}",
        cands.len(),
        t_all.elapsed()
    );

    // 等待异步水合（同步客户端或缩略图宿主进程可能稍后才下载）
    pump_for(Duration::from_secs(settle));
    let mut hydr_late = 0;
    for (p, b) in &befores {
        let a = placeholder(p);
        if hydrated(b, &a) {
            hydr_late += 1;
            println!(
                "  延迟水合：{}  {} -> {}",
                p.strip_prefix(root).unwrap_or(p).display(),
                ph_str(b),
                ph_str(&a)
            );
        }
    }
    let tested: HashSet<&PathBuf> = befores.iter().map(|(p, _)| p).collect();
    let mut after_all = Vec::new();
    enumerate(Path::new(root), max, &mut after_all);
    let mut other_changes = 0;
    for e in &after_all {
        if let Some(&old) = before_tree.get(&e.path) {
            let was_online = old & (A_RECALL_ON_DATA_ACCESS | A_OFFLINE) != 0;
            let now_online = e.attrs & (A_RECALL_ON_DATA_ACCESS | A_OFFLINE) != 0;
            if was_online != now_online && !tested.contains(&e.path) {
                other_changes += 1;
            }
        }
    }
    println!(
        "等待 {settle}s 后：被测文件累计水合 {hydr_late}/{}；未被本程序触碰却改变在线状态的文件 {other_changes} 个（归因于同步客户端）",
        befores.len()
    );
    unsafe { CoUninitialize() };
    0
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::InCache => "IShellItemImageFactory SIIGBF_INCACHEONLY|THUMBNAILONLY",
        Mode::ThumbOnly => "IShellItemImageFactory SIIGBF_THUMBNAILONLY（允许提取）",
        Mode::Default => "IShellItemImageFactory SIIGBF_RESIZETOFIT（缩略图失败回退图标）",
        Mode::IconOnly => "IShellItemImageFactory SIIGBF_ICONONLY",
        Mode::ExtIcon => "SHGetFileInfoW SHGFI_USEFILEATTRIBUTES（仅按扩展名）",
    }
}

fn get_image(p: &Path, mode: Mode) -> Result<String> {
    let w = wide(&p.to_string_lossy());
    if mode == Mode::ExtIcon {
        let mut sfi = SHFILEINFOW::default();
        let r = unsafe {
            SHGetFileInfoW(
                PCWSTR(w.as_ptr()),
                FILE_ATTRIBUTE_NORMAL,
                Some(&mut sfi),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_USEFILEATTRIBUTES | SHGFI_ICON | SHGFI_LARGEICON | SHGFI_SYSICONINDEX,
            )
        };
        if r == 0 || sfi.hIcon.is_invalid() {
            return Err(Error::from_thread());
        }
        unsafe {
            let _ = DestroyIcon(sfi.hIcon);
        }
        return Ok(format!("系统图标索引 {}", sfi.iIcon));
    }
    let f: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None)? };
    let flags = match mode {
        Mode::InCache => SIIGBF(SIIGBF_INCACHEONLY.0 | SIIGBF_THUMBNAILONLY.0),
        Mode::ThumbOnly => SIIGBF_THUMBNAILONLY,
        Mode::Default => SIIGBF_RESIZETOFIT,
        Mode::IconOnly => SIIGBF_ICONONLY,
        Mode::ExtIcon => unreachable!(),
    };
    let hb = unsafe { f.GetImage(SIZE { cx: 256, cy: 256 }, flags)? };
    let mut bm = BITMAP::default();
    unsafe {
        GetObjectW(
            HGDIOBJ(hb.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut _),
        );
        let _ = DeleteObject(HGDIOBJ(hb.0));
    }
    Ok(format!("{}x{}", bm.bmWidth, bm.bmHeight))
}

// ---------------------------------------------------------------- placeholder / read-file

pub fn cmd_placeholder(args: &[String]) -> i32 {
    for a in args {
        println!("{}", ph_str(&placeholder(Path::new(a))));
    }
    0
}

/// 显式读取全部内容：模拟用户明确“打开/下载”时允许获取原文件的路径。
pub fn cmd_read_file(args: &[String]) -> i32 {
    let Some(f) = args.first() else {
        println!("需要文件路径");
        return 2;
    };
    let p = Path::new(f);
    let b = placeholder(p);
    println!("读取前：{}", ph_str(&b));
    let t = Instant::now();
    match std::fs::read(p) {
        Ok(data) => println!("读取 {} 字节，用时 {:?}", data.len(), t.elapsed()),
        Err(e) => {
            println!("读取失败：{e}");
            return 1;
        }
    }
    let a = placeholder(p);
    println!("读取后：{}", ph_str(&a));
    println!("发生水合：{}", hydrated(&b, &a));
    0
}
