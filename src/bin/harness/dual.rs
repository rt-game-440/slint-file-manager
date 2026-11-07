//! 双栏独立标签与会话恢复的交互脚本（Issue #6）。
//!
//! - [`dual`]：同一个窗口里的双栏流程——活动面板（鼠标与键盘）、各面板独立导航与标签、快捷键、
//!   单栏往返、快速切换、路径失效。
//! - [`restore`]：重启恢复——用同一个临时会话目录先后打开两个窗口（模拟重启），覆盖位置／排序／视图／单栏隐藏侧、
//!   磁盘或路径缺失与无权限、会话文件损坏与版本不兼容、大量标签的惰性读取。
//!
//! 每一步记录界面状态（见 dump::extras）并断言；两个脚本都不接触用户真实目录与真实会话
//! （会话一律在 `%TEMP%\sfm-harness\sessions\…`）。

use crate::ctx::{Ctx, Opts, Res};
use crate::dump::extras;
use crate::fixture;
use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementRoot};
use serde_json::{Value, json};
use slint::Model;
use slint::platform::Key;
use slint_file_manager::browse::session::{FILE, LoadOutcome, SessionState, SessionStore};
use slint_file_manager::browse::{LoadState, SortKey, SortSpec, TabSeed, ViewMode};
use std::path::{Path, PathBuf};

/// 矩形（窗口逻辑坐标）。
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    fn of(e: &ElementHandle) -> Rect {
        let (p, s) = (e.absolute_position(), e.size());
        Rect {
            x: p.x,
            y: p.y,
            w: s.width,
            h: s.height,
        }
    }
    fn contains_center(&self, e: &ElementHandle) -> bool {
        let r = Rect::of(e);
        let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
        cx >= self.x && cx <= self.x + self.w && cy >= self.y && cy <= self.y + self.h
    }
}

pub fn pane_name(p: usize) -> &'static str {
    if p == 0 { "左面板" } else { "右面板" }
}

impl Ctx {
    /// 面板根元素（`groupbox`，标签为“左面板”／“右面板”）。隐藏的面板不在元素树中，返回错误。
    pub fn pane_el(&self, p: usize) -> Res<ElementHandle> {
        let mut v = self.find_all(AccessibleRole::Groupbox, pane_name(p));
        match v.len() {
            1 => Ok(v.remove(0)),
            0 => Err(format!("{} 不在元素树中（隐藏或不存在）", pane_name(p)).into()),
            n => Err(format!("{} 不唯一（{n} 个）", pane_name(p)).into()),
        }
    }

    pub fn pane_rect(&self, p: usize) -> Res<Rect> {
        Ok(Rect::of(&self.pane_el(p)?))
    }

    pub fn has_pane(&self, p: usize) -> bool {
        self.pane_el(p).is_ok()
    }

    /// 在指定面板内按角色与标签查找唯一元素。
    pub fn find_in(&self, p: usize, role: AccessibleRole, label: &str) -> Res<ElementHandle> {
        let area = self.pane_rect(p)?;
        let mut v: Vec<_> = self
            .find_all(role, label)
            .into_iter()
            .filter(|e| area.contains_center(e))
            .collect();
        match v.len() {
            1 => Ok(v.remove(0)),
            0 => Err(format!("{} 内找不到 {role:?}：{label}", pane_name(p)).into()),
            n => Err(format!("{} 内 {role:?}：{label} 不唯一（{n} 个）", pane_name(p)).into()),
        }
    }

    pub fn item_in(&self, p: usize, name: &str) -> Res<ElementHandle> {
        self.find_in(p, AccessibleRole::ListItem, name)
    }

    pub fn button_in(&self, p: usize, label: &str) -> Res<ElementHandle> {
        self.find_in(p, AccessibleRole::Button, label)
    }

    /// 面板里的标签（按从左到右排序）。
    pub fn chips(&self, p: usize) -> Res<Vec<ElementHandle>> {
        let area = self.pane_rect(p)?;
        let mut v: Vec<_> = self
            .win
            .root_element()
            .query_descendants()
            .match_accessible_role(AccessibleRole::Tab)
            .find_all()
            .into_iter()
            .filter(|e| {
                e.accessible_label()
                    .is_some_and(|l| l.starts_with("浏览标签 "))
                    && area.contains_center(e)
            })
            .collect();
        v.sort_by(|a, b| a.absolute_position().x.total_cmp(&b.absolute_position().x));
        Ok(v)
    }

    /// 带 Ctrl 的普通按键，例如 Ctrl+T。
    pub fn ctrl(&self, text: &str) {
        self.with_mods(&[Key::Control], || self.key(text));
    }

    pub fn ctrl_special(&self, k: Key) {
        self.special(k, &[Key::Control]);
    }
}

// ---------- 状态读取 ----------

fn n_tabs(c: &Ctx, p: usize) -> usize {
    c.ctl.ws().pane(p).len()
}
fn cur_tab(c: &Ctx, p: usize) -> usize {
    c.ctl.ws().pane(p).active_index()
}
fn active(c: &Ctx) -> usize {
    c.ctl.ws().active_pane()
}
fn loc(c: &Ctx, p: usize) -> Option<PathBuf> {
    c.ctl.location_in(p)
}
fn sel(c: &Ctx, p: usize) -> Vec<String> {
    c.ctl.selected_in(p)
}
/// 面板内容区状态（界面上显示的）。
fn cs(c: &Ctx, p: usize) -> String {
    let v = if p == 0 {
        c.win.get_left_view()
    } else {
        c.win.get_right_view()
    };
    format!("{:?}", v.state)
}
fn shown_items(c: &Ctx, p: usize) -> usize {
    if p == 0 {
        c.win.get_left_items().row_count()
    } else {
        c.win.get_right_items().row_count()
    }
}
fn is_desc_active(c: &Ctx, p: usize) -> bool {
    c.pane_el(p)
        .ok()
        .and_then(|e| e.accessible_description())
        .is_some_and(|d| d == "活动面板")
}
fn msg(c: &Ctx) -> String {
    c.win.get_status_message().to_string()
}
/// 标签的加载状态是否为 `Idle`（尚未读取）。
fn idle(c: &Ctx, p: usize, i: usize) -> bool {
    c.ctl
        .ws()
        .pane(p)
        .tab(i)
        .is_some_and(|t| matches!(t.state(), LoadState::Idle))
}
fn unavailable(c: &Ctx, p: usize, i: usize) -> bool {
    c.ctl
        .ws()
        .pane(p)
        .tab(i)
        .is_some_and(|t| t.is_unavailable())
}

/// 样例目录里各位置应有的可见条目数。
fn expected_items(p: &Path) -> Option<usize> {
    if p == fixture::root() {
        Some(fixture::root_visible_count())
    } else if p == fixture::path(fixture::PHOTOS) {
        Some(12)
    } else if p == fixture::path(fixture::EMPTY) {
        Some(0)
    } else if p == fixture::deep_path(fixture::DEEP.len()) {
        Some(1)
    } else {
        None
    }
}

/// 每个标签状态都已读取完成，且条目数与位置一致。
fn all_tabs_consistent(c: &Ctx) -> Result<(), String> {
    let ws = c.ctl.ws();
    for p in 0..2 {
        for (i, t) in ws.pane(p).tabs().iter().enumerate() {
            let Some(l) = t.location() else {
                return Err(format!("{} 标签 {i} 没有位置", pane_name(p)));
            };
            let Some(exp) = expected_items(l) else {
                continue;
            };
            match t.state() {
                LoadState::Ready if t.rows().len() == exp => {}
                // 从未读取过的标签允许保持 Idle（惰性读取）
                LoadState::Idle => {}
                other => {
                    return Err(format!(
                        "{} 标签 {i}（{}）状态 {other:?}，条目 {}，应为 {exp}",
                        pane_name(p),
                        crate::dump::rel_path(l),
                        t.rows().len()
                    ));
                }
            }
        }
    }
    Ok(())
}

// ---------- 记录器 ----------

pub struct Rec {
    pub steps: Vec<Value>,
    pub fails: Vec<String>,
}

impl Rec {
    pub fn new() -> Rec {
        Rec {
            steps: Vec::new(),
            fails: Vec::new(),
        }
    }

    pub fn step(&mut self, c: &Ctx, name: &str, checks: Vec<(bool, &str)>) {
        let mut failed = Vec::new();
        for (ok, m) in checks {
            if !ok {
                eprintln!("  未通过：{name}：{m}");
                failed.push(m.to_string());
                self.fails.push(format!("{name}：{m}"));
            }
        }
        self.steps
            .push(json!({"step": name, "t_ms": c.elapsed(), "state": extras(c), "failed": failed}));
    }

    /// 记录一个由脚本自己计算出的失败说明。
    pub fn check_msg(&mut self, c: &Ctx, name: &str, r: Result<(), String>) {
        match r {
            Ok(()) => self.step(c, name, vec![(true, "")]),
            Err(e) => {
                self.fails.push(format!("{name}：{e}"));
                self.steps.push(json!({"step": name, "t_ms": c.elapsed(),
                    "state": extras(c), "failed": [e]}));
            }
        }
    }
}

/// 用 F6 把活动面板切到左面板（已在左面板则什么也不做）。
fn focus_left(c: &Ctx) {
    if active(c) != 0 {
        c.special(Key::F6, &[]);
    }
}

fn open_dual(size: (u32, u32), scale: f32, session: Option<PathBuf>) -> Res<Ctx> {
    Ctx::open(
        size,
        scale,
        Opts {
            split: true,
            session,
            ..Opts::default()
        },
    )
}

/// 双栏交互脚本。
pub fn dual(size: (u32, u32), scale: f32) -> Res<(Vec<Value>, Vec<String>)> {
    let c = &open_dual(size, scale, None)?;
    let mut r = Rec::new();
    let w = &c.win;
    let root = fixture::root();
    let photos = fixture::path(fixture::PHOTOS);
    let empty = fixture::path(fixture::EMPTY);

    // ---------- A. 初始：等宽双栏、左为活动面板、焦点在左 ----------
    let (l, rt) = (c.pane_rect(0)?, c.pane_rect(1)?);
    r.step(
        c,
        "A 初始双栏",
        vec![
            (c.ctl.ws().is_split() && w.get_split(), "默认双栏"),
            (
                active(c) == 0 && w.get_active_pane() == 0,
                "左面板为活动面板",
            ),
            ((l.w - rt.w).abs() <= 1.0, "两个面板等宽（相差不超过 1px）"),
            (rt.x > l.x, "右面板在右侧"),
            (
                c.ctl.names_in(0).len() == fixture::root_visible_count()
                    && c.ctl.names_in(1).len() == fixture::root_visible_count(),
                "两个面板各显示样例目录",
            ),
            (n_tabs(c, 0) == 1 && n_tabs(c, 1) == 1, "各有一个标签"),
            (
                c.chips(0)?.len() == 1 && c.chips(1)?.len() == 1,
                "界面上各有一个标签",
            ),
            (w.get_focused_pane() == 0, "键盘焦点在左面板"),
            (
                is_desc_active(c, 0) && !is_desc_active(c, 1),
                "只有左面板标记为活动面板",
            ),
        ],
    );

    // ---------- B/C. 鼠标点击决定活动面板；选择互相独立 ----------
    c.click(&c.item_in(1, "说明.md")?);
    r.step(
        c,
        "B 点击右面板条目",
        vec![
            (
                active(c) == 1 && w.get_active_pane() == 1,
                "右面板成为活动面板",
            ),
            (w.get_focused_pane() == 1, "键盘焦点随之移到右面板"),
            (sel(c, 1) == ["说明.md"], "右面板选中说明.md"),
            (sel(c, 0).is_empty(), "左面板没有选择"),
            (
                is_desc_active(c, 1) && !is_desc_active(c, 0),
                "活动标记移到右面板",
            ),
        ],
    );
    c.click(&c.item_in(0, "图片001.webp")?);
    r.step(
        c,
        "C 点击左面板条目",
        vec![
            (
                active(c) == 0 && w.get_focused_pane() == 0,
                "活动面板与焦点回到左面板",
            ),
            (sel(c, 0) == ["图片001.webp"], "左面板选中图片001"),
            (sel(c, 1) == ["说明.md"], "右面板的选择保持不变"),
        ],
    );

    // ---------- D. 各面板独立导航与历史 ----------
    c.double_click(&c.item_in(1, fixture::PHOTOS)?);
    let pending = c.ctl.pending();
    c.pump();
    r.step(
        c,
        "D1 右面板进入照片",
        vec![
            (pending == 1, "只有 1 个读取（右面板）"),
            (active(c) == 1, "右面板为活动面板"),
            (
                loc(c, 1).as_deref() == Some(photos.as_path()),
                "右面板在照片",
            ),
            (
                loc(c, 0).as_deref() == Some(root.as_path()),
                "左面板位置不变",
            ),
            (sel(c, 0) == ["图片001.webp"], "左面板选择不变"),
            (
                shown_items(c, 1) == 12 && shown_items(c, 0) == fixture::root_visible_count(),
                "两侧条目数各自正确",
            ),
        ],
    );
    c.click(&c.button_in(1, "后退")?);
    c.pump();
    r.step(
        c,
        "D2 右面板后退",
        vec![
            (
                loc(c, 1).as_deref() == Some(root.as_path()),
                "右面板回到样例目录",
            ),
            (
                c.ctl.ws().active_tab(1).can_forward() && !c.ctl.ws().active_tab(0).can_forward(),
                "后退只记在右面板的标签上",
            ),
            (!c.ctl.ws().active_tab(0).can_back(), "左面板没有历史"),
        ],
    );

    // ---------- E. 标签：新建、导航、切换（右面板为活动面板） ----------
    c.ctrl("t");
    let pending = c.ctl.pending();
    c.pump();
    r.step(
        c,
        "E1 Ctrl+T 在活动（右）面板新建标签",
        vec![
            (
                n_tabs(c, 1) == 2 && n_tabs(c, 0) == 1,
                "右面板 2 个标签，左面板仍 1 个",
            ),
            (cur_tab(c, 1) == 1, "新标签成为当前标签"),
            (
                c.chips(1)?.len() == 2 && c.chips(0)?.len() == 1,
                "界面标签数一致",
            ),
            (
                loc(c, 1).as_deref() == Some(root.as_path()),
                "新标签沿用当前位置",
            ),
            (pending == 1 && cs(c, 1) == "Ready", "新标签读取完成"),
            (sel(c, 1).is_empty(), "新标签没有选择"),
        ],
    );
    c.enter_address(&empty.to_string_lossy())?;
    c.pump();
    r.step(
        c,
        "E2 新标签中导航到空文件夹",
        vec![
            (
                loc(c, 1).as_deref() == Some(empty.as_path()),
                "当前标签在空文件夹",
            ),
            (
                c.ctl
                    .ws()
                    .pane(1)
                    .tab(0)
                    .and_then(|t| t.location().map(Path::to_path_buf))
                    == Some(root.clone()),
                "另一个标签位置不变",
            ),
            (
                c.ctl.ws().pane(1).tab(1).is_some_and(|t| t.can_back())
                    && c.ctl.ws().pane(1).tab(0).is_some_and(|t| !t.can_back()),
                "历史归属于各自的标签",
            ),
            (
                loc(c, 0).as_deref() == Some(root.as_path()),
                "左面板不受影响",
            ),
        ],
    );
    c.click(&c.chips(1)?[0]);
    r.step(
        c,
        "E3 点击第一个标签",
        vec![
            (
                cur_tab(c, 1) == 0 && loc(c, 1).as_deref() == Some(root.as_path()),
                "切到第一个标签（样例目录）",
            ),
            (
                cs(c, 1) == "Ready" && shown_items(c, 1) == fixture::root_visible_count(),
                "无需重新读取，内容立即可见",
            ),
            (cur_tab(c, 0) == 0 && n_tabs(c, 0) == 1, "左面板不受影响"),
        ],
    );
    c.ctrl_special(Key::Tab);
    let a = cur_tab(c, 1);
    c.special(Key::Tab, &[Key::Control, Key::Shift]);
    let b = cur_tab(c, 1);
    c.ctrl("2");
    let c2 = cur_tab(c, 1);
    c.ctrl("1");
    let c1 = cur_tab(c, 1);
    c.ctrl("9");
    let c9 = cur_tab(c, 1);
    c.ctrl_special(Key::PageUp);
    let pu = cur_tab(c, 1);
    c.ctrl_special(Key::PageDown);
    let pd = cur_tab(c, 1);
    r.step(
        c,
        "E4 键盘切换标签",
        vec![
            (a == 1, "Ctrl+Tab 切到下一个"),
            (b == 0, "Ctrl+Shift+Tab 切到上一个"),
            (c2 == 1, "Ctrl+2 切到第 2 个"),
            (c1 == 0, "Ctrl+1 切到第 1 个"),
            (c9 == 1, "Ctrl+9 切到最后一个"),
            (pu == 0, "Ctrl+PageUp 切到上一个"),
            (pd == 1, "Ctrl+PageDown 切到下一个"),
            (
                cur_tab(c, 0) == 0 && active(c) == 1,
                "快捷键只作用于活动（右）面板",
            ),
        ],
    );
    c.click(&c.button_in(1, "后退")?);
    c.pump();
    let t0_back = c.ctl.ws().pane(1).tab(0).is_some_and(|t| t.can_back());
    r.step(
        c,
        "E5 在第二个标签后退",
        vec![
            (
                loc(c, 1).as_deref() == Some(root.as_path()),
                "第二个标签回到样例目录",
            ),
            (!t0_back, "第一个标签没有被影响"),
            (c.ctl.ws().active_tab(1).can_forward(), "第二个标签可前进"),
        ],
    );

    // ---------- F. 左面板的标签操作；关闭规则 ----------
    c.click(&c.item_in(0, "图片001.webp")?);
    c.ctrl("t");
    c.pump();
    r.step(
        c,
        "F1 点击左面板后 Ctrl+T",
        vec![
            (
                n_tabs(c, 0) == 2 && n_tabs(c, 1) == 2,
                "左面板 2 个标签；右面板仍 2 个",
            ),
            (
                active(c) == 0 && cur_tab(c, 0) == 1 && cur_tab(c, 1) == 1,
                "只有左面板的当前标签变化",
            ),
        ],
    );
    c.ctrl("w");
    let after_close = n_tabs(c, 0);
    c.ctrl("w");
    r.step(
        c,
        "F2 Ctrl+W 关闭；最后一个标签不能关闭",
        vec![
            (after_close == 1, "第一次 Ctrl+W 关闭当前标签"),
            (n_tabs(c, 0) == 1, "只剩一个标签时 Ctrl+W 不再关闭"),
            (msg(c).contains("至少保留"), "状态栏提示至少保留一个标签"),
            (n_tabs(c, 1) == 2, "右面板的标签不受影响"),
            (
                c.chips(0)?.len() == 1,
                "界面只剩一个标签，且没有关闭按钮可见",
            ),
        ],
    );
    c.click(&c.item_in(1, "数据表.xlsx")?);
    c.click(&c.button_in(1, "关闭标签 样例目录")?);
    r.step(
        c,
        "F3 点击当前标签的关闭按钮",
        vec![
            (
                n_tabs(c, 1) == 1 && n_tabs(c, 0) == 1,
                "右面板关闭一个标签，左面板不变",
            ),
            (c.chips(1)?.len() == 1, "界面同步"),
            (cs(c, 1) == "Ready", "剩下的标签内容可见"),
        ],
    );

    // ---------- G. 单栏往返：隐藏侧状态完整保留 ----------
    c.click(&c.item_in(0, "图片001.webp")?);
    c.ctrl("t");
    c.enter_address(&photos.to_string_lossy())?;
    c.pump();
    c.click(&c.button("列表视图")?);
    c.click(&c.item_in(1, "数据表.xlsx")?);
    c.click(&c.button("排序方式")?);
    c.advance(16);
    c.click(&c.button("大小")?);
    let before = c.ctl.ws().snapshot();
    let (sel_l, sel_r) = (sel(c, 0), sel(c, 1));
    r.step(
        c,
        "G0 准备：左 2 标签（照片为列表视图），右按大小排序并选中数据表.xlsx",
        vec![
            (
                n_tabs(c, 0) == 2 && active(c) == 1,
                "左 2 标签，右为活动面板",
            ),
            (
                before.panes[0].tabs[1].view == ViewMode::List,
                "左面板第二个标签为列表视图",
            ),
            (
                before.panes[0].tabs[0].view == ViewMode::Grid,
                "左面板第一个标签仍是网格视图",
            ),
            (
                before.panes[1].tabs[0].sort.key == SortKey::Size
                    && before.panes[0].tabs[1].sort.key == SortKey::Name,
                "排序按标签独立：只有右面板标签是按大小",
            ),
        ],
    );
    c.click(&c.button("双栏")?);
    let full = c.pane_rect(1)?;
    r.step(
        c,
        "G1 切为单栏",
        vec![
            (!c.ctl.ws().is_split() && !w.get_split(), "进入单栏"),
            (
                active(c) == 1 && c.has_pane(1) && !c.has_pane(0),
                "只显示活动面板（右），左面板不在元素树中",
            ),
            (full.w > rt.w * 1.7, "显示的面板占满内容区"),
            (
                c.ctl.ws().snapshot().panes == before.panes,
                "两侧标签、位置、排序、视图、历史都不变",
            ),
            (sel(c, 0) == sel_l && sel(c, 1) == sel_r, "隐藏侧的选择保留"),
            (n_tabs(c, 0) == 2, "隐藏侧标签没有丢"),
        ],
    );
    c.special(Key::F6, &[]);
    r.step(
        c,
        "G2 F6 切到左面板（单栏显示左侧）",
        vec![
            (
                active(c) == 0 && c.has_pane(0) && !c.has_pane(1),
                "显示左面板，右面板隐藏",
            ),
            (
                cur_tab(c, 0) == 1 && c.ctl.ws().active_tab(0).view() == ViewMode::List,
                "左面板仍停在第二个标签（列表视图）",
            ),
            (sel(c, 1) == sel_r, "隐藏的右面板选择保留"),
            (w.get_focused_pane() == 0, "焦点在显示的面板"),
        ],
    );
    c.enter_address(&empty.to_string_lossy())?;
    c.pump();
    c.special(Key::F6, &[]);
    r.step(
        c,
        "G3 单栏下左面板导航后切回右面板",
        vec![
            (
                loc(c, 0).as_deref() == Some(empty.as_path()),
                "左面板已进入空文件夹",
            ),
            (active(c) == 1 && c.has_pane(1), "回到右面板"),
            (
                c.ctl.ws().snapshot().panes[1] == before.panes[1],
                "右面板状态完全没有被清除",
            ),
            (sel(c, 1) == sel_r, "右面板选择保留"),
        ],
    );
    c.click(&c.button("双栏")?);
    c.pump();
    let (l2, r2) = (c.pane_rect(0)?, c.pane_rect(1)?);
    let after = c.ctl.ws().snapshot();
    r.step(
        c,
        "G4 回到双栏",
        vec![
            (c.ctl.ws().is_split(), "恢复双栏"),
            ((l2.w - r2.w).abs() <= 1.0, "仍然等宽"),
            (after.panes[1] == before.panes[1], "右面板与切换前完全一致"),
            (
                after.panes[0].tabs[0] == before.panes[0].tabs[0]
                    && after.panes[0].tabs[1].location == empty,
                "左面板除单栏下的那次导航外保持不变",
            ),
            (sel(c, 1) == sel_r, "右面板选择保留"),
            (c.item_in(1, "数据表.xlsx").is_ok(), "右面板内容重新可见"),
        ],
    );

    // ---------- H. 快速切换 ----------
    focus_left(c);
    c.ctrl("t");
    c.enter_address(&photos.to_string_lossy())?;
    c.pump();
    let right_before = c.ctl.ws().snapshot().panes[1].clone();
    for i in 0..48 {
        match i % 4 {
            0 => c.ctrl_special(Key::Tab),
            1 => c.special(Key::F5, &[]),
            2 => c.special(Key::F6, &[]),
            _ => c.special(Key::F5, &[]),
        }
    }
    let queued = c.ctl.pending();
    let mut pumped = 0;
    while c.pump() > 0 {
        pumped += 1;
        if pumped > 10 {
            break;
        }
    }
    r.check_msg(
        c,
        "H1 快速切换后所有标签内容与位置一致",
        all_tabs_consistent(c),
    );
    r.step(
        c,
        "H2 快速切换（Ctrl+Tab／F6／F5 交错 48 次，不等读取）",
        vec![
            (
                queued > 0,
                "切换期间确有读取排队（结果会在标签切走后才到达）",
            ),
            (pumped <= 3, "读取完成后不再有后续请求（没有循环加载）"),
            (c.ctl.pending() == 0, "没有遗留请求"),
            (n_tabs(c, 0) == 3 && n_tabs(c, 1) == 1, "标签数量不变"),
            (
                c.ctl.ws().snapshot().panes[1].tabs == right_before.tabs,
                "右面板标签的位置／排序／视图不变",
            ),
            (
                (0..2).all(|p| {
                    let ws = c.ctl.ws();
                    expected_items(ws.active_tab(p).location().unwrap_or(Path::new("")))
                        .is_none_or(|n| shown_items(c, p) == n)
                }),
                "两个面板界面上显示的条目数与各自当前标签一致",
            ),
        ],
    );

    // ---------- I. 路径失效 ----------
    let gone = fixture::scratch_dir("gone");
    std::fs::create_dir_all(&gone)?;
    focus_left(c);
    c.ctrl("t");
    c.enter_address(&gone.to_string_lossy())?;
    c.pump();
    let tabs_before = n_tabs(c, 0);
    let ready_before = cs(c, 0);
    std::fs::remove_dir_all(&gone)?;
    c.special(Key::F5, &[]);
    c.pump();
    let idx = cur_tab(c, 0);
    let again = c.pump();
    r.step(
        c,
        "I1 路径被删除后刷新",
        vec![
            (ready_before == "Empty", "删除前是空目录"),
            (cs(c, 0) == "NotFound", "显示“找不到这个位置”"),
            (n_tabs(c, 0) == tabs_before, "标签没有被关闭"),
            (unavailable(c, 0, idx), "标签标记为位置不可用"),
            (
                c.chips(0)?
                    .get(idx)
                    .and_then(|e| e.accessible_description())
                    .is_some_and(|d| d == "位置不可用"),
                "标签带上有“位置不可用”提示",
            ),
            (again == 0 && c.ctl.pending() == 0, "没有自动重试或循环读取"),
        ],
    );
    c.ctrl_special(Key::PageUp);
    c.pump();
    c.ctrl_special(Key::PageDown);
    let queued = c.ctl.pending();
    c.pump();
    r.step(
        c,
        "I2 切走再切回：每次激活只重试一次",
        vec![
            (queued == 1, "切回时重试一次"),
            (
                cs(c, 0) == "NotFound" && c.ctl.pending() == 0,
                "仍不可用，不再重试",
            ),
            (n_tabs(c, 0) == tabs_before, "标签仍在"),
        ],
    );
    std::fs::create_dir_all(&gone)?;
    c.click(&c.button("重试")?);
    c.pump();
    r.step(
        c,
        "I3 路径恢复后点击重试",
        vec![
            (cs(c, 0) == "Empty", "重新读取成功（空目录）"),
            (!unavailable(c, 0, cur_tab(c, 0)), "不可用标记消失"),
        ],
    );
    let _ = std::fs::remove_dir_all(&gone);

    // 最后检查：窗口里两个面板的元素在整个流程后仍然完整
    r.step(
        c,
        "J 流程结束后双栏仍完整",
        vec![
            (c.has_pane(0) && c.has_pane(1), "两个面板都在"),
            (c.ctl.ws().is_split(), "仍是双栏"),
            (c.ctl.pending() == 0, "没有遗留读取"),
        ],
    );
    Ok((r.steps, r.fails))
}

// ---------- 重启恢复 ----------

fn tab_seed(location: PathBuf, view: ViewMode, sort: SortSpec) -> TabSeed {
    TabSeed {
        location,
        sort,
        view,
        back: Vec::new(),
        forward: Vec::new(),
    }
}

fn backups(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("session.backup-"))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// 重启恢复脚本（同进程内先后打开两个窗口模拟重启）。
pub fn restore(size: (u32, u32), scale: f32) -> Res<(Vec<Value>, Vec<String>)> {
    let mut r = Rec::new();
    let root = fixture::root();
    let photos = fixture::path(fixture::PHOTOS);
    let empty = fixture::path(fixture::EMPTY);
    let deep = fixture::deep_path(fixture::DEEP.len());
    let open = |dir: &Path, load: bool| -> Res<Ctx> {
        Ctx::open(
            size,
            scale,
            Opts {
                split: true,
                load,
                session: Some(dir.to_path_buf()),
                ..Opts::default()
            },
        )
    };

    // ---------- R1. 位置／排序／视图／单栏隐藏侧 ----------
    let dir = fixture::session_dir("roundtrip")?;
    let before;
    {
        let c = &open(&dir, true)?;
        // 左：样例目录（网格／名称）、照片（列表／按大小）、空文件夹（沿用上一个标签的列表／大小）
        c.click(&c.item_in(0, "图片001.webp")?);
        c.ctrl("t");
        c.enter_address(&photos.to_string_lossy())?;
        c.pump();
        c.click(&c.button("列表视图")?);
        c.click(&c.button("排序方式")?);
        c.advance(16);
        c.click(&c.button("大小")?);
        c.ctrl("t");
        c.enter_address(&empty.to_string_lossy())?;
        c.pump();
        c.ctrl("2");
        // 右：样例目录、深层长路径；当前为第一个
        c.click(&c.item_in(1, "数据表.xlsx")?);
        c.ctrl("t");
        c.enter_address(&deep.to_string_lossy())?;
        c.pump();
        c.ctrl("1");
        // 切到单栏：右面板可见，左面板（3 个标签）隐藏
        c.click(&c.button("双栏")?);
        before = c.ctl.ws().snapshot();
        let saved = c.ctl.save_now();
        r.step(
            c,
            "R1a 建立会话并保存",
            vec![
                (saved.is_ok(), "保存成功"),
                (
                    n_tabs(c, 0) == 3 && n_tabs(c, 1) == 2,
                    "左 3 标签、右 2 标签",
                ),
                (
                    !before.split && before.active_pane == 1,
                    "单栏，右面板为活动面板",
                ),
                (dir.join(FILE).exists(), "会话文件已写入临时目录"),
                (
                    before.panes[0].active == 1 && before.panes[1].active == 0,
                    "各面板活动标签正确",
                ),
            ],
        );
    }
    {
        let c = &open(&dir, false)?;
        let queued = c.ctl.pending();
        let restored = c.ctl.ws().snapshot();
        r.step(
            c,
            "R1b 重启：恢复完整会话，只读取可见面板的活动标签",
            vec![
                (
                    restored == before,
                    "标签、位置、排序、视图、历史、活动标签／面板、单栏状态与退出前完全一致",
                ),
                (queued == 1, "只排队 1 个读取（右面板当前标签），不阻塞窗口"),
                (
                    idle(c, 0, 0) && idle(c, 0, 1) && idle(c, 0, 2),
                    "隐藏的左面板标签都未读取",
                ),
                (c.opened.borrow().is_empty(), "不自动打开任何文件"),
                (
                    !c.ctl.ws().is_split() && !c.win.get_split(),
                    "仍是单栏（会话优先于默认双栏）",
                ),
                (c.has_pane(1) && !c.has_pane(0), "显示右面板，左面板隐藏"),
                (c.win.get_focused_pane() == 1, "焦点在活动面板"),
            ],
        );
        c.pump();
        r.step(
            c,
            "R1c 读取完成",
            vec![
                (
                    cs(c, 1) == "Ready" && loc(c, 1).as_deref() == Some(root.as_path()),
                    "右面板显示样例目录",
                ),
                (idle(c, 1, 1), "右面板另一个标签仍未读取（惰性）"),
            ],
        );
        c.click(&c.button("双栏")?);
        let queued = c.ctl.pending();
        c.pump();
        let ws = c.ctl.ws();
        let left = ws.pane(0);
        let ok_view = left.tabs()[0].view() == ViewMode::Grid
            && left.tabs()[1].view() == ViewMode::List
            && left.tabs()[1].sort().key == SortKey::Size
            && left.tabs()[0].sort().key == SortKey::Name;
        let idle_others = matches!(left.tabs()[0].state(), LoadState::Idle)
            && matches!(left.tabs()[2].state(), LoadState::Idle);
        let loc1 = left.tabs()[1].location().map(Path::to_path_buf);
        drop(ws);
        r.step(
            c,
            "R1d 回到双栏：只读取左面板的当前标签",
            vec![
                (queued == 1, "只读取 1 个标签"),
                (
                    ok_view,
                    "各标签的排序与视图独立保留（第一个网格／名称，第二个列表／大小）",
                ),
                (idle_others, "左面板其他标签继续保持未读取"),
                (
                    loc1.as_deref() == Some(photos.as_path()) && cs(c, 0) == "Ready",
                    "左面板当前标签（照片）已读取",
                ),
                (shown_items(c, 0) == 12, "照片 12 项"),
            ],
        );
    }

    // ---------- R2. 位置缺失／无权限／指向文件：标签保留、不崩溃、不循环 ----------
    let dir = fixture::session_dir("unavailable")?;
    let gone = fixture::scratch_dir("restore-gone");
    let _ = std::fs::remove_dir_all(&gone);
    let denied = fixture::path(fixture::DENIED);
    let pdf = fixture::path(fixture::LONG_PDF);
    let (g, d, p) = (gone.clone(), denied.clone(), pdf.clone());
    let mut state = SessionState::fresh(root.clone(), root.clone(), true);
    let seed = |l: PathBuf| tab_seed(l, ViewMode::Grid, SortSpec::default());
    state.panes[0].tabs = vec![seed(root.clone()), seed(g), seed(d), seed(p)];
    state.panes[1].tabs = vec![seed(root.clone()), seed(deep.clone())];
    SessionStore::at(&dir).save(&state)?;
    {
        let c = &open(&dir, true)?;
        let mut kinds = Vec::new();
        for i in 1..4 {
            c.ctrl(&(i + 1).to_string());
            c.pump();
            kinds.push(cs(c, 0));
        }
        let extra_pump = c.pump();
        let saved = c.ctl.save_now();
        let back = SessionStore::at(&dir).load();
        let kept = matches!(&back, LoadOutcome::Restored(s)
            if s.panes[0].tabs.iter().map(|t| t.location.clone()).collect::<Vec<_>>()
                == state.panes[0].tabs.iter().map(|t| t.location.clone()).collect::<Vec<_>>()
                && s.panes[1].tabs.len() == 2);
        r.step(
            c,
            "R2 恢复含缺失目录／无权限／文件路径的会话",
            vec![
                (n_tabs(c, 0) == 4 && n_tabs(c, 1) == 2, "全部标签都保留"),
                (
                    kinds == ["NotFound", "AccessDenied", "NotAFolder"],
                    "各标签分别显示找不到／无权限／不是文件夹",
                ),
                (
                    unavailable(c, 0, 1) && unavailable(c, 0, 2),
                    "缺失与无权限的标签标记为不可用",
                ),
                (c.opened.borrow().is_empty(), "指向文件的标签不会被自动打开"),
                (
                    extra_pump == 0 && c.ctl.pending() == 0,
                    "失败后不自动重试，没有循环读取",
                ),
                (c.chips(0)?.len() == 4, "界面上 4 个标签都在"),
                (
                    saved.is_ok() && kept,
                    "再次保存后失败的标签仍在会话里（不丢标签）",
                ),
                (active(c) == 0, "仍有活动面板与标签"),
            ],
        );
    }
    let _ = std::fs::remove_dir_all(&gone);

    // ---------- R3. 会话文件损坏：备份、提示、使用默认会话 ----------
    let dir = fixture::session_dir("corrupt")?;
    let bad = "{ 这不是合法的 JSON，\"version\": 1, \"panes\": [";
    std::fs::write(dir.join(FILE), bad)?;
    {
        let c = &open(&dir, true)?;
        let bk = backups(&dir);
        let kept_text = bk.first().and_then(|p| std::fs::read_to_string(p).ok());
        let saved = c.ctl.save_now();
        r.step(
            c,
            "R3a 会话文件损坏",
            vec![
                (
                    n_tabs(c, 0) == 1 && n_tabs(c, 1) == 1,
                    "使用默认会话（每侧一个标签）",
                ),
                (
                    loc(c, 0).as_deref() == Some(root.as_path()),
                    "默认位置可用，窗口正常",
                ),
                (
                    bk.len() == 1 && bk[0].to_string_lossy().contains("corrupt"),
                    "原文件被移成 session.backup-…-corrupt.json",
                ),
                (
                    kept_text.as_deref() == Some(bad),
                    "备份保留原始内容，没有被覆盖",
                ),
                (
                    !msg(c).is_empty() && msg(c).contains("会话"),
                    "状态栏显示会话降级说明（不是假装恢复成功）",
                ),
                (
                    saved.is_ok() && dir.join(FILE).exists(),
                    "之后写入的是新的有效会话",
                ),
            ],
        );
    }
    {
        let c = &open(&dir, true)?;
        r.step(
            c,
            "R3b 下次启动：正常恢复，不再提示",
            vec![
                (msg(c).is_empty(), "没有降级说明"),
                (backups(&dir).len() == 1, "备份没有增加"),
            ],
        );
    }

    // ---------- R4. 版本不兼容 ----------
    let dir = fixture::session_dir("newer")?;
    let newer = "{\"version\": 99, \"panes\": [], \"future\": true}";
    std::fs::write(dir.join(FILE), newer)?;
    {
        let c = &open(&dir, true)?;
        let bk = backups(&dir);
        let kept_text = bk.first().and_then(|p| std::fs::read_to_string(p).ok());
        r.step(
            c,
            "R4 会话文件来自更新版本",
            vec![
                (
                    bk.len() == 1 && bk[0].to_string_lossy().contains("v99"),
                    "原文件备份为 session.backup-…-v99.json",
                ),
                (
                    kept_text.as_deref() == Some(newer),
                    "原内容完整保留，供新版本程序使用",
                ),
                (
                    msg(c).contains("会话") && msg(c).contains("99"),
                    "状态栏说明版本不兼容",
                ),
                (
                    n_tabs(c, 0) == 1 && cs(c, 0) == "Ready",
                    "使用默认会话，窗口正常",
                ),
            ],
        );
    }

    // ---------- R5. 大量标签：不阻塞 ----------
    let dir = fixture::session_dir("many")?;
    let mut state = SessionState::fresh(root.clone(), root.clone(), true);
    state.panes[0].tabs = (0..100)
        .map(|i| {
            seed(if i % 2 == 0 {
                root.clone()
            } else {
                photos.clone()
            })
        })
        .collect();
    state.panes[0].active = 50;
    SessionStore::at(&dir).save(&state)?;
    {
        let c = &open(&dir, false)?;
        let queued = c.ctl.pending();
        let all_idle = (0..100).filter(|i| *i != 50).all(|i| idle(c, 0, i));
        c.pump();
        r.step(
            c,
            "R5 恢复 100 个标签",
            vec![
                (
                    n_tabs(c, 0) == 100 && cur_tab(c, 0) == 50,
                    "100 个标签与活动标签恢复",
                ),
                (queued == 2, "只排队 2 个读取（每个可见面板的当前标签）"),
                (all_idle, "其余 99 个标签尚未读取"),
                (cs(c, 0) == "Ready", "当前标签内容可见"),
            ],
        );
    }
    Ok((r.steps, r.fails))
}
