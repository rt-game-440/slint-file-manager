//! 搜索的界面接线（Issue #11）：搜索框 → 防抖/回车 → 后台搜索 → 事件回到界面线程 → 结果面板。
//!
//! - 一次只有一个搜索。每次发起（改查询、切换范围）都会取消上一个并递增会话号；
//!   事件带会话号回到界面线程，旧会话的迟到事件一律丢弃，所以改查询不会混入旧结果。
//! - 范围默认“当前目录及子目录”，目录在搜索面板首次出现时锁定（之后改查询不随浏览位置漂移）；
//!   一键切到“全盘”会按当前磁盘列表重新搜索。
//! - 增强后端（Everything）不可用或失败时的提示、回退由 [`crate::search::run`] 决定，这里只负责如实显示。
//! - harness 使用手动模式：搜索任务排队，调用 [`Controller::pump`] 才执行并把事件逐个送达
//!   （[`Controller::search_pump`] 可限制送达的事件数，用来截取“扫描中”的中间状态）。

use super::{Controller, Msg, post};
use crate::browse::{Entry, TabId, format};
use crate::search::{
    self, BackendKind, BackendPref, Cancel, Enhancer, Event, Hit, Notice, NoticeCode, NoticeKind,
    Query, Request, Scope, Summary,
};
use crate::{SearchItem, SearchNotice, SearchPhase, SearchView};
use slint::VecModel;
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 搜索所需的外部环境（产品用 [`SearchEnv::production`]，harness 注入）。
pub struct SearchEnv {
    /// 手动模式：任务排队，由 `pump` 执行（harness）；否则每次搜索一个后台线程（产品）
    pub manual: bool,
    /// 可选增强后端；`None` 表示不使用
    pub enhancer: Option<Arc<dyn Enhancer>>,
    pub pref: BackendPref,
    /// 全盘搜索的根目录；`None` 时用当前识别到的磁盘与云盘本地目录。harness 可注入并随时修改以模拟磁盘变化。
    pub all_roots: Option<Rc<RefCell<Vec<PathBuf>>>>,
    /// 输入后等待多久自动发起搜索
    pub debounce: Duration,
}

impl SearchEnv {
    /// 产品环境。环境变量 `SFM_SEARCH_BACKEND=basic` 强制只用基本搜索（排错、对比速度用）。
    pub fn production() -> SearchEnv {
        let basic = std::env::var("SFM_SEARCH_BACKEND")
            .map(|v| v.eq_ignore_ascii_case("basic"))
            .unwrap_or(false);
        SearchEnv {
            manual: false,
            enhancer: Some(Arc::new(search::everything::EverythingIpc::new())),
            pref: if basic {
                BackendPref::BasicOnly
            } else {
                BackendPref::Auto
            },
            all_roots: None,
            debounce: Duration::from_millis(250),
        }
    }

    /// 无头测试默认环境：手动模式、无增强、全盘根目录为空（需要时由场景注入）。
    pub fn inert() -> SearchEnv {
        SearchEnv {
            manual: true,
            enhancer: None,
            pref: BackendPref::Auto,
            all_roots: Some(Rc::new(RefCell::new(Vec::new()))),
            debounce: Duration::from_millis(250),
        }
    }
}

struct St {
    active: bool,
    text: String,
    scope_all: bool,
    scope_dir: Option<PathBuf>,
    roots: Vec<PathBuf>,
    phase: SearchPhase,
    backend: Option<BackendKind>,
    notice: Option<Notice>,
    progress: (u64, u64),
    summary: Option<Summary>,
    selected: Option<usize>,
    cancel: Option<Cancel>,
    /// 打开／定位失败等操作提示（显示在状态行）
    note: String,
}

impl St {
    fn idle() -> St {
        St {
            active: false,
            text: String::new(),
            scope_all: false,
            scope_dir: None,
            roots: Vec::new(),
            phase: SearchPhase::Scanning,
            backend: None,
            notice: None,
            progress: (0, 0),
            summary: None,
            selected: None,
            cancel: None,
            note: String::new(),
        }
    }
}

type ManualJob = (u64, Request, Cancel);

/// 控制器持有的搜索状态。
pub(super) struct SearchUi {
    env: SearchEnv,
    id: u64,
    model: Rc<VecModel<SearchItem>>,
    st: RefCell<St>,
    hits: RefCell<Vec<Hit>>,
    session: Cell<u64>,
    jobs: RefCell<Vec<ManualJob>>,
    events: RefCell<std::collections::VecDeque<(u64, Event)>>,
    debounce: slint::Timer,
    /// 定位结果：目标标签加载完成后选中这个名称
    locate: RefCell<Option<(TabId, OsString)>>,
}

impl SearchUi {
    pub(super) fn new(env: SearchEnv, id: u64) -> SearchUi {
        SearchUi {
            env,
            id,
            model: Rc::new(VecModel::default()),
            st: RefCell::new(St::idle()),
            hits: RefCell::new(Vec::new()),
            session: Cell::new(0),
            jobs: RefCell::new(Vec::new()),
            events: RefCell::new(Default::default()),
            debounce: slint::Timer::default(),
            locate: RefCell::new(None),
        }
    }

    pub(super) fn model(&self) -> slint::ModelRc<SearchItem> {
        slint::ModelRc::from(self.model.clone())
    }

    /// 手动模式下尚未执行或尚未送达的内容数。
    pub(super) fn pending(&self) -> usize {
        self.jobs.borrow().len() + self.events.borrow().len()
    }
}

/// 给 harness 与测试读取的搜索状态快照。
#[derive(Clone, Debug)]
pub struct SearchSnapshot {
    pub active: bool,
    pub phase: &'static str,
    pub scope_all: bool,
    pub scope_text: String,
    pub status_text: String,
    pub backend_text: String,
    pub notice_kind: &'static str,
    pub notice_text: String,
    pub backend: Option<BackendKind>,
    pub count: usize,
    pub selected: Option<usize>,
    pub summary: Option<Summary>,
}

fn phase_name(p: SearchPhase) -> &'static str {
    match p {
        SearchPhase::Scanning => "scanning",
        SearchPhase::Done => "done",
        SearchPhase::Empty => "empty",
        SearchPhase::Cancelled => "cancelled",
        SearchPhase::Unavailable => "unavailable",
    }
}

fn item_of(h: &Hit) -> SearchItem {
    let e = Entry::new(
        h.path.file_name().unwrap_or_default().to_os_string(),
        h.is_dir,
        0,
        None,
    );
    SearchItem {
        name: h.name().into(),
        location: h.parent().into(),
        icon: format::icon_name(&e).into(),
        is_dir: h.is_dir,
        selected: false,
        missing: false,
    }
}

// ---------- 文字（纯函数，可测） ----------

fn secs(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} 毫秒")
    } else {
        format!("{:.1} 秒", ms as f64 / 1000.0)
    }
}

pub(crate) fn scope_text(
    scope_all: bool,
    dir: Option<&std::path::Path>,
    roots: &[PathBuf],
) -> String {
    if scope_all {
        let list: Vec<String> = roots
            .iter()
            .map(|r| r.to_string_lossy().trim_end_matches('\\').to_string())
            .collect();
        if list.is_empty() {
            "范围：本机全部磁盘（当前没有识别到可搜索的磁盘）".into()
        } else {
            format!(
                "范围：本机全部磁盘与云盘本地目录（{}）；只按名称搜索，不读取文件内容，不登录云账号",
                list.join("、")
            )
        }
    } else {
        match dir {
            Some(d) => format!("范围：{} 及其全部子文件夹", d.display()),
            None => "范围：当前目录（当前标签没有可搜索的位置）".into(),
        }
    }
}

pub(crate) fn backend_text(b: Option<BackendKind>) -> String {
    match b {
        None => String::new(),
        Some(BackendKind::Basic) => "搜索能力：基本搜索".into(),
        Some(BackendKind::Everything) => "搜索能力：Everything（增强）".into(),
    }
}

/// 结果之后的补充说明：权限跳过、云文件夹、不可访问位置、截断、陈旧项。
fn extras(s: &Summary) -> String {
    let mut v = Vec::new();
    if s.denied > 0 {
        v.push(format!("已跳过 {} 个无权限的文件夹", s.denied));
    }
    if s.cloud_unexpanded > 0 {
        v.push(format!(
            "{} 个云文件夹尚未同步到本机，只显示名称、未展开",
            s.cloud_unexpanded
        ));
    }
    if s.links_skipped > 0 {
        v.push(format!(
            "{} 个链接文件夹未进入（避免循环）",
            s.links_skipped
        ));
    }
    if s.roots_unavailable > 0 {
        v.push(format!(
            "{} 个位置不可访问（磁盘已移除或未就绪）",
            s.roots_unavailable
        ));
    }
    if s.roots_denied > 0 {
        v.push(format!("{} 个位置没有读取权限", s.roots_denied));
    }
    if s.stale_dropped > 0 {
        v.push(format!("已剔除 {} 条已不存在的索引项", s.stale_dropped));
    }
    if s.truncated {
        v.push(format!(
            "结果过多，仅显示前 {} 项，请缩小条件",
            search::MAX_HITS
        ));
    }
    if v.is_empty() {
        String::new()
    } else {
        format!("；{}", v.join("；"))
    }
}

pub(crate) fn status_text(
    phase: SearchPhase,
    count: usize,
    progress: (u64, u64),
    sum: Option<&Summary>,
    scope_all: bool,
) -> String {
    match phase {
        SearchPhase::Scanning => {
            if progress.0 > 0 {
                format!(
                    "正在扫描… 已找到 {count} 项，已扫描 {} 个文件夹",
                    progress.0
                )
            } else {
                format!("正在搜索… 已找到 {count} 项")
            }
        }
        SearchPhase::Done | SearchPhase::Empty => {
            let s = sum.cloned().unwrap_or_default();
            let scanned = if s.dirs > 0 {
                format!("，扫描 {} 个文件夹、{} 个条目", s.dirs, s.entries)
            } else {
                String::new()
            };
            let head = if phase == SearchPhase::Empty {
                "已搜索完毕，没有找到匹配项".to_string()
            } else {
                format!("搜索完成：共 {count} 项")
            };
            format!(
                "{head}（用时 {}{scanned}）{}",
                secs(s.elapsed_ms),
                extras(&s)
            )
        }
        SearchPhase::Cancelled => {
            format!("已取消：已显示 {count} 项，未扫描完，结果不完整")
        }
        SearchPhase::Unavailable => {
            let s = sum.cloned().unwrap_or_default();
            if s.roots_denied > 0 && s.roots_unavailable == 0 {
                "搜索范围无法读取：没有访问权限".to_string()
            } else if scope_all {
                "搜索范围不可访问：没有识别到可用的磁盘".to_string()
            } else {
                "搜索范围不可访问：目录不存在、磁盘已移除或设备未就绪".to_string()
            }
        }
    }
}

impl Controller {
    // ---------- 回调入口 ----------

    /// 搜索框内容变化：空则关闭搜索，否则防抖后搜索。
    pub(super) fn search_edited(self: &Rc<Self>, text: &str) {
        if text.trim().is_empty() {
            self.search_close_inner(false);
            return;
        }
        let weak = Rc::downgrade(self);
        self.search.debounce.start(
            slint::TimerMode::SingleShot,
            self.search.env.debounce,
            move || {
                if let Some(c) = weak.upgrade() {
                    c.search_from_box();
                }
            },
        );
    }

    /// 回车：查询未变且有选中结果时打开它，否则立即搜索。
    pub(super) fn search_submit(&self) {
        let Some(text) = self.box_text() else { return };
        let same = {
            let st = self.search.st.borrow();
            st.active && st.text == text && st.selected.is_some()
        };
        if same {
            self.search_open_selected();
        } else {
            self.search_run(&text);
        }
    }

    pub(super) fn search_set_scope(&self, all: bool) {
        {
            let mut st = self.search.st.borrow_mut();
            if st.scope_all == all {
                return;
            }
            st.scope_all = all;
        }
        let text = self.search.st.borrow().text.clone();
        if !text.is_empty() {
            self.search_run(&text);
        }
    }

    pub(super) fn search_cancel(&self) {
        {
            let mut st = self.search.st.borrow_mut();
            if !st.active || st.phase != SearchPhase::Scanning {
                return;
            }
            if let Some(c) = st.cancel.take() {
                c.cancel();
            }
            st.phase = SearchPhase::Cancelled;
        }
        // 取消后迟到的事件一律丢弃
        self.search.session.set(self.search.session.get() + 1);
        self.search.events.borrow_mut().clear();
        self.search.jobs.borrow_mut().clear();
        self.search_sync();
    }

    pub(super) fn search_close(&self) {
        self.search_close_inner(true);
    }

    pub(super) fn search_select(&self, i: i32) {
        if let Some(i) = super::idx(i) {
            self.select_hit(Some(i));
        }
    }

    pub(super) fn search_nav(&self, delta: i32) {
        let n = self.search.hits.borrow().len();
        if n == 0 || !self.search.st.borrow().active {
            return;
        }
        let cur = self.search.st.borrow().selected;
        let next = match cur {
            None if delta >= 0 => 0,
            None => n - 1,
            Some(c) => (c as i64 + delta as i64).clamp(0, n as i64 - 1) as usize,
        };
        self.select_hit(Some(next));
    }

    pub(super) fn search_open(&self, i: i32) {
        let Some(i) = super::idx(i) else { return };
        self.select_hit(Some(i));
        self.open_hit(i);
    }

    pub(super) fn search_open_selected(&self) {
        let sel = self.search.st.borrow().selected;
        if let Some(i) = sel {
            self.open_hit(i);
        }
    }

    /// 定位：关闭搜索，在活动面板进入结果所在目录并选中它。
    pub(super) fn search_locate(&self) {
        let Some(i) = self.search.st.borrow().selected else {
            return;
        };
        let Some(hit) = self.live_hit(i) else { return };
        let (dir, select) = match hit.path.parent() {
            Some(p) if !p.as_os_str().is_empty() => (
                p.to_path_buf(),
                hit.path.file_name().map(|n| n.to_os_string()),
            ),
            _ => (hit.path.clone(), None),
        };
        self.search_close_inner(true);
        let p = self.ws.borrow().active_pane();
        let tab = self.ws.borrow().active_tab(p).id();
        *self.search.locate.borrow_mut() = select.map(|n| (tab, n));
        self.navigate_path(p, dir);
    }

    // ---------- 内部 ----------

    fn box_text(&self) -> Option<String> {
        self.window
            .upgrade()
            .map(|w| w.get_search_text().to_string())
    }

    fn search_from_box(&self) {
        if let Some(t) = self.box_text() {
            self.search_run(&t);
        }
    }

    fn search_all_roots(&self) -> Vec<PathBuf> {
        match &self.search.env.all_roots {
            Some(r) => r.borrow().clone(),
            None => self.live.search_roots(),
        }
    }

    /// 发起（或重新发起）搜索：取消上一个，清空旧结果，递增会话号。
    fn search_run(&self, text: &str) {
        self.search.debounce.stop();
        let q = Query::parse(text);
        if q.is_empty() {
            self.search_close_inner(false);
            return;
        }
        let sid = self.search.session.get() + 1;
        self.search.session.set(sid);
        self.search.hits.borrow_mut().clear();
        self.search.model.set_vec(Vec::new());
        self.search.events.borrow_mut().clear();
        self.search.jobs.borrow_mut().clear();
        let scope = {
            let first = !self.search.st.borrow().active;
            let loc = if first { self.location() } else { None };
            let mut st = self.search.st.borrow_mut();
            if let Some(c) = st.cancel.take() {
                c.cancel();
            }
            if first {
                st.scope_dir = loc;
            }
            st.active = true;
            st.text = text.to_string();
            st.phase = SearchPhase::Scanning;
            st.backend = None;
            st.notice = None;
            st.progress = (0, 0);
            st.summary = None;
            st.selected = None;
            st.note.clear();
            if st.scope_all {
                let roots = self.search_all_roots();
                st.roots = roots.clone();
                Some(Scope::AllDisks(roots))
            } else {
                st.roots = st.scope_dir.iter().cloned().collect();
                st.scope_dir.clone().map(Scope::Dir)
            }
        };
        let Some(scope) = scope else {
            self.search.st.borrow_mut().phase = SearchPhase::Unavailable;
            self.search_sync();
            return;
        };
        if scope.roots().is_empty() {
            let mut st = self.search.st.borrow_mut();
            st.phase = SearchPhase::Unavailable;
            st.summary = Some(Summary::default());
            drop(st);
            self.search_sync();
            return;
        }
        let req = Request {
            query: q,
            scope,
            pref: self.search.env.pref,
        };
        let cancel = Cancel::new();
        self.search.st.borrow_mut().cancel = Some(cancel.clone());
        if self.search.env.manual {
            self.search.jobs.borrow_mut().push((sid, req, cancel));
        } else {
            let id = self.search.id;
            let enh = self.search.env.enhancer.clone();
            std::thread::spawn(move || {
                search::run(&req, enh.as_deref(), &cancel, &|event| {
                    post(
                        id,
                        Msg::Search {
                            session: sid,
                            event,
                        },
                    )
                });
            });
        }
        self.search_sync();
    }

    /// 手动模式：执行排队的搜索，并至多送达 `limit` 个事件；返回送达数。线程模式恒为 0。
    pub fn search_pump(&self, limit: usize) -> usize {
        if !self.search.env.manual {
            return 0;
        }
        let jobs: Vec<ManualJob> = std::mem::take(&mut *self.search.jobs.borrow_mut());
        for (sid, req, cancel) in jobs {
            let out = Mutex::new(Vec::new());
            search::run(&req, self.search.env.enhancer.as_deref(), &cancel, &|e| {
                out.lock().unwrap().push((sid, e))
            });
            self.search
                .events
                .borrow_mut()
                .extend(out.into_inner().unwrap());
        }
        let mut n = 0;
        while n < limit {
            let next = self.search.events.borrow_mut().pop_front();
            let Some((sid, ev)) = next else { break };
            self.search_event(sid, ev);
            n += 1;
        }
        n
    }

    /// 当前搜索会话号（每次发起、取消、关闭都会递增；测试用它构造“旧会话的迟到事件”）。
    pub fn search_session(&self) -> u64 {
        self.search.session.get()
    }

    /// 送达一个搜索事件；会话号不是当前会话的一律丢弃（迟到的旧事件）。
    pub fn search_event(&self, session: u64, ev: Event) {
        if session != self.search.session.get() || !self.search.st.borrow().active {
            return;
        }
        match ev {
            Event::Started { backend, notice } => {
                let mut st = self.search.st.borrow_mut();
                st.backend = Some(backend);
                st.notice = notice;
            }
            Event::Reset => {
                self.search.hits.borrow_mut().clear();
                self.search.model.set_vec(Vec::new());
                self.search.st.borrow_mut().selected = None;
            }
            Event::Hits(h) => {
                let rows: Vec<SearchItem> = h.iter().map(item_of).collect();
                self.search.hits.borrow_mut().extend(h);
                for r in rows {
                    self.search.model.push(r);
                }
            }
            Event::Progress { dirs, entries } => {
                self.search.st.borrow_mut().progress = (dirs, entries);
            }
            Event::Done(sum) => {
                let count = self.search.hits.borrow().len();
                let mut st = self.search.st.borrow_mut();
                st.phase = if sum.cancelled {
                    SearchPhase::Cancelled
                } else if count == 0
                    && sum.dirs == 0
                    && sum.backend == Some(BackendKind::Basic)
                    && sum.roots_unavailable + sum.roots_denied > 0
                {
                    SearchPhase::Unavailable
                } else if count == 0 {
                    SearchPhase::Empty
                } else {
                    SearchPhase::Done
                };
                st.cancel = None;
                st.summary = Some(sum);
            }
        }
        self.search_sync();
    }

    fn live_hit(&self, i: usize) -> Option<Hit> {
        let hit = self.search.hits.borrow().get(i).cloned()?;
        // 结果可能已过期（文件被删、磁盘被移除）：不把过期条目当作在线文件打开
        if std::fs::symlink_metadata(&hit.path).is_err() {
            if let Some(mut r) = slint::Model::row_data(&*self.search.model, i) {
                r.missing = true;
                slint::Model::set_row_data(&*self.search.model, i, r);
            }
            self.search.st.borrow_mut().note = format!(
                "“{}”已不存在，或所在磁盘已移除（搜索结果已过期）",
                hit.name()
            );
            self.search_sync();
            return None;
        }
        Some(hit)
    }

    fn open_hit(&self, i: usize) {
        let Some(hit) = self.live_hit(i) else { return };
        if hit.is_dir {
            self.search_close_inner(true);
            let p = self.ws.borrow().active_pane();
            self.navigate_path(p, hit.path);
        } else {
            self.open(vec![hit.path]);
        }
    }

    fn select_hit(&self, sel: Option<usize>) {
        let old = self.search.st.borrow().selected;
        if old == sel {
            return;
        }
        let set = |i: usize, on: bool| {
            if let Some(mut r) = slint::Model::row_data(&*self.search.model, i)
                && r.selected != on
            {
                r.selected = on;
                slint::Model::set_row_data(&*self.search.model, i, r);
            }
        };
        if let Some(o) = old {
            set(o, false);
        }
        if let Some(n) = sel {
            set(n, true);
        }
        let mut st = self.search.st.borrow_mut();
        st.selected = sel;
        st.note.clear();
        drop(st);
        self.search_sync();
    }

    fn search_close_inner(&self, clear_box: bool) {
        self.search.debounce.stop();
        let was_active = {
            let mut st = self.search.st.borrow_mut();
            if let Some(c) = st.cancel.take() {
                c.cancel();
            }
            std::mem::replace(&mut *st, St::idle()).active
        };
        self.search.session.set(self.search.session.get() + 1);
        self.search.hits.borrow_mut().clear();
        self.search.model.set_vec(Vec::new());
        self.search.events.borrow_mut().clear();
        self.search.jobs.borrow_mut().clear();
        if let Some(w) = self.window.upgrade() {
            if clear_box {
                w.set_search_text("".into());
            }
            self.search_sync();
            if was_active {
                let p = self.ws.borrow().active_pane();
                self.focus_pane(p);
            }
        }
    }

    /// 导航或侧栏点击等离开搜索的操作调用。
    pub(super) fn search_dismiss(&self) {
        if self.search.st.borrow().active {
            self.search_close_inner(true);
        }
    }

    /// 目录读取完成后：若是“定位”的目标标签，选中目标名称。
    pub(super) fn search_after_load(&self, tab: TabId) {
        let target = {
            let g = self.search.locate.borrow();
            match &*g {
                Some((t, n)) if *t == tab => Some(n.clone()),
                _ => None,
            }
        };
        let Some(name) = target else { return };
        *self.search.locate.borrow_mut() = None;
        let p = {
            let mut ws = self.ws.borrow_mut();
            let Some((p, _)) = ws.locate(tab) else { return };
            let Some(t) = ws.tab_by_id_mut(tab) else {
                return;
            };
            if let Some(i) = t.rows().iter().position(|r| r.entry.os_name == name) {
                t.click(i, false, false);
                t.take_changed();
            }
            p
        };
        self.update_panes(&[p]);
        self.sync_global();
    }

    fn snapshot_parts(&self) -> (SearchView, String, &'static str) {
        let st = self.search.st.borrow();
        let count = self.search.hits.borrow().len();
        let mut status = status_text(
            st.phase,
            count,
            st.progress,
            st.summary.as_ref(),
            st.scope_all,
        );
        if !st.note.is_empty() {
            status = format!("{status}　{}", st.note);
        }
        // “未安装增强”不是故障，不弹提示条（否则没装 Everything 的用户每次搜索都被打扰），
        // 只在能力说明里写明；未运行、索引未就绪、通信失败、查询失败是需要说明的降级，用提示条。
        let not_installed = st
            .notice
            .as_ref()
            .is_some_and(|n| n.code == NoticeCode::NotInstalled);
        let (notice, notice_text) = match st.notice.as_ref().filter(|_| !not_installed) {
            Some(n) => (
                match n.kind {
                    NoticeKind::Info => SearchNotice::Info,
                    NoticeKind::Warn => SearchNotice::Warn,
                },
                n.text.clone(),
            ),
            None => (SearchNotice::None, String::new()),
        };
        let v = SearchView {
            active: st.active,
            phase: st.phase,
            scope_all: st.scope_all,
            scope_text: scope_text(st.scope_all, st.scope_dir.as_deref(), &st.roots).into(),
            status_text: status.into(),
            backend_text: if not_installed {
                format!("{}（未检测到 Everything）", backend_text(st.backend))
            } else {
                backend_text(st.backend)
            }
            .into(),
            notice,
            notice_text: notice_text.into(),
            count: count as i32,
            selected: st.selected.map_or(-1, |i| i as i32),
            can_act: st.selected.is_some(),
        };
        let nk = match notice {
            SearchNotice::None => "none",
            SearchNotice::Info => "info",
            SearchNotice::Warn => "warn",
        };
        (v, String::new(), nk)
    }

    pub(super) fn search_sync(&self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let (v, _, _) = self.snapshot_parts();
        w.set_search_view(v);
    }

    /// 搜索状态快照（harness 与测试读取）。
    pub fn search_snapshot(&self) -> SearchSnapshot {
        let (v, _, nk) = self.snapshot_parts();
        let st = self.search.st.borrow();
        SearchSnapshot {
            active: v.active,
            phase: phase_name(v.phase),
            scope_all: v.scope_all,
            scope_text: v.scope_text.to_string(),
            status_text: v.status_text.to_string(),
            backend_text: v.backend_text.to_string(),
            notice_kind: nk,
            notice_text: v.notice_text.to_string(),
            backend: st.backend,
            count: v.count as usize,
            selected: st.selected,
            summary: st.summary.clone(),
        }
    }

    /// 当前全部结果的完整路径（harness 与测试读取）。
    pub fn search_hit_paths(&self) -> Vec<PathBuf> {
        self.search
            .hits
            .borrow()
            .iter()
            .map(|h| h.path.clone())
            .collect()
    }

    /// 结果行里被标为“已过期”的行数。
    pub fn search_missing_rows(&self) -> usize {
        slint::Model::iter(&*self.search.model)
            .filter(|r| r.missing)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_texts_show_the_concrete_range() {
        let t = scope_text(false, Some(std::path::Path::new("C:\\项目")), &[]);
        assert!(t.contains("C:\\项目") && t.contains("全部子文件夹"));
        let t = scope_text(true, None, &[PathBuf::from("C:\\"), PathBuf::from("E:\\")]);
        assert!(t.contains("C:、E:") && t.contains("不读取文件内容"));
        assert!(scope_text(true, None, &[]).contains("没有识别到"));
    }

    #[test]
    fn status_texts_distinguish_states() {
        let s = status_text(SearchPhase::Scanning, 3, (12, 99), None, false);
        assert!(s.contains("正在扫描") && s.contains("已找到 3 项") && s.contains("12 个文件夹"));
        let sum = Summary {
            hits: 0,
            dirs: 5,
            entries: 40,
            denied: 2,
            cloud_unexpanded: 1,
            elapsed_ms: 1234,
            ..Default::default()
        };
        let s = status_text(SearchPhase::Empty, 0, (0, 0), Some(&sum), false);
        assert!(s.contains("没有找到匹配项") && s.contains("1.2 秒"));
        let fast = Summary {
            dirs: 1,
            entries: 2,
            elapsed_ms: 37,
            ..Default::default()
        };
        assert!(status_text(SearchPhase::Done, 1, (0, 0), Some(&fast), false).contains("37 毫秒"));
        assert!(s.contains("跳过 2 个无权限") && s.contains("云文件夹"));
        let s = status_text(SearchPhase::Done, 7, (0, 0), Some(&sum), false);
        assert!(s.contains("共 7 项"));
        let s = status_text(SearchPhase::Cancelled, 4, (0, 0), None, false);
        assert!(s.contains("已取消") && s.contains("不完整"));
        let s = status_text(
            SearchPhase::Unavailable,
            0,
            (0, 0),
            Some(&Summary {
                roots_unavailable: 1,
                ..Default::default()
            }),
            false,
        );
        assert!(s.contains("不可访问"));
        let s = status_text(
            SearchPhase::Unavailable,
            0,
            (0, 0),
            Some(&Summary {
                roots_denied: 1,
                ..Default::default()
            }),
            false,
        );
        assert!(s.contains("没有访问权限"));
    }

    #[test]
    fn backend_texts() {
        assert_eq!(backend_text(None), "");
        assert!(backend_text(Some(BackendKind::Basic)).contains("基本搜索"));
        assert!(backend_text(Some(BackendKind::Everything)).contains("Everything"));
    }
}
