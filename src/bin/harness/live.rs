//! 磁盘、收藏与目录变化同步的无头脚本（Issue #7）。命令：`harness live`。
//!
//! 与 `run` 不同：这里要等真实的文件系统通知、监视线程和磁盘轮询线程，等待时间取决于真实时钟，
//! 所以不参与“重复运行记录逐项一致”的比较，只断言结果，并把每步结论写进 `artifacts/state/live.json`。
//!
//! - 外部变化：新建、重命名、删除、修改、变更爆发、快速换目录、目录消失再恢复、后台标签、本应用刷新不产生重复。
//! - 磁盘：用 `subst` 创建真实的虚拟盘符，经真实的磁盘轮询线程观察“插入／拔出”，不使用注入的列表；
//!   注入的列表只用于侧栏外观、收藏失效判定和“可弹出磁盘用轮询监视”。
//! - 收藏：增删、重启保留、失效（目录不存在、磁盘未连接、盘符被另一块盘占用）、损坏文件降级。
//!
//! 所有数据都在 `%TEMP%\sfm-harness\scratch\…` 与 `sessions\…`，不碰用户真实目录与真实收藏。

use crate::ctx::{Ctx, Opts, Res};
use crate::fixture;
use serde_json::{Value, json};
use slint_file_manager::ContentState;
use slint_file_manager::app::{DriveSource, LiveEnv};
use slint_file_manager::browse::LoadState;
use slint_file_manager::browse::places::{
    Drive, DriveKind, FavLoad, FavState, Favorites, FavoritesStore, drive_letter, drive_mask,
    volume_serial,
};
use slint_file_manager::browse::watch::WatchMode;
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

const SIZE: (u32, u32) = (1100, 720);

pub struct Rep {
    pub items: Vec<Value>,
    pub fails: Vec<String>,
    pub review: Option<PathBuf>,
}

impl Rep {
    fn ok(&mut self, name: &str, cond: bool, detail: impl Into<String>) {
        let detail = detail.into();
        if !cond {
            eprintln!("  未通过：{name}：{detail}");
            self.fails.push(format!("{name}：{detail}"));
        }
        self.items
            .push(json!({"step": name, "passed": cond, "detail": detail}));
    }

    fn snap(&self, c: &Ctx, file: &str) -> Res<()> {
        if let Some(d) = &self.review {
            c.snapshot()?.write_png(&d.join(file))?;
        }
        Ok(())
    }
}

// ---------- 等待与读取 ----------

/// 推进模拟时钟与加载直到条件成立；真实等待上限 6 秒。
fn wait_until(c: &Ctx, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(6);
    loop {
        if cond() {
            return true;
        }
        if Instant::now() > end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
        c.advance(100);
        c.pump();
    }
}

/// 让监视与刷新自然走完（真实 ~0.4 秒）。
fn quiet(c: &Ctx) {
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(20));
        c.advance(100);
        c.pump();
    }
}

fn names(c: &Ctx) -> Vec<String> {
    c.ctl.names()
}

fn disk_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn unique(v: &[String]) -> bool {
    v.iter().collect::<HashSet<_>>().len() == v.len()
}

fn tab_unavailable(c: &Ctx) -> bool {
    let ws = c.ctl.ws();
    ws.active_tab(ws.active_pane()).is_unavailable()
}

fn tab_ready(c: &Ctx) -> bool {
    let ws = c.ctl.ws();
    *ws.active_tab(ws.active_pane()).state() == LoadState::Ready
}

fn cursor_name(c: &Ctx) -> Option<String> {
    let ws = c.ctl.ws();
    let t = ws.active_tab(ws.active_pane());
    t.cursor()
        .and_then(|i| t.rows().get(i))
        .map(|r| r.entry.name.clone())
}

fn size_of(c: &Ctx, name: &str) -> Option<u64> {
    let ws = c.ctl.ws();
    ws.active_tab(ws.active_pane())
        .rows()
        .iter()
        .find(|r| r.entry.name == name)
        .map(|r| r.entry.size)
}

fn live_env(watch: bool, drives: DriveSource, fav: Option<PathBuf>) -> LiveEnv {
    LiveEnv {
        drives,
        favorites: fav.map(FavoritesStore::at),
        onedrive: None,
        icloud: None,
        icloud_photos: None,
        watch,
    }
}

fn open(start: &Path, live: LiveEnv) -> Res<Ctx> {
    Ctx::open(
        SIZE,
        1.0,
        Opts {
            start: start.to_path_buf(),
            live: Some(live),
            ..Opts::default()
        },
    )
}

fn scratch(name: &str) -> Res<PathBuf> {
    fixture::fresh_dir(&fixture::scratch_dir(name))
}

// ---------- 展示用环境（截图与基线共用） ----------

/// 固定的展示环境：磁盘 = 样例目录所在盘 + 两个虚构盘；收藏 = 一个有效、一个“盘符被另一块盘占用”、一个“磁盘未连接”；
/// OneDrive 指向样例目录里的“空文件夹”。不监视目录，截图确定。
pub fn showcase_env(tag: &str) -> Res<LiveEnv> {
    let root = fixture::root();
    let l = drive_letter(&root).ok_or("样例目录不在盘符路径上")?;
    let serial = volume_serial(&root);
    let mut others = ['D', 'E', 'F', 'G', 'H'].into_iter().filter(|x| *x != l);
    let (o1, o2, o3) = (
        others.next().unwrap(),
        others.next().unwrap(),
        others.next().unwrap(),
    );
    let drives = vec![
        Drive::new(l, "系统", DriveKind::Fixed, serial),
        Drive::new(o1, "数据", DriveKind::Fixed, Some(2222)),
        Drive::new(o2, "U盘", DriveKind::Removable, Some(3333)),
    ];
    let mut drives = drives;
    drives.sort_by_key(|d| d.letter);
    let dir = fixture::session_dir(tag)?;
    let store = FavoritesStore::at(&dir);
    let mut f = Favorites::default();
    f.add(&fixture::path(fixture::PHOTOS), serial);
    f.add(&PathBuf::from(format!("{o2}:\\旅行素材")), Some(9999));
    f.add(&PathBuf::from(format!("{o3}:\\项目归档")), Some(1234));
    store.save(&f)?;
    Ok(LiveEnv {
        drives: DriveSource::Fixed(Rc::new(RefCell::new(drives))),
        favorites: Some(FavoritesStore::at(&dir)),
        onedrive: Some(fixture::path(fixture::EMPTY)),
        icloud: None,
        icloud_photos: None,
        watch: false,
    })
}

// ---------- 脚本 ----------

pub fn run(review: Option<PathBuf>) -> Res<(Vec<Value>, Vec<String>)> {
    if let Some(d) = &review {
        fs::create_dir_all(d)?;
    }
    let mut r = Rep {
        items: Vec::new(),
        fails: Vec::new(),
        review,
    };
    sidebar_and_favorites_ui(&mut r)?;
    favorites_persistence(&mut r)?;
    external_changes(&mut r)?;
    background_tab(&mut r)?;
    poll_mode(&mut r)?;
    real_drive_plug(&mut r)?;
    Ok((r.items, r.fails))
}

/// A. 侧栏：磁盘、收藏、云盘入口，失效收藏，星标。
fn sidebar_and_favorites_ui(r: &mut Rep) -> Res<()> {
    let env = showcase_env("live-showcase")?;
    let c = open(&fixture::root(), env)?;
    let rows = c.ctl.sidebar_rows();
    let ids: Vec<&str> = rows.iter().map(|(i, _)| i.as_str()).collect();
    r.ok(
        "A1 侧栏包含主目录、收藏、磁盘与云盘分组",
        ids.contains(&"home")
            && ids.contains(&"hdr-favorites")
            && ids.contains(&"hdr-this-pc")
            && ids.contains(&"hdr-cloud")
            && ids.contains(&"onedrive")
            && !ids.contains(&"icloud"),
        format!("{ids:?}"),
    );
    let drive_rows = ids.iter().filter(|i| i.starts_with("drive:")).count();
    r.ok(
        "A2 磁盘行数等于注入的磁盘数",
        drive_rows == 3,
        format!("{drive_rows}"),
    );
    let states = c.ctl.favorite_states();
    r.ok(
        "A3 收藏状态：有效、盘符被换、磁盘未连接",
        states == [FavState::Ok, FavState::OtherVolume, FavState::Offline],
        format!("{states:?}"),
    );
    // 侧栏进入与地址栏进入同一浏览流程
    c.click(&c.button("照片")?);
    c.pump();
    let photos = fixture::path(fixture::PHOTOS);
    r.ok(
        "A4 点收藏进入该目录并读取",
        c.ctl.location() == Some(photos.clone()) && c.ctl.names().len() == 12,
        format!("{:?}", c.ctl.location()),
    );
    r.ok(
        "A5 当前位置对应的侧栏项高亮",
        c.win.get_sidebar_current() == "fav:0",
        c.win.get_sidebar_current().to_string(),
    );
    r.ok("A6 工具栏星标为已收藏", c.win.get_current_favorite(), "");
    r.snap(&c, "01-sidebar-drives-favorites.png")?;
    // 失效收藏：不导航，说明原因
    let before = c.ctl.location();
    c.click(&c.button("旅行素材")?);
    c.pump();
    let msg = c.win.get_status_message().to_string();
    r.ok(
        "A7 点失效收藏（盘符被另一块盘占用）不导航并提示",
        c.ctl.location() == before && msg.contains("已失效") && msg.contains("另一块磁盘"),
        msg.clone(),
    );
    r.snap(&c, "02-invalid-favorite-clicked.png")?;
    c.click(&c.button("项目归档")?);
    let msg = c.win.get_status_message().to_string();
    r.ok(
        "A8 点失效收藏（磁盘未连接）不导航并提示",
        c.ctl.location() == before && msg.contains("未连接"),
        msg,
    );
    // 地址栏进入磁盘根：与侧栏同一流程
    let l = drive_letter(&photos).unwrap();
    c.click(&c.button(&format!("系统 ({l}:)"))?);
    c.pump();
    r.ok(
        "A9 点磁盘进入其根目录",
        c.ctl.location() == Some(PathBuf::from(format!("{l}:\\"))),
        format!("{:?}", c.ctl.location()),
    );
    // 注入列表模拟插拔：U 盘行随之增删
    let drives = c.drives.clone().ok_or("缺少注入的磁盘列表")?;
    let usb = drives
        .borrow()
        .iter()
        .find(|d| d.kind == DriveKind::Removable)
        .cloned()
        .ok_or("没有可移动磁盘")?;
    drives.borrow_mut().retain(|d| d.letter != usb.letter);
    c.ctl.tick();
    c.pump();
    let gone = !c
        .ctl
        .sidebar_rows()
        .iter()
        .any(|(i, _)| i == &format!("drive:{}", usb.letter));
    r.ok("A10 移除 U 盘后侧栏立即去掉该盘", gone, "");
    r.ok(
        "A11 U 盘移除后其上的收藏仍为失效且不指向别的盘",
        c.ctl.favorite_states()[1] == FavState::Offline,
        format!("{:?}", c.ctl.favorite_states()),
    );
    drives.borrow_mut().push(usb.clone());
    drives.borrow_mut().sort_by_key(|d| d.letter);
    c.ctl.tick();
    let back = c
        .ctl
        .sidebar_rows()
        .iter()
        .any(|(i, _)| i == &format!("drive:{}", usb.letter));
    r.ok("A12 重新插入后侧栏恢复该盘", back, "");
    r.ok(
        "A13 同盘符的另一块盘不被当作原收藏",
        c.ctl.favorite_states()[1] == FavState::OtherVolume,
        format!("{:?}", c.ctl.favorite_states()),
    );
    Ok(())
}

/// B. 收藏增删、重启保留、目录被删后失效、损坏降级。
fn favorites_persistence(r: &mut Rep) -> Res<()> {
    let dir = scratch("live-fav-data")?;
    let work = scratch("live-fav-work")?;
    let a = work.join("项目A");
    let b = work.join("项目B");
    fs::create_dir_all(&a)?;
    fs::create_dir_all(&b)?;
    let real: Vec<Drive> = slint_file_manager::browse::places::enumerate_drives();
    let mk = || {
        live_env(
            false,
            DriveSource::Fixed(Rc::new(RefCell::new(real.clone()))),
            Some(dir.clone()),
        )
    };
    let c = open(&a, mk())?;
    r.ok("B1 初始没有收藏", c.ctl.favorite_paths().is_empty(), "");
    c.ctrl("d");
    c.pump();
    r.ok(
        "B2 Ctrl+D 收藏当前文件夹并立即写盘",
        c.ctl.favorite_paths() == vec![a.clone()] && dir.join("favorites.json").exists(),
        format!("{:?}", c.ctl.favorite_paths()),
    );
    r.ok(
        "B3 侧栏出现收藏行且星标选中",
        c.button("项目A").is_ok() && c.win.get_current_favorite(),
        "",
    );
    c.ctl.navigate_path(0, b.clone());
    c.pump();
    r.ok(
        "B4 换到未收藏的位置星标取消选中",
        !c.win.get_current_favorite(),
        "",
    );
    c.click(&c.button("收藏当前位置")?);
    r.ok(
        "B5 点工具栏星标收藏第二个文件夹",
        c.ctl.favorite_paths() == vec![a.clone(), b.clone()],
        format!("{:?}", c.ctl.favorite_paths()),
    );
    let json: Value = serde_json::from_slice(&fs::read(dir.join("favorites.json"))?)?;
    r.ok(
        "B6 文件带版本且路径与卷号齐全",
        json["version"] == 1 && json["items"].as_array().is_some_and(|a| a.len() == 2),
        json["version"].to_string(),
    );
    drop(c);
    // 重启
    let c = open(&a, mk())?;
    r.ok(
        "B7 重启后收藏保留且顺序不变",
        c.ctl.favorite_paths() == vec![a.clone(), b.clone()],
        format!("{:?}", c.ctl.favorite_paths()),
    );
    r.ok(
        "B8 重启后收藏有效",
        c.ctl.favorite_states() == [FavState::Ok, FavState::Ok],
        format!("{:?}", c.ctl.favorite_states()),
    );
    // 目录被删：点击时即时判定为失效
    fs::remove_dir_all(&b)?;
    let before = c.ctl.location();
    c.click(&c.button("项目B")?);
    c.pump();
    let msg = c.win.get_status_message().to_string();
    r.ok(
        "B9 收藏的目录被删后点击：提示失效且不导航",
        c.ctl.location() == before && msg.contains("已失效") && msg.contains("不存在"),
        msg,
    );
    // 侧栏 × 取消收藏
    c.click(&c.button("取消收藏 项目B")?);
    r.ok(
        "B10 点 × 取消收藏并写盘",
        c.ctl.favorite_paths() == vec![a.clone()]
            && fs::read_to_string(dir.join("favorites.json"))?.contains("项目A")
            && !fs::read_to_string(dir.join("favorites.json"))?.contains("项目B"),
        format!("{:?}", c.ctl.favorite_paths()),
    );
    drop(c);
    // 同盘符被另一块盘占用：手写一个卷号对不上的收藏
    let store = FavoritesStore::at(&dir);
    let mut f = Favorites::default();
    f.add(&a, volume_serial(&a).map(|s| s ^ 0xFFFF));
    store.save(&f)?;
    let c = open(&a, mk())?;
    r.ok(
        "B11 卷号不符：收藏标为失效（不自动指向现在占用该盘符的磁盘）",
        c.ctl.favorite_states() == [FavState::OtherVolume],
        format!("{:?}", c.ctl.favorite_states()),
    );
    drop(c);
    // 损坏降级
    fs::write(
        dir.join("favorites.json"),
        b"{ \"version\": 1, \"items\": [ oops",
    )?;
    let c = open(&a, mk())?;
    let msg = c.win.get_status_message().to_string();
    let backups = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("favorites.backup-")
        })
        .count();
    r.ok(
        "B12 收藏文件损坏：降级为空收藏、状态栏说明、原文件备份",
        c.ctl.favorite_paths().is_empty() && msg.contains("已备份") && backups == 1,
        msg,
    );
    c.ctrl("d");
    r.ok(
        "B13 降级后仍可继续收藏并写出新文件",
        c.ctl.favorite_paths() == vec![a.clone()]
            && matches!(
                FavoritesStore::at(&dir).load(),
                FavLoad::Restored(ref f) if f.len() == 1
            ),
        "",
    );
    Ok(())
}

/// C. 外部变化：新建、重命名、删除、修改、爆发、快速换目录、目录消失与恢复、本应用刷新。
fn external_changes(r: &mut Rep) -> Res<()> {
    let s = scratch("live-ext")?;
    for n in ["a.txt", "b.txt", "c.txt"] {
        fs::write(s.join(n), n)?;
    }
    let c = open(&s, live_env(true, DriveSource::System, None))?;
    r.ok(
        "C1 打开即监视该目录",
        c.ctl.watched_paths() == vec![s.clone()] && names(&c) == ["a.txt", "b.txt", "c.txt"],
        format!("{:?} {:?}", c.ctl.watched_paths(), names(&c)),
    );
    r.ok(
        "C1b 本地目录使用句柄通知模式",
        c.ctl.watch_mode_of(&s) == Some(WatchMode::Notify),
        "",
    );
    c.click(&c.item("b.txt")?);
    r.ok(
        "C2 选中 b.txt",
        c.ctl.selected_names() == ["b.txt"] && cursor_name(&c).as_deref() == Some("b.txt"),
        "",
    );
    // 新建
    fs::write(s.join("d.txt"), "d")?;
    let seen = wait_until(&c, || names(&c).contains(&"d.txt".to_string()));
    r.ok(
        "C3 外部新建：出现新文件，选择与光标不跳走",
        seen && c.ctl.selected_names() == ["b.txt"] && cursor_name(&c).as_deref() == Some("b.txt"),
        format!("{:?} 光标 {:?}", names(&c), cursor_name(&c)),
    );
    // 重命名
    fs::rename(s.join("a.txt"), s.join("a2.txt"))?;
    let seen = wait_until(&c, || {
        names(&c).contains(&"a2.txt".to_string()) && !names(&c).contains(&"a.txt".to_string())
    });
    r.ok("C4 外部重命名可见", seen, format!("{:?}", names(&c)));
    // 删除（删的不是选中项）
    fs::remove_file(s.join("c.txt"))?;
    let seen = wait_until(&c, || !names(&c).contains(&"c.txt".to_string()));
    r.ok(
        "C5 外部删除可见，选择保持",
        seen && c.ctl.selected_names() == ["b.txt"],
        format!("{:?}", names(&c)),
    );
    // 修改内容（大小变化）
    fs::write(s.join("b.txt"), "0123456789")?;
    let seen = wait_until(&c, || size_of(&c, "b.txt") == Some(10));
    r.ok(
        "C6 外部修改内容：大小更新，选择保持",
        seen && c.ctl.selected_names() == ["b.txt"],
        format!("{:?}", size_of(&c, "b.txt")),
    );
    // 选中项被外部删除：焦点不跳到别的文件之外的奇怪位置（落到相邻条目，选择清掉）
    fs::remove_file(s.join("b.txt"))?;
    let seen = wait_until(&c, || !names(&c).contains(&"b.txt".to_string()));
    r.ok(
        "C7 选中项被外部删除：选择清空，光标落在相邻条目",
        seen && c.ctl.selected_names().is_empty() && cursor_name(&c).is_some(),
        format!("{:?} 光标 {:?}", names(&c), cursor_name(&c)),
    );
    r.snap(&c, "03-external-changes-applied.png")?;

    // 爆发合并：写入线程持续约 0.6 秒，期间界面线程持续推进节拍
    let base = names(&c).len();
    let before = c.ctl.silent_reloads();
    let sd = s.clone();
    let writer = std::thread::spawn(move || {
        for i in 0..300 {
            let _ = fs::write(sd.join(format!("burst{i:03}.txt")), "x");
            if i % 3 == 0 {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        for i in 0..100 {
            let _ = fs::write(sd.join(format!("burst{i:03}.txt")), "xyz");
        }
    });
    let all = wait_until(&c, || names(&c).len() == base + 300);
    let _ = writer.join();
    quiet(&c);
    let reloads = c.ctl.silent_reloads() - before;
    r.ok(
        "C8 变更爆发（400 次写入）：最终内容与磁盘一致、无重复",
        all && sorted(names(&c)) == disk_names(&s) && unique(&names(&c)),
        format!("{} 项，磁盘 {} 项", names(&c).len(), disk_names(&s).len()),
    );
    r.ok(
        "C9 爆发被合并：静默刷新次数远小于写入次数",
        (1..=12).contains(&reloads),
        format!("{reloads} 次"),
    );
    r.ok(
        "C10 爆发结束后安静：没有待处理刷新",
        c.ctl.live_quiet() && c.ctl.pending() == 0,
        "",
    );

    // 快速换目录：旧监视器的通知不能污染当前列表
    let sub = s.join("sub");
    fs::create_dir_all(&sub)?;
    fs::write(sub.join("inside.txt"), "i")?;
    quiet(&c);
    c.ctl.navigate_path(0, sub.clone());
    fs::write(s.join("leak.txt"), "l")?;
    c.pump();
    quiet(&c);
    r.ok(
        "C11 换到子目录后：监视只剩子目录，上级目录的新文件不污染列表",
        c.ctl.watched_paths() == vec![sub.clone()] && names(&c) == ["inside.txt"],
        format!("{:?} {:?}", c.ctl.watched_paths(), names(&c)),
    );
    for i in 0..20 {
        c.ctl
            .navigate_path(0, if i % 2 == 0 { s.clone() } else { sub.clone() });
        let _ = fs::write(sub.join(format!("hop{i}.txt")), "h");
        let _ = fs::write(s.join(format!("hop{i}.txt")), "h");
        if i % 4 == 0 {
            c.pump();
        }
    }
    c.pump();
    quiet(&c);
    let here = c.ctl.location().unwrap();
    r.ok(
        "C12 连续快速换目录 20 次：最终列表与所在目录磁盘内容一致",
        sorted(names(&c)) == disk_names(&here) && unique(&names(&c)),
        format!("{} {} 项", here.display(), names(&c).len()),
    );
    c.ctl.navigate_path(0, s.clone());
    c.pump();
    quiet(&c);
    r.ok(
        "C13 回到上级后看到换目录期间新建的文件",
        names(&c).contains(&"leak.txt".to_string()),
        "",
    );

    // 目录消失再恢复
    let gone = s.join("会消失的目录");
    fs::create_dir_all(&gone)?;
    fs::write(gone.join("x.txt"), "x")?;
    quiet(&c);
    c.ctl.navigate_path(0, gone.clone());
    c.pump();
    r.ok("C14 进入将被删除的目录", names(&c) == ["x.txt"], "");
    fs::remove_dir_all(&gone)?;
    let down = wait_until(&c, || tab_unavailable(&c));
    r.ok(
        "C15 目录被删：显示不可用状态而不是崩溃",
        down && c.win.get_content_state() == ContentState::NotFound && names(&c).is_empty(),
        format!("{:?}", c.win.get_content_state()),
    );
    r.snap(&c, "04-directory-removed-unavailable.png")?;
    fs::create_dir_all(&gone)?;
    fs::write(gone.join("y.txt"), "y")?;
    let back = wait_until(&c, || tab_ready(&c) && names(&c) == ["y.txt"]);
    r.ok(
        "C16 目录重新出现：自动恢复浏览",
        back && c.win.get_content_state() == ContentState::Ready,
        format!("{:?}", names(&c)),
    );
    r.ok(
        "C17 恢复后重新监视并能继续看到变化",
        {
            fs::write(gone.join("z.txt"), "z")?;
            wait_until(&c, || names(&c) == ["y.txt", "z.txt"])
        },
        format!("{:?}", names(&c)),
    );

    // 本应用的刷新与监视同时发生：不重复
    c.ctl.navigate_path(0, s.clone());
    c.pump();
    quiet(&c);
    let before = c.ctl.silent_reloads();
    fs::write(s.join("own-op.txt"), "o")?;
    c.ctl.refresh(0);
    c.pump();
    quiet(&c);
    let n = names(&c);
    r.ok(
        "C18 本应用操作后的显式刷新与监视刷新叠加：条目不重复、不遗漏",
        unique(&n) && sorted(n.clone()) == disk_names(&s) && n.contains(&"own-op.txt".to_string()),
        format!("{} 项", n.len()),
    );
    r.ok(
        "C19 显式刷新覆盖了待处理信号：额外静默刷新至多 1 次",
        c.ctl.silent_reloads() - before <= 1,
        format!("{} 次", c.ctl.silent_reloads() - before),
    );
    // F5 可靠刷新入口
    c.special(slint::platform::Key::F5, &[]);
    c.pump();
    r.ok(
        "C20 F5 刷新入口可用",
        tab_ready(&c) && unique(&names(&c)),
        "",
    );
    Ok(())
}

/// D. 后台标签：不监视，回到前台时补一次静默刷新。
fn background_tab(r: &mut Rep) -> Res<()> {
    let s = scratch("live-bg")?;
    let t2 = scratch("live-bg-2")?;
    fs::write(s.join("one.txt"), "1")?;
    fs::write(t2.join("other.txt"), "o")?;
    let c = open(&s, live_env(true, DriveSource::System, None))?;
    c.ctl.new_tab(0);
    c.pump();
    c.ctl.navigate_path(0, t2.clone());
    c.pump();
    r.ok(
        "D1 只监视可见标签的目录",
        c.ctl.watched_paths() == vec![t2.clone()],
        format!("{:?}", c.ctl.watched_paths()),
    );
    fs::write(s.join("two.txt"), "2")?;
    quiet(&c);
    c.ctl.select_tab(0, 0);
    c.pump();
    quiet(&c);
    r.ok(
        "D2 回到后台期间被改动的标签：显示最新内容",
        names(&c) == ["one.txt", "two.txt"],
        format!("{:?}", names(&c)),
    );
    r.ok(
        "D3 切换标签后监视跟随",
        c.ctl.watched_paths() == vec![s.clone()],
        format!("{:?}", c.ctl.watched_paths()),
    );
    Ok(())
}

/// E. 可弹出磁盘用轮询监视（不持有目录句柄）。
fn poll_mode(r: &mut Rep) -> Res<()> {
    let s = scratch("live-poll")?;
    fs::write(s.join("p1.txt"), "1")?;
    let l = drive_letter(&s).ok_or("临时目录不在盘符上")?;
    let serial = volume_serial(&s);
    // 把该盘声明为可弹出（USB）：只影响监视模式
    let drives = vec![Drive::new(l, "移动硬盘", DriveKind::Fixed, serial).with_ejectable(true)];
    let c = open(
        &s,
        live_env(
            true,
            DriveSource::Fixed(Rc::new(RefCell::new(drives))),
            None,
        ),
    )?;
    r.ok(
        "E1 可弹出磁盘上的目录使用轮询监视",
        c.ctl.watch_mode_of(&s) == Some(WatchMode::Poll),
        format!("{:?}", c.ctl.watch_mode_of(&s)),
    );
    std::thread::sleep(Duration::from_millis(100));
    fs::write(s.join("p2.txt"), "2")?;
    let seen = wait_until(&c, || names(&c).contains(&"p2.txt".to_string()));
    let st = format!("{:?}", c.ctl.ws().active_tab(0).state());
    r.ok(
        "E2 轮询模式也能发现新建",
        seen,
        format!("{:?} {st}", names(&c)),
    );
    Ok(())
}

fn free_letter() -> Option<char> {
    let mask = drive_mask();
    ('M'..='Z')
        .rev()
        .find(|l| mask & (1 << (*l as u8 - b'A')) == 0)
}

fn subst(args: &[&str]) -> bool {
    Command::new("subst")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// F. 真实磁盘插拔：`subst` 创建／删除盘符，经真实的磁盘轮询线程观察。
fn real_drive_plug(r: &mut Rep) -> Res<()> {
    let Some(l) = free_letter() else {
        r.ok("F0 找不到空闲盘符，跳过真实插拔", true, "已跳过");
        return Ok(());
    };
    let root = scratch("live-disk")?;
    fs::create_dir_all(root.join("资料"))?;
    fs::write(root.join("资料").join("笔记.txt"), "n")?;
    fs::write(root.join("根文件.txt"), "r")?;
    let ls = l.to_string();
    let drive_root = PathBuf::from(format!("{l}:\\"));
    if !subst(&[&format!("{l}:"), &root.to_string_lossy()]) {
        r.ok("F0 subst 创建虚拟盘失败，跳过真实插拔", true, "已跳过");
        return Ok(());
    }
    let result = (|| -> Res<()> {
        let c = open(&fixture::root(), live_env(true, DriveSource::System, None))?;
        let row_id = format!("drive:{l}");
        let listed = wait_until(&c, || {
            c.ctl.sidebar_rows().iter().any(|(i, _)| *i == row_id)
        });
        r.ok(
            "F1 真实新增盘符后侧栏列出该磁盘",
            listed,
            format!("{:?}", c.ctl.sidebar_rows()),
        );
        let label = c
            .ctl
            .sidebar_rows()
            .into_iter()
            .find(|(i, _)| *i == row_id)
            .map(|(_, l)| l)
            .unwrap_or_default();
        r.ok(
            "F2 行标签带盘符",
            label.contains(&format!("({ls}:)")),
            label.clone(),
        );
        c.click(&c.button(&label)?);
        c.pump();
        r.ok(
            "F3 点磁盘进入其根目录并监视",
            c.ctl.location() == Some(drive_root.clone())
                && names(&c) == ["资料", "根文件.txt"]
                && c.ctl.watched_paths() == vec![drive_root.clone()],
            format!("{:?} {:?}", c.ctl.location(), names(&c)),
        );
        c.double_click(&c.item("资料")?);
        c.pump();
        r.ok(
            "F4 进入盘内子目录",
            names(&c) == ["笔记.txt"],
            format!("{:?}", names(&c)),
        );
        r.snap(&c, "05-real-drive-connected.png")?;
        // 拔出
        let removed = subst(&[&format!("{l}:"), "/d"]);
        let down = wait_until(&c, || tab_unavailable(&c));
        r.ok(
            "F5 拔出磁盘：正在浏览的位置标为不可用而不是崩溃",
            removed && down && c.win.get_content_state() == ContentState::NotFound,
            format!("{:?}", c.win.get_content_state()),
        );
        let gone = wait_until(&c, || {
            !c.ctl.sidebar_rows().iter().any(|(i, _)| *i == row_id)
        });
        r.ok("F6 侧栏去掉该磁盘", gone, "");
        let msg = c.win.get_status_message().to_string();
        r.ok("F7 状态栏提示磁盘已断开", msg.contains("已断开"), msg);
        r.ok(
            "F8 标签保留且位置不变（不改指向其他磁盘）",
            c.ctl.location() == Some(drive_root.join("资料")),
            format!("{:?}", c.ctl.location()),
        );
        r.snap(&c, "06-real-drive-removed.png")?;
        // 重新插入
        subst(&[&format!("{l}:"), &root.to_string_lossy()]);
        let back = wait_until(&c, || {
            tab_ready(&c)
                && names(&c) == ["笔记.txt"]
                && c.ctl.sidebar_rows().iter().any(|(i, _)| *i == row_id)
        });
        r.ok(
            "F9 重新插入：磁盘出现在侧栏，可用时自动恢复浏览",
            back && c.ctl.sidebar_rows().iter().any(|(i, _)| *i == row_id),
            format!("{:?}", names(&c)),
        );
        r.snap(&c, "07-real-drive-restored.png")?;
        Ok(())
    })();
    // 无论成败都拆掉虚拟盘
    subst(&[&format!("{l}:"), "/d"]);
    result
}
