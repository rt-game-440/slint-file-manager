//! 位置入口与外部变化同步（Issue #7）：侧栏的磁盘列表与收藏、磁盘插拔、目录监视与静默刷新。
//!
//! 全部在界面线程的一个 100ms 节拍（[`TICK`]）里推进，工作线程只做两件事：监视器线程发“目录变了”信号，
//! 磁盘线程发现盘符变化后把新列表放进共享槽；真正的目录读取仍走 [`super::Controller`] 的加载器（工作线程）。
//! 节拍计数代替真实时钟，harness 的模拟时钟因此能确定地驱动防抖。
//!
//! - **防抖合并**：收到变化信号后，安静 [`QUIET_TICKS`] 个节拍（或最长 [`MAX_WAIT_TICKS`] 个节拍）才发一次静默刷新；
//!   上一次读取未返回时不发新的，返回后若又有变化则再合并成一次。读取中发生的变化由标签的 `stale` 标记兜底。
//! - **旧通知失效**：每个监视器有编号；换目录、关标签、标签离开可见状态都会丢弃监视器，迟到的通知找不到编号被忽略。
//!   用户显式导航或刷新（F5、本应用操作后的刷新）会清掉该标签待处理的变化信号——新的读取已覆盖它们。
//! - **只监视可见标签**：后台标签不监视，再次可见时静默刷新一次（`Tab::mark_needs_refresh`）。
//! - **目录消失／磁盘断开**：监视器发 `Gone` 或磁盘线程报告断开，标签立刻标为不可用；之后每约 2 秒探测一次
//!   可见的不可用标签，位置恢复就自动回到列表，不需要手动点“重试”。
//! - **U 盘等可弹出磁盘**用轮询模式监视（不持有句柄，见 [`crate::browse::watch`]）。

use super::{Controller, Env};
use crate::SidebarEntry;
use crate::browse::places::{
    self, AddOutcome, Drive, FavLoad, FavState, Favorites, FavoritesStore, drive_letter,
};
use crate::browse::watch::{DirWatcher, WatchEvent, WatchMode, WatchMsg};
use crate::browse::{Job, LoadState, TabId, location};
use slint::{ModelRc, VecModel};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 节拍间隔。
pub const TICK: Duration = Duration::from_millis(100);
/// 最后一次变化信号之后安静多少个节拍才刷新。
const QUIET_TICKS: u64 = 2;
/// 从第一次变化信号起最多等待多少个节拍（持续写入的目录大约每秒刷新一次）。
const MAX_WAIT_TICKS: u64 = 10;
/// 探测不可用标签、重建失效监视器的间隔（节拍）。
const PROBE_TICKS: u64 = 20;

/// 磁盘列表来源。
pub enum DriveSource {
    /// 系统磁盘：后台线程每秒检查盘符变化。
    System,
    /// 由测试直接给定并修改的磁盘列表（模拟 U 盘插拔）。
    Fixed(Rc<RefCell<Vec<Drive>>>),
}

/// 位置入口与目录监视的外部依赖（产品用 [`LiveEnv::production`]，harness 注入）。
pub struct LiveEnv {
    pub drives: DriveSource,
    pub favorites: Option<FavoritesStore>,
    pub onedrive: Option<PathBuf>,
    pub icloud: Option<PathBuf>,
    /// 是否启动目录监视。
    pub watch: bool,
}

impl LiveEnv {
    pub fn production(favorites: Option<FavoritesStore>) -> LiveEnv {
        let (onedrive, icloud) = places::detect_cloud_roots();
        LiveEnv {
            drives: DriveSource::System,
            favorites,
            onedrive,
            icloud,
            watch: true,
        }
    }

    /// 无磁盘、无收藏存储、无云盘、不监视：默认的无头测试环境，行为与引入本功能前一致。
    pub fn inert() -> LiveEnv {
        LiveEnv {
            drives: DriveSource::Fixed(Rc::new(RefCell::new(Vec::new()))),
            favorites: None,
            onedrive: None,
            icloud: None,
            watch: false,
        }
    }
}

struct SysDrives {
    shared: Arc<Mutex<(u64, Vec<Drive>)>>,
    stop: Arc<AtomicBool>,
}

impl SysDrives {
    fn start() -> SysDrives {
        let shared = Arc::new(Mutex::new((0u64, Vec::<Drive>::new())));
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, st2) = (shared.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut last_mask = u32::MAX;
            let mut round = 0u32;
            while !st2.load(Ordering::SeqCst) {
                let mask = places::drive_mask();
                // 盘符不变时每 5 秒也重读一次（读卡器插拔介质不改变盘符位图）
                if mask != last_mask || round.is_multiple_of(5) {
                    last_mask = mask;
                    let list = places::enumerate_drives();
                    let mut g = s2.lock().unwrap();
                    if g.1 != list || g.0 == 0 {
                        g.0 += 1;
                        g.1 = list;
                    }
                }
                round += 1;
                for _ in 0..10 {
                    if st2.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        SysDrives { shared, stop }
    }
}

impl Drop for SysDrives {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

struct WatchEntry {
    id: u64,
    tab: TabId,
    path: PathBuf,
    mode: WatchMode,
    watcher: Option<DirWatcher>,
    /// 监视器结束（或创建失败）的节拍；到点后才重建。
    dead_since: Option<u64>,
    first: Option<u64>,
    last: Option<u64>,
}

pub struct Live {
    src: DriveSource,
    sys: Option<SysDrives>,
    seen_gen: Cell<u64>,
    drives: RefCell<Vec<Drive>>,
    favs: RefCell<Favorites>,
    fav_store: Option<FavoritesStore>,
    fav_states: RefCell<Vec<FavState>>,
    fav_save_failed: Cell<bool>,
    onedrive: Option<PathBuf>,
    icloud: Option<PathBuf>,
    sidebar: Rc<VecModel<SidebarEntry>>,
    watch_on: bool,
    hub: RefCell<Vec<WatchEntry>>,
    tx: Sender<WatchMsg>,
    rx: Receiver<WatchMsg>,
    next_watch: Cell<u64>,
    tick: Cell<u64>,
    inflight: RefCell<HashSet<TabId>>,
    reloads: Cell<u64>,
    pub(super) ticker: slint::Timer,
}

impl Live {
    /// 返回 (状态, 需要显示的收藏降级说明)。
    pub(super) fn new(env: LiveEnv) -> (Live, String) {
        let mut notice = String::new();
        let favs = match env.favorites.as_ref().map(FavoritesStore::load) {
            Some(FavLoad::Restored(f)) => f,
            Some(FavLoad::Degraded { message }) => {
                notice = message;
                Favorites::default()
            }
            Some(FavLoad::Missing) | None => Favorites::default(),
        };
        let (tx, rx) = channel();
        let sys = matches!(env.drives, DriveSource::System).then(SysDrives::start);
        let n = favs.len();
        (
            Live {
                src: env.drives,
                sys,
                seen_gen: Cell::new(0),
                drives: RefCell::new(Vec::new()),
                favs: RefCell::new(favs),
                fav_store: env.favorites,
                fav_states: RefCell::new(vec![FavState::Ok; n]),
                fav_save_failed: Cell::new(false),
                onedrive: env.onedrive,
                icloud: env.icloud,
                sidebar: Rc::new(VecModel::default()),
                watch_on: env.watch,
                hub: RefCell::new(Vec::new()),
                tx,
                rx,
                next_watch: Cell::new(1),
                tick: Cell::new(0),
                inflight: RefCell::new(HashSet::new()),
                reloads: Cell::new(0),
                ticker: slint::Timer::default(),
            },
            notice,
        )
    }

    pub(super) fn model(&self) -> ModelRc<SidebarEntry> {
        ModelRc::from(self.sidebar.clone())
    }

    pub(super) fn clear_pending(&self, tab: TabId) {
        for e in self.hub.borrow_mut().iter_mut() {
            if e.tab == tab {
                e.first = None;
                e.last = None;
            }
        }
    }

    pub(super) fn note_loaded(&self, tab: TabId) {
        self.inflight.borrow_mut().remove(&tab);
    }

    fn drive_for(&self, p: &Path) -> Option<Drive> {
        let l = drive_letter(p)?;
        self.drives.borrow().iter().find(|d| d.letter == l).cloned()
    }
}

fn same(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .trim_end_matches('\\')
        .eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches('\\'))
}

impl Env {
    /// 把当前环境里的位置入口设置换成给定值（harness 链式构造用）。
    pub fn with_live(mut self, live: LiveEnv) -> Env {
        self.live = live;
        self
    }
}

impl Controller {
    /// 启动节拍并生成首个侧栏。`wire` 在控制器建好后调用一次。
    pub(super) fn live_start(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.live
            .ticker
            .start(slint::TimerMode::Repeated, TICK, move || {
                if let Some(c) = weak.upgrade() {
                    c.tick();
                }
            });
        if let DriveSource::Fixed(list) = &self.live.src {
            let v = list.borrow().clone();
            *self.live.drives.borrow_mut() = v;
        }
        self.refresh_fav_states();
        self.rebuild_sidebar();
        self.sync_watches();
    }

    /// 推进一个节拍：处理监视信号与磁盘变化、到点刷新、定期探测。节拍由定时器驱动，测试也可直接调用。
    pub fn tick(&self) {
        let t = self.live.tick.get() + 1;
        self.live.tick.set(t);
        self.poll_drives();
        self.drain_watch(t);
        self.fire_pending(t);
        if t.is_multiple_of(PROBE_TICKS) {
            self.probe_unavailable();
            self.sync_watches();
        }
    }

    // ---------- 磁盘 ----------

    fn poll_drives(&self) {
        let new = match &self.live.src {
            DriveSource::Fixed(list) => {
                let v = list.borrow().clone();
                if v == *self.live.drives.borrow() {
                    return;
                }
                v
            }
            DriveSource::System => {
                let Some(sys) = &self.live.sys else { return };
                let g = sys.shared.lock().unwrap();
                if g.0 == self.live.seen_gen.get() {
                    return;
                }
                self.live.seen_gen.set(g.0);
                g.1.clone()
            }
        };
        self.on_drives_changed(new);
    }

    /// 磁盘列表变化：断开的磁盘上的标签立即不可用；恢复的磁盘上可见的不可用标签自动重读；刷新侧栏与收藏状态。
    fn on_drives_changed(&self, new: Vec<Drive>) {
        let old = self.live.drives.replace(new.clone());
        let (added, removed) = places::diff_drives(&old, &new);
        let first_report = old.is_empty() && self.live.seen_gen.get() <= 1;
        let mut touched: Vec<usize> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        // 断开：该盘符上所有标签（含后台标签）立即标为不可用
        for d in &removed {
            // 同盘符换了另一块盘时，该盘符仍在 new 里：旧盘上的标签同样失效，由重新读取决定去向
            let mut hit = false;
            {
                let mut ws = self.ws.borrow_mut();
                for p in 0..2 {
                    for i in 0..ws.pane(p).len() {
                        let Some(t) = ws.pane_mut(p).tab_mut(i) else {
                            continue;
                        };
                        if t.location().and_then(drive_letter) == Some(d.letter)
                            && t.mark_unavailable(&format!("磁盘 {}: 已断开或不可用", d.letter))
                        {
                            hit = true;
                            if !touched.contains(&p) {
                                touched.push(p);
                            }
                        }
                    }
                }
            }
            let still_there = new.iter().any(|n| n.letter == d.letter);
            if !still_there || hit {
                notes.push(format!("{} 已断开", d.display()));
            }
        }
        // 连接：该盘符上可见的不可用标签自动重读（后台标签激活时由 ensure_loaded 重试）
        let mut jobs = Vec::new();
        for d in &added {
            if !first_report {
                notes.push(format!("已连接 {}", d.display()));
            }
            let mut ws = self.ws.borrow_mut();
            for p in 0..2 {
                if !ws.is_visible(p) {
                    continue;
                }
                let t = ws.active_tab_mut(p);
                if t.is_unavailable() && t.location().and_then(drive_letter) == Some(d.letter) {
                    let id = t.id();
                    if let Some(req) = t.reload_silent() {
                        jobs.push(Job { tab: id, req });
                    }
                }
            }
        }
        for job in jobs {
            self.run_silent(job);
        }
        self.refresh_fav_states();
        self.rebuild_sidebar();
        if !touched.is_empty() {
            self.update_panes(&touched);
        }
        if let Some(n) = notes.last() {
            self.set_message(n);
        } else {
            self.sync_global();
        }
    }

    // ---------- 目录监视 ----------

    /// 让监视器与“可见面板的当前标签”一致：新位置建监视器，离开的丢弃（旧通知随之失效）。
    pub(super) fn sync_watches(&self) {
        if !self.live.watch_on {
            return;
        }
        let tick = self.live.tick.get();
        // (标签, 位置, 状态允许监视)
        let mut desired: Vec<(TabId, PathBuf, bool)> = Vec::new();
        {
            let ws = self.ws.borrow();
            for p in 0..2 {
                if !ws.is_visible(p) {
                    continue;
                }
                let t = ws.active_tab(p);
                if let Some(loc) = t.location()
                    && !matches!(t.state(), LoadState::Idle)
                {
                    let ok = matches!(t.state(), LoadState::Ready | LoadState::Loading);
                    if !desired.iter().any(|(id, l, _)| *id == t.id() && l == loc) {
                        desired.push((t.id(), loc.to_path_buf(), ok));
                    }
                }
            }
        }
        let mut hub = self.live.hub.borrow_mut();
        // 丢弃不再需要的（含模式已变化的）
        let mut dropped: Vec<(TabId, PathBuf)> = Vec::new();
        hub.retain(|e| {
            let keep = desired
                .iter()
                .any(|(id, l, _)| *id == e.tab && *l == e.path)
                && self.mode_for(&e.path) == e.mode;
            if !keep {
                dropped.push((e.tab, e.path.clone()));
            }
            keep
        });
        // 创建缺少的；失效的在间隔之后重建
        for (tab, path, ok) in desired {
            let mode = self.mode_for(&path);
            let existing = hub.iter().position(|e| e.tab == tab && e.path == path);
            let idx = match existing {
                Some(i) => i,
                None => {
                    let id = self.live.next_watch.get();
                    self.live.next_watch.set(id + 1);
                    hub.push(WatchEntry {
                        id,
                        tab,
                        path: path.clone(),
                        mode,
                        watcher: None,
                        dead_since: None,
                        first: None,
                        last: None,
                    });
                    hub.len() - 1
                }
            };
            let e = &mut hub[idx];
            let due = e.dead_since.is_none_or(|d| tick >= d + PROBE_TICKS);
            if e.watcher.is_none() && ok && due {
                match DirWatcher::start(&e.path, e.id, self.live.tx.clone(), e.mode) {
                    Ok(w) => {
                        // 监视中断期间可能漏了变化：重建后补一次刷新
                        if e.dead_since.take().is_some() {
                            e.first = Some(tick);
                            e.last = Some(tick);
                        }
                        e.watcher = Some(w);
                    }
                    Err(_) => e.dead_since = Some(tick),
                }
            }
        }
        drop(hub);
        // 离开监视的标签（位置没变，只是不再可见）：再次可见时需要静默刷新
        let mut ws = self.ws.borrow_mut();
        for (tab, path) in dropped {
            if let Some(t) = ws.tab_by_id_mut(tab)
                && t.location() == Some(path.as_path())
            {
                t.mark_needs_refresh();
            }
        }
    }

    fn mode_for(&self, p: &Path) -> WatchMode {
        match self.live.drive_for(p) {
            Some(d) if d.ejectable => WatchMode::Poll,
            _ => WatchMode::Notify,
        }
    }

    fn drain_watch(&self, tick: u64) {
        let mut hub = self.live.hub.borrow_mut();
        while let Ok((id, ev)) = self.live.rx.try_recv() {
            // 编号不在表里：来自已丢弃的监视器（换目录、关标签之后迟到的通知），忽略
            let Some(e) = hub.iter_mut().find(|e| e.id == id) else {
                continue;
            };
            match ev {
                WatchEvent::Changed => {
                    e.first.get_or_insert(tick);
                    e.last = Some(tick);
                }
                WatchEvent::Gone => {
                    // 监视器已结束：立即刷新一次以得知目录是否还在
                    e.watcher = None;
                    e.dead_since = Some(tick);
                    e.first = Some(tick);
                    e.last = Some(tick.saturating_sub(QUIET_TICKS));
                }
            }
        }
    }

    /// 到点的变化合并成一次静默刷新。
    fn fire_pending(&self, tick: u64) {
        let due: Vec<TabId> = self
            .live
            .hub
            .borrow_mut()
            .iter_mut()
            .filter_map(|e| {
                let (f, l) = (e.first?, e.last?);
                if self.live.inflight.borrow().contains(&e.tab) {
                    return None; // 读取未返回：保留待处理，返回后再合并
                }
                if tick.saturating_sub(l) >= QUIET_TICKS || tick.saturating_sub(f) >= MAX_WAIT_TICKS
                {
                    e.first = None;
                    e.last = None;
                    Some(e.tab)
                } else {
                    None
                }
            })
            .collect();
        for tab in due {
            self.reload_tab_silent(tab);
        }
    }

    fn reload_tab_silent(&self, tab: TabId) {
        let job = {
            let mut ws = self.ws.borrow_mut();
            ws.tab_by_id_mut(tab)
                .and_then(|t| t.reload_silent().map(|req| Job { tab, req }))
        };
        if let Some(j) = job {
            self.run_silent(j);
        }
    }

    pub(super) fn run_silent(&self, job: Job) {
        self.live.inflight.borrow_mut().insert(job.tab);
        self.live.reloads.set(self.live.reloads.get() + 1);
        self.run_job(job);
    }

    /// 读取完成之后：读取期间又有变化的标签再静默刷新一次。
    pub(super) fn live_after_load(&self, tab: TabId) {
        let stale = self
            .ws
            .borrow_mut()
            .tab_by_id_mut(tab)
            .is_some_and(|t| t.take_stale());
        if stale {
            self.reload_tab_silent(tab);
        }
    }

    /// 可见的不可用标签（目录被删、磁盘断开）定期探测，位置恢复就自动回到列表。
    fn probe_unavailable(&self) {
        let ids: Vec<TabId> = {
            let ws = self.ws.borrow();
            (0..2)
                .filter(|&p| ws.is_visible(p))
                .map(|p| ws.active_tab(p))
                .filter(|t| {
                    matches!(
                        t.state(),
                        LoadState::Failed { kind, .. }
                            if matches!(kind, crate::browse::ListErrorKind::NotFound
                                | crate::browse::ListErrorKind::Other)
                    )
                })
                .map(|t| t.id())
                .collect()
        };
        for id in ids {
            if !self.live.inflight.borrow().contains(&id) {
                self.reload_tab_silent(id);
            }
        }
    }

    // ---------- 收藏 ----------

    /// 工具栏星标 / Ctrl+D：收藏或取消收藏活动面板的当前位置。
    pub fn toggle_favorite(&self) {
        let loc = {
            let ws = self.ws.borrow();
            ws.active_tab(ws.active_pane())
                .location()
                .map(PathBuf::from)
        };
        let Some(path) = loc else { return };
        let name = location::leaf_label(&path);
        let removed = self.live.favs.borrow_mut().remove(&path);
        let msg = if removed {
            format!("已取消收藏“{name}”")
        } else {
            let vol = places::volume_serial(&path);
            match self.live.favs.borrow_mut().add(&path, vol) {
                AddOutcome::Added => format!("已收藏“{name}”"),
                AddOutcome::AlreadyThere => String::new(),
                AddOutcome::Rejected(why) => why.to_string(),
            }
        };
        self.after_favorites_changed();
        if !msg.is_empty() {
            self.set_message(&msg);
        }
    }

    /// 侧栏上移除第 `i` 个收藏。
    pub fn remove_favorite(&self, i: usize) {
        let removed = self.live.favs.borrow_mut().remove_at(i);
        if let Some(f) = removed {
            self.after_favorites_changed();
            self.set_message(&format!("已取消收藏“{}”", f.name));
        }
    }

    fn after_favorites_changed(&self) {
        self.refresh_fav_states();
        self.rebuild_sidebar();
        self.save_favorites();
        self.sync_global();
    }

    fn save_favorites(&self) {
        let Some(store) = &self.live.fav_store else {
            return;
        };
        match store.save(&self.live.favs.borrow()) {
            Ok(()) => self.live.fav_save_failed.set(false),
            Err(e) => {
                if !self.live.fav_save_failed.replace(true) {
                    self.set_message(&format!("收藏保存失败：{e}"));
                }
            }
        }
    }

    /// 重新判定全部收藏的可用状态。只在磁盘变化、增删收藏、点击收藏时调用，避免平时访问磁盘。
    fn refresh_fav_states(&self) {
        let drives = self.live.drives.borrow();
        let favs = self.live.favs.borrow();
        let states = favs
            .items()
            .iter()
            .map(|f| places::favorite_state(f, &drives, |p| p.is_dir()))
            .collect();
        *self.live.fav_states.borrow_mut() = states;
    }

    pub fn favorite_paths(&self) -> Vec<PathBuf> {
        self.live
            .favs
            .borrow()
            .items()
            .iter()
            .map(|f| f.path.clone())
            .collect()
    }

    /// 全部收藏的可用状态（与 [`Controller::favorite_paths`] 同序）。
    pub fn favorite_states(&self) -> Vec<FavState> {
        self.live.fav_states.borrow().clone()
    }

    /// 当前识别到的磁盘。
    pub fn drives(&self) -> Vec<Drive> {
        self.live.drives.borrow().clone()
    }

    /// 侧栏条目 id 与标签（harness 与测试读取）。
    pub fn sidebar_rows(&self) -> Vec<(String, String)> {
        slint::Model::iter(&*self.live.sidebar)
            .map(|e| (e.id.to_string(), e.label.to_string()))
            .collect()
    }

    pub fn watched_paths(&self) -> Vec<PathBuf> {
        self.live
            .hub
            .borrow()
            .iter()
            .filter(|e| e.watcher.is_some())
            .map(|e| e.path.clone())
            .collect()
    }

    /// 某目录当前使用的监视模式（没有活动监视器时为 `None`）。
    pub fn watch_mode_of(&self, p: &Path) -> Option<WatchMode> {
        self.live
            .hub
            .borrow()
            .iter()
            .find(|e| e.watcher.is_some() && e.path == p)
            .map(|e| e.mode)
    }

    /// 至今发出的静默刷新次数（测试用来确认爆发被合并）。
    pub fn silent_reloads(&self) -> u64 {
        self.live.reloads.get()
    }

    /// 没有待合并的变化信号，也没有静默刷新在途。
    pub fn live_quiet(&self) -> bool {
        self.live.inflight.borrow().is_empty()
            && self
                .live
                .hub
                .borrow()
                .iter()
                .all(|e| e.first.is_none() && e.last.is_none())
    }

    // ---------- 侧栏 ----------

    fn rebuild_sidebar(&self) {
        let l = &self.live;
        let mut v: Vec<SidebarEntry> = Vec::new();
        let row = |id: String, label: String, icon: &str| SidebarEntry {
            id: id.into(),
            label: label.into(),
            icon: icon.into(),
            enabled: true,
            header: false,
            warn: false,
            description: Default::default(),
            removable: false,
        };
        let header = |id: &str, label: &str| SidebarEntry {
            id: id.into(),
            label: label.into(),
            icon: Default::default(),
            enabled: false,
            header: true,
            warn: false,
            description: Default::default(),
            removable: false,
        };
        if self.home.is_some() {
            v.push(row("home".into(), "主目录".into(), "home"));
        }
        v.push(header("hdr-favorites", "收藏"));
        let favs = l.favs.borrow();
        let states = l.fav_states.borrow();
        if favs.is_empty() {
            let mut e = row("fav-empty".into(), "尚无收藏".into(), "star");
            e.enabled = false;
            e.description = "点工具栏的星标收藏当前文件夹".into();
            v.push(e);
        }
        for (i, f) in favs.items().iter().enumerate() {
            let st = states.get(i).copied().unwrap_or(FavState::Ok);
            let mut e = row(format!("fav:{i}"), f.name.clone(), "star");
            e.removable = true;
            if !st.is_ok() {
                e.warn = true;
                e.icon = "warning".into();
                e.description = format!("已失效：{}", st.reason()).into();
            } else {
                e.description = f.path.to_string_lossy().into_owned().into();
            }
            v.push(e);
        }
        v.push(header("hdr-this-pc", "此电脑"));
        let drives = l.drives.borrow();
        for d in drives.iter() {
            let icon = if d.ejectable { "usb" } else { "drive" };
            let mut e = row(format!("drive:{}", d.letter), d.display(), icon);
            if !d.ready() {
                e.description = "未就绪".into();
            }
            v.push(e);
        }
        if l.onedrive.is_some() || l.icloud.is_some() {
            v.push(header("hdr-cloud", "云盘"));
            if l.onedrive.is_some() {
                v.push(row("onedrive".into(), "OneDrive".into(), "cloud"));
            }
            if l.icloud.is_some() {
                v.push(row("icloud".into(), "iCloud".into(), "cloud"));
            }
        }
        l.sidebar.set_vec(v);
    }

    /// 与活动面板当前位置对应的侧栏条目 id（没有则为空）。
    pub(super) fn sidebar_current_id(&self, loc: Option<&Path>) -> String {
        let Some(loc) = loc else { return String::new() };
        if self.home.as_deref().is_some_and(|h| same(h, loc)) {
            return "home".into();
        }
        for (i, f) in self.live.favs.borrow().items().iter().enumerate() {
            if same(&f.path, loc) {
                return format!("fav:{i}");
            }
        }
        for d in self.live.drives.borrow().iter() {
            if same(&d.root, loc) {
                return format!("drive:{}", d.letter);
            }
        }
        if self.live.onedrive.as_deref().is_some_and(|p| same(p, loc)) {
            return "onedrive".into();
        }
        if self.live.icloud.as_deref().is_some_and(|p| same(p, loc)) {
            return "icloud".into();
        }
        String::new()
    }

    /// 当前位置是否已收藏（工具栏星标的选中状态）。
    pub(super) fn is_current_favorite(&self, loc: Option<&Path>) -> bool {
        loc.is_some_and(|l| self.live.favs.borrow().contains(l))
    }

    /// 侧栏入口作用于活动面板，与地址栏进入同一浏览流程（[`Controller::navigate_path`]）。
    pub(super) fn sidebar_click(&self, id: &str) {
        let p = self.ws.borrow().active_pane();
        if id == "home" {
            if let Some(h) = self.home.clone() {
                self.navigate_path(p, h);
            }
        } else if id == "recycle-bin" {
            // 回收站是外壳虚拟位置：已在其中时只刷新，否则进入
            if self.ws.borrow().active_tab(p).is_recycle_bin() {
                self.refresh(p);
            } else {
                self.navigate_path(p, crate::browse::location::recycle_bin_path());
            }
        } else if let Some(letter) = id.strip_prefix("drive:") {
            let drive = self
                .live
                .drives
                .borrow()
                .iter()
                .find(|d| d.letter.to_string() == letter)
                .cloned();
            match drive {
                Some(d) => self.navigate_path(p, d.root),
                None => self.set_message(&format!("磁盘 {letter}: 当前未连接")),
            }
        } else if let Some(i) = id
            .strip_prefix("fav:")
            .and_then(|i| i.parse::<usize>().ok())
        {
            let fav = self.live.favs.borrow().items().get(i).cloned();
            let Some(f) = fav else { return };
            // 点击时即时重判：目录刚被删、盘刚被换都能识别
            self.refresh_fav_states();
            let st = self
                .live
                .fav_states
                .borrow()
                .get(i)
                .copied()
                .unwrap_or(FavState::Ok);
            if st.is_ok() {
                self.navigate_path(p, f.path);
            } else {
                self.rebuild_sidebar();
                self.set_message(&format!(
                    "收藏“{}”已失效（{}），不会改指向其他位置；可点 × 取消收藏",
                    f.name,
                    st.reason()
                ));
            }
        } else if id == "onedrive" || id == "icloud" {
            let dir = if id == "onedrive" {
                self.live.onedrive.clone()
            } else {
                self.live.icloud.clone()
            };
            if let Some(d) = dir {
                self.navigate_path(p, d);
            }
        }
    }

    /// 侧栏上的“×”：目前只用于收藏。
    pub(super) fn sidebar_remove(&self, id: &str) {
        if let Some(i) = id
            .strip_prefix("fav:")
            .and_then(|i| i.parse::<usize>().ok())
        {
            self.remove_favorite(i);
        }
    }
}
