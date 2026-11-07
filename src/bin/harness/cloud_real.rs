//! 真实云盘只读核查（Issue #14）：`harness cloud-real`。
//!
//! 在用户真实的 OneDrive／iCloud 同步目录上，用**产品自己的代码**走一遍会碰到云文件的入口（目录枚举与元数据、云状态判定、
//! 原生缩略图守卫、预览守卫、系统缩略图“仅缓存”、搜索遍历、目录监视），并用“整棵目录树前后快照差异”统计水合数：
//! 研究 0003 的方法——属性里的“按需下载”位从有变无的文件就是被下载到本机的文件；只统计，不看内容。
//! 差异再分为“本次抽样接触过的文件”与“其余文件”（对照组）：对照组的变化只能来自同步客户端自己的后台活动。
//!
//! 约束：
//! - 只读。不创建、删除、修改任何文件，不改变“始终保留／释放空间”状态；不打开仅在线文件的数据流。
//! - 不记录也不输出任何文件名或路径，只输出数量统计（写入 `artifacts/state/cloud-real.json`）。
//! - 没有发现的云盘目录直接跳过。

use crate::ctx::Res;
use serde_json::{Value, json};
use slint_file_manager::browse::entry::list_dir_with;
use slint_file_manager::browse::watch::{DirWatcher, WatchMode};
use slint_file_manager::cloud::{self, CloudProbe, Guard, Pin, ProviderStatus, SYSTEM};
use slint_file_manager::preview::{self, Failure};
use slint_file_manager::search::{
    BackendPref, Cancel, Event, Query, Request, Scope, engine::run as search_run,
};
use slint_file_manager::thumbs::decode::{Limits, generate};
use slint_file_manager::thumbs::shell::{ShellProvider, SystemShell};
use slint_file_manager::thumbs::{Fail, Source};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

const IMAGE_EXT: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "heic",
];
const VIDEO_EXT: &[&str] = &["mp4", "mov", "mkv", "avi", "wmv", "m4v"];

/// 每类抽样数量上限（控制对真实文件的接触面）。
const SAMPLE_ONLINE_IMAGES: usize = 40;
const SAMPLE_ONLINE_VIDEOS: usize = 20;
const SAMPLE_ONLINE_OTHERS: usize = 20;
const SAMPLE_LOCAL_IMAGES: usize = 20;
const SAMPLE_PIN_CHECK: usize = 300;
const SAMPLE_PS_DIRS: usize = 12;

struct Snap {
    /// 路径 → 属性（只在内存里，不输出）。
    files: HashMap<PathBuf, u32>,
    dirs: Vec<PathBuf>,
    recall_dirs: u64,
    hidden_files: u64,
    hidden_dirs: u64,
    errors: u64,
    ms: u128,
}

/// 整树元数据快照：只用 `read_dir` 的目录项元数据（FindNextFile 数据），不打开任何文件；不进入带“按需下载”标记的文件夹。
fn scan(root: &Path) -> Snap {
    let t0 = Instant::now();
    let mut s = Snap {
        files: HashMap::new(),
        dirs: Vec::new(),
        recall_dirs: 0,
        hidden_files: 0,
        hidden_dirs: 0,
        errors: 0,
        ms: 0,
    };
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            s.errors += 1;
            continue;
        };
        s.dirs.push(d.clone());
        for de in rd {
            let Ok(de) = de else {
                s.errors += 1;
                continue;
            };
            let Ok(md) = de.metadata() else {
                s.errors += 1;
                continue;
            };
            let attrs = md.file_attributes();
            if attrs & 0x2 != 0 {
                if attrs & cloud::ATTR_DIRECTORY != 0 {
                    s.hidden_dirs += 1;
                } else {
                    s.hidden_files += 1;
                }
            }
            if attrs & cloud::ATTR_DIRECTORY != 0 {
                let link = de.file_type().map(|t| t.is_symlink()).unwrap_or(false);
                if link {
                    continue;
                }
                if cloud::is_recall(attrs) {
                    s.recall_dirs += 1;
                } else {
                    stack.push(de.path());
                }
            } else {
                s.files.insert(de.path(), attrs);
            }
        }
    }
    s.ms = t0.elapsed().as_millis();
    s
}

fn ext_of(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// 均匀抽样（按路径排序后等距取），保证重复运行大体一致。
fn pick(mut v: Vec<PathBuf>, n: usize) -> Vec<PathBuf> {
    v.sort();
    if v.len() <= n {
        return v;
    }
    let step = v.len() as f64 / n as f64;
    (0..n)
        .map(|i| v[(i as f64 * step) as usize].clone())
        .collect()
}

fn count(m: &mut BTreeMap<String, u64>, k: &str) {
    *m.entry(k.to_string()).or_default() += 1;
}

fn map_json(m: &BTreeMap<String, u64>) -> Value {
    json!(m)
}

/// 目录枚举与云状态：对每个目录用产品的 `list_dir_with`（真实云文件 API）。
fn enumerate_all(snap: &Snap) -> Value {
    let t0 = Instant::now();
    let mut files: BTreeMap<String, u64> = BTreeMap::new();
    let mut dirs: BTreeMap<String, u64> = BTreeMap::new();
    let mut errors = 0u64;
    let mut roots_seen: BTreeMap<String, u64> = BTreeMap::new();
    for d in &snap.dirs {
        match list_dir_with(d, &SYSTEM) {
            Ok((entries, root)) => {
                if let Some(r) = root {
                    count(
                        &mut roots_seen,
                        &format!("{}:{}", cloud::provider_label(&r.provider), r.status.code()),
                    );
                }
                for e in entries {
                    let m = if e.is_dir { &mut dirs } else { &mut files };
                    count(m, e.cloud.code());
                }
            }
            Err(_) => errors += 1,
        }
    }
    json!({
        "directories_listed": snap.dirs.len(),
        "files_by_state": map_json(&files),
        "dirs_by_state": map_json(&dirs),
        "provider_status_per_listing": map_json(&roots_seen),
        "list_errors": errors,
        "elapsed_ms": t0.elapsed().as_millis() as u64,
    })
}

/// 属性位与云文件 API 的对照：属性里的“始终保留”位与占位信息的固定状态是否一致（抽样）。
fn pin_check(snap: &Snap) -> Value {
    let locals: Vec<PathBuf> = snap
        .files
        .iter()
        .filter(|(_, a)| !cloud::is_recall(**a))
        .map(|(p, _)| p.clone())
        .collect();
    let sample = pick(locals, SAMPLE_PIN_CHECK);
    let mut table: BTreeMap<String, u64> = BTreeMap::new();
    for p in &sample {
        let attrs = snap.files[p];
        let ph = SYSTEM.placeholder(p);
        let pin = match ph {
            None => "无占位信息",
            Some(x) => match x.pin {
                Pin::Pinned => "pin=固定",
                Pin::Unpinned => "pin=取消固定",
                Pin::Excluded => "pin=排除",
                Pin::Inherit => "pin=继承",
                Pin::Unspecified => "pin=未指定",
            },
        };
        let bits = format!(
            "属性P={} U={}",
            (attrs & cloud::ATTR_PINNED != 0) as u8,
            (attrs & cloud::ATTR_UNPINNED != 0) as u8
        );
        count(&mut table, &format!("{bits} / {pin}"));
    }
    json!({"sampled_local_files": sample.len(), "attr_bits_vs_placeholder": map_json(&table)})
}

/// 与 PowerShell 的 `Attributes` 对照（独立于本程序的读取代码）：抽样若干目录，比较每个文件的“按需下载”位是否一致。
fn powershell_check(snap: &Snap) -> Value {
    let dirs_with_files: Vec<PathBuf> = {
        let mut set: HashSet<PathBuf> = HashSet::new();
        for p in snap.files.keys() {
            if let Some(d) = p.parent() {
                set.insert(d.to_path_buf());
            }
        }
        set.into_iter().collect()
    };
    let sample = pick(dirs_with_files, SAMPLE_PS_DIRS);
    let (mut same, mut diff, mut missing) = (0u64, 0u64, 0u64);
    let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
        Get-ChildItem -LiteralPath $env:SFM_DIR -Force -File | \
        ForEach-Object { '{0}|{1}' -f $_.Name, [int]$_.Attributes }";
    for d in &sample {
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", script])
            .env("SFM_DIR", d)
            .output();
        let Ok(out) = out else { continue };
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut ps: HashMap<String, u32> = HashMap::new();
        for line in text.lines() {
            if let Some((n, a)) = line.trim_end().rsplit_once('|')
                && let Ok(a) = a.trim().parse::<u32>()
            {
                ps.insert(n.to_string(), a);
            }
        }
        for (p, a) in snap
            .files
            .iter()
            .filter(|(p, _)| p.parent() == Some(d.as_path()))
        {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
            match name.and_then(|n| ps.get(&n)) {
                Some(pa) if cloud::is_recall(*pa) == cloud::is_recall(*a) => same += 1,
                Some(_) => diff += 1,
                None => missing += 1,
            }
        }
    }
    json!({"dirs_checked": sample.len(), "recall_flag_agree": same, "recall_flag_differ": diff, "not_listed_by_powershell": missing})
}

struct Sample {
    online_images: Vec<PathBuf>,
    online_videos: Vec<PathBuf>,
    online_others: Vec<PathBuf>,
    local_images: Vec<PathBuf>,
}

fn choose(snap: &Snap) -> Sample {
    let mut oi = Vec::new();
    let mut ov = Vec::new();
    let mut oo = Vec::new();
    let mut li = Vec::new();
    for (p, a) in &snap.files {
        let e = ext_of(p);
        if cloud::is_recall(*a) {
            if IMAGE_EXT.contains(&e.as_str()) {
                oi.push(p.clone());
            } else if VIDEO_EXT.contains(&e.as_str()) {
                ov.push(p.clone());
            } else {
                oo.push(p.clone());
            }
        } else if IMAGE_EXT.contains(&e.as_str()) {
            li.push(p.clone());
        }
    }
    Sample {
        online_images: pick(oi, SAMPLE_ONLINE_IMAGES),
        online_videos: pick(ov, SAMPLE_ONLINE_VIDEOS),
        online_others: pick(oo, SAMPLE_ONLINE_OTHERS),
        local_images: pick(li, SAMPLE_LOCAL_IMAGES),
    }
}

fn source_of(p: &Path) -> Source {
    let md = std::fs::metadata(p).ok();
    Source::new(
        p.to_path_buf(),
        md.as_ref().map_or(0, |m| m.len()),
        md.and_then(|m| m.modified().ok()),
    )
}

fn fail_name(f: &Fail) -> &'static str {
    f.as_str()
}

/// 对抽样文件逐个走会碰到内容的入口（产品代码）；对仅在线文件，期望全部被守卫挡住或只查缓存。
fn exercise(s: &Sample) -> Value {
    let lim = Limits::default();
    let plim = preview::Limits::default();
    let shell = SystemShell;

    let mut thumb_native: BTreeMap<String, u64> = BTreeMap::new();
    let mut preview_online: BTreeMap<String, u64> = BTreeMap::new();
    let mut guard_online: BTreeMap<String, u64> = BTreeMap::new();
    let mut shell_online_img: BTreeMap<String, u64> = BTreeMap::new();
    let mut shell_online_vid: BTreeMap<String, u64> = BTreeMap::new();
    for p in &s.online_images {
        match generate(&source_of(p), &lim) {
            Ok(_) => count(&mut thumb_native, "读取并生成（不应出现）"),
            Err(f) => count(&mut thumb_native, fail_name(&f)),
        }
        match preview::decode_file(p, &plim) {
            Ok(_) => count(&mut preview_online, "读取并解码（不应出现）"),
            Err(Failure::Cloud) => count(&mut preview_online, "cloud（挡住）"),
            Err(_) => count(&mut preview_online, "其他失败"),
        }
        match shell.thumbnail(p, true) {
            Ok(_) => count(&mut shell_online_img, "系统缓存里已有"),
            Err(f) => count(&mut shell_online_img, fail_name(&f)),
        }
    }
    for p in s
        .online_others
        .iter()
        .chain(&s.online_images)
        .chain(&s.online_videos)
    {
        match cloud::open_local_only(p) {
            Ok(_) => count(&mut guard_online, "打开了（不应出现）"),
            Err(Guard::Cloud) => count(&mut guard_online, "cloud（挡住）"),
            Err(Guard::NotFile) => count(&mut guard_online, "not-file"),
            Err(Guard::Io) => count(&mut guard_online, "io"),
        }
    }
    for p in &s.online_videos {
        match shell.thumbnail(p, true) {
            Ok(_) => count(&mut shell_online_vid, "系统缓存里已有"),
            Err(f) => count(&mut shell_online_vid, fail_name(&f)),
        }
    }
    let mut thumb_local: BTreeMap<String, u64> = BTreeMap::new();
    for p in &s.local_images {
        match generate(&source_of(p), &lim) {
            Ok(_) => count(&mut thumb_local, "生成"),
            Err(f) => count(&mut thumb_local, fail_name(&f)),
        }
    }
    json!({
        "sample_sizes": {
            "online_images": s.online_images.len(), "online_videos": s.online_videos.len(),
            "online_others": s.online_others.len(), "local_images": s.local_images.len()},
        "native_thumbnail_on_online_images": map_json(&thumb_native),
        "preview_on_online_images": map_json(&preview_online),
        "guard_open_local_only_on_online_files": map_json(&guard_online),
        "shell_cache_only_on_online_images": map_json(&shell_online_img),
        "shell_cache_only_on_online_videos": map_json(&shell_online_vid),
        "native_thumbnail_on_local_images": map_json(&thumb_local),
    })
}

fn search_check(root: &Path) -> Value {
    let one = |q: &str| {
        let req = Request {
            query: Query::parse(q),
            scope: Scope::Dir(root.to_path_buf()),
            pref: BackendPref::BasicOnly,
        };
        let sum = Mutex::new(None);
        let t0 = Instant::now();
        search_run(&req, None, &Cancel::new(), &|e| {
            if let Event::Done(s) = e {
                *sum.lock().unwrap() = Some(s);
            }
        });
        let s = sum.into_inner().unwrap().unwrap_or_default();
        json!({"dirs": s.dirs, "entries": s.entries, "hits": s.hits,
               "cloud_folders_not_expanded": s.cloud_unexpanded, "denied": s.denied,
               "elapsed_ms": t0.elapsed().as_millis() as u64})
    };
    json!({"pure_walk": one(".zzzq"), "by_extension": one(".jpg")})
}

fn watch_check(root: &Path) -> Value {
    let (tx, rx) = channel();
    let started = DirWatcher::start(root, 1, tx, WatchMode::Notify).is_ok();
    let t0 = Instant::now();
    let mut events = 0u64;
    while t0.elapsed() < Duration::from_secs(3) {
        if rx.recv_timeout(Duration::from_millis(200)).is_ok() {
            events += 1;
        }
    }
    json!({"started": started, "seconds": 3, "events": events})
}

struct DiffStat {
    hydrated: u64,
    dehydrated: u64,
    added: u64,
    removed: u64,
    unchanged: u64,
}

fn diff_part(
    before: &HashMap<PathBuf, u32>,
    after: &HashMap<PathBuf, u32>,
    keep: &dyn Fn(&Path) -> bool,
) -> DiffStat {
    let mut d = DiffStat {
        hydrated: 0,
        dehydrated: 0,
        added: 0,
        removed: 0,
        unchanged: 0,
    };
    for (p, a0) in before {
        if !keep(p) {
            continue;
        }
        match after.get(p) {
            None => d.removed += 1,
            Some(a1) => match (cloud::is_recall(*a0), cloud::is_recall(*a1)) {
                (true, false) => d.hydrated += 1,
                (false, true) => d.dehydrated += 1,
                _ => d.unchanged += 1,
            },
        }
    }
    for p in after.keys() {
        if keep(p) && !before.contains_key(p) {
            d.added += 1;
        }
    }
    d
}

fn diff_json(d: &DiffStat) -> Value {
    json!({"hydrated_online_to_local": d.hydrated, "dehydrated_local_to_online": d.dehydrated,
           "added": d.added, "removed": d.removed, "unchanged": d.unchanged})
}

fn audit_root(label: &str, root: &Path) -> Value {
    println!("核查 {label}……");
    let status = SYSTEM.sync_root(root);
    let before = scan(root);
    let online_before = before
        .files
        .values()
        .filter(|a| cloud::is_recall(**a))
        .count();
    println!(
        "  快照前：{} 个文件（{} 个仅在线）、{} 个文件夹，耗时 {}ms",
        before.files.len(),
        online_before,
        before.dirs.len(),
        before.ms
    );
    let enumeration = enumerate_all(&before);
    let pins = pin_check(&before);
    let ps = powershell_check(&before);
    let sample = choose(&before);
    let touched: HashSet<PathBuf> = sample
        .online_images
        .iter()
        .chain(&sample.online_videos)
        .chain(&sample.online_others)
        .chain(&sample.local_images)
        .cloned()
        .collect();
    let exercised = exercise(&sample);
    let search = search_check(root);
    let watch = watch_check(root);
    let after = scan(root);
    let online_after = after
        .files
        .values()
        .filter(|a| cloud::is_recall(**a))
        .count();
    println!(
        "  快照后：{} 个文件（{} 个仅在线），耗时 {}ms",
        after.files.len(),
        online_after,
        after.ms
    );
    let online_touched = |p: &Path| touched.contains(p);
    let outside = |p: &Path| !touched.contains(p);
    let total = diff_part(&before.files, &after.files, &|_| true);
    let in_sample = diff_part(&before.files, &after.files, &online_touched);
    let control = diff_part(&before.files, &after.files, &outside);
    json!({
        "provider": status.as_ref().map(|s| cloud::provider_label(&s.provider).to_string()),
        "provider_status": status.as_ref().map(|s| s.status.code()),
        "provider_ok": status.as_ref().map(|s| s.status == ProviderStatus::Connected),
        "tree": {
            "files": before.files.len(), "online_only_files": online_before,
            "folders_entered": before.dirs.len(), "recall_folders_not_entered": before.recall_dirs,
            "hidden_files": before.hidden_files, "hidden_folders": before.hidden_dirs,
            "scan_errors": before.errors, "scan_ms": before.ms as u64,
        },
        "enumeration_and_state": enumeration,
        "attribute_bits_vs_cloud_api": pins,
        "independent_powershell_attribute_check": ps,
        "content_entry_points_on_samples": exercised,
        "search": search,
        "directory_watch": watch,
        "hydration_diff": {
            "method": "整树前后快照；按需下载位从有变无 = 被下载到本机",
            "all_files": diff_json(&total),
            "touched_by_this_audit_sample": {"files": touched.len(), "diff": diff_json(&in_sample)},
            "control_untouched": diff_json(&control),
            "attribution": "对照组的变化来自同步客户端自身的后台活动，与本审计无关",
        },
    })
}

pub fn run(out: &Path) -> Res<i32> {
    let roots = cloud::roots::discover();
    let list = [
        ("OneDrive", roots.onedrive),
        ("iCloud 云盘", roots.icloud),
        ("iCloud 照片", roots.icloud_photos),
    ];
    let mut report = serde_json::Map::new();
    let mut hydrated_in_sample = 0u64;
    let mut found = 0;
    for (label, root) in list {
        let Some(root) = root else {
            report.insert(label.into(), json!({"found": false}));
            println!("{label}：本机未发现同步目录，跳过");
            continue;
        };
        found += 1;
        let v = audit_root(label, &root);
        hydrated_in_sample +=
            v["hydration_diff"]["touched_by_this_audit_sample"]["diff"]["hydrated_online_to_local"]
                .as_u64()
                .unwrap_or(0);
        report.insert(label.into(), v);
    }
    let v = json!({
        "note": "只读核查；不含任何文件名或路径，只有统计",
        "roots_found": found,
        "hydrated_among_files_touched_by_audit": hydrated_in_sample,
        "roots": report,
    });
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(out, serde_json::to_string_pretty(&v)? + "\n")?;
    println!("统计输出 {}", out.display());
    println!("本次审计接触过的文件中，被下载的数量：{hydrated_in_sample}");
    Ok(if hydrated_in_sample == 0 { 0 } else { 1 })
}
