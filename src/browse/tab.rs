//! 浏览上下文：一个浏览标签的状态——当前位置、历史、已排序条目、排序与视图方式、选择与键盘光标、加载状态。
//!
//! 本模块不接触界面与线程：导航产生 [`LoadRequest`]，由调用方在任意线程执行 [`LoadRequest::run`]，
//! 再把 [`Loaded`] 交回 [`Tab::apply`]。序号不匹配的旧结果会被丢弃，快速连续导航时不会把旧目录贴到新位置。
//! 选择以条目为单位保存在行上，排序与视图切换不改变选择集合。

use super::entry::{Entry, ListError, ListErrorKind, list_dir};
use super::history::History;
use super::sort::{SortSpec, sort_by};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

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
    pub fn run(&self) -> Loaded {
        let result = list_dir(&self.path).map(|mut v| {
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

/// 激活（双击、回车）的结果。
#[derive(Debug)]
pub enum Activation {
    None,
    Navigate(LoadRequest),
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

#[derive(Debug, Default)]
pub struct Tab {
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
}

impl Tab {
    pub fn new() -> Tab {
        Tab::default()
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
        self.location.as_deref().and_then(Path::parent).is_some()
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
    /// 第 `i` 行的完整路径。
    pub fn path_of(&self, i: usize) -> Option<PathBuf> {
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
        self.rows.clear();
        self.changed.clear();
        self.cursor = None;
        self.anchor = None;
        self.selected_count = 0;
        self.state = LoadState::Loading;
        self.seq += 1;
        self.restore = restore;
        LoadRequest {
            seq: self.seq,
            path,
            sort: self.sort,
        }
    }

    /// 应用加载结果。旧请求的结果返回 `false` 并被丢弃。
    pub fn apply(&mut self, loaded: Loaded) -> bool {
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
}
