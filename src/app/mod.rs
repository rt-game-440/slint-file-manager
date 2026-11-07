//! 界面接线：把 Slint 回调连到工作区（[`crate::browse::Workspace`]：两个面板、各自的标签组），并把状态写回界面。主程序与 harness 共用。
//!
//! - 所有面板相关的回调第一个参数是面板编号（0 左、1 右）。对某个面板的任何操作（点击、按键、标签、导航）
//!   都会先把它设为活动面板并把键盘焦点交给它，因此“活动面板 = 键盘焦点所在面板”，快捷键只作用于活动面板。
//! - 目录读取在工作线程执行（[`Loader::Threaded`]），结果带标签编号经 `invoke_from_event_loop` 回到界面线程；
//!   标签已关闭则丢弃，标签在后台（非当前标签、或隐藏面板）时结果照常写入该标签，不影响界面。
//!   harness 使用 [`Loader::Manual`]，由测试显式 [`Controller::pump`]，从而能确定地截到“加载中”状态。
//! - 会话：启动时从 [`SessionStore`] 恢复（位置、排序、视图、历史、活动标签与面板、单栏／双栏）；只读取可见面板的
//!   活动标签，其余标签在首次激活时才读取，所以标签再多也不阻塞窗口显示。变化后至多约 2 秒写盘一次，
//!   正常退出时再写一次（[`Controller::save_now`]）。
//! - 打开文件交给 Windows 默认应用（[`Opener::Shell`]，工作线程执行）；harness 用 [`Opener::Record`] 只记录路径。
//! - 删除与回收站（见 [`ops`]）：Delete 移到回收站、Shift+Delete 永久删除，都先弹应用内确认；回收站是虚拟位置
//!   （[`crate::browse::location::RECYCLE_BIN`]），由外壳命名空间读取，视图里可“还原所选项目”。操作在工作线程逐项执行，
//!   按真实结果汇报；完成后重新读取受影响位置的所有标签（本应用操作后刷新，不依赖目录监视，也不会重复条目）。
//!   文件后端可注入（[`Env::backend`]）：产品用系统回收站，harness 用临时目录“回收站”。
//! - 开启 `--debug-perf` 时记录进入目录的耗时：`dir-loaded-us`（结果回到界面线程）、
//!   `dir-ready-us`（新内容第一帧渲染完成），均自导航请求起算；会话恢复耗时为 `session-restore-us`。

pub mod live;
pub mod model;

pub use live::{DriveSource, LiveEnv};

pub mod ops;
use crate::browse::location::{self, Crumb as PathCrumb};
use crate::browse::session::{LoadOutcome, SessionState, SessionStore};
use crate::browse::workspace::{CloseError, MAX_TABS, Pane};
use crate::browse::{
    Activation, Job, ListErrorKind, LoadState, Loaded, Nav, SortKey as CoreSortKey, SortSpec, Tab,
    TabId, ViewMode as CoreView, Workspace, format,
};
use crate::fileops::{Backend, SystemBackend};
use crate::{
    ContentState, Crumb, MainWindow, NavKey, PaneView, SortKey, TabInfo, ViewMode, perf, shell,
};
use model::ItemsModel;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::{Cell, Ref, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 变化后保存会话的检查间隔。
const SAVE_INTERVAL: Duration = Duration::from_millis(2000);

/// 目录读取方式。
pub enum Loader {
    /// 每个请求一个工作线程，结果回到事件循环（产品使用）。
    Threaded,
    /// 请求排队，调用 [`Controller::pump`] 时在当前线程执行（harness 使用，结果确定）。
    Manual(RefCell<VecDeque<Job>>),
}

/// 打开文件的方式。
pub enum Opener {
    /// 交给 Windows 默认应用。
    Shell,
    /// 只记录路径（harness 使用，不启动任何程序）。
    Record(Rc<RefCell<Vec<PathBuf>>>),
}

/// 接线所需的外部环境。
pub struct Env {
    /// 外部请求的起始目录（`--open=`）；`None` 时使用主目录。
    /// 没有会话可恢复时作为左面板的起点；有会话时按“打开目录请求”处理（见 [`Controller::open_directory`]）。
    pub start: Option<PathBuf>,
    /// 侧栏“主目录”入口，也是右面板的默认起点。
    pub home: Option<PathBuf>,
    pub loader: Loader,
    pub opener: Opener,
    /// 会话存取位置；`None` 表示不恢复也不保存会话。
    pub store: Option<SessionStore>,
    /// 没有可恢复的会话时（首次运行、会话文件缺失或已降级）是否双栏。
    pub fresh_split: bool,
    /// 侧栏的磁盘与收藏、目录监视（见 [`live`]）。
    pub live: LiveEnv,
    /// 删除、回收站枚举与还原的后端：产品为系统回收站，harness 注入临时目录“回收站”。
    pub backend: Arc<dyn Backend>,
}

impl Env {
    pub fn production(start: Option<PathBuf>, store: Option<SessionStore>) -> Env {
        Env {
            start,
            home: shell::home_dir(),
            loader: Loader::Threaded,
            opener: Opener::Shell,
            fresh_split: true,
            live: LiveEnv::production(
                store
                    .as_ref()
                    .map(|s| crate::browse::places::FavoritesStore::at(s.dir())),
            ),
            store,
            backend: Arc::new(SystemBackend),
        }
    }
}

enum Msg {
    Loaded { tab: TabId, loaded: Loaded },
    OpDone(ops::OpDone),
    OpenFailed { failed: usize, message: String },
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static REGISTRY: RefCell<HashMap<u64, Weak<Controller>>> = RefCell::new(HashMap::new());
}

/// 从工作线程把消息投递回界面线程上的控制器。窗口已关闭时丢弃。
fn post(id: u64, msg: Msg) {
    let _ = slint::invoke_from_event_loop(move || deliver(id, msg));
}

fn deliver(id: u64, msg: Msg) {
    let c = REGISTRY.with(|r| r.borrow().get(&id).and_then(Weak::upgrade));
    if let Some(c) = c {
        c.handle(msg);
    }
}

/// 一个面板对应的界面模型。
struct PaneUi {
    items: Rc<ItemsModel>,
    crumbs: Rc<VecModel<Crumb>>,
    crumb_paths: RefCell<Vec<PathCrumb>>,
    tabs: Rc<VecModel<TabInfo>>,
}

pub struct Controller {
    id: u64,
    window: slint::Weak<MainWindow>,
    ws: Rc<RefCell<Workspace>>,
    ui: [PaneUi; 2],
    loader: Loader,
    opener: Opener,
    backend: Arc<dyn Backend>,
    home: Option<PathBuf>,
    store: Option<SessionStore>,
    /// 当前打开的对话框（删除确认或结果说明）；同一时刻至多一个
    dialog: RefCell<Option<ops::Dialog>>,
    /// 有删除或还原正在后台执行
    busy: Cell<bool>,
    /// 当前提示是成功／普通信息（false 时状态栏显示为警示红）
    message_ok: Cell<bool>,
    /// 自上次保存后，会话相关状态是否有变化
    dirty: Cell<bool>,
    /// 最近一次保存是否失败（避免每次定时都重复提示）
    save_failed: Cell<bool>,
    nav_started: Cell<Option<Instant>>,
    /// 最近一次操作的提示（出错、数量上限等），下一次导航时清除
    message: RefCell<String>,
    /// 会话降级说明，整个运行期间保留在状态栏（没有更紧急的提示时显示）
    notice: RefCell<String>,
    saver: slint::Timer,
    live: live::Live,
}

impl Drop for Controller {
    fn drop(&mut self) {
        let id = self.id;
        let _ = REGISTRY.try_with(|r| r.borrow_mut().remove(&id));
    }
}

/// 连接界面回调、恢复会话并开始读取可见面板的活动标签。返回控制器（harness 用它驱动加载）。
pub fn wire(window: &MainWindow, env: Env) -> Rc<Controller> {
    let t0 = Instant::now();
    let start = env
        .start
        .clone()
        .or_else(|| env.home.clone())
        .unwrap_or_else(|| PathBuf::from("C:\\"));
    let home = env.home.clone().unwrap_or_else(|| start.clone());
    let fresh = || SessionState::fresh(start.clone(), home.clone(), env.fresh_split);
    let mut notice = String::new();
    let mut restored = false;
    let state = match env.store.as_ref().map(SessionStore::load) {
        Some(LoadOutcome::Restored(s)) => {
            restored = true;
            s
        }
        Some(LoadOutcome::Degraded(d)) => {
            notice = d.message;
            fresh()
        }
        Some(LoadOutcome::Missing) | None => fresh(),
    };
    let (live, fav_notice) = live::Live::new(env.live);
    if !fav_notice.is_empty() {
        if !notice.is_empty() {
            notice.push('\n');
        }
        notice.push_str(&fav_notice);
    }
    let ws = Rc::new(RefCell::new(Workspace::from_session(&state)));
    let ui = [0, 1].map(|p| {
        let items = Rc::new(ItemsModel::new(ws.clone(), p));
        let crumbs = Rc::new(VecModel::<Crumb>::default());
        let tabs = Rc::new(VecModel::<TabInfo>::default());
        PaneUi {
            items,
            crumbs,
            crumb_paths: RefCell::new(Vec::new()),
            tabs,
        }
    });
    window.set_left_items(ModelRc::from(ui[0].items.clone()));
    window.set_right_items(ModelRc::from(ui[1].items.clone()));
    window.set_left_crumbs(ModelRc::from(ui[0].crumbs.clone()));
    window.set_right_crumbs(ModelRc::from(ui[1].crumbs.clone()));
    window.set_left_tabs(ModelRc::from(ui[0].tabs.clone()));
    window.set_right_tabs(ModelRc::from(ui[1].tabs.clone()));
    window.set_sidebar_places(live.model());
    let c = Rc::new(Controller {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        window: window.as_weak(),
        ws,
        ui,
        loader: env.loader,
        opener: env.opener,
        backend: env.backend,
        home: env.home,
        store: env.store,
        dialog: RefCell::new(None),
        busy: Cell::new(false),
        message_ok: Cell::new(false),
        dirty: Cell::new(false),
        save_failed: Cell::new(false),
        nav_started: Cell::new(None),
        message: RefCell::new(String::new()),
        notice: RefCell::new(notice),
        saver: slint::Timer::default(),
        live,
    });
    REGISTRY.with(|r| r.borrow_mut().insert(c.id, Rc::downgrade(&c)));
    connect(window, &c);
    c.live_start();
    {
        let weak = Rc::downgrade(&c);
        c.saver
            .start(slint::TimerMode::Repeated, SAVE_INTERVAL, move || {
                if let Some(c) = weak.upgrade() {
                    c.save_if_dirty();
                }
            });
    }
    let jobs = c.ws.borrow_mut().load_visible();
    c.dispatch(jobs);
    // 外部请求（--open）遇到已恢复的会话：优先激活已打开该路径的标签，否则在当前面板新开标签
    if restored && let Some(p) = env.start {
        c.open_directory(p);
    }
    c.update_all();
    c.focus_pane(c.ws.borrow().active_pane());
    if perf::enabled() {
        perf::event("session-restore-us", Some(t0.elapsed().as_micros() as i64));
    }
    c
}

fn connect(w: &MainWindow, c: &Rc<Controller>) {
    // 宏参数中显式写出 `c`，保证闭包体能引用到克隆后的控制器（macro_rules 卫生规则）
    macro_rules! on {
        ($setter:ident, $c:ident => || $body:expr) => {{
            let $c = c.clone();
            w.$setter(move || $body);
        }};
        ($setter:ident, $c:ident => |$($arg:ident),*| $body:expr) => {{
            let $c = c.clone();
            w.$setter(move |$($arg),*| $body);
        }};
    }
    on!(on_navigate_to, c => |p, text| c.navigate_text(pane(p), &text));
    on!(on_go_back, c => |p| c.back(pane(p)));
    on!(on_go_forward, c => |p| c.forward(pane(p)));
    on!(on_go_up, c => |p| c.up(pane(p)));
    on!(on_refresh, c => |p| c.refresh(pane(p)));
    on!(on_crumb_clicked, c => |p, i| c.crumb(pane(p), i));
    on!(on_item_pointer, c => |p, i, ctrl, shift| c.pointer(pane(p), i, ctrl, shift));
    on!(on_item_activate, c => |p, i| c.activate_index(pane(p), i));
    on!(on_background_pointer, c => |p, ctrl| c.background(pane(p), ctrl));
    on!(on_nav_key, c => |p, key, shift, ctrl, cols, page| c.nav(
        pane(p), key, shift, ctrl, cols, page
    ));
    on!(on_select_all, c => |p| c.select_all(pane(p)));
    on!(on_activate_selection, c => |p| c.activate_selection(pane(p)));
    on!(on_toggle_cursor, c => |p| c.toggle_cursor(pane(p)));
    on!(on_open_location, c => |p| c.open_location(pane(p)));
    on!(on_delete_selected, c => |p, permanent| c.delete_selected(pane(p), permanent));
    on!(on_restore_selected, c => |p| c.restore_selected(pane(p)));
    on!(on_dialog_accepted, c => || c.accept_dialog());
    on!(on_dialog_cancelled, c => || c.cancel_dialog());
    on!(on_set_sort, c => |p, key, asc| c.set_sort(pane(p), key, asc));
    on!(on_set_view, c => |p, v| c.set_view(pane(p), v));
    on!(on_tab_clicked, c => |p, i| c.select_tab(pane(p), i));
    on!(on_tab_close, c => |p, i| c.close_tab(pane(p), i));
    on!(on_tab_new, c => |p| c.new_tab(pane(p)));
    on!(on_tab_cycle, c => |p, d| c.cycle_tab(pane(p), d));
    on!(on_tab_number, c => |p, n| c.tab_number(pane(p), n));
    on!(on_pane_activated, c => |p| c.activate_pane(pane(p)));
    on!(on_switch_pane, c => || c.switch_pane());
    on!(on_toggle_split, c => || c.toggle_split());
    on!(on_sidebar_clicked, c => |id| c.sidebar(&id));
    on!(on_sidebar_remove, c => |id| c.sidebar_remove(&id));
    on!(on_toggle_favorite, c => || c.toggle_favorite());
    let weak = w.as_weak();
    w.on_request_close(move || {
        if let Some(w) = weak.upgrade() {
            let _ = w.hide();
        }
    });
}

/// 界面传来的面板编号收敛到 0／1。
fn pane(p: i32) -> usize {
    usize::from(p > 0)
}

fn core_sort(k: SortKey) -> CoreSortKey {
    match k {
        SortKey::Name => CoreSortKey::Name,
        SortKey::Modified => CoreSortKey::Modified,
        SortKey::Size => CoreSortKey::Size,
        SortKey::Kind => CoreSortKey::Kind,
    }
}

fn ui_sort(k: CoreSortKey) -> SortKey {
    match k {
        CoreSortKey::Name => SortKey::Name,
        CoreSortKey::Modified => SortKey::Modified,
        CoreSortKey::Size => SortKey::Size,
        CoreSortKey::Kind => SortKey::Kind,
    }
}

fn idx(i: i32) -> Option<usize> {
    usize::try_from(i).ok()
}

fn content_state(t: &Tab) -> (ContentState, String) {
    match t.state() {
        LoadState::Idle | LoadState::Loading => (ContentState::Loading, String::new()),
        LoadState::Ready if t.rows().is_empty() => (ContentState::Empty, String::new()),
        LoadState::Ready => (ContentState::Ready, String::new()),
        LoadState::Failed { kind, message } => (
            match kind {
                ListErrorKind::NotFound => ContentState::NotFound,
                ListErrorKind::AccessDenied => ContentState::AccessDenied,
                ListErrorKind::NotADirectory => ContentState::NotAFolder,
                ListErrorKind::Other => ContentState::Failed,
            },
            message.clone(),
        ),
    }
}

fn pane_view(pane: &Pane) -> PaneView {
    let t = pane.active_tab();
    let (state, detail) = content_state(t);
    PaneView {
        tab_index: pane.active_index() as i32,
        tab_count: pane.len() as i32,
        state,
        error_detail: detail.into(),
        // 回收站要并排显示“删除日期／原位置”，固定用列表呈现（标签自己的视图设置不变）
        view: match t.view() {
            _ if t.is_recycle_bin() => ViewMode::List,
            CoreView::Grid => ViewMode::Grid,
            CoreView::List => ViewMode::List,
        },
        sort_key: ui_sort(t.sort().key),
        sort_ascending: t.sort().ascending,
        location_text: t
            .location()
            .map(location::display)
            .unwrap_or_default()
            .into(),
        cursor_index: t.cursor().map_or(-1, |c| c as i32),
        can_back: t.can_back(),
        can_forward: t.can_forward(),
        can_up: t.can_up(),
        recycle: t.is_recycle_bin(),
        selected_count: t.selected_count() as i32,
    }
}

fn tab_infos(pane: &Pane) -> Vec<TabInfo> {
    pane.tabs()
        .iter()
        .enumerate()
        .map(|(i, t)| TabInfo {
            title: t
                .location()
                .map(location::leaf_label)
                .unwrap_or_default()
                .into(),
            current: i == pane.active_index(),
            unavailable: t.is_unavailable(),
        })
        .collect()
}

impl Controller {
    // ---------- 面板与布局 ----------

    /// 工作区的只读视图（harness 与测试使用）。
    pub fn ws(&self) -> Ref<'_, Workspace> {
        self.ws.borrow()
    }

    /// 把某个面板设为活动面板并把键盘焦点交给它。已是活动面板时什么也不做。
    pub fn activate_pane(&self, p: usize) {
        let (changed, job) = {
            let mut ws = self.ws.borrow_mut();
            if ws.active_pane() == p {
                (false, None)
            } else {
                (true, ws.set_active_pane(p))
            }
        };
        if !changed {
            return;
        }
        self.mark_dirty();
        let touched = self.dispatch(job);
        self.update_panes(&touched);
        self.sync_global();
        self.focus_pane(p);
    }

    /// F6：切换活动面板（单栏时同时切换显示的一侧）。
    pub fn switch_pane(&self) {
        let other = 1 - self.ws.borrow().active_pane();
        self.activate_pane(other);
    }

    pub fn toggle_split(&self) {
        let split = !self.ws.borrow().is_split();
        self.set_split(split);
    }

    /// 切换双栏／单栏。单栏只是隐藏：另一面板的标签与全部状态保留在内存并随会话保存。
    pub fn set_split(&self, split: bool) {
        let jobs = self.ws.borrow_mut().set_split(split);
        self.mark_dirty();
        let touched = self.dispatch(jobs);
        self.update_panes(&touched);
        self.sync_global();
        let active = self.ws.borrow().active_pane();
        self.focus_pane(active);
    }

    /// 把键盘焦点交给活动面板（窗口显示之后调用一次，保证启动时“活动面板 = 焦点所在面板”）。
    pub fn focus_active(&self) {
        let p = self.ws.borrow().active_pane();
        self.focus_pane(p);
    }

    fn focus_pane(&self, p: usize) {
        if let Some(w) = self.window.upgrade() {
            w.invoke_focus_pane(p as i32);
        }
    }

    // ---------- 标签 ----------

    pub fn new_tab(&self, p: usize) {
        self.activate_pane(p);
        let r = self.ws.borrow_mut().new_tab(p);
        match r {
            Ok(job) => {
                self.message.borrow_mut().clear();
                self.mark_dirty();
                self.nav_started.set(Some(Instant::now()));
                let touched = self.dispatch(job);
                self.update_panes(&touched);
                self.update_panes(&[p]);
                self.sync_global();
                self.focus_pane(p);
            }
            Err(_) => self.set_message(&format!("每个面板最多 {MAX_TABS} 个标签")),
        }
    }

    pub fn close_tab(&self, p: usize, i: i32) {
        self.activate_pane(p);
        let Some(i) = idx(i) else { return };
        let r = self.ws.borrow_mut().close_tab(p, i);
        match r {
            Ok(job) => {
                self.message.borrow_mut().clear();
                self.mark_dirty();
                self.nav_started.set(Some(Instant::now()));
                self.dispatch(job);
                self.update_panes(&[p]);
                self.sync_global();
                self.focus_pane(p);
            }
            Err(CloseError::LastTab) => self.set_message("每个面板至少保留一个标签"),
            Err(CloseError::NoSuchTab) => {}
        }
    }

    pub fn select_tab(&self, p: usize, i: i32) {
        self.activate_pane(p);
        let Some(i) = idx(i) else { return };
        let job = self.ws.borrow_mut().select_tab(p, i);
        self.after_tab_switch(p, job);
    }

    pub fn cycle_tab(&self, p: usize, delta: i32) {
        self.activate_pane(p);
        let job = self.ws.borrow_mut().cycle_tab(p, delta as isize);
        self.after_tab_switch(p, job);
    }

    pub fn tab_number(&self, p: usize, n: i32) {
        self.activate_pane(p);
        let Some(n) = idx(n) else { return };
        let job = self.ws.borrow_mut().select_tab_number(p, n);
        self.after_tab_switch(p, job);
    }

    fn after_tab_switch(&self, p: usize, job: Option<Job>) {
        self.mark_dirty();
        if job.is_some() {
            self.nav_started.set(Some(Instant::now()));
        }
        self.dispatch(job);
        self.update_panes(&[p]);
        self.sync_global();
        self.focus_pane(p);
    }

    /// 外部请求打开目录：优先激活已打开该路径的标签（先左后右，单栏时切到那一侧）；
    /// 没有则在当前面板新开标签。不打开任何文件内容。
    pub fn open_directory(&self, path: PathBuf) {
        let found = self.ws.borrow().find_path(&path);
        match found {
            Some((p, i)) => {
                self.select_tab(p, i as i32);
            }
            None => {
                let p = self.ws.borrow().active_pane();
                let r = self.ws.borrow_mut().open_in_pane(p, path);
                match r {
                    Ok(job) => {
                        self.mark_dirty();
                        self.nav_started.set(Some(Instant::now()));
                        self.dispatch(Some(job));
                        self.update_panes(&[p]);
                        self.sync_global();
                    }
                    Err(_) => self.set_message(&format!("每个面板最多 {MAX_TABS} 个标签")),
                }
            }
        }
    }

    // ---------- 导航 ----------

    fn nav_op(&self, p: usize, f: impl FnOnce(&mut Tab) -> Option<crate::browse::LoadRequest>) {
        self.activate_pane(p);
        let job = {
            let mut ws = self.ws.borrow_mut();
            let t = ws.active_tab_mut(p);
            let tab = t.id();
            f(t).map(|req| Job { tab, req })
        };
        if let Some(j) = job {
            self.start(p, j);
        }
    }

    /// 地址栏提交。
    pub fn navigate_text(&self, p: usize, text: &str) {
        self.activate_pane(p);
        let cur = self.ws.borrow().active_tab(p).location().map(PathBuf::from);
        match location::parse_address(text, cur.as_deref()) {
            Some(path) => self.navigate_path(p, path),
            None => self.set_message("请输入完整路径，例如 C:\\Users"),
        }
    }

    pub fn navigate_path(&self, p: usize, path: PathBuf) {
        self.nav_op(p, |t| Some(t.navigate(path)));
    }

    pub fn back(&self, p: usize) {
        self.nav_op(p, Tab::back);
    }

    pub fn forward(&self, p: usize) {
        self.nav_op(p, Tab::forward);
    }

    pub fn up(&self, p: usize) {
        self.nav_op(p, Tab::up);
    }

    pub fn refresh(&self, p: usize) {
        self.nav_op(p, Tab::refresh);
    }

    fn crumb(&self, p: usize, i: i32) {
        let target = idx(i).and_then(|i| {
            self.ui[p]
                .crumb_paths
                .borrow()
                .get(i)
                .map(|c| c.path.clone())
        });
        if let Some(path) = target {
            let same = self.ws.borrow().active_tab(p).location() == Some(path.as_path());
            if same {
                self.refresh(p);
            } else {
                self.navigate_path(p, path);
            }
        }
    }

    /// 侧栏入口作用于活动面板。
    fn sidebar(&self, id: &str) {
        self.sidebar_click(id);
    }

    /// 导航产生的读取：清提示、记时、排队，并刷新面板（显示“加载中”）。
    fn start(&self, p: usize, job: Job) {
        self.nav_started.set(Some(Instant::now()));
        self.message.borrow_mut().clear();
        self.mark_dirty();
        self.dispatch(Some(job));
        self.update_panes(&[p]);
        self.sync_global();
    }

    /// 把读取交给加载器，返回涉及的面板（用于刷新）。
    fn dispatch(&self, jobs: impl IntoIterator<Item = Job>) -> Vec<usize> {
        let mut touched = Vec::new();
        for job in jobs {
            if let Some((p, _)) = self.ws.borrow().locate(job.tab)
                && !touched.contains(&p)
            {
                touched.push(p);
            }
            // 显式读取（导航、刷新、本应用操作后的刷新）覆盖此前待处理的外部变化信号，不会再重复读一次
            self.live.clear_pending(job.tab);
            self.run_job(job);
        }
        touched
    }

    /// 把一次读取交给加载器。
    fn run_job(&self, job: Job) {
        match &self.loader {
            Loader::Threaded => {
                let id = self.id;
                let backend = self.backend.clone();
                std::thread::spawn(move || {
                    post(
                        id,
                        Msg::Loaded {
                            tab: job.tab,
                            loaded: job.req.run_with(&*backend),
                        },
                    )
                });
            }
            Loader::Manual(q) => q.borrow_mut().push_back(job),
        }
    }

    /// 手动模式：执行所有排队的加载请求并应用结果，返回执行数量。线程模式下恒为 0。
    pub fn pump(&self) -> usize {
        let Loader::Manual(q) = &self.loader else {
            return 0;
        };
        let mut n = 0;
        loop {
            let job = q.borrow_mut().pop_front();
            let Some(job) = job else { break };
            self.on_loaded(job.tab, job.req.run_with(&*self.backend));
            n += 1;
        }
        n
    }

    /// 手动模式下尚未执行的请求数。
    pub fn pending(&self) -> usize {
        match &self.loader {
            Loader::Manual(q) => q.borrow().len(),
            Loader::Threaded => 0,
        }
    }

    fn handle(&self, msg: Msg) {
        match msg {
            Msg::Loaded { tab, loaded } => self.on_loaded(tab, loaded),
            Msg::OpDone(done) => self.finish_op(done),
            Msg::OpenFailed { failed, message } => {
                self.set_message(&format!("有 {failed} 个文件未能打开：{message}"))
            }
        }
    }

    fn on_loaded(&self, tab: TabId, l: Loaded) {
        self.live.note_loaded(tab);
        self.apply_loaded(tab, l);
        self.live_after_load(tab);
    }

    fn apply_loaded(&self, tab: TabId, l: Loaded) {
        // (面板, 是否为该面板当前标签, 条目数)；标签已关闭则丢弃
        let outcome = {
            let mut ws = self.ws.borrow_mut();
            let Some((p, i)) = ws.locate(tab) else { return };
            let Some(t) = ws.pane_mut(p).tab_mut(i) else {
                return;
            };
            if !t.apply(l) {
                return;
            }
            let n = t.rows().len();
            (p, i == ws.pane(p).active_index(), n)
        };
        let (p, is_current, n) = outcome;
        if is_current {
            self.ui[p].items.reset();
            self.sync_pane(p);
            self.sync_global();
            if perf::enabled()
                && let Some(t0) = self.nav_started.get()
            {
                perf::event("dir-loaded-us", Some(t0.elapsed().as_micros() as i64));
                perf::event("dir-entries", Some(n as i64));
                perf::mark_next_frame("dir-ready-us", t0);
            }
        } else {
            // 后台标签（非当前标签）：只有标签带上的“不可用”标记可能变化
            self.sync_tabs(p);
        }
    }

    // ---------- 选择 ----------

    fn pointer(&self, p: usize, i: i32, ctrl: bool, shift: bool) {
        if let Some(i) = idx(i) {
            self.mutate(p, |t| t.click(i, ctrl, shift));
        }
    }

    fn background(&self, p: usize, ctrl: bool) {
        self.mutate(p, |t| t.click_empty(ctrl));
    }

    fn nav(&self, p: usize, key: NavKey, shift: bool, ctrl: bool, cols: i32, page: i32) {
        let nav = match key {
            NavKey::Left => Nav::Left,
            NavKey::Right => Nav::Right,
            NavKey::Up => Nav::Up,
            NavKey::Down => Nav::Down,
            NavKey::Home => Nav::Home,
            NavKey::End => Nav::End,
            NavKey::PageUp => Nav::PageUp,
            NavKey::PageDown => Nav::PageDown,
        };
        let cols = idx(cols).unwrap_or(1);
        let page = idx(page).unwrap_or(1);
        self.mutate(p, |t| t.move_cursor(nav, cols, page, shift, ctrl));
    }

    fn select_all(&self, p: usize) {
        self.mutate(p, Tab::select_all);
    }

    fn toggle_cursor(&self, p: usize) {
        self.mutate(p, Tab::toggle_cursor);
    }

    /// 修改选择并只通知变化的行。
    fn mutate(&self, p: usize, f: impl FnOnce(&mut Tab)) {
        self.activate_pane(p);
        let changed = {
            let mut ws = self.ws.borrow_mut();
            let t = ws.active_tab_mut(p);
            f(t);
            t.take_changed()
        };
        for i in changed {
            self.ui[p].items.row_changed(i);
        }
        self.sync_pane_view(p);
        self.sync_global();
    }

    // ---------- 激活 ----------

    fn activate_index(&self, p: usize, i: i32) {
        self.activate_pane(p);
        let Some(i) = idx(i) else { return };
        let a = self.ws.borrow_mut().active_tab_mut(p).activate_index(i);
        self.run_activation(p, a);
    }

    fn activate_selection(&self, p: usize) {
        self.activate_pane(p);
        let a = self.ws.borrow_mut().active_tab_mut(p).activate_selection();
        self.run_activation(p, a);
    }

    fn run_activation(&self, p: usize, a: Activation) {
        match a {
            Activation::None => {}
            Activation::InRecycleBin => {
                self.set_message("回收站里的项目不能直接打开；请先选中并点“还原所选项目”");
            }
            Activation::Navigate(req) => {
                let tab = self.ws.borrow().active_tab(p).id();
                self.start(p, Job { tab, req });
            }
            Activation::Open { paths, truncated } => {
                self.open(paths);
                if truncated > 0 {
                    self.set_message(&format!(
                        "一次最多打开 {} 个文件，其余 {truncated} 个未打开",
                        crate::browse::tab::MAX_OPEN
                    ));
                }
            }
        }
    }

    /// 地址指向文件时，状态面板上的“用默认应用打开”。
    fn open_location(&self, p: usize) {
        self.activate_pane(p);
        let loc = self.ws.borrow().active_tab(p).location().map(PathBuf::from);
        if let Some(path) = loc {
            self.open(vec![path]);
        }
    }

    fn open(&self, paths: Vec<PathBuf>) {
        match &self.opener {
            Opener::Record(log) => log.borrow_mut().extend(paths),
            Opener::Shell => {
                let id = self.id;
                std::thread::spawn(move || {
                    let errors: Vec<String> = paths
                        .iter()
                        .filter_map(|p| shell::open_with_default(p).err())
                        .collect();
                    if let Some(first) = errors.first() {
                        post(
                            id,
                            Msg::OpenFailed {
                                failed: errors.len(),
                                message: first.clone(),
                            },
                        );
                    }
                });
            }
        }
    }

    // ---------- 排序与视图（只改该面板当前标签） ----------

    fn set_sort(&self, p: usize, key: SortKey, ascending: bool) {
        self.activate_pane(p);
        self.ws.borrow_mut().active_tab_mut(p).set_sort(SortSpec {
            key: core_sort(key),
            ascending,
        });
        self.mark_dirty();
        self.ui[p].items.reset();
        self.sync_pane(p);
        self.sync_global();
    }

    fn set_view(&self, p: usize, v: ViewMode) {
        self.activate_pane(p);
        self.ws.borrow_mut().active_tab_mut(p).set_view(match v {
            ViewMode::Grid => CoreView::Grid,
            ViewMode::List => CoreView::List,
        });
        self.mark_dirty();
        self.sync_pane(p);
        self.sync_global();
    }

    // ---------- 会话保存 ----------

    fn mark_dirty(&self) {
        self.dirty.set(true);
    }

    /// 有变化时保存（定时器调用）。
    fn save_if_dirty(&self) {
        if self.dirty.get() {
            let _ = self.save_now();
        }
    }

    /// 立即保存会话。没有会话存储时什么也不做。失败时在状态栏提示一次，并返回错误说明。
    pub fn save_now(&self) -> Result<(), String> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let state = self.ws.borrow().snapshot();
        match store.save(&state) {
            Ok(()) => {
                self.dirty.set(false);
                self.save_failed.set(false);
                Ok(())
            }
            Err(e) => {
                let msg = format!("会话保存失败：{e}");
                if !self.save_failed.replace(true) {
                    self.set_message(&msg);
                }
                Err(msg)
            }
        }
    }

    // ---------- 写回界面 ----------

    /// 状态栏提示（出错、被拒绝、数量上限等）：警示红。
    fn set_message(&self, m: &str) {
        self.message_ok.set(false);
        *self.message.borrow_mut() = m.to_string();
        self.sync_global();
    }

    /// 状态栏提示（操作成功、进行中等普通信息）：常规灰。
    fn set_info(&self, m: &str) {
        self.message_ok.set(true);
        *self.message.borrow_mut() = m.to_string();
        self.sync_global();
    }

    /// 状态栏右侧的提示：最近操作的提示 > 当前标签位置不可用的说明 > 会话降级说明。
    fn status_message(&self) -> String {
        let m = self.message.borrow();
        if !m.is_empty() {
            return m.clone();
        }
        let ws = self.ws.borrow();
        let t = ws.active_tab(ws.active_pane());
        if t.is_restored() && t.is_unavailable() {
            return "此标签的位置当前不可用，标签已保留；磁盘重新连接或路径恢复后点“重试”".into();
        }
        self.notice.borrow().clone()
    }

    pub fn update_all(&self) {
        for p in [0, 1] {
            self.ui[p].items.reset();
            self.sync_pane(p);
        }
        self.sync_global();
    }

    /// 重置并刷新这些面板的条目与状态。
    fn update_panes(&self, panes: &[usize]) {
        for &p in panes {
            self.ui[p].items.reset();
            self.sync_pane(p);
        }
    }

    /// 面板状态与标签带、路径分段。
    fn sync_pane(&self, p: usize) {
        self.sync_pane_view(p);
        self.sync_tabs(p);
        let ws = self.ws.borrow();
        let cr = ws
            .active_tab(p)
            .location()
            .map(location::crumbs)
            .unwrap_or_default();
        drop(ws);
        self.ui[p].crumbs.set_vec(
            cr.iter()
                .map(|c| Crumb {
                    label: c.label.as_str().into(),
                })
                .collect::<Vec<_>>(),
        );
        *self.ui[p].crumb_paths.borrow_mut() = cr;
    }

    fn sync_pane_view(&self, p: usize) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let v = pane_view(self.ws.borrow().pane(p));
        if p == 0 {
            w.set_left_view(v);
        } else {
            w.set_right_view(v);
        }
    }

    fn sync_tabs(&self, p: usize) {
        let infos = tab_infos(self.ws.borrow().pane(p));
        let model = &self.ui[p].tabs;
        if slint::Model::row_count(&**model) == infos.len() {
            for (i, info) in infos.into_iter().enumerate() {
                if slint::Model::row_data(&**model, i).as_ref() != Some(&info) {
                    slint::Model::set_row_data(&**model, i, info);
                }
            }
        } else {
            model.set_vec(infos);
        }
    }

    /// 与具体面板无关的状态：活动面板、单栏／双栏、侧栏高亮、状态栏（活动面板的统计）。
    fn sync_global(&self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let message = self.status_message();
        let ws = self.ws.borrow();
        let active = ws.active_pane();
        let t = ws.active_tab(active);
        w.set_active_pane(active as i32);
        w.set_split(ws.is_split());
        w.set_sidebar_current(if t.is_recycle_bin() {
            "recycle-bin".into()
        } else {
            self.sidebar_current_id(t.location()).into()
        });
        w.set_current_favorite(self.is_current_favorite(t.location()));
        w.set_selected_count(t.selected_count() as i32);
        let status: SharedString = if *t.state() == LoadState::Ready {
            let mut s = format!("{} 个项目", t.rows().len());
            if t.selected_count() > 0 {
                s += &format!("　已选择 {} 个项目", t.selected_count());
                let size = t.selected_size();
                if size > 0 {
                    s += &format!("（{}）", format::size_text(size));
                }
            }
            s.into()
        } else if *t.state() == LoadState::Loading {
            "正在读取…".into()
        } else {
            SharedString::default()
        };
        w.set_status_text(status);
        w.set_status_message_ok(self.message_ok.get() && !self.message.borrow().is_empty());
        w.set_status_message(message.into());
        drop(ws);
        self.sync_watches();
    }

    // ---------- 只读访问（harness 与基准使用） ----------

    /// 活动面板当前标签的位置。
    pub fn location(&self) -> Option<PathBuf> {
        let ws = self.ws.borrow();
        ws.active_tab(ws.active_pane())
            .location()
            .map(PathBuf::from)
    }

    /// 指定面板当前标签的位置。
    pub fn location_in(&self, p: usize) -> Option<PathBuf> {
        self.ws.borrow().active_tab(p).location().map(PathBuf::from)
    }

    pub fn selected_names(&self) -> Vec<String> {
        let ws = self.ws.borrow();
        self.selected_in_ws(&ws, ws.active_pane())
    }

    pub fn selected_in(&self, p: usize) -> Vec<String> {
        self.selected_in_ws(&self.ws.borrow(), p)
    }

    fn selected_in_ws(&self, ws: &Workspace, p: usize) -> Vec<String> {
        ws.active_tab(p)
            .selected_names()
            .into_iter()
            .map(String::from)
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        let ws = self.ws.borrow();
        names_of(ws.active_tab(ws.active_pane()))
    }

    pub fn names_in(&self, p: usize) -> Vec<String> {
        names_of(self.ws.borrow().active_tab(p))
    }
}

fn names_of(t: &Tab) -> Vec<String> {
    t.rows().iter().map(|r| r.entry.name.clone()).collect()
}

/// 进入目录计时：反复在 `target` 与其上级之间切换（在活动面板的当前标签），每次进入 `target` 前记录
/// `bench-iter`，随后的 `dir-ready-us` 即为该次“进入目录到可操作”的耗时。结束后调用 `done`。
/// 需要开启调试日志（`--debug-perf`）才有数据。
pub fn start_bench(
    c: Rc<Controller>,
    target: PathBuf,
    runs: u32,
    done: impl Fn() + 'static,
) -> slint::Timer {
    let parent = target.parent().map(PathBuf::from).unwrap_or(target.clone());
    let timer = slint::Timer::default();
    let k = Cell::new(0u32);
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(400),
        move || {
            let i = k.get();
            k.set(i + 1);
            let p = c.ws().active_pane();
            if i > runs * 2 {
                done();
            } else if i % 2 == 1 {
                perf::event("bench-iter", Some((i / 2) as i64));
                c.navigate_path(p, target.clone());
            } else {
                c.navigate_path(p, parent.clone());
            }
        },
    );
    timer
}
