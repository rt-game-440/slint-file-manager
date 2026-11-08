//! “在文件夹中显示”的接线（Issue #18）：把每个标签登记为外壳窗口，外壳要求选中项目时切到该标签并选中。
//!
//! - **启用条件**：只有“用本应用打开文件夹”处于“已接管”状态（[`manage::State::Taken`]）且主窗口已有句柄时才登记，
//!   与 #17 共用同一个开关；停用接管后立即全部注销。登记只在进程存活期间有效，不写任何持久配置。
//! - **登记范围**：两个面板的所有标签（槽号 = 标签 id），位置是可访问的文件系统文件夹；回收站等虚拟位置与
//!   不可用的位置不登记。标签导航、新建、关闭后在 [`Controller::reveal_sync`] 里增量更新。
//! - **选中**：外壳的 `IShellView::SelectItem` 在 COM 调用内部只投递消息（[`Msg::Reveal`]），由事件循环处理：
//!   切到对应标签、还原窗口、关闭盖在面板上的面板，选中项目；项目已不存在时在状态栏提示。
//!
//! 实测与限制见 `docs/research/0016-system-entry-takeover.md` 第 11 节与 `docs/system-entry.md`。

use super::*;
use crate::sysentry::manage::State;
use crate::sysentry::shellwin::{SVSI_DESELECTOTHERS, Selection, ShellWindows};
use std::ffi::OsString;

/// 是否输出每次外壳调用（环境变量 `SFM_REVEAL_TRACE=1`，调试用）。
fn trace_enabled() -> bool {
    std::env::var_os("SFM_REVEAL_TRACE").is_some_and(|v| v == "1")
}

#[derive(Default)]
pub(super) struct RevealUi {
    reg: RefCell<Option<ShellWindows>>,
    /// 上次登记的槽数（只在变化时输出一行）
    last_count: Cell<usize>,
}

impl Controller {
    /// 按接管状态启用或停用登记（窗口句柄就绪、接管启停之后调用）。
    pub fn reveal_refresh(&self) {
        let want = self.hwnd.get() != 0
            && matches!(
                self.entry_status(),
                super::entry::EntryStatus::Known(State::Taken)
            );
        let active = self.reveal.reg.borrow().is_some();
        if want && !active {
            let id = self.id;
            let sink = Rc::new(move |s: Selection| post(id, Msg::Reveal(s)));
            let trace: crate::sysentry::shellwin::Trace = if trace_enabled() {
                Rc::new(|m: &str| println!("reveal: {m}"))
            } else {
                Rc::new(|_: &str| {})
            };
            match ShellWindows::new(self.hwnd.get(), sink, trace) {
                Ok(sw) => {
                    *self.reveal.reg.borrow_mut() = Some(sw);
                    println!("reveal: 已启用（在文件夹中显示）");
                }
                Err(e) => println!("reveal: 无法连接外壳窗口列表，不接管“在文件夹中显示”：{e}"),
            }
        } else if !want && active {
            // 丢弃即全部注销
            self.reveal.reg.borrow_mut().take();
            self.reveal.last_count.set(0);
            println!("reveal: 已停用");
        }
        self.reveal_sync();
    }

    /// 让登记与当前标签一致（标签导航、新建、关闭之后调用；没有变化时不做外壳调用）。
    pub(super) fn reveal_sync(&self) {
        if self.reveal.reg.borrow().is_none() {
            return;
        }
        let want: Vec<(u64, PathBuf)> = {
            let ws = self.ws.borrow();
            [0, 1]
                .iter()
                .flat_map(|&p| ws.pane(p).tabs().iter())
                .filter(|t| !t.is_unavailable())
                .filter_map(|t| {
                    let loc = t.location()?;
                    (!location::is_recycle_bin(loc)).then(|| (t.id(), loc.to_path_buf()))
                })
                .collect()
        };
        // 外壳调用可能在等待期间分发进来的 COM 调用；选中请求只投递消息，不会重入这里
        let (failed, count) = {
            let mut g = self.reveal.reg.borrow_mut();
            let Some(sw) = g.as_mut() else { return };
            let f = sw.sync(&want);
            (f, sw.registered().len())
        };
        for (slot, why) in failed {
            println!("reveal: 标签 {slot} 未登记：{why}");
        }
        if self.reveal.last_count.replace(count) != count {
            println!("reveal: 已登记 {count} 个标签");
        }
    }

    /// 退出前注销（进程退出也会失效，但显式注销让外壳立即忘掉本窗口）。
    pub fn reveal_shutdown(&self) {
        if self.reveal.reg.borrow_mut().take().is_some() {
            println!("reveal: 已注销");
        }
    }

    /// 当前登记的（标签 id, 文件夹），测试用。
    pub fn reveal_registered(&self) -> Vec<(u64, PathBuf)> {
        self.reveal
            .reg
            .borrow()
            .as_ref()
            .map(|s| s.registered())
            .unwrap_or_default()
    }

    /// 外壳要求选中项目（`SHOpenFolderAndSelectItems` 找到了本窗口的某个标签）。
    pub(super) fn on_reveal(&self, s: Selection) {
        let found = self.ws.borrow().locate(s.slot);
        let Some((p, i)) = found else {
            // 标签在请求到达前被关掉了：按外部打开处理（复用或新建标签）
            let r = self.handle_request(crate::sysentry::request::Request::Open {
                folder: s.item.clone().unwrap_or_else(|| s.folder.clone()),
                select: None,
            });
            println!("reveal: 标签已关闭，改为外部打开：{}", r.message);
            return;
        };
        let replace = s.flags & SVSI_DESELECTOTHERS != 0;
        if replace {
            // 第一个项目：切到标签、关闭盖在面板上的面板、还原并前置窗口（多个项目时其余的只追加选中）。
            // 外壳只在部分情况下替我们前置窗口（原型实测），这里再试一次；系统不允许时只是不前置。
            self.leave_overlays();
            self.select_tab(p, i as i32);
            self.raise_window();
        }
        let Some(item) = s.item else {
            println!("reveal: 已切到标签 {}（没有可选中的文件系统项目）", s.slot);
            return;
        };
        let name = item.file_name().map(OsString::from).unwrap_or_default();
        let shown = name.to_string_lossy().into_owned();
        if std::fs::symlink_metadata(&item).is_err() {
            let m = format!("“{shown}”已不存在（可能已被移动或删除）");
            self.set_message(&m);
            println!("reveal: {m}");
            return;
        }
        if replace {
            self.locate_in(s.slot, name);
        } else {
            self.reveal_add(s.slot, &name);
        }
        self.focus_active();
        println!(
            "reveal: 已在标签 {} 选中“{shown}”（{}）",
            s.slot,
            location::display(&s.folder)
        );
    }

    /// 追加选中（同一请求里的第 2 个及以后的项目）；目录还没读完时忽略。
    fn reveal_add(&self, tab: u64, name: &OsString) {
        let p = {
            let mut ws = self.ws.borrow_mut();
            let Some((p, _)) = ws.locate(tab) else { return };
            let Some(t) = ws.tab_by_id_mut(tab) else {
                return;
            };
            if *t.state() != LoadState::Ready {
                return;
            }
            let Some(i) = t.rows().iter().position(|r| &r.entry.os_name == name) else {
                return;
            };
            if !t.is_selected(i) {
                t.click(i, true, false);
            }
            t.take_changed();
            p
        };
        self.update_panes(&[p]);
        self.sync_global();
    }
}
