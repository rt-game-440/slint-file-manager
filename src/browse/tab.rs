//! 浏览上下文：一个浏览标签的状态——当前位置、历史、已排序条目、排序与视图方式、选择与键盘光标、加载状态。
//!
//! 本模块不接触界面与线程：导航产生 [`LoadRequest`]，由调用方在任意线程执行 [`LoadRequest::run`]，
//! 再把 [`Loaded`] 交回 [`Tab::apply`]。序号不匹配的旧结果会被丢弃，快速连续导航时不会把旧目录贴到新位置。
//! 选择以条目为单位保存在行上，排序与视图切换不改变选择集合。
//!
//! 每个标签有自己的编号（[`TabId`]，由 [`super::workspace::Workspace`] 分配）、历史、排序与视图。
//! 从会话文件恢复的标签先处于 [`LoadState::Idle`]（只带位置，不读取目录），首次被激活时才读取；
//! 位置读取失败时标签保留，不自动换位置，也不自动重试。

use super::entry::{Entry, ListError, ListErrorKind, list_dir};
use super::history::History;
use super::location::is_recycle_bin;
use super::sort::{SortSpec, sort_by};
use crate::fileops::{Backend, BinItem};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// 标签编号：窗口生命周期内唯一，用来把异步读取结果送回正确的标签。
pub type TabId = u64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewMode {
    #[default]
    Grid,
    List,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub enum LoadState {
    /// 尚未进入任何位置。
    #[default]
    Idle,
    Loading,
    Ready,
    Failed {
        kind: ListErrorKind,
        message: String,
    },
}

#[derive(Clone, Debug)]
pub struct Row {
    pub entry: Entry,
    pub selected: bool,
}

/// 一次目录加载请求。可发送到工作线程执行。
#[derive(Clone, Debug)]
pub struct LoadRequest {
    pub seq: u64,
    pub path: PathBuf,
    pub sort: SortSpec,
}

/// 加载结果。
#[derive(Debug)]
pub struct Loaded {
    pub seq: u64,
    pub sort: SortSpec,
    pub result: Result<Vec<Entry>, ListError>,
}

impl LoadRequest {
    /// 读取并按请求时的排序方式排好（在工作线程执行，减少界面线程工作量）。
    /// 回收站位置走外壳命名空间（系统后端），其余位置按目录读取。
    pub fn run(&self) -> Loaded {
        self.run_with(&crate::fileops::SystemBackend)
    }

    /// 同 [`LoadRequest::run`]，回收站内容由 `backend` 提供（测试与 harness 注入用）。
    pub fn run_with(&self, backend: &dyn Backend) -> Loaded {
        let listed = if is_recycle_bin(&self.path) {
            list_bin(backend)
        } else {
            list_dir(&self.path)
        };
        let result = listed.map(|mut v| {
            sort_by(&mut v, self.sort, |e| e);
            v
        });
        Loaded {
            seq: self.seq,
            sort: self.sort,
            result,
        }
    }
}

/// 回收站内容转成条目（保留外壳标识与原位置，供恢复使用）。
fn list_bin(backend: &dyn Backend) -> Result<Vec<Entry>, ListError> {
    backend
        .list_bin()
        .map(|items| items.into_iter().map(Entry::from_bin).collect())
        .map_err(|message| ListError {
            kind: ListErrorKind::Other,
            os_code: None,
            message,
        })
}

/// 激活（双击、回车）的结果。
#[derive(Debug)]
pub enum Activation {
    None,
    Navigate(LoadRequest),
    /// 回收站里的项目不能直接打开或进入：先还原到原位置。
    InRecycleBin,
    /// 交给默认应用打开的文件；`truncated` 为超过上限未打开的数量。
    Open {
        paths: Vec<PathBuf>,
        truncated: usize,
    },
}

/// 一次最多交给默认应用打开的文件数，防止误按回车打开大量文件。
pub const MAX_OPEN: usize = 16;

/// 键盘移动方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
}

/// 加载完成后要恢复的选择。
#[derive(Debug, Default)]
enum Restore {
    #[default]
    None,
    /// 选中并聚焦某个名称（返回上级或后退时选中来源文件夹）。
    Focus(OsString),
    /// 刷新时保留原有选择与光标。
    Keep {
        selected: Vec<OsString>,
        cursor: Option<OsString>,
    },
}

/// 恢复标签所需的持久状态（不含选择与光标）。
#[derive(Clone, Debug, PartialEq)]
pub struct TabSeed {
    pub location: PathBuf,
    pub sort: SortSpec,
    pub view: ViewMode,
    pub back: Vec<PathBuf>,
    pub forward: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Tab {
    id: TabId,
    /// 由会话恢复而来、且尚未被用户导航过
    restored: bool,
    location: Option<PathBuf>,
    history: History,
    rows: Vec<Row>,
    sort: SortSpec,
    view: ViewMode,
    state: LoadState,
    seq: u64,
    cursor: Option<usize>,
    anchor: Option<usize>,
    selected_count: usize,
    changed: Vec<usize>,
    restore: Restore,
    /// 静默刷新请求的序号（外部变化、磁盘恢复探测）：不显示“加载中”、不清空列表，结果按名称合并。
    silent_seq: Option<u64>,
    /// 读取过程中又收到了外部变化通知：读取完成后需要再静默刷新一次。
    stale: bool,
    /// 标签离开可见状态期间没有被监视，再次可见时需要静默刷新。
    needs_refresh: bool,
}

impl Tab {
    pub fn new() -> Tab {
        Tab::default()
    }

    /// 带编号的空标签。
    pub fn with_id(id: TabId) -> Tab {
        Tab {
            id,
            ..Tab::default()
        }
    }

    /// 从持久状态恢复：只设置位置、排序、视图与历史，状态为 `Idle`，不读取目录。
    pub fn from_seed(id: TabId, seed: TabSeed) -> Tab {
        Tab {
            id,
            restored: true,
            location: Some(seed.location),
            sort: seed.sort,
            view: seed.view,
            history: History::from_parts(seed.back, seed.forward),
            ..Tab::default()
        }
    }

    /// 以当前位置、排序和视图新开一个标签（不带历史、选择）。新标签与原标签此后互不影响。
    pub fn duplicate(&self, id: TabId) -> Tab {
        Tab {
            id,
            location: self.location.clone(),
            sort: self.sort,
            view: self.view,
            ..Tab::default()
        }
    }

    /// 持久状态；没有位置的标签（尚未进入任何目录）返回 `None`。
    pub fn seed(&self) -> Option<TabSeed> {
        let (back, forward) = self.history.parts();
        Some(TabSeed {
            location: self.location.clone()?,
            sort: self.sort,
            view: self.view,
            back,
            forward,
        })
    }

    pub fn id(&self) -> TabId {
        self.id
    }

    /// 由会话恢复且用户尚未导航过。
    pub fn is_restored(&self) -> bool {
        self.restored
    }

    /// 位置当前读取失败（路径不存在、无权限、磁盘未连接等），标签与位置都保留。
    pub fn is_unavailable(&self) -> bool {
        matches!(self.state, LoadState::Failed { .. })
    }

    /// 标签被激活（切换到、所在面板变为可见）时调用：尚未读取、或上次读取失败的标签读取一次。
    /// 已就绪或正在读取的标签返回 `None`，因此反复切换不会产生重复请求；失败不会自动重试。
    pub fn ensure_loaded(&mut self) -> Option<LoadRequest> {
        let path = self.location.clone()?;
        match self.state {
            LoadState::Idle => Some(self.begin(path, Restore::None)),
            LoadState::Failed { .. } => self.refresh(),
            LoadState::Ready if self.needs_refresh => self.reload_silent(),
            _ => None,
        }
    }

    /// 标签不再被监视（切到后台、面板隐藏）：之后再次可见时静默刷新一次。
    pub fn mark_needs_refresh(&mut self) {
        if self.state == LoadState::Ready {
            self.needs_refresh = true;
        }
    }

    /// 外部变化（目录监视、磁盘恢复探测）引起的静默刷新：保留列表、选择与光标，不显示“加载中”。
    /// 正在读取时不另发请求，而是记下“读完后再刷新一次”；尚未读取（`Idle`）或没有位置时返回 `None`。
    pub fn reload_silent(&mut self) -> Option<LoadRequest> {
        let path = self.location.clone()?;
        match self.state {
            LoadState::Idle => None,
            LoadState::Loading => {
                self.stale = true;
                None
            }
            LoadState::Ready | LoadState::Failed { .. } => {
                self.needs_refresh = false;
                self.seq += 1;
                self.silent_seq = Some(self.seq);
                Some(LoadRequest {
                    seq: self.seq,
                    path,
                    sort: self.sort,
                })
            }
        }
    }

    /// 取走“读取期间又有变化”的标记。
    pub fn take_stale(&mut self) -> bool {
        std::mem::take(&mut self.stale)
    }

    /// 位置所在的磁盘已断开等：立即把标签标为不可用（清空列表、丢弃在途读取），位置保留。
    /// 已经是不可用或尚未读取时返回 `false`。
    pub fn mark_unavailable(&mut self, message: &str) -> bool {
        if self.location.is_none() || !matches!(self.state, LoadState::Ready | LoadState::Loading) {
            return false;
        }
        self.clear_rows();
        self.seq += 1;
        self.silent_seq = None;
        self.stale = false;
        self.state = LoadState::Failed {
            kind: ListErrorKind::NotFound,
            message: message.to_string(),
        };
        true
    }

    fn clear_rows(&mut self) {
        self.rows.clear();
        self.changed.clear();
        self.cursor = None;
        self.anchor = None;
        self.selected_count = 0;
    }

    // ---------- 读取 ----------

    pub fn location(&self) -> Option<&Path> {
        self.location.as_deref()
    }
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }
    pub fn state(&self) -> &LoadState {
        &self.state
    }
    pub fn sort(&self) -> SortSpec {
        self.sort
    }
    pub fn view(&self) -> ViewMode {
        self.view
    }
    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }
    pub fn selected_count(&self) -> usize {
        self.selected_count
    }
    pub fn can_back(&self) -> bool {
        self.history.can_back()
    }
    pub fn can_forward(&self) -> bool {
        self.history.can_forward()
    }
    pub fn can_up(&self) -> bool {
        !self.is_recycle_bin() && self.location.as_deref().and_then(Path::parent).is_some()
    }
    /// 当前位置是回收站（虚拟位置，条目来自外壳而不是磁盘目录）。
    pub fn is_recycle_bin(&self) -> bool {
        self.location.as_deref().is_some_and(is_recycle_bin)
    }
    /// 已选项目的完整路径（回收站里的项目没有当前路径，返回空）。
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        if self.is_recycle_bin() {
            return Vec::new();
        }
        (0..self.rows.len())
            .filter(|&i| self.rows[i].selected)
            .filter_map(|i| self.path_of(i))
            .collect()
    }
    /// 已选的回收站项目（非回收站视图返回空）。
    pub fn selected_bin_items(&self) -> Vec<BinItem> {
        self.rows
            .iter()
            .filter(|r| r.selected)
            .filter_map(|r| r.entry.bin.as_deref().cloned())
            .collect()
    }
    /// 已选文件的总字节数（不含文件夹）。
    pub fn selected_size(&self) -> u64 {
        self.rows
            .iter()
            .filter(|r| r.selected && !r.entry.is_dir)
            .map(|r| r.entry.size)
            .sum()
    }
    pub fn selected_names(&self) -> Vec<&str> {
        self.rows
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.entry.name.as_str())
            .collect()
    }
    /// 第 `i` 行是否被选中（越界为 `false`）。
    pub fn is_selected(&self, i: usize) -> bool {
        self.rows.get(i).is_some_and(|r| r.selected)
    }
    /// 第 `i` 行是否为文件夹（越界为 `false`）。
    pub fn is_dir_at(&self, i: usize) -> bool {
        self.rows.get(i).is_some_and(|r| r.entry.is_dir)
    }
    /// 第 `i` 行的完整路径；回收站里的项目没有当前路径。
    pub fn path_of(&self, i: usize) -> Option<PathBuf> {
        if self.is_recycle_bin() {
            return None;
        }
        Some(
            self.location
                .as_ref()?
                .join(&self.rows.get(i)?.entry.os_name),
        )
    }
    /// 取出自上次调用以来选择状态变化的行号。
    pub fn take_changed(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.changed)
    }

    // ---------- 导航 ----------

    /// 进入新位置（记入历史）。
    pub fn navigate(&mut self, path: PathBuf) -> LoadRequest {
        self.restored = false;
        if let Some(old) = self.location.take()
            && old != path
        {
            self.history.visit(old);
        }
        self.begin(path, Restore::None)
    }

    pub fn back(&mut self) -> Option<LoadRequest> {
        let cur = self.location.clone()?;
        let p = self.history.back(cur.clone())?;
        let r = child_name(&p, &cur).map_or(Restore::None, Restore::Focus);
        Some(self.begin(p, r))
    }

    pub fn forward(&mut self) -> Option<LoadRequest> {
        let cur = self.location.clone()?;
        let p = self.history.forward(cur.clone())?;
        let r = child_name(&p, &cur).map_or(Restore::None, Restore::Focus);
        Some(self.begin(p, r))
    }

    /// 返回上级，并选中来源文件夹。
    pub fn up(&mut self) -> Option<LoadRequest> {
        if self.is_recycle_bin() {
            return None;
        }
        let cur = self.location.clone()?;
        let parent = cur.parent()?.to_path_buf();
        self.history.visit(cur.clone());
        let r = cur
            .file_name()
            .map_or(Restore::None, |n| Restore::Focus(n.to_os_string()));
        Some(self.begin(parent, r))
    }

    /// 重新读取当前位置，保留选择。
    pub fn refresh(&mut self) -> Option<LoadRequest> {
        let p = self.location.clone()?;
        let selected = self
            .rows
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.entry.os_name.clone())
            .collect();
        let cursor = self.cursor_name();
        Some(self.begin(p, Restore::Keep { selected, cursor }))
    }

    fn begin(&mut self, path: PathBuf, restore: Restore) -> LoadRequest {
        self.location = Some(path.clone());
        self.clear_rows();
        self.state = LoadState::Loading;
        self.seq += 1;
        self.silent_seq = None;
        self.stale = false;
        self.needs_refresh = false;
        self.restore = restore;
        LoadRequest {
            seq: self.seq,
            path,
            sort: self.sort,
        }
    }

    /// 应用加载结果。旧请求的结果返回 `false` 并被丢弃。
    pub fn apply(&mut self, loaded: Loaded) -> bool {
        if self.silent_seq == Some(loaded.seq) && loaded.seq == self.seq {
            self.silent_seq = None;
            return self.apply_silent(loaded);
        }
        if loaded.seq != self.seq || self.state != LoadState::Loading {
            return false;
        }
        match loaded.result {
            Ok(entries) => {
                self.rows = entries
                    .into_iter()
                    .map(|entry| Row {
                        entry,
                        selected: false,
                    })
                    .collect();
                if loaded.sort != self.sort {
                    sort_by(&mut self.rows, self.sort, |r| &r.entry);
                }
                self.state = LoadState::Ready;
                match std::mem::take(&mut self.restore) {
                    Restore::None => {}
                    Restore::Focus(name) => {
                        if let Some(i) = self.index_of(&name) {
                            self.set_sel(i, true);
                            self.cursor = Some(i);
                            self.anchor = Some(i);
                        }
                    }
                    Restore::Keep { selected, cursor } => {
                        for n in selected {
                            if let Some(i) = self.index_of(&n) {
                                self.set_sel(i, true);
                            }
                        }
                        self.cursor = cursor.and_then(|n| self.index_of(&n));
                        self.anchor = self.cursor;
                    }
                }
                self.changed.clear(); // 整体刷新，由调用方重置模型
            }
            Err(e) => {
                self.state = LoadState::Failed {
                    kind: e.kind,
                    message: e.message,
                };
            }
        }
        true
    }

    /// 静默刷新结果：与现有列表相同则不动（返回 `false`，界面无需重绘）；不同则按名称合并，
    /// 保留仍存在条目的选择；光标条目被删时落到原位置的相邻条目，不跳到别处。
    fn apply_silent(&mut self, loaded: Loaded) -> bool {
        if !matches!(self.state, LoadState::Ready | LoadState::Failed { .. }) {
            return false;
        }
        let mut entries = match loaded.result {
            Ok(v) => v,
            Err(e) => {
                let new = LoadState::Failed {
                    kind: e.kind,
                    message: e.message,
                };
                if self.state == new {
                    return false;
                }
                self.clear_rows();
                self.state = new;
                return true;
            }
        };
        if loaded.sort != self.sort {
            sort_by(&mut entries, self.sort, |e| e);
        }
        if self.state == LoadState::Ready
            && entries.len() == self.rows.len()
            && entries
                .iter()
                .zip(&self.rows)
                .all(|(a, b)| same_entry(a, &b.entry))
        {
            return false;
        }
        let selected: std::collections::HashSet<OsString> = self
            .rows
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.entry.os_name.clone())
            .collect();
        let cursor_name = self.cursor_name();
        let cursor_old = self.cursor;
        let anchor_name = self
            .anchor
            .and_then(|i| self.rows.get(i))
            .map(|r| r.entry.os_name.clone());
        self.rows = entries
            .into_iter()
            .map(|entry| Row {
                selected: selected.contains(&entry.os_name),
                entry,
            })
            .collect();
        self.selected_count = self.rows.iter().filter(|r| r.selected).count();
        self.cursor = cursor_name
            .and_then(|n| self.index_of(&n))
            .or_else(|| cursor_old.filter(|_| !self.rows.is_empty()))
            .map(|i| i.min(self.rows.len().saturating_sub(1)));
        self.anchor = anchor_name.and_then(|n| self.index_of(&n)).or(self.cursor);
        self.changed.clear();
        self.state = LoadState::Ready;
        true
    }

    // ---------- 排序与视图 ----------

    /// 改变排序，保留选择、光标与锚点。
    pub fn set_sort(&mut self, spec: SortSpec) {
        if spec == self.sort {
            return;
        }
        self.sort = spec;
        let cursor = self.cursor_name();
        let anchor = self
            .anchor
            .and_then(|i| self.rows.get(i))
            .map(|r| r.entry.os_name.clone());
        sort_by(&mut self.rows, spec, |r| &r.entry);
        self.cursor = cursor.and_then(|n| self.index_of(&n));
        self.anchor = anchor.and_then(|n| self.index_of(&n));
        self.changed.clear(); // 顺序整体改变，由调用方重置模型
    }

    pub fn set_view(&mut self, view: ViewMode) {
        self.view = view;
    }

    // ---------- 选择 ----------

    fn set_sel(&mut self, i: usize, v: bool) {
        let r = &mut self.rows[i];
        if r.selected != v {
            r.selected = v;
            self.changed.push(i);
            if v {
                self.selected_count += 1;
            } else {
                self.selected_count -= 1;
            }
        }
    }

    fn select_only_range(&mut self, a: usize, b: usize) {
        let (lo, hi) = (a.min(b), a.max(b));
        for i in 0..self.rows.len() {
            self.set_sel(i, i >= lo && i <= hi);
        }
    }

    /// 鼠标按下：普通点击单选；Ctrl 切换；Shift 从锚点到此处范围选择（Ctrl+Shift 追加范围）。
    pub fn click(&mut self, i: usize, ctrl: bool, shift: bool) {
        if i >= self.rows.len() {
            return;
        }
        if shift {
            let a = self.anchor.unwrap_or(0).min(self.rows.len() - 1);
            if ctrl {
                for k in a.min(i)..=a.max(i) {
                    self.set_sel(k, true);
                }
            } else {
                self.select_only_range(a, i);
            }
            self.anchor = Some(a);
        } else if ctrl {
            let v = !self.rows[i].selected;
            self.set_sel(i, v);
            self.anchor = Some(i);
        } else {
            self.select_only_range(i, i);
            self.anchor = Some(i);
        }
        self.cursor = Some(i);
    }

    /// 点击空白处：不按 Ctrl 时清空选择。
    pub fn click_empty(&mut self, ctrl: bool) {
        if !ctrl {
            self.clear_selection();
        }
    }

    pub fn clear_selection(&mut self) {
        for i in 0..self.rows.len() {
            self.set_sel(i, false);
        }
    }

    pub fn select_all(&mut self) {
        for i in 0..self.rows.len() {
            self.set_sel(i, true);
        }
        if self.cursor.is_none() && !self.rows.is_empty() {
            self.cursor = Some(0);
        }
    }

    /// Ctrl+空格：切换光标所在项。
    pub fn toggle_cursor(&mut self) {
        if let Some(i) = self.cursor {
            let v = !self.rows[i].selected;
            self.set_sel(i, v);
            self.anchor = Some(i);
        } else if !self.rows.is_empty() {
            self.click(0, false, false);
        }
    }

    /// 键盘移动。`cols` 为网格列数（列表视图为 1，此时左右键无效），`page` 为一页的行数。
    /// 普通移动单选；Shift 从锚点范围选择；Ctrl 只移动光标。
    pub fn move_cursor(&mut self, nav: Nav, cols: usize, page: usize, shift: bool, ctrl: bool) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let cols = cols.max(1);
        let page = page.max(1);
        let target = match self.cursor {
            None => 0,
            Some(c) => match nav {
                Nav::Left if cols > 1 => c.saturating_sub(1),
                Nav::Right if cols > 1 => (c + 1).min(n - 1),
                Nav::Left | Nav::Right => c,
                Nav::Up => c.checked_sub(cols).unwrap_or(c),
                Nav::Down => {
                    if c + cols < n {
                        c + cols
                    } else {
                        c
                    }
                }
                Nav::Home => 0,
                Nav::End => n - 1,
                Nav::PageUp => c.saturating_sub(cols * page),
                Nav::PageDown => (c + cols * page).min(n - 1),
            },
        };
        if shift {
            let a = self.anchor.unwrap_or(target);
            self.select_only_range(a, target);
            self.anchor = Some(a);
        } else if !ctrl {
            self.select_only_range(target, target);
            self.anchor = Some(target);
        }
        self.cursor = Some(target);
    }

    // ---------- 激活 ----------

    /// 双击某项：文件夹进入，文件交给默认应用。
    pub fn activate_index(&mut self, i: usize) -> Activation {
        let Some(row) = self.rows.get(i) else {
            return Activation::None;
        };
        if self.is_recycle_bin() {
            return Activation::InRecycleBin;
        }
        let path = self.path_of(i).expect("有行就有位置");
        if row.entry.is_dir {
            Activation::Navigate(self.navigate(path))
        } else {
            Activation::Open {
                paths: vec![path],
                truncated: 0,
            }
        }
    }

    /// 回车：有选中文件时打开全部选中文件（最多 [`MAX_OPEN`] 个）；只选了文件夹时进入第一个；
    /// 没有选择时作用于光标项。
    pub fn activate_selection(&mut self) -> Activation {
        if self.is_recycle_bin() {
            return if self.rows.is_empty() {
                Activation::None
            } else {
                Activation::InRecycleBin
            };
        }
        let selected: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.rows[i].selected)
            .collect();
        if selected.is_empty() {
            return match self.cursor {
                Some(c) => self.activate_index(c),
                None => Activation::None,
            };
        }
        let files: Vec<usize> = selected
            .iter()
            .copied()
            .filter(|&i| !self.rows[i].entry.is_dir)
            .collect();
        if files.is_empty() {
            return self.activate_index(selected[0]);
        }
        let truncated = files.len().saturating_sub(MAX_OPEN);
        let paths = files
            .into_iter()
            .take(MAX_OPEN)
            .filter_map(|i| self.path_of(i))
            .collect();
        Activation::Open { paths, truncated }
    }

    fn cursor_name(&self) -> Option<OsString> {
        self.cursor
            .and_then(|i| self.rows.get(i))
            .map(|r| r.entry.os_name.clone())
    }

    fn index_of(&self, name: &OsString) -> Option<usize> {
        self.rows.iter().position(|r| &r.entry.os_name == name)
    }
}

/// 两个条目显示信息是否相同（名称、类型、大小、修改时间）。
fn same_entry(a: &Entry, b: &Entry) -> bool {
    a.os_name == b.os_name && a.is_dir == b.is_dir && a.size == b.size && a.modified == b.modified
}

/// `child` 是 `parent` 的直接子路径时返回其名称。
fn child_name(parent: &Path, child: &Path) -> Option<OsString> {
    (child.parent()? == parent).then(|| child.file_name().map(|n| n.to_os_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browse::sort::SortKey;

    fn loaded(tab: &Tab, req: &LoadRequest, names: &[(&str, bool, u64)]) -> Loaded {
        let mut v: Vec<Entry> = names
            .iter()
            .map(|(n, d, s)| Entry::new((*n).into(), *d, *s, None))
            .collect();
        sort_by(&mut v, req.sort, |e| e);
        let _ = tab;
        Loaded {
            seq: req.seq,
            sort: req.sort,
            result: Ok(v),
        }
    }

    const ITEMS: &[(&str, bool, u64)] = &[
        ("子目录", true, 0),
        ("a.txt", false, 30),
        ("b.txt", false, 10),
        ("c.txt", false, 20),
        ("d.txt", false, 40),
    ];

    fn ready() -> Tab {
        let mut t = Tab::new();
        let r = t.navigate(r"C:\根".into());
        let l = loaded(&t, &r, ITEMS);
        assert!(t.apply(l));
        t
    }

    #[test]
    fn stale_results_are_dropped() {
        let mut t = Tab::new();
        let r1 = t.navigate(r"C:\一".into());
        let r2 = t.navigate(r"C:\二".into());
        assert!(!t.apply(loaded(&t, &r1, ITEMS)));
        assert_eq!(t.state(), &LoadState::Loading);
        assert!(t.apply(loaded(&t, &r2, &ITEMS[..2])));
        assert_eq!(t.rows().len(), 2);
        assert_eq!(t.location(), Some(Path::new(r"C:\二")));
        assert!(t.can_back());
    }

    #[test]
    fn selected_paths_follow_selection() {
        let mut t = ready();
        assert!(t.selected_paths().is_empty());
        t.click(1, false, false);
        t.click(3, true, false);
        assert_eq!(
            t.selected_paths(),
            [Path::new(r"C:\根\a.txt"), Path::new(r"C:\根\c.txt")]
        );
        assert!(t.is_selected(1) && !t.is_selected(2) && !t.is_selected(99));
        assert!(t.is_dir_at(0) && !t.is_dir_at(1) && !t.is_dir_at(99));
    }

    #[test]
    fn click_ctrl_shift_and_all() {
        let mut t = ready();
        t.click(1, false, false);
        t.click(3, false, true);
        assert_eq!(t.selected_names(), ["a.txt", "b.txt", "c.txt"]);
        t.click(0, true, false);
        assert_eq!(t.selected_count(), 4);
        t.click(2, true, false);
        assert_eq!(t.selected_names(), ["子目录", "a.txt", "c.txt"]);
        t.click_empty(false);
        assert_eq!(t.selected_count(), 0);
        t.select_all();
        assert_eq!(t.selected_count(), 5);
        // 只记录真正变化的行：1 + 2 + 1 + 1 + 3（清空）+ 5（全选）
        assert_eq!(t.take_changed().len(), 13);
    }

    #[test]
    fn selection_survives_sort() {
        let mut t = ready();
        t.click(1, false, false); // a.txt
        t.click(3, true, false); // c.txt
        t.set_sort(SortSpec {
            key: SortKey::Size,
            ascending: false,
        });
        let names: Vec<_> = t.rows().iter().map(|r| r.entry.name.as_str()).collect();
        assert_eq!(names, ["d.txt", "a.txt", "c.txt", "b.txt", "子目录"]);
        assert_eq!(t.selected_names(), ["a.txt", "c.txt"]);
        assert_eq!(t.rows()[t.cursor().unwrap()].entry.name, "c.txt");
        t.set_view(ViewMode::List);
        assert_eq!(t.selected_count(), 2);
    }

    #[test]
    fn keyboard_moves_in_grid_and_list() {
        let mut t = ready();
        t.move_cursor(Nav::Right, 3, 2, false, false); // 无光标时先到 0
        assert_eq!(t.cursor(), Some(0));
        t.move_cursor(Nav::Down, 3, 2, false, false);
        assert_eq!(t.cursor(), Some(3));
        t.move_cursor(Nav::Right, 3, 2, true, false);
        assert_eq!(t.selected_names(), ["c.txt", "d.txt"]);
        t.move_cursor(Nav::Home, 3, 2, false, true); // Ctrl 只移动光标
        assert_eq!(t.cursor(), Some(0));
        assert_eq!(t.selected_count(), 2);
        t.move_cursor(Nav::Right, 1, 2, false, false); // 列表视图左右键无效
        assert_eq!(t.cursor(), Some(0));
        t.move_cursor(Nav::End, 1, 2, false, false);
        assert_eq!(t.selected_names(), ["d.txt"]);
    }

    #[test]
    fn activation_rules() {
        let mut t = ready();
        t.click(1, false, false);
        t.click(2, true, false);
        match t.activate_selection() {
            Activation::Open { paths, truncated } => {
                assert_eq!(
                    paths,
                    [PathBuf::from(r"C:\根\a.txt"), PathBuf::from(r"C:\根\b.txt")]
                );
                assert_eq!(truncated, 0);
            }
            a => panic!("{a:?}"),
        }
        t.click(0, false, false);
        match t.activate_selection() {
            Activation::Navigate(r) => assert_eq!(r.path, PathBuf::from(r"C:\根\子目录")),
            a => panic!("{a:?}"),
        }
    }

    #[test]
    fn up_and_back_focus_source_folder() {
        let mut t = Tab::new();
        let r = t.navigate(r"C:\根\子目录".into());
        t.apply(loaded(&t, &r, &[]));
        let r = t.up().unwrap();
        assert_eq!(r.path, PathBuf::from(r"C:\根"));
        t.apply(loaded(&t, &r, ITEMS));
        assert_eq!(t.selected_names(), ["子目录"]);
        assert_eq!(t.cursor(), Some(0));
        let r = t.back().unwrap();
        assert_eq!(r.path, PathBuf::from(r"C:\根\子目录"));
        t.apply(loaded(&t, &r, &[]));
        let r = t.forward().unwrap();
        t.apply(loaded(&t, &r, ITEMS));
        assert_eq!(t.selected_names(), ["子目录"]);
    }

    #[test]
    fn refresh_keeps_selection_and_failure_state() {
        let mut t = ready();
        t.click(2, false, false);
        let r = t.refresh().unwrap();
        t.apply(loaded(&t, &r, ITEMS));
        assert_eq!(t.selected_names(), ["b.txt"]);
        let r = t.navigate(r"C:\无".into());
        t.apply(Loaded {
            seq: r.seq,
            sort: r.sort,
            result: Err(ListError {
                kind: ListErrorKind::AccessDenied,
                os_code: Some(5),
                message: "拒绝访问".into(),
            }),
        });
        assert!(matches!(
            t.state(),
            LoadState::Failed {
                kind: ListErrorKind::AccessDenied,
                ..
            }
        ));
        assert!(t.rows().is_empty());
    }

    // ---------- 静默刷新（外部变化，Issue #7） ----------

    fn silent(t: &mut Tab, names: &[(&str, bool, u64)]) -> bool {
        let r = t.reload_silent().expect("就绪的标签应能静默刷新");
        let l = loaded(t, &r, names);
        t.apply(l)
    }

    #[test]
    fn silent_reload_keeps_selection_and_ignores_identical() {
        let mut t = ready();
        t.click(2, false, false); // b.txt
        t.click(3, false, true); // 选到 c.txt
        let before = t
            .selected_names()
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert!(!silent(&mut t, ITEMS), "内容相同：不需要重绘");
        assert_eq!(t.state(), &LoadState::Ready, "静默刷新不显示加载中");
        // 外部新增 e.txt
        let mut v = ITEMS.to_vec();
        v.push(("e.txt", false, 5));
        assert!(silent(&mut t, &v));
        assert_eq!(t.rows().len(), 6);
        assert_eq!(
            t.selected_names()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            before,
            "选择按名称保留"
        );
        assert_eq!(t.selected_count(), 2);
        // 重新进入后再次相同则不重绘
        assert!(!silent(&mut t, &v));
    }

    #[test]
    fn silent_reload_cursor_stays_on_same_file_or_neighbor() {
        let mut t = ready();
        t.click(3, false, false); // c.txt（行 3）
        assert_eq!(t.cursor(), Some(3));
        // 在它前面新增文件：光标跟着文件走
        let mut v = ITEMS.to_vec();
        v.push(("a0.txt", false, 1));
        assert!(silent(&mut t, &v));
        let cur = t.cursor().unwrap();
        assert_eq!(t.rows()[cur].entry.name, "c.txt");
        // 删除光标文件：落到原位置的相邻条目，且仍有光标
        let v: Vec<_> = v.into_iter().filter(|(n, _, _)| *n != "c.txt").collect();
        assert!(silent(&mut t, &v));
        let cur = t.cursor().expect("光标不应丢失");
        assert!(cur < t.rows().len());
        assert_ne!(t.rows()[cur].entry.name, "c.txt");
        // 全部删除后光标为空
        assert!(silent(&mut t, &[]));
        assert_eq!(t.cursor(), None);
        assert_eq!(t.rows().len(), 0);
    }

    #[test]
    fn silent_reload_during_loading_marks_stale_and_explicit_nav_supersedes() {
        let mut t = Tab::new();
        let r = t.navigate(r"C:\根".into());
        assert!(t.reload_silent().is_none(), "读取中不另发请求");
        assert!(t.take_stale(), "但记下读完后要再刷新");
        assert!(!t.take_stale());
        let l = loaded(&t, &r, ITEMS);
        assert!(t.apply(l));
        // 静默请求在途时用户导航：旧结果丢弃
        let r1 = t.reload_silent().unwrap();
        let r2 = t.navigate(r"C:\别处".into());
        let old = loaded(&t, &r1, ITEMS);
        assert!(!t.apply(old), "导航之后，迟到的静默结果不能贴到新位置");
        assert_eq!(t.location(), Some(Path::new(r"C:\别处")));
        let _ = r2;
    }

    #[test]
    fn unavailable_then_recovers_through_silent_probe() {
        let mut t = ready();
        t.click(1, false, false);
        assert!(t.mark_unavailable("磁盘已断开"));
        assert!(t.is_unavailable());
        assert_eq!(t.rows().len(), 0);
        assert_eq!(
            t.location(),
            Some(Path::new(r"C:\根")),
            "位置保留，不改指向"
        );
        assert!(!t.mark_unavailable("again"), "已不可用不重复处理");
        // 探测仍失败：状态不变且不要求重绘
        let r = t.reload_silent().unwrap();
        let fail = Loaded {
            seq: r.seq,
            sort: r.sort,
            result: Err(ListError {
                kind: ListErrorKind::NotFound,
                os_code: Some(3),
                message: "找不到路径".into(),
            }),
        };
        assert!(
            t.apply(fail),
            "错误说明从‘已断开’变为系统说明，需要重绘一次"
        );
        let r = t.reload_silent().unwrap();
        let fail = Loaded {
            seq: r.seq,
            sort: r.sort,
            result: Err(ListError {
                kind: ListErrorKind::NotFound,
                os_code: Some(3),
                message: "找不到路径".into(),
            }),
        };
        assert!(!t.apply(fail), "同样的失败不重绘");
        // 恢复
        assert!(silent(&mut t, ITEMS));
        assert_eq!(t.state(), &LoadState::Ready);
        assert_eq!(t.rows().len(), ITEMS.len());
    }

    #[test]
    fn hidden_tab_refreshes_when_shown_again() {
        let mut t = ready();
        assert!(t.ensure_loaded().is_none());
        t.mark_needs_refresh();
        let r = t.ensure_loaded().expect("再次可见时静默刷新一次");
        assert_eq!(t.state(), &LoadState::Ready);
        let _ = r;
        assert!(t.ensure_loaded().is_none(), "只刷新一次");
    }
}
