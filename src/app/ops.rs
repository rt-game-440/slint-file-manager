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

use super::*;
use crate::fileops::{BinItem, DeleteMode, OpKind, Report, parent_dirs, run_delete, run_restore};
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
}

/// 一次后台操作完成后回到界面线程的结果。
pub struct OpDone {
    pub report: Report,
    /// 内容可能变化的目录。
    pub dirs: Vec<PathBuf>,
    /// 回收站内容是否可能变化。
    pub bin_changed: bool,
}

enum Task {
    Delete {
        mode: DeleteMode,
        paths: Vec<PathBuf>,
    },
    Restore(Vec<BinItem>),
}

impl Task {
    fn run(self, backend: &dyn Backend) -> OpDone {
        match self {
            Task::Delete { mode, paths } => {
                let report = run_delete(backend, &paths, mode);
                OpDone {
                    report,
                    dirs: parent_dirs(paths.iter().map(PathBuf::as_path)),
                    bin_changed: mode == DeleteMode::Recycle,
                }
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
                OpDone {
                    report,
                    dirs,
                    bin_changed: true,
                }
            }
        }
    }

    fn progress_text(&self) -> String {
        let n = match self {
            Task::Delete { paths, .. } => paths.len(),
            Task::Restore(items) => items.len(),
        };
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

    fn open_dialog(
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
        w.set_dialog_danger(danger);
        w.set_dialog_token(w.get_dialog_token() + 1);
        w.set_dialog_visible(true);
    }

    fn show_notice(&self, title: &str, body: &str) {
        self.open_dialog(Dialog::Notice, title, body, "知道了", "", false);
    }

    /// 关闭对话框并把键盘焦点还给活动面板，返回刚才的对话框。
    fn close_dialog(&self) -> Option<Dialog> {
        let d = self.dialog.borrow_mut().take();
        if let Some(w) = self.window.upgrade() {
            w.set_dialog_visible(false);
        }
        let p = self.ws.borrow().active_pane();
        self.focus_pane(p);
        d
    }

    /// 对话框确定。
    pub fn accept_dialog(&self) {
        match self.close_dialog() {
            Some(Dialog::ConfirmDelete { mode, paths }) => {
                self.run_task(Task::Delete { mode, paths })
            }
            Some(Dialog::Notice) | None => {}
        }
    }

    /// 对话框取消（Esc 或“取消”）：什么都不改变。
    pub fn cancel_dialog(&self) {
        if let Some(Dialog::ConfirmDelete { .. }) = self.close_dialog() {
            self.set_info("已取消，没有删除任何项目");
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

    fn run_task(&self, task: Task) {
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
        } = done;
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
        if report.fail_count() == 0 {
            self.set_info(&summary.line);
        } else {
            self.set_message(&summary.line);
        }
        if let Some(detail) = summary.detail {
            let title = match (report.kind, report.ok_count()) {
                (OpKind::Restore, 0) => "没有还原任何项目",
                (OpKind::Restore, _) => "部分项目没有还原",
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
