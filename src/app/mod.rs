//! 界面接线：把 Slint 回调连到浏览上下文（[`crate::browse::Tab`]），并把状态写回界面。主程序与 harness 共用。
//!
//! - 目录读取在工作线程执行（[`Loader::Threaded`]），结果经 `invoke_from_event_loop` 回到界面线程；
//!   harness 使用 [`Loader::Manual`]，由测试显式 [`Controller::pump`]，从而能确定地截到“加载中”状态。
//! - 打开文件交给 Windows 默认应用（[`Opener::Shell`]，工作线程执行）；harness 用 [`Opener::Record`] 只记录路径。
//! - 开启 `--debug-perf` 时记录进入目录的耗时：`dir-loaded-us`（结果回到界面线程）、
//!   `dir-ready-us`（新内容第一帧渲染完成），均自导航请求起算。

pub mod model;

use crate::browse::location::{self, Crumb as PathCrumb};
use crate::browse::{
    Activation, ListErrorKind, LoadRequest, LoadState, Loaded, Nav, SortKey as CoreSortKey,
    SortSpec, Tab, ViewMode as CoreView, format,
};
use crate::{ContentState, Crumb, MainWindow, NavKey, SortKey, ViewMode, perf, shell};
use model::ItemsModel;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 目录读取方式。
pub enum Loader {
    /// 每个请求一个工作线程，结果回到事件循环（产品使用）。
    Threaded,
    /// 请求排队，调用 [`Controller::pump`] 时在当前线程执行（harness 使用，结果确定）。
    Manual(RefCell<VecDeque<LoadRequest>>),
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
    /// 启动位置；`None` 时使用主目录。
    pub start: Option<PathBuf>,
    /// 侧栏“主目录”入口。
    pub home: Option<PathBuf>,
    pub loader: Loader,
    pub opener: Opener,
}

impl Env {
    pub fn production(start: Option<PathBuf>) -> Env {
        Env {
            start,
            home: shell::home_dir(),
            loader: Loader::Threaded,
            opener: Opener::Shell,
        }
    }
}

enum Msg {
    Loaded(Loaded),
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

pub struct Controller {
    id: u64,
    window: slint::Weak<MainWindow>,
    tab: Rc<RefCell<Tab>>,
    items: Rc<ItemsModel>,
    crumbs: Rc<VecModel<Crumb>>,
    crumb_paths: RefCell<Vec<PathCrumb>>,
    loader: Loader,
    opener: Opener,
    home: Option<PathBuf>,
    nav_started: Cell<Option<Instant>>,
    message: RefCell<String>,
}

impl Drop for Controller {
    fn drop(&mut self) {
        let id = self.id;
        let _ = REGISTRY.try_with(|r| r.borrow_mut().remove(&id));
    }
}

/// 连接界面回调并进入启动位置。返回控制器（harness 用它驱动加载）。
pub fn wire(window: &MainWindow, env: Env) -> Rc<Controller> {
    let tab = Rc::new(RefCell::new(Tab::new()));
    let items = Rc::new(ItemsModel::new(tab.clone()));
    let crumbs = Rc::new(VecModel::<Crumb>::default());
    window.set_items(ModelRc::from(items.clone()));
    window.set_crumbs(ModelRc::from(crumbs.clone()));
    let start = env
        .start
        .clone()
        .or_else(|| env.home.clone())
        .unwrap_or_else(|| PathBuf::from("C:\\"));
    let c = Rc::new(Controller {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        window: window.as_weak(),
        tab,
        items,
        crumbs,
        crumb_paths: RefCell::new(Vec::new()),
        loader: env.loader,
        opener: env.opener,
        home: env.home,
        nav_started: Cell::new(None),
        message: RefCell::new(String::new()),
    });
    REGISTRY.with(|r| r.borrow_mut().insert(c.id, Rc::downgrade(&c)));
    connect(window, &c);
    c.navigate_path(start);
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
    on!(on_navigate_to, c => |text| c.navigate_text(&text));
    on!(on_go_back, c => || c.back());
    on!(on_go_forward, c => || c.forward());
    on!(on_go_up, c => || c.up());
    on!(on_refresh, c => || c.refresh());
    on!(on_crumb_clicked, c => |i| c.crumb(i));
    on!(on_item_pointer, c => |i, ctrl, shift| c.pointer(i, ctrl, shift));
    on!(on_item_activate, c => |i| c.activate_index(i));
    on!(on_background_pointer, c => |ctrl| c.background(ctrl));
    on!(on_nav_key, c => |key, shift, ctrl, cols, page| c.nav(
        key, shift, ctrl, cols, page
    ));
    on!(on_select_all, c => || c.select_all());
    on!(on_activate_selection, c => || c.activate_selection());
    on!(on_toggle_cursor, c => || c.toggle_cursor());
    on!(on_open_location, c => || c.open_location());
    on!(on_set_sort, c => |key, asc| c.set_sort(key, asc));
    on!(on_set_view, c => |v| c.set_view(v));
    on!(on_sidebar_clicked, c => |id| c.sidebar(&id));
    let weak = w.as_weak();
    w.on_request_close(move || {
        if let Some(w) = weak.upgrade() {
            let _ = w.hide();
        }
    });
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

impl Controller {
    // ---------- 导航 ----------

    /// 地址栏提交。
    pub fn navigate_text(&self, text: &str) {
        let cur = self.tab.borrow().location().map(PathBuf::from);
        match location::parse_address(text, cur.as_deref()) {
            Some(p) => self.navigate_path(p),
            None => self.set_message("请输入完整路径，例如 C:\\Users"),
        }
    }

    pub fn navigate_path(&self, p: PathBuf) {
        let req = self.tab.borrow_mut().navigate(p);
        self.start(req);
    }

    pub fn back(&self) {
        let req = self.tab.borrow_mut().back();
        if let Some(r) = req {
            self.start(r);
        }
    }

    pub fn forward(&self) {
        let req = self.tab.borrow_mut().forward();
        if let Some(r) = req {
            self.start(r);
        }
    }

    pub fn up(&self) {
        let req = self.tab.borrow_mut().up();
        if let Some(r) = req {
            self.start(r);
        }
    }

    pub fn refresh(&self) {
        let req = self.tab.borrow_mut().refresh();
        if let Some(r) = req {
            self.start(r);
        }
    }

    fn crumb(&self, i: i32) {
        let p = idx(i).and_then(|i| self.crumb_paths.borrow().get(i).map(|c| c.path.clone()));
        if let Some(p) = p {
            let same = self.tab.borrow().location() == Some(p.as_path());
            if same {
                self.refresh();
            } else {
                self.navigate_path(p);
            }
        }
    }

    fn sidebar(&self, id: &str) {
        if id == "home"
            && let Some(h) = self.home.clone()
        {
            self.navigate_path(h);
        }
    }

    fn start(&self, req: LoadRequest) {
        self.nav_started.set(Some(Instant::now()));
        self.message.borrow_mut().clear();
        self.items.reset();
        self.sync_all();
        match &self.loader {
            Loader::Threaded => {
                let id = self.id;
                std::thread::spawn(move || post(id, Msg::Loaded(req.run())));
            }
            Loader::Manual(q) => q.borrow_mut().push_back(req),
        }
    }

    /// 手动模式：执行所有排队的加载请求并应用结果，返回执行数量。线程模式下恒为 0。
    pub fn pump(&self) -> usize {
        let Loader::Manual(q) = &self.loader else {
            return 0;
        };
        let mut n = 0;
        loop {
            let req = q.borrow_mut().pop_front();
            let Some(req) = req else { break };
            self.on_loaded(req.run());
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
            Msg::Loaded(l) => self.on_loaded(l),
            Msg::OpenFailed { failed, message } => {
                self.set_message(&format!("有 {failed} 个文件未能打开：{message}"))
            }
        }
    }

    fn on_loaded(&self, l: Loaded) {
        let (applied, n) = {
            let mut t = self.tab.borrow_mut();
            let applied = t.apply(l);
            (applied, t.rows().len())
        };
        if !applied {
            return;
        }
        self.items.reset();
        self.sync_all();
        if perf::enabled()
            && let Some(t0) = self.nav_started.get()
        {
            perf::event("dir-loaded-us", Some(t0.elapsed().as_micros() as i64));
            perf::event("dir-entries", Some(n as i64));
            perf::mark_next_frame("dir-ready-us", t0);
        }
    }

    // ---------- 选择 ----------

    fn pointer(&self, i: i32, ctrl: bool, shift: bool) {
        if let Some(i) = idx(i) {
            self.mutate(|t| t.click(i, ctrl, shift));
        }
    }

    fn background(&self, ctrl: bool) {
        self.mutate(|t| t.click_empty(ctrl));
    }

    fn nav(&self, key: NavKey, shift: bool, ctrl: bool, cols: i32, page: i32) {
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
        self.mutate(|t| t.move_cursor(nav, cols, page, shift, ctrl));
    }

    fn select_all(&self) {
        self.mutate(Tab::select_all);
    }

    fn toggle_cursor(&self) {
        self.mutate(Tab::toggle_cursor);
    }

    /// 修改选择并只通知变化的行。
    fn mutate(&self, f: impl FnOnce(&mut Tab)) {
        let changed = {
            let mut t = self.tab.borrow_mut();
            f(&mut t);
            t.take_changed()
        };
        for i in changed {
            self.items.row_changed(i);
        }
        self.sync_selection();
    }

    // ---------- 激活 ----------

    fn activate_index(&self, i: i32) {
        let Some(i) = idx(i) else { return };
        let a = self.tab.borrow_mut().activate_index(i);
        self.run_activation(a);
    }

    fn activate_selection(&self) {
        let a = self.tab.borrow_mut().activate_selection();
        self.run_activation(a);
    }

    fn run_activation(&self, a: Activation) {
        match a {
            Activation::None => {}
            Activation::Navigate(req) => self.start(req),
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
    fn open_location(&self) {
        let p = self.tab.borrow().location().map(PathBuf::from);
        if let Some(p) = p {
            self.open(vec![p]);
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

    // ---------- 排序与视图 ----------

    fn set_sort(&self, key: SortKey, ascending: bool) {
        self.tab.borrow_mut().set_sort(SortSpec {
            key: core_sort(key),
            ascending,
        });
        self.items.reset();
        self.sync_all();
    }

    fn set_view(&self, v: ViewMode) {
        self.tab.borrow_mut().set_view(match v {
            ViewMode::Grid => CoreView::Grid,
            ViewMode::List => CoreView::List,
        });
        self.sync_all();
    }

    // ---------- 写回界面 ----------

    fn set_message(&self, m: &str) {
        *self.message.borrow_mut() = m.to_string();
        if let Some(w) = self.window.upgrade() {
            w.set_status_message(m.into());
        }
    }

    fn sync_all(&self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let t = self.tab.borrow();
        let loc = t.location().map(PathBuf::from);
        let (state, detail) = match t.state() {
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
        };
        w.set_content_state(state);
        w.set_error_detail(detail.into());
        w.set_view(match t.view() {
            CoreView::Grid => ViewMode::Grid,
            CoreView::List => ViewMode::List,
        });
        w.set_sort_key(ui_sort(t.sort().key));
        w.set_sort_ascending(t.sort().ascending);
        w.set_can_back(t.can_back());
        w.set_can_forward(t.can_forward());
        w.set_can_up(t.can_up());
        let (text, title, cr) = match &loc {
            Some(p) => (
                location::display(p),
                location::leaf_label(p),
                location::crumbs(p),
            ),
            None => (String::new(), String::new(), Vec::new()),
        };
        w.set_location_text(text.into());
        w.set_tab_title(title.into());
        self.crumbs.set_vec(
            cr.iter()
                .map(|c| Crumb {
                    label: c.label.as_str().into(),
                })
                .collect::<Vec<_>>(),
        );
        *self.crumb_paths.borrow_mut() = cr;
        let home_current = loc.is_some() && loc == self.home;
        w.set_sidebar_current(if home_current { "home" } else { "" }.into());
        w.set_status_message(self.message.borrow().as_str().into());
        drop(t);
        self.sync_selection();
    }

    fn sync_selection(&self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let t = self.tab.borrow();
        w.set_cursor_index(t.cursor().map_or(-1, |c| c as i32));
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
    }

    // ---------- 只读访问（harness 与基准使用） ----------

    pub fn location(&self) -> Option<PathBuf> {
        self.tab.borrow().location().map(PathBuf::from)
    }

    pub fn selected_names(&self) -> Vec<String> {
        self.tab
            .borrow()
            .selected_names()
            .into_iter()
            .map(String::from)
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tab
            .borrow()
            .rows()
            .iter()
            .map(|r| r.entry.name.clone())
            .collect()
    }
}

/// 进入目录计时：反复在 `target` 与其上级之间切换，每次进入 `target` 前记录 `bench-iter`，
/// 随后的 `dir-ready-us` 即为该次“进入目录到可操作”的耗时。结束后调用 `done`。
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
            if i > runs * 2 {
                done();
            } else if i % 2 == 1 {
                perf::event("bench-iter", Some((i / 2) as i64));
                c.navigate_path(target.clone());
            } else {
                c.navigate_path(parent.clone());
            }
        },
    );
    timer
}
