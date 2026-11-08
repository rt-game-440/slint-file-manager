//! 新建、重命名、剪贴板与拖放的界面接线（Issue #12，设计见 docs/create-rename-transfer.md）。
//!
//! - **新建**：工具栏“新建”菜单与 Ctrl+Shift+N。先在目标目录里挑一个不重名的名称（“新建文件夹 (2)”），
//!   用“目标已存在则失败”的调用创建，成功后刷新、选中并立刻进入重命名（默认选中不含扩展名的部分）。
//! - **重命名**：F2。输入框里 Enter 确定、Esc 取消、失去焦点确定；名称不合法或重名时留在编辑状态并说明原因
//!   （失去焦点触发时则放弃本次编辑）。大小写变更直接生效，不会被当成重名。
//! - **剪贴板**：Ctrl+C / Ctrl+X / Ctrl+V，与资源管理器互通（`CF_HDROP` + `Preferred DropEffect`）。剪切只是“标记”，
//!   图标变淡，粘贴成功后才真正移动；剪贴板被别处改写时标记自动失效。
//! - **拖放**：应用内（含左右面板之间）靠条目上的指针事件识别，超过 6 像素才算拖动；指针离开窗口时改用系统拖放
//!   （[`crate::dragdrop::start_drag`]）拖给资源管理器；资源管理器拖进来由自己的 `IDropTarget` 接管（[`attach_drop`]）。
//!   默认同卷移动、跨卷复制，Ctrl 强制复制、Shift 强制移动；跟随指针的提示文字与实际执行的操作用同一份判定，
//!   不会出现“提示复制、实际移动”。
//! - **不静默破坏**：复制与移动先做计划（[`crate::xfer::plan`]），同名冲突弹框让用户选“保留两者 / 跳过 / 取消”，
//!   没有“替换”；执行按真实结果逐项汇报。

use super::ops::{CreateKind, Dialog, Task};
use super::*;
use crate::clipboard::{Clipboard, Effect, MemClipboard, SystemClipboard};
use crate::dragdrop::{self, DropHost, EFFECT_COPY, EFFECT_MOVE, MK_CONTROL, MK_SHIFT};
use crate::naming::{self, Style};
use crate::shellnew::Template;
use crate::xfer::{self, Mode, Policy};
use crate::{DragBus, DragPhase, RenameBus};
use std::collections::HashSet;
use std::ffi::OsString;

/// 指针按下后移动超过这个距离（逻辑像素）才算开始拖动。
const DRAG_START: f32 = 6.0;
/// 应用内拖放允许的效果（复制与移动）。
const ALLOW_BOTH: u32 = EFFECT_COPY | EFFECT_MOVE;

/// 拖出窗口的处理方式。
pub enum OsDrag {
    /// 发起真实的系统拖放（产品使用）。
    Shell,
    /// 只记录被拖出的路径（harness 使用，不进入系统的模态拖放循环）。
    Record(Rc<RefCell<Vec<Vec<PathBuf>>>>),
}

/// 新建、剪贴板与拖放所需的外部环境。
pub struct XferEnv {
    pub clipboard: Rc<dyn Clipboard>,
    /// “新建”菜单里的文件模板（文件夹固定存在，不在此列表）。
    pub templates: Vec<Template>,
    pub os_drag: OsDrag,
}

impl XferEnv {
    pub fn production() -> XferEnv {
        XferEnv {
            clipboard: Rc::new(SystemClipboard),
            templates: crate::shellnew::system_templates(),
            os_drag: OsDrag::Shell,
        }
    }

    /// 内存剪贴板、只有“文本文档”模板、拖出只记录：测试用。
    pub fn inert() -> XferEnv {
        XferEnv {
            clipboard: Rc::new(MemClipboard::new()),
            templates: vec![Template::empty("文本文档", ".txt")],
            os_drag: OsDrag::Record(Rc::default()),
        }
    }
}

/// 一次复制／移动的来由：决定完成后是否清理剪贴板。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// 来自剪贴板粘贴；记录粘贴时的剪贴板序号。
    Clipboard(u32),
    /// 来自拖放。
    Drag,
}

/// 等待用户处理同名冲突的传输。
pub struct Pending {
    pub plan: xfer::Plan,
    pub origin: Origin,
    /// 目标面板（完成后选中结果、让键盘焦点回到它）。
    pub pane: Option<usize>,
}

#[derive(Clone)]
struct Cut {
    seq: u32,
}

#[derive(Clone)]
struct Rename {
    pane: usize,
    dir: PathBuf,
    old: OsString,
}

struct Press {
    pane: usize,
    index: usize,
    at: (f32, f32),
}

struct Dest {
    pane: usize,
    dir: PathBuf,
}

struct Drag {
    srcs: Vec<PathBuf>,
    /// 来源面板；`None` 表示从资源管理器拖进来。
    from: Option<usize>,
}

/// 本模块的全部可变状态。
#[derive(Default)]
pub struct State {
    cut: Option<Cut>,
    rename: Option<Rename>,
    /// 新建完成后要进入重命名的（面板，名称）。
    pending_rename: Option<(usize, OsString)>,
    press: Option<Press>,
    /// 多选中按下已选项：没有拖动、直接松开时才收拢为单选。
    collapse: Option<(usize, usize)>,
    drag: Option<Drag>,
}

/// 一次拖放位置的判定结果：提示文字与实际执行共用。
struct Eval {
    dest: Option<Dest>,
    mode: Mode,
    ok: bool,
    hint: String,
    /// 被高亮的文件夹条目（面板，行号）。
    mark: Option<(usize, usize)>,
    pane: i32,
    index: i32,
}

impl Eval {
    fn none(hint: &str) -> Eval {
        Eval {
            dest: None,
            mode: Mode::Copy,
            ok: false,
            hint: hint.into(),
            mark: None,
            pane: -1,
            index: -1,
        }
    }
}

fn effect_of(mode: Mode) -> u32 {
    match mode {
        Mode::Copy => EFFECT_COPY,
        Mode::Move => EFFECT_MOVE,
    }
}

fn label_of(srcs: &[PathBuf]) -> String {
    match srcs {
        [one] => one
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        _ => format!("{} 个项目", srcs.len()),
    }
}

fn dest_error_text(e: &crate::fileops::OpError) -> String {
    use crate::fileops::FailKind;
    match e.kind {
        FailKind::DestMissing | FailKind::NotFound => "目标文件夹不存在或已不可用".into(),
        _ => e.text(),
    }
}

impl Controller {
    // ---------- 新建 ----------

    /// Ctrl+Shift+N / “新建 → 文件夹”。
    pub fn new_folder(&self, p: usize) {
        self.begin_create(p, CreateKind::Folder);
    }

    /// “新建 → 文件模板”（序号对应 [`Env`] 里的模板列表）。
    pub fn new_file(&self, p: usize, i: i32) {
        let Some(t) = idx(i).and_then(|i| self.templates.get(i)).cloned() else {
            return;
        };
        self.begin_create(p, CreateKind::File(t));
    }

    fn begin_create(&self, p: usize, kind: CreateKind) {
        self.activate_pane(p);
        self.commit_rename_quietly();
        if !self.can_start_op() {
            return;
        }
        let dir = {
            let ws = self.ws.borrow();
            let t = ws.active_tab(p);
            if t.is_recycle_bin() {
                drop(ws);
                self.set_message("回收站里不能新建项目");
                return;
            }
            match (t.state(), t.location()) {
                (LoadState::Ready, Some(l)) => l.to_path_buf(),
                _ => {
                    drop(ws);
                    self.set_message("当前位置还没有读取完成，不能新建项目");
                    return;
                }
            }
        };
        let (base, ext) = match &kind {
            CreateKind::Folder => ("新建文件夹".to_string(), String::new()),
            CreateKind::File(t) => (format!("新建 {}", t.label), t.ext.clone()),
        };
        let name = naming::unique_in(&dir, &base, &ext, Style::New);
        self.run_task(Task::Create {
            path: dir.join(name),
            kind,
            pane: p,
        });
    }

    /// 有对话框或后台操作时不再开始新的文件操作。
    pub(super) fn can_start_op(&self) -> bool {
        if self.dialog.borrow().is_some() {
            return false;
        }
        if self.busy.get() {
            self.set_message("上一个操作还在进行，请稍候");
            return false;
        }
        true
    }

    // ---------- 重命名 ----------

    /// F2：重命名活动面板里唯一被选中的项目。
    pub fn rename_selected(&self, p: usize) {
        self.activate_pane(p);
        if !self.can_start_op() {
            return;
        }
        let target = {
            let ws = self.ws.borrow();
            let t = ws.active_tab(p);
            if t.is_recycle_bin() {
                Err("回收站里的项目不能重命名")
            } else {
                match t.selected_count() {
                    0 => Err("请先选择要重命名的项目"),
                    1 => (0..t.rows().len())
                        .find(|&i| t.is_selected(i))
                        .ok_or("请先选择要重命名的项目"),
                    _ => Err("一次只能重命名一个项目，请只选择一项"),
                }
            }
        };
        match target {
            Ok(i) => self.begin_rename(p, i),
            Err(m) => self.set_message(m),
        }
    }

    fn begin_rename(&self, p: usize, i: usize) {
        self.commit_rename_quietly();
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let (dir, os_name, name, is_dir) = {
            let ws = self.ws.borrow();
            let t = ws.active_tab(p);
            let (Some(dir), Some(row)) = (t.location(), t.rows().get(i)) else {
                return;
            };
            (
                dir.to_path_buf(),
                row.entry.os_name.clone(),
                row.entry.name.clone(),
                row.entry.is_dir,
            )
        };
        let (_, end) = naming::rename_selection(&name, is_dir);
        let bus = w.global::<RenameBus>();
        bus.set_text(name.into());
        bus.set_error("".into());
        bus.set_select_end(end as i32);
        self.state.borrow_mut().rename = Some(Rename {
            pane: p,
            dir,
            old: os_name.clone(),
        });
        self.marks.borrow_mut().editing = Some((p, os_name));
        self.ui[p].items.row_changed(i);
    }

    /// 结束编辑状态（不改文件），返回刚才的编辑信息。
    fn end_rename(&self) -> Option<Rename> {
        let r = self.state.borrow_mut().rename.take()?;
        self.marks.borrow_mut().editing = None;
        let row = self.ws.borrow().active_tab(r.pane).index_of_name(&r.old);
        if let Some(i) = row {
            self.ui[r.pane].items.row_changed(i);
        }
        if let Some(w) = self.window.upgrade() {
            w.global::<RenameBus>().set_error("".into());
        }
        Some(r)
    }

    /// 输入框提交：`blur` 为 `true` 表示由失去焦点触发。
    pub fn rename_commit(&self, text: &str, blur: bool) {
        let Some(r) = self.state.borrow().rename.clone() else {
            return;
        };
        let reject = |reason: String| {
            if blur {
                // 已经没有焦点，不能留在编辑状态：放弃本次编辑并说明
                self.end_rename();
                self.set_message(&format!("没有重命名：{reason}"));
            } else {
                if let Some(w) = self.window.upgrade() {
                    w.global::<RenameBus>().set_error(reason.clone().into());
                }
                self.set_message(&reason);
            }
        };
        let new = match naming::clean_name(text) {
            Ok(n) => n,
            Err(e) => return reject(e.text()),
        };
        let from = r.dir.join(&r.old);
        if OsString::from(&new) == r.old {
            // 没有改动
            self.end_rename();
            if !blur {
                self.focus_pane(r.pane);
            }
            return;
        }
        let to = r.dir.join(&new);
        // 同名项目已存在（只有大小写不同的“同一个项目”不算重名）
        if xfer::key(&to) != xfer::key(&from) && to.symlink_metadata().is_ok() {
            return reject(format!("已存在名为“{new}”的项目"));
        }
        if self.dialog.borrow().is_some() || self.busy.get() {
            self.end_rename();
            self.set_message("上一个操作还在进行，请稍候再重命名");
            return;
        }
        self.end_rename();
        if !blur {
            self.focus_pane(r.pane);
        }
        self.run_task(Task::Rename {
            from,
            to,
            pane: r.pane,
        });
    }

    /// Esc：放弃编辑。
    pub fn rename_cancel(&self) {
        if let Some(r) = self.end_rename() {
            self.focus_pane(r.pane);
        }
    }

    /// 其他操作开始前，把正在进行的重命名按“失去焦点”处理（合法则提交，不合法则放弃）。
    pub(super) fn commit_rename_quietly(&self) {
        if self.state.borrow().rename.is_none() {
            return;
        }
        let text = self
            .window
            .upgrade()
            .map(|w| w.global::<RenameBus>().get_text().to_string())
            .unwrap_or_default();
        self.rename_commit(&text, true);
    }

    /// 是否正在重命名（harness 使用）。
    pub fn rename_active(&self) -> bool {
        self.state.borrow().rename.is_some()
    }

    /// 重命名输入框当前内容（harness 使用）。
    pub fn rename_text(&self) -> String {
        self.window
            .upgrade()
            .map(|w| w.global::<RenameBus>().get_text().to_string())
            .unwrap_or_default()
    }

    /// 模拟在输入框里输入（harness 使用）。
    pub fn set_rename_text(&self, text: &str) {
        if let Some(w) = self.window.upgrade() {
            w.global::<RenameBus>().set_text(text.into());
        }
    }

    // ---------- 新建与传输完成后的收尾钩子 ----------

    /// 某面板的当前标签读取完成后：新建后进入重命名；编辑中的条目没有了就退出编辑。
    pub(super) fn after_load(&self, p: usize) {
        let pend = self.state.borrow().pending_rename.clone();
        if let Some((pp, name)) = pend
            && pp == p
        {
            self.state.borrow_mut().pending_rename = None;
            let i = self.ws.borrow().active_tab(p).index_of_name(&name);
            if let Some(i) = i {
                self.begin_rename(p, i);
            }
        }
        let stale = {
            let st = self.state.borrow();
            let ws = self.ws.borrow();
            st.rename.as_ref().is_some_and(|r| {
                r.pane == p && {
                    let t = ws.active_tab(p);
                    t.location() != Some(r.dir.as_path()) || t.index_of_name(&r.old).is_none()
                }
            })
        };
        if stale {
            self.end_rename();
        }
    }

    pub(super) fn set_pending_rename(&self, p: usize, name: OsString) {
        self.state.borrow_mut().pending_rename = Some((p, name));
    }

    // ---------- 剪贴板 ----------

    /// Ctrl+C（0）、Ctrl+X（1）、Ctrl+V（2）。
    pub fn clip_op(&self, p: usize, op: i32) {
        match op {
            0 => self.clip_copy(p, false),
            1 => self.clip_copy(p, true),
            _ => self.paste(p),
        }
    }

    fn clip_copy(&self, p: usize, cut: bool) {
        self.activate_pane(p);
        if self.state.borrow().rename.is_some() {
            return;
        }
        let (in_bin, paths) = {
            let ws = self.ws.borrow();
            let t = ws.active_tab(p);
            (t.is_recycle_bin(), t.selected_paths())
        };
        if in_bin {
            self.set_message("回收站里的项目请先还原，再复制或剪切");
            return;
        }
        if paths.is_empty() {
            self.set_message(if cut {
                "请先选择要剪切的项目"
            } else {
                "请先选择要复制的项目"
            });
            return;
        }
        let effect = if cut { Effect::Move } else { Effect::Copy };
        if let Err(e) = self.clipboard.write(&paths, effect) {
            self.set_message(&format!("无法写入剪贴板：{e}"));
            return;
        }
        self.drop_cut();
        if cut {
            let keys: HashSet<String> = paths.iter().map(|x| xfer::key(x)).collect();
            self.state.borrow_mut().cut = Some(Cut {
                seq: self.clipboard.sequence(),
            });
            self.marks.borrow_mut().cut = keys.clone();
            self.touch_rows(&keys);
        }
        let verb = if cut { "剪切" } else { "复制" };
        let mut msg = match paths.as_slice() {
            [one] => format!(
                "已{verb}“{}”，到目标位置按 Ctrl+V 粘贴",
                one.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            _ => format!("已{verb} {} 项，到目标位置按 Ctrl+V 粘贴", paths.len()),
        };
        // 复制含仅在线文件：粘贴时才会读取内容并让云盘客户端下载，提前说明
        if !cut {
            let c =
                crate::cloud::count_online_only(&paths, 2000, std::time::Duration::from_millis(40));
            if c.any() {
                msg += &format!("。{}", c.hint().replacen("包含", "其中包含", 1));
            }
        }
        self.set_info(&msg);
    }

    fn paste(&self, p: usize) {
        self.activate_pane(p);
        if self.state.borrow().rename.is_some() || !self.can_start_op() {
            return;
        }
        self.check_cut();
        let Some(clip) = self.clipboard.read() else {
            self.set_message("剪贴板里没有可粘贴的文件");
            return;
        };
        let dir = {
            let ws = self.ws.borrow();
            let t = ws.active_tab(p);
            match (t.is_recycle_bin(), t.state(), t.location()) {
                (false, LoadState::Ready, Some(l)) => Ok(l.to_path_buf()),
                (true, ..) => Err("不能粘贴到回收站"),
                _ => Err("当前位置还没有读取完成，不能粘贴"),
            }
        };
        let dir = match dir {
            Ok(d) => d,
            Err(m) => {
                self.set_message(m);
                return;
            }
        };
        let mode = match clip.effect {
            Effect::Copy => Mode::Copy,
            Effect::Move => Mode::Move,
        };
        let seq = self.clipboard.sequence();
        self.start_transfer(clip.paths, dir, mode, Origin::Clipboard(seq), Some(p));
    }

    /// 剪贴板被别处改写后，本应用的“剪切”标记作废。
    pub(super) fn check_cut(&self) {
        let stale = self
            .state
            .borrow()
            .cut
            .as_ref()
            .is_some_and(|c| self.clipboard.sequence() != c.seq);
        if stale {
            self.drop_cut();
        }
    }

    /// 清除剪切标记（不动剪贴板）。
    pub(super) fn drop_cut(&self) {
        let had = self.state.borrow_mut().cut.take().is_some();
        if had {
            let keys = std::mem::take(&mut self.marks.borrow_mut().cut);
            self.touch_rows(&keys);
        }
    }

    /// 通知两个面板里路径在 `keys` 中的行重绘。
    fn touch_rows(&self, keys: &HashSet<String>) {
        for p in [0, 1] {
            let rows: Vec<usize> = {
                let ws = self.ws.borrow();
                let t = ws.active_tab(p);
                (0..t.rows().len())
                    .filter(|&i| t.path_of(i).is_some_and(|x| keys.contains(&xfer::key(&x))))
                    .collect()
            };
            for i in rows {
                self.ui[p].items.row_changed(i);
            }
        }
    }

    /// 传输成功完成：剪贴板粘贴的移动完成后清空剪贴板与标记。
    pub(super) fn after_moved_from_clipboard(&self, seq: u32) {
        if self.clipboard.sequence() == seq {
            self.clipboard.clear();
        }
        self.drop_cut();
    }

    /// 剪贴板对象（harness 与测试读写）。
    pub fn clipboard(&self) -> &Rc<dyn Clipboard> {
        &self.clipboard
    }

    /// 是否有“剪切”标记（harness 使用）。
    pub fn cut_active(&self) -> bool {
        self.state.borrow().cut.is_some()
    }

    // ---------- 传输（粘贴与拖放共用） ----------

    /// 对 `srcs` 到 `dest` 的复制或移动做计划；无冲突直接执行，有冲突先询问。
    pub(super) fn start_transfer(
        &self,
        srcs: Vec<PathBuf>,
        dest: PathBuf,
        mode: Mode,
        origin: Origin,
        pane: Option<usize>,
    ) {
        if !self.can_start_op() {
            return;
        }
        match xfer::plan(&srcs, &dest, mode) {
            Err(e) => {
                let body = format!(
                    "无法{}到“{}”：{}。\n\n没有{}任何项目。",
                    mode.verb(),
                    location::display(&dest),
                    dest_error_text(&e),
                    mode.verb()
                );
                self.show_notice(&format!("无法{}", mode.verb()), &body);
            }
            Ok(plan) if plan.conflicts.is_empty() => {
                self.run_task(Task::Transfer { plan, origin, pane });
            }
            Ok(plan) => self.open_conflict(Pending { plan, origin, pane }),
        }
    }

    fn open_conflict(&self, pend: Pending) {
        let verb = pend.plan.mode.verb();
        let n = pend.plan.conflicts.len();
        let mut names = String::new();
        for c in pend.plan.conflicts.iter().take(5) {
            let name = c
                .existing
                .file_name()
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_default();
            names += &format!("・{name}\n");
        }
        if n > 5 {
            names += &format!("……等共 {n} 项\n");
        }
        let body = format!(
            "“{}”里已有同名项目：\n{}\n“保留两者”会给新来的项目改名（如“名称 (2)”），不会覆盖或删除任何已有项目；“跳过”则不{verb}这些项目；“取消”不做任何改动。",
            location::leaf_label(&pend.plan.dest_dir),
            names.trim_end()
        );
        let title = if n == 1 {
            "目标位置已有同名项目".to_string()
        } else {
            format!("目标位置已有 {n} 个同名项目")
        };
        self.open_dialog(
            Dialog::Conflict(pend),
            &title,
            &body,
            "保留两者",
            "取消",
            false,
        );
        if let Some(w) = self.window.upgrade() {
            w.set_dialog_secondary_label("跳过".into());
        }
    }

    /// 冲突对话框的选择。
    pub(super) fn resolve_conflict(&self, mut pend: Pending, policy: Policy) {
        xfer::resolve(&mut pend.plan, policy);
        self.run_task(Task::Transfer {
            plan: pend.plan,
            origin: pend.origin,
            pane: pend.pane,
        });
    }

    // ---------- 拖放：应用内 ----------

    /// 修饰键状态（`MK_*`）：测试可覆盖，否则读取真实键盘。
    fn keys_now(&self) -> u32 {
        if let Some(k) = self.keys_override.get() {
            return k;
        }
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_SHIFT};
        // SAFETY: GetAsyncKeyState 只读取键盘状态。
        let down = |vk: i32| unsafe { GetAsyncKeyState(vk) } < 0;
        (if down(VK_CONTROL.0 as i32) {
            MK_CONTROL
        } else {
            0
        }) | (if down(VK_SHIFT.0 as i32) { MK_SHIFT } else { 0 })
    }

    /// 测试：固定修饰键（`MK_CONTROL | MK_SHIFT` 的组合），`None` 恢复读取真实键盘。
    pub fn set_keys_override(&self, keys: Option<u32>) {
        self.keys_override.set(keys);
    }

    /// 条目上的指针阶段（界面回调）。
    pub fn item_drag(&self, p: i32, i: i32, phase: DragPhase, x: f32, y: f32) {
        let (Some(i), p) = (idx(i), pane(p)) else {
            return;
        };
        match phase {
            DragPhase::Down => {
                self.state.borrow_mut().press = Some(Press {
                    pane: p,
                    index: i,
                    at: (x, y),
                });
            }
            DragPhase::Move => self.drag_move(x, y),
            DragPhase::Up => self.drag_up(x, y),
            DragPhase::Cancel => self.drag_abort(),
        }
    }

    /// 多选里按下已选项：不立刻收拢选择，等松开时没有拖动才收拢（先登记，松开时执行）。
    pub(super) fn defer_collapse(&self, p: usize, i: usize) {
        self.state.borrow_mut().collapse = Some((p, i));
    }

    pub(super) fn clear_collapse(&self) {
        self.state.borrow_mut().collapse = None;
    }

    fn drag_move(&self, x: f32, y: f32) {
        let started = self.state.borrow().drag.is_some();
        if !started {
            let begin = {
                let st = self.state.borrow();
                st.press.as_ref().and_then(|pr| {
                    let d = ((x - pr.at.0).powi(2) + (y - pr.at.1).powi(2)).sqrt();
                    (d >= DRAG_START).then_some((pr.pane, pr.index))
                })
            };
            let Some((pane, index)) = begin else {
                return;
            };
            if !self.begin_drag(pane, index) {
                // 回收站等不能拖：放弃这次按下
                self.state.borrow_mut().press = None;
                return;
            }
        }
        if self.outside_window(x, y) {
            self.drag_out(x, y);
            return;
        }
        let keys = self.keys_now();
        self.drag_update(x, y, keys, ALLOW_BOTH);
    }

    fn begin_drag(&self, pane: usize, index: usize) -> bool {
        let srcs = {
            let ws = self.ws.borrow();
            let t = ws.active_tab(pane);
            if t.is_recycle_bin() {
                return false;
            }
            if t.is_selected(index) {
                t.selected_paths()
            } else {
                t.path_of(index).into_iter().collect()
            }
        };
        if srcs.is_empty() {
            return false;
        }
        let mut st = self.state.borrow_mut();
        st.collapse = None;
        st.drag = Some(Drag {
            srcs,
            from: Some(pane),
        });
        true
    }

    fn drag_up(&self, x: f32, y: f32) {
        let (collapse, drag) = {
            let mut st = self.state.borrow_mut();
            st.press = None;
            (st.collapse.take(), st.drag.take())
        };
        if let Some(d) = drag {
            let ev = self.evaluate(&d.srcs, self.hit(x, y), self.keys_now(), ALLOW_BOTH);
            self.hide_drag();
            match (ev.ok, ev.dest) {
                (true, Some(dest)) => {
                    self.start_transfer(d.srcs, dest.dir, ev.mode, Origin::Drag, Some(dest.pane))
                }
                _ if !ev.hint.is_empty() => self.set_message(&ev.hint),
                _ => {}
            }
            return;
        }
        if let Some((p, i)) = collapse {
            self.mutate(p, |t| t.click(i, false, false));
        }
    }

    /// 放弃当前的按下与拖动（Esc、指针事件被取消）。
    fn drag_abort(&self) {
        let was_dragging = {
            let mut st = self.state.borrow_mut();
            st.press = None;
            st.collapse = None;
            st.drag.take().is_some()
        };
        if was_dragging {
            self.hide_drag();
        }
    }

    /// Esc：取消拖放。
    pub fn drag_cancel(&self) {
        let was = self.state.borrow().drag.is_some();
        self.drag_abort();
        if was {
            self.set_info("已取消拖放");
        }
    }

    /// 是否正在拖动（harness 使用）。
    pub fn drag_active(&self) -> bool {
        self.state.borrow().drag.is_some()
    }

    /// 窗口内一点命中的（面板，条目）；不在任何面板内容区为 `None`。
    fn hit(&self, x: f32, y: f32) -> Option<(usize, Option<usize>)> {
        let w = self.window.upgrade()?;
        let h = w.invoke_drop_hit(x, y);
        (h.pane >= 0).then(|| (pane(h.pane), idx(h.index)))
    }

    fn outside_window(&self, x: f32, y: f32) -> bool {
        let Some(w) = self.window.upgrade() else {
            return false;
        };
        let size = w.window().size();
        let s = w.window().scale_factor();
        x < 0.0 || y < 0.0 || x >= size.width as f32 / s || y >= size.height as f32 / s
    }

    /// 判定放在 `hit` 处会发生什么：目标文件夹、复制或移动、是否可行、提示文字。
    fn evaluate(
        &self,
        srcs: &[PathBuf],
        hit: Option<(usize, Option<usize>)>,
        keys: u32,
        allowed: u32,
    ) -> Eval {
        let Some((pane, index)) = hit else {
            return Eval::none("");
        };
        let (dir, mark) = {
            let ws = self.ws.borrow();
            let t = ws.active_tab(pane);
            let mut bad = Eval::none("");
            bad.pane = pane as i32;
            if t.is_recycle_bin() {
                bad.hint = "不能放进回收站".into();
                return bad;
            }
            if *t.state() != LoadState::Ready {
                bad.hint = "此位置现在不可放置".into();
                return bad;
            }
            // 指在文件夹上放进这个文件夹；指在文件或空白处放进面板当前的文件夹
            let sub = index
                .filter(|&i| t.is_dir_at(i))
                .and_then(|i| t.path_of(i).map(|p| (i, p)))
                .filter(|(_, p)| !srcs.iter().any(|s| xfer::key(s) == xfer::key(p)));
            match (sub, t.location()) {
                (Some((i, p)), _) => (p, Some((pane, i))),
                (None, Some(l)) => (l.to_path_buf(), None),
                (None, None) => {
                    bad.hint = "此位置不可放置".into();
                    return bad;
                }
            }
        };
        let leaf = location::leaf_label(&dir);
        let mut ev = Eval {
            dest: Some(Dest {
                pane,
                dir: dir.clone(),
            }),
            mode: Mode::Copy,
            ok: false,
            hint: String::new(),
            mark,
            pane: pane as i32,
            index: mark.map_or(-1, |(_, i)| i as i32),
        };
        let mut mode = if keys & MK_CONTROL != 0 {
            Mode::Copy
        } else if keys & MK_SHIFT != 0 {
            Mode::Move
        } else {
            xfer::default_mode(srcs, &dir)
        };
        // 来源只允许其中一种效果时服从来源
        if mode == Mode::Move && allowed & EFFECT_MOVE == 0 {
            mode = Mode::Copy;
        }
        if mode == Mode::Copy && allowed & EFFECT_COPY == 0 {
            if allowed & EFFECT_MOVE != 0 {
                mode = Mode::Move;
            } else {
                ev.hint = "此来源不允许放置".into();
                return ev;
            }
        }
        ev.mode = mode;
        if srcs.iter().any(|s| xfer::is_inside(&dir, s)) {
            ev.hint = "不能把文件夹放进它自己里面".into();
            return ev;
        }
        let all_here = srcs
            .iter()
            .all(|s| s.parent().is_some_and(|p| xfer::key(p) == xfer::key(&dir)));
        if mode == Mode::Move && all_here {
            ev.hint = format!("已经在“{leaf}”里了");
            return ev;
        }
        ev.ok = true;
        ev.hint = format!("{}到 {leaf}", mode.verb());
        ev
    }

    /// 评估并刷新拖放提示；返回要告诉系统的效果（0 为不可放置）。
    fn drag_update(&self, x: f32, y: f32, keys: u32, allowed: u32) -> u32 {
        let srcs = self.state.borrow().drag.as_ref().map(|d| d.srcs.clone());
        let Some(srcs) = srcs else {
            return 0;
        };
        let ev = self.evaluate(&srcs, self.hit(x, y), keys, allowed);
        self.set_drop_mark(ev.mark);
        if let Some(w) = self.window.upgrade() {
            let bus = w.global::<DragBus>();
            bus.set_active(true);
            bus.set_x(x);
            bus.set_y(y);
            bus.set_label(label_of(&srcs).into());
            bus.set_hint(ev.hint.clone().into());
            bus.set_hint_ok(ev.ok);
            bus.set_target_pane(if ev.ok { ev.pane } else { -1 });
            bus.set_target_index(if ev.ok { ev.index } else { -1 });
        }
        if ev.ok { effect_of(ev.mode) } else { 0 }
    }

    fn hide_drag(&self) {
        self.set_drop_mark(None);
        if let Some(w) = self.window.upgrade() {
            let bus = w.global::<DragBus>();
            bus.set_active(false);
            bus.set_target_pane(-1);
            bus.set_target_index(-1);
        }
    }

    fn set_drop_mark(&self, m: Option<(usize, usize)>) {
        let old = {
            let mut marks = self.marks.borrow_mut();
            if marks.drop == m {
                return;
            }
            std::mem::replace(&mut marks.drop, m)
        };
        for (p, i) in old.into_iter().chain(m) {
            self.ui[p].items.row_changed(i);
        }
    }

    /// 指针拖出窗口：改用系统拖放，把文件拖给资源管理器或别的程序。
    fn drag_out(&self, x: f32, y: f32) {
        let srcs = {
            let mut st = self.state.borrow_mut();
            st.press = None;
            st.drag.take().map(|d| d.srcs)
        };
        self.hide_drag();
        let Some(srcs) = srcs else {
            return;
        };
        // 拖给别的程序时，对方读取文件会让云盘客户端下载仅在线文件：拖出时就说明
        let c = crate::cloud::count_online_only(&srcs, 2000, std::time::Duration::from_millis(40));
        if c.any() {
            self.set_info(&format!("正在拖出 {}：{}", label_of(&srcs), c.hint()));
        }
        match &self.os_drag {
            OsDrag::Record(log) => log.borrow_mut().push(srcs),
            OsDrag::Shell => {
                let r = dragdrop::start_drag(self.hwnd.get(), &srcs);
                // 系统的模态拖放循环吞掉了鼠标松开事件：补发一次，让界面的按下状态复位
                if let Some(w) = self.window.upgrade() {
                    w.window()
                        .dispatch_event(slint::platform::WindowEvent::PointerReleased {
                            position: slint::LogicalPosition::new(x.max(0.0), y.max(0.0)),
                            button: slint::platform::PointerEventButton::Left,
                        });
                }
                match r {
                    Ok(dragdrop::DragOut::Dropped) => {
                        let dirs = crate::fileops::parent_dirs(srcs.iter().map(PathBuf::as_path));
                        let jobs = self.ws.borrow_mut().refresh_where(|loc| {
                            dirs.iter().any(|d| xfer::key(d) == xfer::key(loc))
                        });
                        let touched = self.dispatch(jobs);
                        self.update_panes(&touched);
                    }
                    Ok(dragdrop::DragOut::Cancelled) => {}
                    Err(e) => self.set_message(&e),
                }
            }
        }
    }

    /// 被拖出窗口的路径记录（harness 使用）。
    pub fn os_drag_log(&self) -> Vec<Vec<PathBuf>> {
        match &self.os_drag {
            OsDrag::Record(log) => log.borrow().clone(),
            OsDrag::Shell => Vec::new(),
        }
    }

    // ---------- 拖放：资源管理器拖进来 ----------

    /// 资源管理器拖入窗口（DropHost 实现与 harness 模拟共用）。
    pub fn ext_enter(&self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        self.state.borrow_mut().drag = Some(Drag {
            srcs: paths.to_vec(),
            from: None,
        });
    }

    fn logical(&self, client: (i32, i32)) -> (f32, f32) {
        let s = self
            .window
            .upgrade()
            .map_or(1.0, |w| w.window().scale_factor());
        (client.0 as f32 / s, client.1 as f32 / s)
    }

    pub fn ext_over(&self, client: (i32, i32), keys: u32, allowed: u32) -> u32 {
        let (x, y) = self.logical(client);
        self.drag_update(x, y, keys, allowed)
    }

    pub fn ext_leave(&self) {
        let was_external = {
            let mut st = self.state.borrow_mut();
            if st.drag.as_ref().is_some_and(|d| d.from.is_none()) {
                st.drag = None;
                true
            } else {
                false
            }
        };
        if was_external {
            self.hide_drag();
        }
    }

    pub fn ext_drop(&self, client: (i32, i32), keys: u32, allowed: u32) -> u32 {
        let (x, y) = self.logical(client);
        let srcs = {
            let mut st = self.state.borrow_mut();
            match st.drag.take() {
                Some(d) if d.from.is_none() => d.srcs,
                other => {
                    st.drag = other;
                    return 0;
                }
            }
        };
        let ev = self.evaluate(&srcs, self.hit(x, y), keys, allowed);
        self.hide_drag();
        match (ev.ok, ev.dest) {
            (true, Some(dest)) => {
                let mode = ev.mode;
                self.start_transfer(srcs, dest.dir, mode, Origin::Drag, Some(dest.pane));
                // 移动由本应用自己完成，回答“没有效果”，来源就不会再去删它自己的文件
                if mode == Mode::Move { 0 } else { EFFECT_COPY }
            }
            _ => {
                if !ev.hint.is_empty() {
                    self.set_message(&ev.hint);
                }
                0
            }
        }
    }
}

/// 把 [`DropHost`] 接到控制器上（弱引用，窗口关闭后自然失效）。
struct Host(Weak<Controller>);

impl DropHost for Host {
    fn enter(&self, paths: &[PathBuf]) {
        if let Some(c) = self.0.upgrade() {
            c.ext_enter(paths);
        }
    }
    fn over(&self, client: (i32, i32), keys: u32, allowed: u32) -> u32 {
        self.0
            .upgrade()
            .map_or(0, |c| c.ext_over(client, keys, allowed))
    }
    fn leave(&self) {
        if let Some(c) = self.0.upgrade() {
            c.ext_leave();
        }
    }
    fn dropped(&self, client: (i32, i32), keys: u32, allowed: u32) -> u32 {
        self.0
            .upgrade()
            .map_or(0, |c| c.ext_drop(client, keys, allowed))
    }
}

/// 窗口显示之后调用：登记自己的拖放目标，接管资源管理器拖进来的文件。成功返回 `true`。
pub fn attach_drop(c: &Rc<Controller>, window: &MainWindow) -> bool {
    let Some(hwnd) = dragdrop::hwnd_of(window.window()) else {
        return false;
    };
    c.hwnd.set(hwnd);
    // 有了窗口句柄才能登记为外壳窗口（“在文件夹中显示”，Issue #18；只在已接管时登记）
    c.reveal_refresh();
    dragdrop::attach(hwnd, Rc::new(Host(Rc::downgrade(c))))
}
