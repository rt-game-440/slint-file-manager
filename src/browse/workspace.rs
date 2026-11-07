//! 工作区：两个浏览面板，每个面板拥有自己的标签组（ADR 0002）。
//!
//! - 标签归属于面板：新建、关闭、切换、前进后退、排序、视图都只作用于对应面板里对应的标签，没有全局标签。
//! - 单栏只是“隐藏”：单栏模式下显示活动面板，另一面板的标签与全部状态原样保留，切回双栏时恢复。
//! - 异步读取结果按标签编号（[`TabId`]）送回，标签已关闭则丢弃；标签在后台时结果照常应用。
//! - 惰性读取：只有被激活（可见面板的活动标签）的标签才读取目录，其余恢复出来的标签保持 `Idle`。
//!
//! 本模块不接触界面与线程。会触发读取的操作返回 [`Job`]，由调用方执行。

use super::session::{PaneSeed, SessionState};
use super::tab::{LoadRequest, Tab, TabId, TabSeed};
use std::path::{Path, PathBuf};

/// 每个面板最多的标签数（防止误操作或损坏的会话文件造成大量标签）。
pub const MAX_TABS: usize = 100;

/// 面板索引：0 为左、1 为右。
pub const LEFT: usize = 0;
pub const RIGHT: usize = 1;

/// 一次待执行的目录读取，带上目标标签。
#[derive(Clone, Debug)]
pub struct Job {
    pub tab: TabId,
    pub req: LoadRequest,
}

/// 面板标签数已达上限。
#[derive(Debug, PartialEq, Eq)]
pub struct TabLimit;

/// 关闭标签失败的原因。
#[derive(Debug, PartialEq, Eq)]
pub enum CloseError {
    /// 面板只剩一个标签，不能关闭（每个面板至少保留一个标签）。
    LastTab,
    /// 索引越界。
    NoSuchTab,
}

#[derive(Debug)]
pub struct Pane {
    tabs: Vec<Tab>,
    active: usize,
}

impl Pane {
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }
    pub fn len(&self) -> usize {
        self.tabs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }
    pub fn active_index(&self) -> usize {
        self.active
    }
    /// 面板始终至少有一个标签。
    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }
    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }
    pub fn tab(&self, i: usize) -> Option<&Tab> {
        self.tabs.get(i)
    }
    pub fn tab_mut(&mut self, i: usize) -> Option<&mut Tab> {
        self.tabs.get_mut(i)
    }
}

#[derive(Debug)]
pub struct Workspace {
    panes: [Pane; 2],
    active: usize,
    split: bool,
    next_id: TabId,
}

fn path_key(p: &Path) -> String {
    p.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_lowercase()
}

fn job(t: &mut Tab) -> Option<Job> {
    let tab = t.id();
    t.ensure_loaded().map(|req| Job { tab, req })
}

impl Workspace {
    /// 由会话状态构造。所有标签都未读取；调用 [`Workspace::load_visible`] 取得首批读取。
    pub fn from_session(s: &SessionState) -> Workspace {
        let mut next_id: TabId = 1;
        let mut make = |p: &PaneSeed| {
            let tabs: Vec<Tab> = p
                .tabs
                .iter()
                .map(|seed| {
                    let t = Tab::from_seed(next_id, seed.clone());
                    next_id += 1;
                    t
                })
                .collect();
            let active = p.active.min(tabs.len().saturating_sub(1));
            Pane { tabs, active }
        };
        let left = make(&s.panes[LEFT]);
        let right = make(&s.panes[RIGHT]);
        Workspace {
            panes: [left, right],
            active: s.active_pane.min(1),
            split: s.split,
            next_id,
        }
    }

    /// 导出会话状态。没有位置的标签（理论上不存在）被跳过；面板至少保留一项。
    pub fn snapshot(&self) -> SessionState {
        let pane = |p: &Pane| {
            let mut tabs: Vec<TabSeed> = Vec::new();
            let mut active = 0;
            for (i, t) in p.tabs.iter().enumerate() {
                if let Some(seed) = t.seed() {
                    if i == p.active {
                        active = tabs.len();
                    }
                    tabs.push(seed);
                }
            }
            if tabs.is_empty() {
                tabs.push(TabSeed {
                    location: PathBuf::from("C:\\"),
                    sort: Default::default(),
                    view: Default::default(),
                    back: Vec::new(),
                    forward: Vec::new(),
                });
            }
            PaneSeed { tabs, active }
        };
        SessionState {
            split: self.split,
            active_pane: self.active,
            panes: [pane(&self.panes[LEFT]), pane(&self.panes[RIGHT])],
        }
    }

    // ---------- 读取 ----------

    pub fn pane(&self, p: usize) -> &Pane {
        &self.panes[p.min(1)]
    }
    pub fn pane_mut(&mut self, p: usize) -> &mut Pane {
        &mut self.panes[p.min(1)]
    }
    pub fn active_pane(&self) -> usize {
        self.active
    }
    pub fn is_split(&self) -> bool {
        self.split
    }
    /// 面板当前是否显示：双栏时两侧都显示，单栏时只显示活动面板。
    pub fn is_visible(&self, p: usize) -> bool {
        self.split || p == self.active
    }
    pub fn active_tab(&self, p: usize) -> &Tab {
        self.pane(p).active_tab()
    }
    pub fn active_tab_mut(&mut self, p: usize) -> &mut Tab {
        self.pane_mut(p).active_tab_mut()
    }

    /// 按编号查找标签，返回 (面板, 标签序号)。
    pub fn locate(&self, id: TabId) -> Option<(usize, usize)> {
        for (p, pane) in self.panes.iter().enumerate() {
            if let Some(i) = pane.tabs.iter().position(|t| t.id() == id) {
                return Some((p, i));
            }
        }
        None
    }

    pub fn tab_by_id_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        let (p, i) = self.locate(id)?;
        Some(&mut self.panes[p].tabs[i])
    }

    // ---------- 读取调度 ----------

    /// 可见面板的活动标签中需要读取的（启动恢复后调用）。
    pub fn load_visible(&mut self) -> Vec<Job> {
        let mut jobs = Vec::new();
        for p in [LEFT, RIGHT] {
            if self.is_visible(p) {
                jobs.extend(job(self.panes[p].active_tab_mut()));
            }
        }
        jobs
    }

    /// 以该面板的活动标签为准读取一次（若需要）。
    fn load_active(&mut self, p: usize) -> Option<Job> {
        job(self.panes[p].active_tab_mut())
    }

    // ---------- 标签 ----------

    /// 新建标签：复制当前标签的位置、排序、视图（不带历史与选择），放在末尾并激活。
    /// 达到上限返回 `Err(TabLimit)`。
    pub fn new_tab(&mut self, p: usize) -> Result<Option<Job>, TabLimit> {
        let p = p.min(1);
        if self.panes[p].tabs.len() >= MAX_TABS {
            return Err(TabLimit);
        }
        let id = self.next_id;
        self.next_id += 1;
        let t = self.panes[p].active_tab().duplicate(id);
        self.panes[p].tabs.push(t);
        self.panes[p].active = self.panes[p].tabs.len() - 1;
        Ok(self.load_active(p))
    }

    /// 在面板末尾新开一个指向 `path` 的标签并激活（排序、视图沿用该面板当前标签）。
    pub fn open_in_pane(&mut self, p: usize, path: PathBuf) -> Result<Job, TabLimit> {
        let p = p.min(1);
        if self.panes[p].tabs.len() >= MAX_TABS {
            return Err(TabLimit);
        }
        let id = self.next_id;
        self.next_id += 1;
        let (sort, view) = {
            let cur = self.panes[p].active_tab();
            (cur.sort(), cur.view())
        };
        let mut t = Tab::with_id(id);
        t.set_view(view);
        t.set_sort(sort);
        let req = t.navigate(path);
        self.panes[p].tabs.push(t);
        self.panes[p].active = self.panes[p].tabs.len() - 1;
        Ok(Job { tab: id, req })
    }

    /// 已打开该路径的标签（先左后右）。路径比较不区分大小写与末尾分隔符。
    pub fn find_path(&self, path: &Path) -> Option<(usize, usize)> {
        let key = path_key(path);
        for (p, pane) in self.panes.iter().enumerate() {
            if let Some(i) = pane
                .tabs
                .iter()
                .position(|t| t.location().is_some_and(|l| path_key(l) == key))
            {
                return Some((p, i));
            }
        }
        None
    }

    /// 关闭标签。关闭当前标签时激活右邻（没有则左邻）；关闭当前标签之前的标签时当前标签不变。
    /// 面板只剩一个标签时拒绝。
    pub fn close_tab(&mut self, p: usize, index: usize) -> Result<Option<Job>, CloseError> {
        let p = p.min(1);
        let pane = &mut self.panes[p];
        if index >= pane.tabs.len() {
            return Err(CloseError::NoSuchTab);
        }
        if pane.tabs.len() == 1 {
            return Err(CloseError::LastTab);
        }
        pane.tabs.remove(index);
        let was_active = index == pane.active;
        if index < pane.active {
            pane.active -= 1;
        }
        pane.active = pane.active.min(pane.tabs.len() - 1);
        Ok(if was_active {
            self.load_active(p)
        } else {
            None
        })
    }

    /// 切换到指定标签（越界忽略）；返回需要的读取。
    pub fn select_tab(&mut self, p: usize, index: usize) -> Option<Job> {
        let p = p.min(1);
        if index >= self.panes[p].tabs.len() {
            return None;
        }
        self.panes[p].active = index;
        self.load_active(p)
    }

    /// 循环切换标签：`delta` 为 +1（下一个）或 -1（上一个），首尾相接。
    pub fn cycle_tab(&mut self, p: usize, delta: isize) -> Option<Job> {
        let n = self.pane(p).len() as isize;
        let next = (self.pane(p).active as isize + delta).rem_euclid(n) as usize;
        self.select_tab(p, next)
    }

    /// 按序号切换（浏览器习惯）：1–8 为第 n 个标签（不存在则忽略），9 为最后一个。
    pub fn select_tab_number(&mut self, p: usize, n: usize) -> Option<Job> {
        let len = self.pane(p).len();
        let index = if n == 9 { len - 1 } else { n.checked_sub(1)? };
        self.select_tab(p, index)
    }

    // ---------- 面板与布局 ----------

    /// 切换活动面板。单栏模式下这同时决定显示哪一侧。
    pub fn set_active_pane(&mut self, p: usize) -> Option<Job> {
        let p = p.min(1);
        if p == self.active {
            return None;
        }
        self.active = p;
        self.load_active(p)
    }

    /// 设置双栏／单栏。单栏保留隐藏侧的全部状态；变回双栏时读取隐藏侧尚未读取的活动标签。
    pub fn set_split(&mut self, split: bool) -> Vec<Job> {
        if split == self.split {
            return Vec::new();
        }
        self.split = split;
        let mut jobs = Vec::new();
        if split {
            let other = 1 - self.active;
            jobs.extend(self.load_active(other));
        }
        jobs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browse::sort::{SortKey, SortSpec};
    use crate::browse::tab::{LoadState, ViewMode};

    fn ws() -> Workspace {
        Workspace::from_session(&SessionState::fresh(r"C:\左".into(), r"C:\右".into(), true))
    }

    fn loc(w: &Workspace, p: usize) -> PathBuf {
        w.active_tab(p).location().unwrap().to_path_buf()
    }

    #[test]
    fn panes_have_independent_tab_groups() {
        let mut w = ws();
        assert_eq!(w.load_visible().len(), 2);
        let id_left_before = w.active_tab(LEFT).id();
        w.new_tab(LEFT).unwrap();
        w.new_tab(LEFT).unwrap();
        assert_eq!(w.pane(LEFT).len(), 3);
        assert_eq!(w.pane(RIGHT).len(), 1);
        assert_eq!(w.pane(LEFT).active_index(), 2);
        assert_eq!(w.pane(RIGHT).active_index(), 0);
        // 切换左侧标签不改变右侧
        w.select_tab(LEFT, 0);
        assert_eq!(w.active_tab(LEFT).id(), id_left_before);
        assert_eq!(loc(&w, RIGHT), PathBuf::from(r"C:\右"));
        // 编号全局唯一
        let mut ids: Vec<TabId> = w
            .pane(LEFT)
            .tabs()
            .iter()
            .chain(w.pane(RIGHT).tabs())
            .map(Tab::id)
            .collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn history_sort_view_belong_to_their_tab() {
        let mut w = ws();
        w.load_visible();
        w.new_tab(LEFT).unwrap(); // 标签 1
        // 标签 1 导航并改排序、视图
        w.active_tab_mut(LEFT).navigate(r"C:\左\子".into());
        w.active_tab_mut(LEFT).set_view(ViewMode::List);
        w.active_tab_mut(LEFT).set_sort(SortSpec {
            key: SortKey::Size,
            ascending: false,
        });
        assert!(w.active_tab(LEFT).can_back());
        // 标签 0 不受影响
        w.select_tab(LEFT, 0);
        assert!(!w.active_tab(LEFT).can_back());
        assert_eq!(w.active_tab(LEFT).view(), ViewMode::Grid);
        assert_eq!(w.active_tab(LEFT).sort(), SortSpec::default());
        assert_eq!(loc(&w, LEFT), PathBuf::from(r"C:\左"));
        // 回到标签 1：状态还在
        w.select_tab(LEFT, 1);
        assert!(w.active_tab(LEFT).can_back());
        assert_eq!(w.active_tab(LEFT).view(), ViewMode::List);
        assert_eq!(w.active_tab(LEFT).sort().key, SortKey::Size);
    }

    #[test]
    fn new_tab_copies_location_sort_view_but_not_history() {
        let mut w = ws();
        w.load_visible();
        w.active_tab_mut(RIGHT).navigate(r"C:\右\子".into());
        w.active_tab_mut(RIGHT).set_view(ViewMode::List);
        let job = w.new_tab(RIGHT).unwrap().expect("新标签需要读取");
        assert_eq!(job.req.path, PathBuf::from(r"C:\右\子"));
        assert_eq!(w.active_tab(RIGHT).view(), ViewMode::List);
        assert!(!w.active_tab(RIGHT).can_back());
        // 之后两个标签互不影响
        w.active_tab_mut(RIGHT).set_view(ViewMode::Grid);
        assert_eq!(w.pane(RIGHT).tab(0).unwrap().view(), ViewMode::List);
    }

    #[test]
    fn close_tab_rules() {
        let mut w = ws();
        w.load_visible();
        for _ in 0..3 {
            w.new_tab(LEFT).unwrap();
        } // 4 个标签，当前为 3
        // 关闭当前之前的标签：当前标签保持
        let cur = w.active_tab(LEFT).id();
        w.close_tab(LEFT, 0).unwrap();
        assert_eq!(w.active_tab(LEFT).id(), cur);
        assert_eq!(w.pane(LEFT).active_index(), 2);
        // 关闭最后一个且当前的标签：激活左邻
        w.close_tab(LEFT, 2).unwrap();
        assert_eq!(w.pane(LEFT).active_index(), 1);
        // 关闭当前（中间）标签：激活右邻
        w.select_tab(LEFT, 0);
        let right_neighbor = w.pane(LEFT).tab(1).unwrap().id();
        w.close_tab(LEFT, 0).unwrap();
        assert_eq!(w.active_tab(LEFT).id(), right_neighbor);
        // 只剩一个：拒绝，且不影响右侧
        assert!(matches!(w.close_tab(LEFT, 0), Err(CloseError::LastTab)));
        assert!(matches!(w.close_tab(LEFT, 9), Err(CloseError::NoSuchTab)));
        assert_eq!(w.pane(LEFT).len(), 1);
        assert_eq!(w.pane(RIGHT).len(), 1);
    }

    #[test]
    fn tab_limit() {
        let mut w = ws();
        for _ in 1..MAX_TABS {
            w.new_tab(LEFT).unwrap();
        }
        assert_eq!(w.pane(LEFT).len(), MAX_TABS);
        assert!(w.new_tab(LEFT).is_err());
        assert!(w.new_tab(RIGHT).is_ok(), "上限按面板计算");
    }

    #[test]
    fn cycle_and_number_keys() {
        let mut w = ws();
        for _ in 0..2 {
            w.new_tab(RIGHT).unwrap();
        } // 3 个，当前 2
        w.cycle_tab(RIGHT, 1);
        assert_eq!(w.pane(RIGHT).active_index(), 0, "末尾下一个回到首个");
        w.cycle_tab(RIGHT, -1);
        assert_eq!(w.pane(RIGHT).active_index(), 2);
        w.select_tab_number(RIGHT, 2);
        assert_eq!(w.pane(RIGHT).active_index(), 1);
        w.select_tab_number(RIGHT, 8); // 不存在：忽略
        assert_eq!(w.pane(RIGHT).active_index(), 1);
        w.select_tab_number(RIGHT, 9); // 最后一个
        assert_eq!(w.pane(RIGHT).active_index(), 2);
        assert_eq!(w.pane(LEFT).active_index(), 0, "左侧不受影响");
    }

    #[test]
    fn tabs_load_lazily_and_only_once() {
        let mut w = Workspace::from_session(&many_tabs(40));
        let first = w.load_visible();
        assert_eq!(first.len(), 2, "只读取两侧活动标签，其余保持 Idle");
        let idle = w
            .pane(LEFT)
            .tabs()
            .iter()
            .filter(|t| *t.state() == LoadState::Idle)
            .count();
        assert_eq!(idle, 39);
        // 激活一个未读取标签：产生一次读取
        assert!(w.select_tab(LEFT, 7).is_some());
        // 再次切到它、或在读取中切回：不重复读取
        w.select_tab(LEFT, 8);
        assert!(w.select_tab(LEFT, 7).is_none());
    }

    fn many_tabs(n: usize) -> SessionState {
        let seed = |i: usize| TabSeed {
            location: format!(r"C:\目录{i}").into(),
            sort: SortSpec::default(),
            view: ViewMode::Grid,
            back: vec![],
            forward: vec![],
        };
        let pane = |active| PaneSeed {
            tabs: (0..n).map(seed).collect(),
            active,
        };
        SessionState {
            split: true,
            active_pane: 0,
            panes: [pane(0), pane(3)],
        }
    }

    #[test]
    fn single_pane_keeps_hidden_state_and_roundtrips() {
        let mut w = ws();
        w.load_visible();
        w.new_tab(RIGHT).unwrap();
        w.active_tab_mut(RIGHT).navigate(r"C:\右\深".into());
        w.active_tab_mut(RIGHT).set_view(ViewMode::List);
        let before = w.snapshot();
        // 切到单栏（左为活动面板）：右侧隐藏但状态不变
        assert!(w.set_split(false).is_empty());
        assert!(w.is_visible(LEFT) && !w.is_visible(RIGHT));
        assert_eq!(w.snapshot().panes[RIGHT], before.panes[RIGHT]);
        // 单栏下切换活动面板：显示另一侧
        w.set_active_pane(RIGHT);
        assert!(!w.is_visible(LEFT) && w.is_visible(RIGHT));
        w.set_active_pane(LEFT);
        // 回到双栏：右侧状态完整
        w.set_split(true);
        let after = w.snapshot();
        assert_eq!(after.panes, before.panes);
        assert!(after.split);
    }

    #[test]
    fn hidden_idle_pane_loads_when_shown() {
        let mut s = many_tabs(2);
        s.split = false;
        let mut w = Workspace::from_session(&s);
        assert_eq!(w.load_visible().len(), 1, "单栏只读取活动面板");
        let jobs = w.set_split(true);
        assert_eq!(jobs.len(), 1, "显示隐藏侧时才读取其活动标签");
        assert_eq!(
            jobs[0].req.path,
            PathBuf::from(r"C:\目录1"),
            "活动索引越界时收敛到最后一个标签"
        );
    }

    #[test]
    fn results_route_by_tab_id() {
        let mut w = ws();
        let jobs = w.load_visible();
        let left_id = jobs[0].tab;
        let right_id = jobs[1].tab;
        assert_ne!(left_id, right_id);
        assert!(w.tab_by_id_mut(right_id).is_some());
        w.new_tab(LEFT).unwrap();
        w.close_tab(LEFT, 0).unwrap();
        assert!(
            w.tab_by_id_mut(left_id).is_none(),
            "已关闭标签的结果无处可投，应被丢弃"
        );
        assert_eq!(w.locate(right_id), Some((RIGHT, 0)));
    }

    #[test]
    fn open_directory_request_prefers_existing_tab() {
        let mut w = ws();
        w.load_visible();
        // C:\右 已在右面板打开：按路径找到（不区分大小写与末尾分隔符）
        assert_eq!(w.find_path(Path::new("c:\\右\\")), Some((RIGHT, 0)));
        assert_eq!(w.find_path(Path::new(r"C:\没有")), None);
        // 没有时在指定面板新开标签并激活，沿用该面板当前标签的视图
        w.active_tab_mut(LEFT).set_view(ViewMode::List);
        let job = w.open_in_pane(LEFT, r"D:\新位置".into()).unwrap();
        assert_eq!(job.req.path, PathBuf::from(r"D:\新位置"));
        assert_eq!(w.pane(LEFT).len(), 2);
        assert_eq!(w.pane(LEFT).active_index(), 1);
        assert_eq!(w.active_tab(LEFT).view(), ViewMode::List);
        assert!(!w.active_tab(LEFT).is_restored());
    }

    #[test]
    fn unavailable_tab_is_kept_and_not_retried_automatically() {
        let mut w = ws();
        let jobs = w.load_visible();
        // 模拟左侧读取失败
        let t = w.tab_by_id_mut(jobs[0].tab).unwrap();
        let loaded = crate::browse::tab::Loaded {
            seq: jobs[0].req.seq,
            sort: jobs[0].req.sort,
            result: Err(crate::browse::entry::ListError {
                kind: crate::browse::ListErrorKind::NotFound,
                os_code: None,
                message: "x".into(),
            }),
        };
        assert!(t.apply(loaded));
        assert!(t.is_unavailable() && t.is_restored());
        assert_eq!(loc(&w, LEFT), PathBuf::from(r"C:\左"), "失败后位置不变");
        // 不切换就不会再产生读取；在其他标签与它之间切换，每次激活才重试一次
        w.new_tab(LEFT).unwrap();
        let retry = w.select_tab(LEFT, 0);
        assert!(retry.is_some(), "激活不可用标签时重试一次");
        assert_eq!(w.pane(LEFT).len(), 2, "标签始终保留");
    }
}
