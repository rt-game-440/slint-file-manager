//! 删除与回收站的界面接线：确认对话框、后台逐项执行、按真实结果汇报、完成后刷新受影响的位置。
//!
//! - **入口**：Delete（快捷键与工具栏“删除”）= 移到回收站；Shift+Delete（与工具栏“永久删除”）= 永久删除。
//!   两者都先弹应用内确认；Esc／“取消”不改变任何文件。移到回收站的确认以 Enter 为默认；永久删除的确认
//!   必须点击按钮（Enter 无效），默认动作是取消。
//! - **不静默退化**：所在位置不支持回收站（可移动磁盘、网络位置、路径过长）的项目，在确认框里说明并在执行时
//!   报告失败，**不会**改成永久删除；全部不支持时只显示说明、不执行任何操作。
//! - **汇报**：逐项结果汇总为状态栏一行；有失败时再弹说明框列出失败项与原因。列表刷新不代表成功。
//! - **刷新**：完成后重新读取所有显示受影响目录（及回收站）的标签（[`Workspace::refresh_where`]），选择保留，
//!   失败项因仍在原处而继续显示。回收站视图里不提供删除，也不提供清空。

use super::transfer::{Origin, Pending};
use super::*;
use crate::fileops::{
    BinItem, DeleteMode, FailKind, OpError, OpKind, Outcome, Report, parent_dirs, run_delete,
    run_restore,
};
use crate::shellnew::Template;
use crate::xfer::{self, Mode, Policy};
use std::ffi::OsString;
use std::path::Path;

/// 当前打开的应用内对话框。
pub enum Dialog {
    /// 删除确认；确认后按 `mode` 删除 `paths`。
    ConfirmDelete {
        mode: DeleteMode,
        paths: Vec<PathBuf>,
    },
    /// 只有“知道了”的说明。
    Notice,
    /// 复制或移动遇到同名项目：保留两者 / 跳过 / 取消。
    Conflict(Pending),
    /// 启用“用本应用打开文件夹”前的确认（见 `entry`）。
    EntryEnable,
    /// 启动自检发现关联指向旧位置：修复 / 恢复原设置 / 以后再说。
    EntryStale,
}

/// 新建的内容。
#[derive(Clone)]
pub enum CreateKind {
    Folder,
    File(Template),
}

/// 一次后台操作完成后回到界面线程的结果。
pub struct OpDone {
    pub report: Report,
    /// 内容可能变化的目录。
    pub dirs: Vec<PathBuf>,
    /// 回收站内容是否可能变化。
    pub bin_changed: bool,
    /// 完成后在这个面板（目录一致时）选中并聚焦的名称。
    pub focus: Option<(usize, PathBuf, OsString)>,
    /// 新建成功后立刻进入重命名。
    pub rename_after: bool,
    /// 剪贴板“剪切”粘贴全部成功时，粘贴所用的剪贴板序号（用来清空剪贴板）。
    pub clear_clip: Option<u32>,
    /// 追加在结果汇报后的说明（复制含仅在线文件时：哪些已被云盘客户端下载，Issue #14）。
    pub note: Option<String>,
}

impl OpDone {
    fn basic(report: Report, dirs: Vec<PathBuf>, bin_changed: bool) -> OpDone {
        OpDone {
            report,
            dirs,
            bin_changed,
            focus: None,
            rename_after: false,
            clear_clip: None,
            note: None,
        }
    }
}

/// 这次传输会读取源文件内容吗：复制一定会；移动在同一磁盘上只是改名（不读内容），跨磁盘才会先复制。
fn reads_content(mode: Mode, src: &Path, dest_dir: &Path) -> bool {
    fn drive(p: &Path) -> Option<char> {
        let s = p.to_string_lossy();
        let mut c = s.chars();
        let l = c.next()?;
        (c.next() == Some(':')).then(|| l.to_ascii_uppercase())
    }
    match mode {
        Mode::Copy => true,
        Mode::Move => drive(src) != drive(dest_dir),
    }
}

/// 传输里会被读取内容的仅在线文件统计（只读枚举属性，不打开文件）。
fn online_in_plan(
    plan: &xfer::Plan,
    max_entries: usize,
    budget: std::time::Duration,
) -> crate::cloud::OnlineCount {
    let srcs: Vec<PathBuf> = plan
        .jobs
        .iter()
        .filter(|j| reads_content(plan.mode, &j.src, &plan.dest_dir))
        .map(|j| j.src.clone())
        .collect();
    crate::cloud::count_online_only(&srcs, max_entries, budget)
}

pub(super) enum Task {
    Delete {
        mode: DeleteMode,
        paths: Vec<PathBuf>,
    },
    Restore(Vec<BinItem>),
    /// 复制或移动（计划已做好、冲突已处理）。
    Transfer {
        plan: xfer::Plan,
        origin: Origin,
        pane: Option<usize>,
    },
    /// 新建文件夹或文件，成功后进入重命名。
    Create {
        path: PathBuf,
        kind: CreateKind,
        pane: usize,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        pane: usize,
    },
}

fn leaf(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

impl Task {
    fn run(self, backend: &dyn Backend) -> OpDone {
        match self {
            Task::Delete { mode, paths } => {
                let report = run_delete(backend, &paths, mode);
                OpDone::basic(
                    report,
                    parent_dirs(paths.iter().map(PathBuf::as_path)),
                    mode == DeleteMode::Recycle,
                )
            }
            Task::Transfer { plan, origin, pane } => {
                let srcs: Vec<PathBuf> = plan.jobs.iter().map(|j| j.src.clone()).collect();
                let dirs = xfer::affected_dirs(&plan.dest_dir, &srcs, plan.mode);
                let mode = plan.mode;
                let dest_dir = plan.dest_dir.clone();
                let first = plan
                    .jobs
                    .first()
                    .and_then(|j| j.dest.file_name())
                    .map(OsString::from);
                // 传输前统计：会被读取内容的仅在线文件（读取源文件会让云盘客户端下载，要让用户知道）
                let online = online_in_plan(&plan, 20_000, std::time::Duration::from_millis(1500));
                let report = xfer::run(backend, plan);
                let clear_clip = match origin {
                    Origin::Clipboard(seq)
                        if mode == Mode::Move
                            && report.fail_count() == 0
                            && report.ok_count() > 0 =>
                    {
                        Some(seq)
                    }
                    _ => None,
                };
                let mut done = OpDone::basic(report, dirs, false);
                done.focus = match (pane, first) {
                    (Some(p), Some(name)) => Some((p, dest_dir, name)),
                    _ => None,
                };
                done.clear_clip = clear_clip;
                done.note = online.any().then(|| online.done_text());
                done
            }
            Task::Create { path, kind, pane } => {
                let name = leaf(&path);
                let result = match &kind {
                    CreateKind::Folder => backend.create_dir(&path),
                    CreateKind::File(t) => match t.content() {
                        Ok(bytes) => backend.create_file(&path, &bytes),
                        Err(e) => Err(OpError::new(FailKind::Other, e)),
                    },
                };
                let dir = path.parent().map(PathBuf::from).unwrap_or_default();
                let report = Report::new(OpKind::Create, vec![Outcome { name, result }]);
                let mut done = OpDone::basic(report, vec![dir.clone()], false);
                done.focus = Some((pane, dir, OsString::from(leaf(&path))));
                done.rename_after = true;
                done
            }
            Task::Rename { from, to, pane } => {
                let name = leaf(&to);
                let result = backend.rename(&from, &to);
                let dir = to.parent().map(PathBuf::from).unwrap_or_default();
                let report = Report::new(OpKind::Rename, vec![Outcome { name, result }]);
                let mut done = OpDone::basic(report, vec![dir.clone()], false);
                done.focus = Some((pane, dir, OsString::from(leaf(&to))));
                done
            }
            Task::Restore(items) => {
                let report = run_restore(backend, &items);
                let mut dirs: Vec<PathBuf> = Vec::new();
                for (it, o) in items.iter().zip(&report.outcomes) {
                    let d = PathBuf::from(&it.original_dir);
                    if o.result.is_ok() && !dirs.iter().any(|x| same_dir(x, &d)) {
                        dirs.push(d);
                    }
                }
                OpDone::basic(report, dirs, true)
            }
        }
    }

    fn progress_text(&self) -> String {
        let n = match self {
            Task::Delete { paths, .. } => paths.len(),
            Task::Restore(items) => items.len(),
            Task::Transfer { plan, .. } => plan.jobs.len(),
            Task::Create { .. } | Task::Rename { .. } => 1,
        };
        match self {
            Task::Transfer { plan, .. } => {
                // 进度提示里先说明会触发下载（快速统计，上限小，不阻塞界面）
                let online = online_in_plan(plan, 500, std::time::Duration::from_millis(30));
                return if online.any() {
                    format!("正在{} {n} 项…{}", plan.mode.verb(), online.hint())
                } else {
                    format!("正在{} {n} 项…", plan.mode.verb())
                };
            }
            Task::Create { path, .. } => return format!("正在新建“{}”…", leaf(path)),
            Task::Rename { to, .. } => return format!("正在重命名为“{}”…", leaf(to)),
            _ => {}
        }
        match self {
            Task::Delete {
                mode: DeleteMode::Recycle,
                ..
            } => format!("正在将 {n} 项移到回收站…"),
            Task::Delete {
                mode: DeleteMode::Permanent,
                ..
            } => format!("正在永久删除 {n} 项…"),
            Task::Restore(_) => format!("正在还原 {n} 项…"),
            _ => String::new(),
        }
    }
}

/// 目录比较：不区分大小写与末尾分隔符。
fn same_dir(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_lowercase()
    };
    key(a) == key(b)
}

/// 前几个名称，供确认框列出。
fn name_list(paths: &[PathBuf]) -> String {
    const SHOW: usize = 5;
    let mut s = String::new();
    for p in paths.iter().take(SHOW) {
        let n = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string());
        s += &format!("・{n}\n");
    }
    if paths.len() > SHOW {
        s += &format!("……等共 {} 项\n", paths.len());
    }
    s.trim_end().to_string()
}

fn first_name(paths: &[PathBuf]) -> String {
    paths
        .first()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl Controller {
    // ---------- 删除 ----------

    /// Delete / Shift+Delete / 工具栏：对活动面板的选择发起删除。先确认，不直接执行。
    pub fn delete_selected(&self, p: usize, permanent: bool) {
        self.activate_pane(p);
        if self.dialog.borrow().is_some() {
            return;
        }
        if self.busy.get() {
            self.set_message("上一个删除或还原操作还在进行，请稍候");
            return;
        }
        let (in_bin, paths) = {
            let ws = self.ws.borrow();
            let t = ws.active_tab(p);
            (t.is_recycle_bin(), t.selected_paths())
        };
        if in_bin {
            self.set_message("回收站里的项目不能在这里删除：选中后点“还原所选项目”可放回原位置");
            return;
        }
        if paths.is_empty() {
            self.set_message("请先选择要删除的项目");
            return;
        }
        let one = paths.len() == 1;
        if permanent {
            let has_dir = paths.iter().any(|p| p.is_dir());
            let title = if one {
                format!("永久删除“{}”？", first_name(&paths))
            } else {
                format!("永久删除 {} 项？", paths.len())
            };
            let mut body = String::from("项目将被直接删除，不会进入回收站，无法恢复。");
            if has_dir {
                body += "\n文件夹中的全部内容也会一并删除。";
            }
            if !one {
                body += &format!("\n\n{}", name_list(&paths));
            }
            if let Some(n) = self.cloud_delete_note(&paths, true) {
                body += &format!("\n\n{n}");
            }
            self.open_dialog(
                Dialog::ConfirmDelete {
                    mode: DeleteMode::Permanent,
                    paths,
                },
                &title,
                &body,
                "永久删除",
                "取消",
                true,
            );
            return;
        }
        // 移到回收站：先检查位置是否支持
        let unsupported: Vec<(PathBuf, String)> = paths
            .iter()
            .filter_map(|p| {
                self.backend
                    .can_recycle(p)
                    .err()
                    .map(|why| (p.clone(), why))
            })
            .collect();
        if unsupported.len() == paths.len() {
            let why = &unsupported[0].1;
            let body = format!(
                "{why}。\n\n没有删除任何项目。如确实要删除，请使用“永久删除”（Shift+Delete）——那样项目无法恢复。"
            );
            self.show_notice("无法移到回收站", &body);
            return;
        }
        let title = if one {
            format!("将“{}”移到回收站？", first_name(&paths))
        } else {
            format!("将 {} 项移到回收站？", paths.len())
        };
        let mut body = String::from("项目会放进回收站，之后可以在“回收站”里还原。");
        if !one {
            body += &format!("\n\n{}", name_list(&paths));
        }
        if !unsupported.is_empty() {
            body += &format!(
                "\n\n其中 {} 项所在位置不支持回收站（{}），这些项目不会被删除，也不会被永久删除。",
                unsupported.len(),
                unsupported[0].1
            );
        }
        if let Some(n) = self.cloud_delete_note(&paths, false) {
            body += &format!("\n\n{n}");
        }
        self.open_dialog(
            Dialog::ConfirmDelete {
                mode: DeleteMode::Recycle,
                paths,
            },
            &title,
            &body,
            "移到回收站",
            "取消",
            false,
        );
    }

    /// 位于云盘同步文件夹里的项目：说明“删除”会同步到云端，与右键菜单里的“释放空间”（只清本机副本）不同（Issue #14）。
    pub fn cloud_delete_note(&self, paths: &[PathBuf], permanent: bool) -> Option<String> {
        let mut provider: Option<String> = None;
        for p in paths {
            if let Some(root) = p.parent().and_then(|d| self.backend.cloud().sync_root(d)) {
                provider = Some(crate::cloud::provider_label(&root.provider).to_string());
                break;
            }
        }
        let p = provider?;
        Some(if permanent {
            format!(
                "这些项目位于 {p} 同步文件夹：永久删除会同步到云端，云端的副本也会被删除，能否找回取决于 {p} 的规则。\n只想腾出本机空间、保留云端内容时，请取消，改用右键菜单里的“释放空间”——那不会删除云端文件。"
            )
        } else {
            format!(
                "这些项目位于 {p} 同步文件夹：移到回收站后，{p} 会把它们从云端一并移除，能否找回取决于 {p} 的回收站规则。\n只想腾出本机空间、保留云端内容时，请取消，改用右键菜单里的“释放空间”——那不会删除云端文件。"
            )
        })
    }

    // ---------- 还原 ----------

    /// 回收站视图：还原所选项目到原位置（无需确认，它不破坏任何东西；冲突与缺目录时项目留在回收站）。
    pub fn restore_selected(&self, p: usize) {
        self.activate_pane(p);
        if self.dialog.borrow().is_some() {
            return;
        }
        if self.busy.get() {
            self.set_message("上一个删除或还原操作还在进行，请稍候");
            return;
        }
        let items = {
            let ws = self.ws.borrow();
            let t = ws.active_tab(p);
            if !t.is_recycle_bin() {
                return;
            }
            t.selected_bin_items()
        };
        if items.is_empty() {
            self.set_message("请先在回收站里选择要还原的项目");
            return;
        }
        self.run_task(Task::Restore(items));
    }

    // ---------- 对话框 ----------

    pub(super) fn open_dialog(
        &self,
        d: Dialog,
        title: &str,
        body: &str,
        confirm: &str,
        cancel: &str,
        danger: bool,
    ) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        *self.dialog.borrow_mut() = Some(d);
        w.set_dialog_title(title.into());
        w.set_dialog_body(body.into());
        w.set_dialog_confirm_label(confirm.into());
        w.set_dialog_cancel_label(cancel.into());
        w.set_dialog_secondary_label("".into());
        w.set_dialog_danger(danger);
        w.set_dialog_token(w.get_dialog_token() + 1);
        w.set_dialog_visible(true);
    }

    pub(super) fn show_notice(&self, title: &str, body: &str) {
        self.open_dialog(Dialog::Notice, title, body, "知道了", "", false);
    }

    /// 关闭对话框并把键盘焦点还给活动面板，返回刚才的对话框。
    fn close_dialog(&self) -> Option<Dialog> {
        let d = self.dialog.borrow_mut().take();
        if let Some(w) = self.window.upgrade() {
            w.set_dialog_visible(false);
        }
        if !self.refocus_settings() {
            let p = self.ws.borrow().active_pane();
            self.focus_pane(p);
        }
        d
    }

    /// 对话框确定。
    pub fn accept_dialog(&self) {
        match self.close_dialog() {
            Some(Dialog::ConfirmDelete { mode, paths }) => {
                self.run_task(Task::Delete { mode, paths })
            }
            Some(Dialog::Conflict(pend)) => self.resolve_conflict(pend, Policy::KeepBoth),
            Some(Dialog::EntryEnable | Dialog::EntryStale) => self.entry_take(),
            Some(Dialog::Notice) | None => {}
        }
    }

    /// 对话框的第三个选项（名称冲突时的“跳过”、关联失效时的“恢复原设置”）。
    pub fn secondary_dialog(&self) {
        match self.close_dialog() {
            Some(Dialog::Conflict(pend)) => self.resolve_conflict(pend, Policy::Skip),
            Some(Dialog::EntryStale) => self.entry_restore(),
            _ => {}
        }
    }

    /// 对话框取消（Esc 或“取消”）：什么都不改变。
    pub fn cancel_dialog(&self) {
        match self.close_dialog() {
            Some(Dialog::ConfirmDelete { .. }) => self.set_info("已取消，没有删除任何项目"),
            Some(Dialog::Conflict(p)) => {
                self.set_info(&format!("已取消，没有{}任何项目", p.plan.mode.verb()))
            }
            _ => {}
        }
    }

    /// 是否有对话框打开（harness 使用）。
    pub fn dialog_open(&self) -> bool {
        self.dialog.borrow().is_some()
    }

    /// 是否有删除或还原正在后台执行（harness 使用）。
    pub fn op_busy(&self) -> bool {
        self.busy.get()
    }

    // ---------- 执行与收尾 ----------

    pub(super) fn run_task(&self, task: Task) {
        self.busy.set(true);
        self.set_info(&task.progress_text());
        let backend = self.backend.clone();
        match &self.loader {
            // 手动模式（harness）：同步执行，结果确定
            Loader::Manual(_) => {
                let done = task.run(&*backend);
                self.finish_op(done);
            }
            Loader::Threaded => {
                let id = self.id;
                std::thread::spawn(move || post(id, Msg::OpDone(task.run(&*backend))));
            }
        }
    }

    /// 后台操作完成：刷新受影响的标签，再按真实结果汇报。
    pub(super) fn finish_op(&self, done: OpDone) {
        self.busy.set(false);
        let OpDone {
            report,
            dirs,
            bin_changed,
            focus,
            rename_after,
            clear_clip,
            note,
        } = done;
        // 新建、重命名、粘贴成功后，在发起操作的面板里选中结果
        if let Some((p, dir, name)) = focus
            && report.ok_count() > 0
        {
            let mut ws = self.ws.borrow_mut();
            let t = ws.active_tab_mut(p);
            if t.location().is_some_and(|l| same_dir(l, &dir)) {
                t.set_focus_after(name.clone());
                if rename_after {
                    drop(ws);
                    self.set_pending_rename(p, name);
                }
            }
        }
        if let Some(seq) = clear_clip {
            self.after_moved_from_clipboard(seq);
        }
        let jobs = self.ws.borrow_mut().refresh_where(|loc| {
            if location::is_recycle_bin(loc) {
                bin_changed
            } else {
                dirs.iter().any(|d| same_dir(d, loc))
            }
        });
        let touched = self.dispatch(jobs);
        self.update_panes(&touched);
        let summary = report.summary();
        let line = match &note {
            Some(n) if report.ok_count() > 0 => format!("{}。{n}", summary.line),
            _ => summary.line.clone(),
        };
        if report.fail_count() == 0 {
            self.set_info(&line);
        } else {
            self.set_message(&line);
        }
        // 用户在系统提示里取消不算出错：只留状态栏一行，不再弹说明框
        let only_aborted =
            report.fail_count() > 0 && report.failures().all(|(_, e)| e.kind == FailKind::Aborted);
        if let Some(detail) = summary.detail
            && !only_aborted
        {
            let title = match (report.kind, report.ok_count()) {
                (OpKind::Restore, 0) => "没有还原任何项目",
                (OpKind::Restore, _) => "部分项目没有还原",
                (OpKind::Copy, 0) => "没有复制任何项目",
                (OpKind::Copy, _) => "部分项目没有复制",
                (OpKind::Move, 0) => "没有移动任何项目",
                (OpKind::Move, _) => "部分项目没有移动",
                (OpKind::Create, _) => "没有新建成功",
                (OpKind::Rename, _) => "没有重命名成功",
                (_, 0) => "没有删除任何项目",
                (_, _) => "部分项目没有删除",
            };
            // 已有对话框（极少见：操作期间用户又触发了别的对话框）时只留状态栏提示
            if self.dialog.borrow().is_none() {
                self.show_notice(title, &detail);
            }
        }
    }
}
