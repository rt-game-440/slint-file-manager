//! 真实窗口里的原生右键菜单自测（Issue #9）。
//!
//! 与 `harness`（无头、只记录菜单请求）不同，这里启动**真实的 Slint 窗口（winit + 产品渲染器）**，
//! 用真实的 Win32 鼠标／键盘消息（`PostMessage` 到主窗口，经 winit 转成 Slint 事件）呼出**真实的系统菜单**，
//! 再用 `shell_menu::drive` 的菜单自动化（向菜单窗口投递按键）选择命令。覆盖：
//!
//! - 单选／多选／空白处／文件夹／键盘呼出的菜单；顶层与 NanaZip 子菜单的取消；
//! - NanaZip 的压缩、解压、打开压缩包（在 `%TEMP%` 受控样本上，检查产物）；新建文件夹、属性；
//! - 菜单模态循环期间 Slint 事件循环的行为（定时器、跨线程投递、重绘、重入）；
//! - 菜单关闭后的前台窗口、键盘焦点、选择状态与残余状态；
//! - 反复弹出菜单时进程 GDI／USER 对象的增长量；
//! - NanaZip 不可用时的提示项。
//!
//! 用法：`menu-selftest [--out=<目录>] [--gdi-runs=N]`。结果打印到控制台并写入 `<目录>\selftest.json`
//! （默认 `artifacts/issue-9-review`），截图 `NN-*.png` 也在该目录。有失败项时退出码为 1，卡死时 3 分钟后看门狗以 3 退出。
//! 只在 `%TEMP%\sfm-menu-selftest` 里创建文件，不触碰用户文件。程序模拟的消息不等同真人操作（真人验收见 docs/checklist）。

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use i_slint_backend_testing::{AccessibleRole, ElementRoot};
use serde_json::{Value, json};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, Timer, TimerMode};
use slint_file_manager::MainWindow;
use slint_file_manager::app::{self, Controller, Env, Loader, MenuMode, Opener};
use slint_file_manager::shell_menu::drive::{self, Plan, TreeItem};
use slint_file_manager::shell_menu::{
    self, Anchor, MenuError, Options, Outcome, Report, Request, Target,
};

mod w32;

/// 看门狗：脚本在此时间内没有任何进展（含菜单卡死）就强制退出。
const WATCHDOG_SECS: u64 = 180;
static PROGRESS_MS: AtomicU64 = AtomicU64::new(0);

/// 截图清单：(文件名, 内容, 预期效果)。编号为在表中的序号。
const SHOTS: &[(&str, &str, &str)] = &[
    (
        "01-single-file",
        "单选文件 a.txt 后右键：顶层菜单",
        "菜单出现在鼠标位置、压在应用窗口上；a.txt 为选中状态；含系统命令与“NanaZip”子菜单入口",
    ),
    (
        "02-nanazip-submenu",
        "同一菜单展开“NanaZip”子菜单",
        "子菜单在右侧展开，列出添加到 a.7z／a.zip、压缩并邮寄等命令（不是空子菜单）",
    ),
    (
        "03-multi-select",
        "多选 a、b、c 三个文件后右键，高亮 NanaZip 的“添加到 .zip”",
        "三个文件均为选中状态；菜单作用于全部三项；子菜单项文字带压缩包名 sample.zip",
    ),
    (
        "04-archive",
        "压缩包 a.7z 右键，NanaZip 子菜单高亮“提取到 a\\”",
        "压缩包的 NanaZip 子菜单是提取／打开类命令，而不是添加类命令",
    ),
    (
        "05-background-new",
        "文件夹空白处右键，展开“新建”子菜单并高亮“文件夹”",
        "背景菜单含查看／排序／新建／粘贴等；“新建”子菜单有内容；末尾有本应用的“刷新”",
    ),
    (
        "06-folder-open",
        "文件夹 dir1 右键，高亮“打开”",
        "文件夹菜单含“打开”；选择后在本应用内进入该文件夹，不另开资源管理器窗口",
    ),
    (
        "07-nanazip-missing",
        "模拟未安装 NanaZip 的菜单（测试里删除 NanaZip 子菜单）",
        "菜单末尾有灰色提示“未检测到 NanaZip（压缩与解压不可用）”，没有伪造的压缩命令",
    ),
    (
        "08-keyboard-anchor",
        "键盘菜单键呼出（选中 b.txt）",
        "菜单锚定在选中项附近（不是屏幕角落或鼠标远处）",
    ),
    (
        "09-after-menu",
        "多选后菜单关闭（取消）的主窗口",
        "三个文件仍为选中；窗口仍是前台；没有残留的菜单窗口或叠影",
    ),
    (
        "10-properties",
        "对 c.txt 执行“属性”后",
        "属性对话框已打开，应用窗口没有被阻塞（命令是异步执行的）",
    ),
];

type Then = Box<dyn FnOnce(&Rc<T>, &Out)>;

/// 一次菜单的结果。
struct Out {
    report: Option<Report>,
    error: Option<String>,
    tree: Vec<TreeItem>,
    /// 从发出右键到拿到结果的墙钟时间
    wall: Duration,
}

impl Out {
    fn outcome(&self) -> String {
        match (&self.report, &self.error) {
            (Some(r), _) => format!("{:?}", r.outcome),
            (_, Some(e)) => format!("错误：{e}"),
            _ => "无结果".into(),
        }
    }
    fn is(&self, f: impl Fn(&Outcome) -> bool) -> bool {
        self.report.as_ref().is_some_and(|r| f(&r.outcome))
    }
    fn has_item(&self, needle: &str) -> bool {
        let n = needle.to_lowercase();
        self.tree
            .iter()
            .any(|i| !i.separator && i.text.to_lowercase().contains(&n))
    }
    fn count_depth(&self, d: usize) -> usize {
        self.tree
            .iter()
            .filter(|i| i.depth == d && !i.separator)
            .count()
    }
}

type DoFn = Box<dyn FnOnce(&Rc<T>)>;
type CondFn = Box<dyn Fn(&Rc<T>) -> bool>;
type ThenFn = Box<dyn FnOnce(&Rc<T>, bool)>;

enum Act {
    Do(DoFn),
    Menu {
        id: String,
        then: Then,
        since: Option<Instant>,
    },
    Until {
        what: String,
        cond: CondFn,
        then: ThenFn,
        timeout: Duration,
        since: Option<Instant>,
    },
    Sleep(Duration, Option<Instant>),
}

/// 菜单自动化与触发方式。
#[derive(Clone)]
struct Spec {
    path: &'static str,
    select: bool,
    hold_ms: u64,
    hide_nanazip: bool,
    shot: Option<&'static str>,
    /// 菜单显示期间做重入测试
    reentry: bool,
}

impl Spec {
    fn cancel(path: &'static str) -> Spec {
        Spec {
            path,
            select: false,
            hold_ms: 0,
            hide_nanazip: false,
            shot: None,
            reentry: false,
        }
    }
    fn select(path: &'static str) -> Spec {
        Spec {
            select: true,
            ..Spec::cancel(path)
        }
    }
    fn shot(mut self, key: &'static str) -> Spec {
        self.shot = Some(key);
        self
    }
    fn hold(mut self, ms: u64) -> Spec {
        self.hold_ms = ms;
        self
    }
    fn hide(mut self) -> Spec {
        self.hide_nanazip = true;
        self
    }
    fn reentry(mut self) -> Spec {
        self.reentry = true;
        self
    }
}

enum Via {
    Item(&'static str),
    Blank,
    Key,
}

#[derive(Default)]
struct Check {
    id: String,
    name: String,
    ok: bool,
    skipped: bool,
    detail: String,
}

struct MenuRec {
    id: String,
    outcome: String,
    build_ms: u128,
    track_ms: u128,
    wall_ms: u128,
    nanazip: bool,
    stats: String,
    gdi: u32,
    user: u32,
    items: usize,
}

struct T {
    win: MainWindow,
    ctl: Rc<Controller>,
    hwnd: Cell<isize>,
    root: PathBuf,
    out_dir: PathBuf,
    t0: Instant,
    gdi_runs: u32,
    /// 右键走 Slint 的 dispatch_event（默认走真实 Win32 消息）
    slint_input: bool,
    queue: RefCell<VecDeque<Act>>,
    in_tick: Cell<bool>,
    done: Cell<bool>,
    fg_ok: Cell<bool>,
    nanazip: Cell<bool>,
    checks: RefCell<Vec<Check>>,
    menus: RefCell<Vec<MenuRec>>,
    gdi_series: RefCell<Vec<(String, u32, u32)>>,
    shots: RefCell<Vec<(String, String)>>,
    trees: RefCell<Vec<(String, Vec<TreeItem>)>>,
    reentry: RefCell<Vec<String>>,
    injected: RefCell<Option<String>>,
    pre_t: Cell<Option<Instant>>,
    menu_rect: Cell<Option<(i32, i32, i32, i32)>>,
    // 事件循环探测：(相对毫秒, 此时是否有菜单正在显示)
    ticks: Rc<RefCell<Vec<(u128, bool)>>>,
    frames: Rc<RefCell<Vec<(u128, bool)>>>,
    posted: Arc<Mutex<Vec<(u128, u128, bool)>>>,
    probe: RefCell<Value>,
    exit_code: Cell<i32>,
}

fn ms(t0: Instant) -> u128 {
    t0.elapsed().as_millis()
}

impl T {
    fn check(&self, id: &str, name: &str, ok: bool, detail: impl Into<String>) {
        let detail = detail.into();
        println!(
            "[{}] {id} {name}{}",
            if ok { "通过" } else { "失败" },
            if detail.is_empty() {
                String::new()
            } else {
                format!("（{detail}）")
            }
        );
        self.checks.borrow_mut().push(Check {
            id: id.into(),
            name: name.into(),
            ok,
            skipped: false,
            detail,
        });
    }

    fn skip(&self, id: &str, name: &str, why: &str) {
        println!("[跳过] {id} {name}（{why}）");
        self.checks.borrow_mut().push(Check {
            id: id.into(),
            name: name.into(),
            ok: true,
            skipped: true,
            detail: why.into(),
        });
    }

    fn push(&self, a: Act) {
        self.queue.borrow_mut().push_back(a);
    }

    fn step(&self, f: impl FnOnce(&Rc<T>) + 'static) {
        self.push(Act::Do(Box::new(f)));
    }

    fn sleep(&self, d: Duration) {
        self.push(Act::Sleep(d, None));
    }

    fn until(
        &self,
        what: &str,
        timeout: Duration,
        cond: impl Fn(&Rc<T>) -> bool + 'static,
        then: impl FnOnce(&Rc<T>, bool) + 'static,
    ) {
        self.push(Act::Until {
            what: what.into(),
            cond: Box::new(cond),
            then: Box::new(then),
            timeout,
            since: None,
        });
    }

    fn hwnd(&self) -> windows::Win32::Foundation::HWND {
        w32::hwnd(self.hwnd.get())
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    // ---------- 界面查找与输入 ----------

    /// 当前面板里文件项的窗口逻辑坐标中心。
    fn item_center(&self, name: &str) -> Option<(f32, f32)> {
        self.win
            .root_element()
            .query_descendants()
            .match_accessible_role(AccessibleRole::ListItem)
            .find_all()
            .into_iter()
            .find(|e| e.accessible_label().is_some_and(|l| l == name))
            .map(|e| {
                let (p, s) = (e.absolute_position(), e.size());
                (p.x + s.width / 2.0, p.y + s.height / 2.0)
            })
    }

    /// 左面板空白处（文件列表下部）。
    fn blank_point(&self) -> Option<(f32, f32)> {
        let e = self
            .win
            .root_element()
            .query_descendants()
            .match_accessible_role(AccessibleRole::Groupbox)
            .find_all()
            .into_iter()
            .find(|e| e.accessible_label().is_some_and(|l| l == "左面板"))?;
        let (p, s) = (e.absolute_position(), e.size());
        Some((p.x + 80.0, p.y + s.height - 40.0))
    }

    /// Slint 层面的左键点击（选择用；带 Ctrl 时先按下 Ctrl 键事件）。
    fn click_item(&self, name: &str, ctrl: bool) -> bool {
        let Some((x, y)) = self.item_center(name) else {
            return false;
        };
        let w = self.win.window();
        let ctrl_text: slint::SharedString = Key::Control.into();
        if ctrl {
            w.dispatch_event(WindowEvent::KeyPressed {
                text: ctrl_text.clone(),
            });
        }
        let position = slint::LogicalPosition::new(x, y);
        w.dispatch_event(WindowEvent::PointerMoved { position });
        w.dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        w.dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
        if ctrl {
            w.dispatch_event(WindowEvent::KeyReleased { text: ctrl_text });
        }
        true
    }

    /// 真实的 Win32 右键消息（经 winit 转给 Slint）。光标先移到点击处，菜单锚点取光标位置。
    fn post_right_click(&self, x: f32, y: f32) {
        if self.slint_input {
            let w = self.win.window();
            let position = slint::LogicalPosition::new(x, y);
            let s = self.win.window().scale_factor();
            let (sx, sy) = w32::client_to_screen(self.hwnd(), (x * s) as i32, (y * s) as i32);
            w32::set_cursor(sx, sy);
            w.dispatch_event(WindowEvent::PointerMoved { position });
            w.dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Right,
            });
            w.dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Right,
            });
            return;
        }
        let s = self.win.window().scale_factor();
        w32::right_click(self.hwnd(), (x * s).round() as i32, (y * s).round() as i32);
    }

    fn selected(&self) -> Vec<String> {
        self.ctl.selected_in(0)
    }

    fn names(&self) -> Vec<String> {
        self.ctl.names_in(0)
    }

    fn has_name(&self, n: &str) -> bool {
        self.names().iter().any(|x| x == n)
    }

    // ---------- 截图 ----------

    /// 截取主窗口与当前所有菜单窗口（及 `extra` 窗口）的并集。
    fn shot(&self, key: &str, extra: &[isize]) {
        let Some(idx) = SHOTS.iter().position(|s| s.0 == key) else {
            return;
        };
        // 只画本进程的窗口（主窗口、菜单窗口、对话框），不读取屏幕像素，避免带进桌面上其他内容
        let mut hs = vec![self.hwnd()];
        hs.extend(extra.iter().map(|&e| w32::hwnd(e)));
        hs.extend(drive::menu_windows());
        let path = self.out_dir.join(format!("{key}.png"));
        let layer = self.win.window().take_snapshot().ok().map(|b| {
            let mut px = b.as_bytes().to_vec();
            for p in px.as_chunks_mut::<4>().0 {
                p[3] = 255; // 快照是预乘 alpha，主窗口不透明
            }
            (self.hwnd(), b.width(), b.height(), px)
        });
        if layer.is_none() {
            println!("  主窗口快照不可用，截图里只有菜单");
        }
        match w32::capture_windows(&hs, layer)
            .map(|(w, h, px)| (w, h, w32::write_png(&path, w, h, &px)))
        {
            Some((w, h, Ok(()))) => {
                println!("  截图 {}（{w}x{h}）", path.display());
                self.shots
                    .borrow_mut()
                    .push((format!("{:02}", idx + 1), key.into()));
            }
            Some((_, _, Err(e))) => self.check("SHOT", &format!("写入截图 {key}"), false, e),
            None => self.check("SHOT", &format!("截取 {key}"), false, "抓屏失败"),
        }
    }

    // ---------- 菜单步骤 ----------

    /// 触发右键菜单并等待结果。`then` 在菜单关闭、通用检查之后调用。
    fn menu(
        self: &Rc<T>,
        id: &str,
        via: Via,
        spec: Spec,
        then: impl FnOnce(&Rc<T>, &Out) + 'static,
    ) {
        let sid = id.to_string();
        // NanaZip 进度窗口、属性对话框会占走前台：像用户点回应用那样把主窗口提回前台再继续
        self.step(|t| {
            if t.fg_ok.get() && w32::foreground() != t.hwnd.get() {
                w32::force_foreground(t.hwnd());
                t.ctl.focus_active();
                t.queue
                    .borrow_mut()
                    .push_front(Act::Sleep(Duration::from_millis(400), None));
            }
        });
        self.step(move |t0| {
            let spec2 = spec.clone();
            let tt = t0.clone();
            t0.ctl.set_menu_hook(Some(Box::new(move |o: &mut Options| {
                let mut plan = if spec2.select {
                    Plan::select(spec2.path)
                } else {
                    Plan::cancel(spec2.path)
                };
                plan = plan.hold(Duration::from_millis(spec2.hold_ms));
                if spec2.shot.is_some() || spec2.reentry {
                    let (tt, spec3) = (tt.clone(), spec2.clone());
                    plan = plan.ready(move || {
                        if let Some(k) = spec3.shot {
                            tt.shot(k, &[]);
                        }
                        if spec3.reentry {
                            tt.reentry_probe();
                        }
                        if let Some(&m) = drive::menu_windows().first() {
                            let r = w32::window_rect(m);
                            tt.menu_rect.set(Some((r.left, r.top, r.right, r.bottom)));
                        }
                    });
                }
                o.drive = Some(plan);
                o.hide_nanazip = spec2.hide_nanazip;
            })));
            let _ = drive::take_tree();
            t0.menu_rect.set(None);
            t0.pre_t.set(Some(Instant::now()));
            let ok = match via {
                Via::Item(name) => t0
                    .item_center(name)
                    .map(|(x, y)| t0.post_right_click(x, y))
                    .is_some(),
                Via::Blank => t0
                    .blank_point()
                    .map(|(x, y)| t0.post_right_click(x, y))
                    .is_some(),
                Via::Key => {
                    w32::menu_key(t0.hwnd());
                    true
                }
            };
            if !ok {
                *t0.injected.borrow_mut() = Some("找不到要右键的元素".into());
            }
        });
        self.push(Act::Menu {
            id: sid,
            then: Box::new(then),
            since: None,
        });
    }

    /// 菜单显示期间（驱动器的 on_ready，运行在菜单的模态循环里）验证重入被拒绝。
    fn reentry_probe(self: &Rc<T>) {
        let mut log = self.reentry.borrow_mut();
        log.push(format!("is_active={}", shell_menu::is_active()));
        let req = Request {
            target: Target::Items(vec![self.path("a.txt")]),
            anchor: Anchor::Cursor,
            options: Options::default(),
        };
        log.push(format!(
            "再次 show：{}",
            match shell_menu::show(&req) {
                Err(MenuError::Busy) => "被拒绝（Busy）".to_string(),
                Err(e) => format!("其他错误 {e}"),
                Ok(_) => "意外弹出了第二个菜单".to_string(),
            }
        ));
        let before = self.ctl.selected_in(0);
        drop(log);
        self.ctl.context_menu(0, 0, 0.0, 0.0, false);
        let after = self.ctl.selected_in(0);
        let none_queued = self.ctl.take_menu_result().is_none();
        self.reentry.borrow_mut().push(format!(
            "控制器再次请求：选择{}，{}",
            if before == after {
                "未变"
            } else {
                "被改变"
            },
            if none_queued {
                "无新结果"
            } else {
                "产生了新结果"
            }
        ));
    }

    /// 通用后续检查：残余状态、焦点、GDI。
    fn after_menu(self: &Rc<T>, id: &str, o: &Out) {
        let (g, u) = shell_menu::gui_resources();
        self.gdi_series.borrow_mut().push((id.to_string(), g, u));
        if let Some(r) = &o.report {
            self.menus.borrow_mut().push(MenuRec {
                id: id.into(),
                outcome: o.outcome(),
                build_ms: r.build.as_millis(),
                track_ms: r.track.as_millis(),
                wall_ms: o.wall.as_millis(),
                nanazip: r.nanazip,
                stats: format!("{:?}", r.stats),
                gdi: g,
                user: u,
                items: o.tree.iter().filter(|i| !i.separator).count(),
            });
        }
        if !o.tree.is_empty() {
            self.trees
                .borrow_mut()
                .push((id.to_string(), o.tree.clone()));
        }
        let left = shell_menu::leftovers(self.hwnd());
        self.check(id, "菜单结束后无残余状态", left.is_empty(), left.join("、"));
        // 执行了命令的菜单：NanaZip 等会弹出自己的进度窗口并占用前台（窗口随之失去激活），这里只检查取消的菜单
        let cancelled = o
            .report
            .as_ref()
            .is_some_and(|r| r.outcome == Outcome::Cancelled);
        let focus = self.win.get_focus_area().to_string();
        if cancelled {
            self.check(
                id,
                "焦点仍在文件内容区",
                focus == "content",
                format!("focus-area={focus}"),
            );
        }
        if self.fg_ok.get() && cancelled {
            let fg = w32::foreground() == self.hwnd.get();
            self.check(id, "主窗口仍是前台窗口", fg, "");
        }
    }

    fn finish(self: &Rc<T>) {
        if self.done.replace(true) {
            return;
        }
        self.ctl.set_menu_hook(None);
        self.write_report();
        let fails = self.checks.borrow().iter().filter(|c| !c.ok).count();
        self.exit_code.set(if fails == 0 { 0 } else { 1 });
        let _ = slint::quit_event_loop();
    }
}

fn read_bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap_or_default()
}

fn is_7z(b: &[u8]) -> bool {
    b.len() > 32 && b.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C])
}

/// zip 末尾“中央目录结束记录”里的条目总数；文件未写完时没有这条记录。
fn zip_entries(b: &[u8]) -> Option<u16> {
    let i = b.windows(4).rposition(|w| w == [0x50, 0x4B, 0x05, 0x06])?;
    (i + 12 <= b.len()).then(|| u16::from_le_bytes([b[i + 10], b[i + 11]]))
}

/// 用系统自带的 `tar -tf` 列出压缩包内容（独立于 NanaZip 的校验）。
fn tar_list(p: &Path) -> Vec<String> {
    std::process::Command::new("tar")
        .arg("-tf")
        .arg(p)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim_end_matches('/').to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn tick(t: &Rc<T>) {
    PROGRESS_MS.store(ms(t.t0) as u64, Ordering::Relaxed);
    if t.hwnd.get() == 0 {
        match shell_menu::owner_window() {
            Some(h) => {
                t.hwnd.set(h.0 as isize);
                // 放到最上层，不被其他窗口遮挡
                w32::set_topmost(h);
            }
            None => return,
        }
    }
    if shell_menu::is_active() || t.in_tick.replace(true) {
        return;
    }
    loop {
        let act = t.queue.borrow_mut().pop_front();
        let Some(act) = act else {
            t.finish();
            break;
        };
        match act {
            Act::Do(f) => f(t),
            Act::Menu { id, then, since } => {
                let since = since.unwrap_or_else(Instant::now);
                let injected = t.injected.borrow_mut().take();
                let result = injected.map(Err).or_else(|| t.ctl.take_menu_result());
                match result {
                    Some(res) => {
                        let wall = t.pre_t.get().map(|p| p.elapsed()).unwrap_or_default();
                        let (report, error) = match res {
                            Ok(r) => (Some(r), None),
                            Err(e) => (None, Some(e)),
                        };
                        let out = Out {
                            report,
                            error,
                            tree: drive::take_tree(),
                            wall,
                        };
                        t.after_menu(&id, &out);
                        then(t, &out);
                    }
                    None if since.elapsed() > Duration::from_secs(40) => {
                        t.check(
                            &id,
                            "菜单在 40 秒内给出结果",
                            false,
                            "超时，菜单可能没有弹出",
                        );
                    }
                    None => {
                        t.queue.borrow_mut().push_front(Act::Menu {
                            id,
                            then,
                            since: Some(since),
                        });
                        break;
                    }
                }
            }
            Act::Until {
                what,
                cond,
                then,
                timeout,
                since,
            } => {
                let since = since.unwrap_or_else(Instant::now);
                if cond(t) {
                    then(t, true);
                } else if since.elapsed() > timeout {
                    t.check(
                        "等待",
                        &format!("{what}（{} 秒内）", timeout.as_secs()),
                        false,
                        "超时",
                    );
                    then(t, false);
                } else {
                    t.queue.borrow_mut().push_front(Act::Until {
                        what,
                        cond,
                        then,
                        timeout,
                        since: Some(since),
                    });
                    break;
                }
            }
            Act::Sleep(d, since) => {
                let since = since.unwrap_or_else(Instant::now);
                if since.elapsed() < d {
                    t.queue.borrow_mut().push_front(Act::Sleep(d, Some(since)));
                    break;
                }
            }
        }
    }
    t.in_tick.set(false);
}

// ---------------------------------------------------------------------------
// 脚本
// ---------------------------------------------------------------------------

fn script(t: &Rc<T>) {
    let to = Duration::from_secs(30);

    // 0. 等首次加载完成，取得前台
    t.until(
        "首次加载出 a.txt",
        to,
        |t| t.has_name("a.txt"),
        |t, ok| {
            if !ok {
                t.skip("S00", "后续步骤", "窗口没有加载出夹具文件");
                t.queue.borrow_mut().clear();
            }
        },
    );
    t.step(|t| {
        t.ctl.focus_active();
        let fg = w32::force_foreground(t.hwnd());
        t.fg_ok.set(fg);
        let r = w32::window_rect(t.hwnd());
        t.check(
            "S00",
            "主窗口取得前台（否则后续前台检查仅供参考）",
            fg,
            format!(
                "窗口 {},{} - {},{}；缩放 {}；{}",
                r.left,
                r.top,
                r.right,
                r.bottom,
                t.win.window().scale_factor(),
                w32::describe(t.hwnd())
            ),
        );
        let gdi = shell_menu::gui_resources();
        t.gdi_series
            .borrow_mut()
            .push(("启动".into(), gdi.0, gdi.1));
    });
    t.sleep(Duration::from_millis(600));
    t.step(|t| {
        t.ctl.focus_active();
        t.check(
            "S00",
            "窗口激活后键盘焦点在文件内容区",
            t.win.get_focus_area() == "content",
            format!(
                "focus-area={}，Win32 焦点窗口是主窗口：{}",
                t.win.get_focus_area(),
                w32::focus() == t.hwnd.get()
            ),
        );
    });
    t.sleep(Duration::from_millis(200));
    // 诊断：真实 Win32 左键消息能否经 winit 改变选择
    t.step(|t| {
        if let Some((x, y)) = t.item_center("b.txt") {
            let s = t.win.window().scale_factor();
            w32::left_click(t.hwnd(), (x * s) as i32, (y * s) as i32);
        }
    });
    t.sleep(Duration::from_millis(500));
    t.step(|t| {
        t.check(
            "S00",
            "真实 Win32 左键消息经 winit 选中了 b.txt",
            t.selected() == ["b.txt"],
            format!("{:?}", t.selected()),
        );
    });

    // S01 单选右键，顶层菜单，取消
    t.step(|t| {
        t.click_item("a.txt", false);
    });
    t.menu(
        "S01",
        Via::Item("a.txt"),
        Spec::cancel("").shot("01-single-file"),
        |t, o| {
            let r = o.report.as_ref();
            t.check(
                "S01",
                "未选中项右键：先单选再弹菜单，取消返回 Cancelled",
                o.is(|x| *x == Outcome::Cancelled) && t.selected() == ["a.txt"],
                o.outcome(),
            );
            t.check(
                "S01",
                "菜单项数合理（>20）",
                o.tree.iter().filter(|i| !i.separator).count() > 20,
                format!("{} 项", o.tree.iter().filter(|i| !i.separator).count()),
            );
            t.nanazip.set(r.is_some_and(|r| r.nanazip));
            t.check("S01", "检测到 NanaZip 子菜单", t.nanazip.get(), "");
            if let Some(r) = r {
                t.check(
                    "S01",
                    "窗口子类收到并转发了菜单消息",
                    r.stats.menuselect > 0 && r.stats.forwarded_ok > 0,
                    format!("{:?}", r.stats),
                );
            }
        },
    );

    // S02 展开 NanaZip 子菜单后逐层取消
    t.menu(
        "S02",
        Via::Item("a.txt"),
        Spec::cancel("NanaZip").shot("02-nanazip-submenu"),
        |t, o| {
            let n = o.count_depth(1);
            t.check(
                "S02",
                "NanaZip 子菜单已延迟填充（>=5 项）并取消",
                o.is(|x| *x == Outcome::Cancelled) && (n >= 5 || !t.nanazip.get()),
                format!("{n} 个二级项"),
            );
            if let Some(r) = &o.report {
                t.check(
                    "S02",
                    "子菜单展开时收到 WM_INITMENUPOPUP 并转发",
                    !t.nanazip.get() || (r.stats.initmenupopup >= 2 && r.stats.forwarded_ok > 0),
                    format!("{:?}", r.stats),
                );
            }
        },
    );

    // S03 事件循环探测：菜单保持 1.5 秒；期间做重入测试
    t.step(|t| {
        t.probe.replace(Value::Null);
        t.reentry.borrow_mut().clear();
    });
    let t_probe = Rc::new(Cell::new(0u128));
    {
        let tp = t_probe.clone();
        t.step(move |t| tp.set(ms(t.t0)));
    }
    t.menu(
        "S03",
        Via::Item("a.txt"),
        Spec::cancel("").hold(1500).reentry(),
        |t, o| {
            t.check(
                "S03",
                "保持 1.5 秒的菜单正常取消",
                o.is(|x| *x == Outcome::Cancelled),
                o.outcome(),
            );
            let log = t.reentry.borrow().clone();
            t.check(
                "S03",
                "菜单期间再次 show 被拒绝（Busy）",
                log.iter().any(|l| l.contains("被拒绝")),
                log.join("；"),
            );
            t.check(
                "S03",
                "菜单期间控制器收到的再次请求被忽略",
                log.iter()
                    .any(|l| l.contains("选择未变") && l.contains("无新结果")),
                "",
            );
        },
    );
    {
        let tp = t_probe.clone();
        t.sleep(Duration::from_millis(700));
        t.step(move |t| t.analyze_loop(tp.get()));
    }

    // S04 GDI 增长：顶层取消 N 次、子菜单取消 N 次
    t.gdi_scenario();

    // NanaZip 操作
    t.nanazip_scenarios();

    // S09 背景菜单：新建文件夹
    t.step(|t| {
        let _ = t.ctl.selected_in(0);
    });
    t.menu(
        "S09",
        Via::Blank,
        Spec::select("新建>文件夹").shot("05-background-new"),
        |t, o| {
            t.check(
                "S09",
                "空白处右键清空选择，菜单作用于文件夹",
                t.selected().is_empty(),
                "",
            );
            t.check(
                "S09",
                "“新建>文件夹”命令成功返回",
                o.is(|x| matches!(x, Outcome::Invoked { .. })),
                o.outcome(),
            );
            t.check("S09", "背景菜单含本应用的“刷新”", o.has_item("刷新"), "");
        },
    );
    t.until(
        "磁盘上出现新建的文件夹",
        Duration::from_secs(10),
        |t| {
            std::fs::read_dir(&t.root).is_ok_and(|d| {
                d.flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with("新建文件夹"))
            })
        },
        |t, ok| t.check("S09", "产物：新建了“新建文件夹”目录", ok, ""),
    );
    t.until(
        "界面刷新后列出新文件夹",
        Duration::from_secs(10),
        |t| t.names().iter().any(|n| n.starts_with("新建文件夹")),
        |t, ok| {
            t.check("S09", "命令后列表自动刷新并显示新文件夹", ok, "");
        },
    );

    // S10 属性：异步，不阻塞
    t.step(|t| {
        t.click_item("c.txt", false);
    });
    t.menu(
        "S10",
        Via::Item("c.txt"),
        Spec::select("properties"),
        |t, o| {
            t.check(
                "S10",
                "“属性”命令返回 Invoked 且不长时间阻塞",
                o.is(|x| matches!(x, Outcome::Invoked { .. }))
                    && o.report
                        .as_ref()
                        .is_some_and(|r| r.track < Duration::from_secs(8)),
                o.outcome(),
            );
        },
    );
    t.until(
        "出现 c.txt 的属性对话框",
        Duration::from_secs(15),
        |_| w32::find_props_dialog("c.txt").is_some(),
        |t, ok| {
            t.check("S10", "产物：属性对话框已打开（本进程）", ok, "");
            if ok && let Some(h) = w32::find_props_dialog("c.txt") {
                t.shot("10-properties", &[h]);
            }
        },
    );
    {
        let ticks = Rc::new(Cell::new(0usize));
        let tk = ticks.clone();
        t.step(move |t| tk.set(t.ticks.borrow().len()));
        t.sleep(Duration::from_millis(600));
        t.step(move |t| {
            let n = t.ticks.borrow().len() - ticks.get();
            t.check(
                "S10",
                "属性对话框打开期间 Slint 事件循环照常运行（定时器仍在触发）",
                n >= 5,
                format!("600ms 内 {n} 次"),
            );
            if let Some(h) = w32::find_props_dialog("c.txt") {
                w32::close(w32::hwnd(h));
            }
        });
    }
    t.until(
        "属性对话框已关闭",
        Duration::from_secs(10),
        |_| w32::find_props_dialog("c.txt").is_none(),
        |t, ok| {
            t.check("S10", "只关闭了自己打开的属性对话框", ok, "");
        },
    );

    // S11 文件夹：接管“打开”
    t.step(|t| {
        let n = w32::explorer_windows();
        t.probe.borrow_mut()["explorer_before"] = json!(n);
    });
    t.menu(
        "S11",
        Via::Item("dir1"),
        Spec::select("open").shot("06-folder-open"),
        |t, o| {
            t.check(
                "S11",
                "文件夹“打开”由本应用接管",
                o.is(|x| matches!(x, Outcome::Intercepted { .. })),
                o.outcome(),
            );
            t.check(
                "S11",
                "接管后在本应用内进入该文件夹",
                t.ctl.location_in(0).as_deref() == Some(t.path("dir1").as_path()),
                format!("{:?}", t.ctl.location_in(0)),
            );
        },
    );
    t.sleep(Duration::from_millis(800));
    t.step(|t| {
        let before = t.probe.borrow()["explorer_before"].as_u64().unwrap_or(0) as usize;
        let after = w32::explorer_windows();
        t.check(
            "S11",
            "没有另开资源管理器窗口",
            after == before,
            format!("{before} -> {after}"),
        );
        t.ctl.up(0);
    });
    t.until("返回上级", to, |t| t.has_name("a.txt"), |_, _| {});

    // S12 NanaZip 不可用提示
    t.menu(
        "S12",
        Via::Item("a.txt"),
        Spec::cancel("").hide().shot("07-nanazip-missing"),
        |t, o| {
            let hint = o.tree.iter().find(|i| i.text.contains("未检测到 NanaZip"));
            t.check(
                "S12",
                "未安装时追加灰色提示项",
                hint.is_some_and(|h| h.disabled),
                hint.map(|h| h.text.clone()).unwrap_or_default(),
            );
            t.check(
                "S12",
                "未安装时没有 NanaZip 子菜单",
                !o.has_item("添加到") && o.report.as_ref().is_some_and(|r| !r.nanazip),
                "",
            );
        },
    );
    t.until(
        "a.7z 存在时检查压缩包提示",
        Duration::from_secs(1),
        |t| t.has_name("a.7z"),
        |_, _| {},
    );
    t.menu(
        "S12b",
        Via::Item("a.7z"),
        Spec::cancel("").hide(),
        |t, o| {
            t.check(
                "S12b",
                "压缩包的未安装提示指向“打开”交给默认应用",
                o.has_item("压缩包可用"),
                "",
            );
            t.check("S12b", "压缩包菜单仍有“打开”项", o.has_item("打开"), "");
        },
    );

    // S13 键盘菜单键
    t.step(|t| {
        t.click_item("b.txt", false);
    });
    t.menu(
        "S13",
        Via::Key,
        Spec::cancel("").shot("08-keyboard-anchor"),
        |t, o| {
            t.check(
                "S13",
                "菜单键呼出，作用于当前选择且不改变选择",
                o.is(|x| *x == Outcome::Cancelled) && t.selected() == ["b.txt"],
                format!("{} / {:?}", o.outcome(), t.selected()),
            );
            let near = match (t.menu_rect.get(), t.item_center("b.txt")) {
                (Some((l, tp, _, _)), Some((x, y))) => {
                    let s = t.win.window().scale_factor();
                    let (cx, cy) = w32::client_to_screen(t.hwnd(), (x * s) as i32, (y * s) as i32);
                    let d = (((l - cx).pow(2) + (tp - cy).pow(2)) as f64).sqrt();
                    (
                        d < 400.0 * s as f64,
                        format!("菜单左上角距选中项中心 {d:.0}px"),
                    )
                }
                _ => (false, "没有取得菜单位置".into()),
            };
            t.check("S13", "键盘呼出的菜单锚定在选中项附近", near.0, near.1);
        },
    );

    // S14 多选取消后窗口状态（截图）与选择保持
    t.step(|t| {
        t.click_item("a.txt", false);
        t.click_item("b.txt", true);
        t.click_item("c.txt", true);
    });
    t.menu(
        "S14",
        Via::Item("b.txt"),
        Spec::cancel("").hold(200),
        |t, o| {
            t.check(
                "S14",
                "已选项上右键：保持 3 项选择，菜单作用于全部",
                o.is(|x| *x == Outcome::Cancelled) && t.selected().len() == 3,
                format!("{:?}", t.selected()),
            );
            t.shot("09-after-menu", &[]);
        },
    );

    t.step(|t| {
        t.ctl.set_menu_hook(None);
        let (g, u) = shell_menu::gui_resources();
        t.gdi_series.borrow_mut().push(("结束".into(), g, u));
    });
}

impl T {
    fn analyze_loop(&self, t_start: u128) {
        let ticks: Vec<(u128, bool)> = self
            .ticks
            .borrow()
            .iter()
            .cloned()
            .filter(|x| x.0 >= t_start)
            .collect();
        let during = ticks.iter().filter(|x| x.1).count();
        let max_gap = ticks.windows(2).map(|w| w[1].0 - w[0].0).max().unwrap_or(0);
        let frames = self
            .frames
            .borrow()
            .iter()
            .filter(|x| x.0 >= t_start)
            .cloned()
            .collect::<Vec<_>>();
        let frames_during = frames.iter().filter(|x| x.1).count();
        let posted: Vec<(u128, u128, bool)> = self
            .posted
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .filter(|x| x.0 >= t_start)
            .collect();
        let posted_during = posted.iter().filter(|x| x.2).count();
        let max_latency = posted
            .iter()
            .map(|x| x.1.saturating_sub(x.0))
            .max()
            .unwrap_or(0);
        let info = json!({
            "menu_hold_ms": 1500,
            "timer_ticks_while_menu_active": during,
            "frames_rendered_while_menu_active": frames_during,
            "posted_closures_run_while_menu_active": posted_during,
            "max_gap_between_timer_ticks_ms": max_gap,
            "max_invoke_from_event_loop_latency_ms": max_latency,
            "timer_ticks_total_in_window": ticks.len(),
        });
        println!("  事件循环探测：{info}");
        self.check(
            "S03",
            "菜单关闭后事件循环恢复（定时器、跨线程投递照常，无永久卡住）",
            ticks.iter().any(|x| !x.1 && x.0 > t_start + 1500) && max_latency < 5000,
            info.to_string(),
        );
        *self.probe.borrow_mut() = info;
    }

    fn gdi_scenario(self: &Rc<T>) {
        let n = self.gdi_runs;
        // 预热 3 次（首次加载外壳扩展 DLL 的一次性开销不算增长）
        for i in 0..3 {
            self.menu(
                &format!("G-warm{i}"),
                Via::Item("a.txt"),
                Spec::cancel("NanaZip"),
                |_, _| {},
            );
        }
        self.step(|t| {
            let (g, u) = shell_menu::gui_resources();
            t.gdi_series.borrow_mut().push(("top-start".into(), g, u));
        });
        for i in 0..n {
            self.menu(
                &format!("G-top{i}"),
                Via::Item("a.txt"),
                Spec::cancel(""),
                |_, _| {},
            );
        }
        self.step(|t| {
            let (g, u) = shell_menu::gui_resources();
            t.gdi_series.borrow_mut().push(("top-end".into(), g, u));
            t.gdi_series.borrow_mut().push(("sub-start".into(), g, u));
        });
        for i in 0..n {
            self.menu(
                &format!("G-sub{i}"),
                Via::Item("a.txt"),
                Spec::cancel("NanaZip"),
                |_, _| {},
            );
        }
        self.step(|t| {
            let (g, u) = shell_menu::gui_resources();
            t.gdi_series.borrow_mut().push(("sub-end".into(), g, u));
            let find = |k: &str| t.gdi_series.borrow().iter().find(|x| x.0 == k).map(|x| (x.1, x.2)).unwrap_or((0, 0));
            let (ts, te, ss, se) = (find("top-start"), find("top-end"), find("sub-start"), find("sub-end"));
            let n = t.gdi_runs.max(1) as f64;
            let top_gdi = (te.0 as f64 - ts.0 as f64) / n;
            let sub_gdi = (se.0 as f64 - ss.0 as f64) / n;
            let top_user = (te.1 as f64 - ts.1 as f64) / n;
            let sub_user = (se.1 as f64 - ss.1 as f64) / n;
            let summary = json!({
                "runs_each": t.gdi_runs,
                "top_level_cancel": {"gdi_start": ts.0, "gdi_end": te.0, "gdi_per_menu": top_gdi, "user_per_menu": top_user},
                "nanazip_submenu_cancel": {"gdi_start": ss.0, "gdi_end": se.0, "gdi_per_menu": sub_gdi, "user_per_menu": sub_user},
            });
            println!("  GDI 量化：{summary}");
            t.probe.borrow_mut()["gdi"] = summary.clone();
            // 不设“必须为零”的断言：这里记录增长量；判据是在上限 10000 前能弹出多少次菜单（见文档）
            let worst = top_gdi.max(sub_gdi);
            t.check("S04", &format!("GDI 增长已量化（每次菜单最坏 {worst:.2} 个，按 8000 告警线可弹出约 {} 次）", if worst > 0.0 { ((8000.0 - ts.0 as f64) / worst) as i64 } else { -1 }), true, summary.to_string());
        });
    }

    fn nanazip_scenarios(self: &Rc<T>) {
        let to = Duration::from_secs(30);
        let skip_all = |t: &Rc<T>| !t.nanazip.get();

        // S05 多选压缩为 zip
        self.step(|t| {
            t.click_item("a.txt", false);
            t.click_item("b.txt", true);
            t.click_item("c.txt", true);
        });
        self.menu(
            "S05",
            Via::Item("b.txt"),
            Spec::select("NanaZip>.zip\"").shot("03-multi-select"),
            move |t, o| {
                if skip_all(t) {
                    t.skip("S05", "NanaZip 多选压缩", "未检测到 NanaZip");
                    return;
                }
                t.check(
                    "S05",
                    "多选右键保持 3 项选择",
                    t.selected().len() == 3,
                    format!("{:?}", t.selected()),
                );
                let label = match o.report.as_ref().map(|r| &r.outcome) {
                    Some(Outcome::Invoked { label, .. }) => label.clone(),
                    _ => String::new(),
                };
                t.check(
                    "S05",
                    "NanaZip“添加到 .zip”命令成功返回",
                    !label.is_empty() && label.contains(".zip"),
                    format!("{} / {label}", o.outcome()),
                );
            },
        );
        let zip = self.path("sample.zip");
        let zip2 = zip.clone();
        self.until(
            "sample.zip 写完（含中央目录）",
            to,
            move |_| zip_entries(&read_bytes(&zip)) == Some(3),
            move |t, ok| {
                let list = tar_list(&zip2);
                let size = std::fs::metadata(&zip2).map(|m| m.len()).unwrap_or(0);
                t.check(
                    "S05",
                    "产物：sample.zip 为合法 zip，含 3 个条目 a/b/c.txt",
                    ok && ["a.txt", "b.txt", "c.txt"]
                        .iter()
                        .all(|n| list.iter().any(|l| l == n)),
                    format!("{size} 字节；tar -tf：{list:?}"),
                );
            },
        );
        self.until(
            "界面列出 sample.zip",
            to,
            |t| t.has_name("sample.zip"),
            |t, ok| t.check("S05", "命令后列表刷新并显示 sample.zip", ok, ""),
        );

        // S06 单选压缩为 7z
        self.step(|t| {
            t.click_item("a.txt", false);
        });
        self.menu(
            "S06",
            Via::Item("a.txt"),
            Spec::select("NanaZip>添加到 \"a.7z\""),
            move |t, o| {
                if skip_all(t) {
                    t.skip("S06", "NanaZip 单选压缩", "未检测到 NanaZip");
                    return;
                }
                t.check(
                    "S06",
                    "NanaZip“添加到 a.7z”命令成功返回",
                    o.is(|x| matches!(x, Outcome::Invoked { .. })),
                    o.outcome(),
                );
            },
        );
        let p7 = self.path("a.7z");
        let p72 = p7.clone();
        self.until(
            "a.7z 写完",
            to,
            move |_| is_7z(&read_bytes(&p7)),
            move |t, ok| {
                let list = tar_list(&p72);
                let size = std::fs::metadata(&p72).map(|m| m.len()).unwrap_or(0);
                t.check(
                    "S06",
                    "产物：a.7z 有 7z 魔数且内含 a.txt",
                    ok && list.iter().any(|l| l == "a.txt"),
                    format!("{size} 字节；tar -tf：{list:?}"),
                );
            },
        );
        self.until(
            "界面列出 a.7z",
            to,
            |t| t.has_name("a.7z"),
            |t, ok| t.check("S06", "命令后列表刷新并显示 a.7z", ok, ""),
        );

        // S07 解压
        self.step(|t| {
            t.click_item("a.7z", false);
        });
        self.menu(
            "S07",
            Via::Item("a.7z"),
            Spec::select("NanaZip>提取到 \"").shot("04-archive"),
            move |t, o| {
                if skip_all(t) {
                    t.skip("S07", "NanaZip 解压", "未检测到 NanaZip");
                    return;
                }
                t.check(
                    "S07",
                    "压缩包的 NanaZip 子菜单有提取与打开命令",
                    o.has_item("提取") && o.has_item("打开压缩包"),
                    "",
                );
                t.check(
                    "S07",
                    "NanaZip“提取到 a\\”命令成功返回",
                    o.is(|x| matches!(x, Outcome::Invoked { .. })),
                    o.outcome(),
                );
            },
        );
        let ex = self.path("a").join("a.txt");
        let (ex2, src) = (ex.clone(), self.path("a.txt"));
        self.until(
            "a\\a.txt 解压出来",
            to,
            move |_| ex.is_file(),
            move |t, ok| {
                let same = ok && read_bytes(&ex2) == read_bytes(&src);
                t.check("S07", "产物：解压出 a\\a.txt 且内容与原文件一致", same, "");
            },
        );

        // S08 打开压缩包：NanaZip 窗口（独立进程）出现后只关闭它
        let archive = self.path("a.7z");
        self.menu(
            "S08",
            Via::Item("a.7z"),
            Spec::select("NanaZip>打开压缩包"),
            move |t, o| {
                if skip_all(t) {
                    t.skip("S08", "NanaZip 打开压缩包", "未检测到 NanaZip");
                    return;
                }
                t.check(
                    "S08",
                    "NanaZip“打开压缩包”命令成功返回",
                    o.is(|x| matches!(x, Outcome::Invoked { .. })),
                    o.outcome(),
                );
            },
        );
        let key = archive.to_string_lossy().to_string();
        let key2 = key.clone();
        self.until(
            "NanaZip 窗口出现",
            Duration::from_secs(20),
            move |t| !t.nanazip.get() || w32::find_by_title(&key).is_some(),
            move |t, ok| {
                if !t.nanazip.get() {
                    return;
                }
                let w = w32::find_by_title(&key2);
                t.check(
                    "S08",
                    "产物：出现标题含压缩包路径的 NanaZip 窗口，且属于其他进程（进程外承载）",
                    ok && w.as_ref().is_some_and(|w| w.pid != w32::pid()),
                    w.as_ref()
                        .map(|w| format!("pid={} 标题={}", w.pid, w.title))
                        .unwrap_or_default(),
                );
                if let Some(w) = w {
                    w32::close(w32::hwnd(w.hwnd));
                }
            },
        );
        let key3 = self.path("a.7z").to_string_lossy().to_string();
        self.until(
            "NanaZip 窗口已关闭",
            Duration::from_secs(15),
            move |t| !t.nanazip.get() || w32::find_by_title(&key3).is_none(),
            |t, ok| {
                if t.nanazip.get() {
                    t.check("S08", "只关闭了测试打开的那个 NanaZip 窗口", ok, "");
                }
            },
        );
    }
}

// ---------------------------------------------------------------------------
// 报告
// ---------------------------------------------------------------------------

impl T {
    fn write_report(&self) {
        let checks = self.checks.borrow();
        let fails: Vec<_> = checks.iter().filter(|c| !c.ok).collect();
        let skipped = checks.iter().filter(|c| c.skipped).count();
        let cj: Vec<Value> = checks.iter().map(|c| json!({"id": c.id, "name": c.name, "ok": c.ok, "skipped": c.skipped, "detail": c.detail})).collect();
        let mj: Vec<Value> = self.menus.borrow().iter().map(|m| json!({
            "id": m.id, "outcome": m.outcome, "build_ms": m.build_ms, "track_ms": m.track_ms, "wall_ms": m.wall_ms,
            "nanazip": m.nanazip, "stats": m.stats, "gdi": m.gdi, "user": m.user, "items": m.items,
        })).collect();
        let gj: Vec<Value> = self
            .gdi_series
            .borrow()
            .iter()
            .map(|g| json!({"at": g.0, "gdi": g.1, "user": g.2}))
            .collect();
        let sj: Vec<Value> = self
            .shots
            .borrow()
            .iter()
            .map(|s| json!({"no": s.0, "file": format!("{}.png", s.1)}))
            .collect();
        let doc = json!({
            "summary": {"checks": checks.len(), "failed": fails.len(), "skipped": skipped, "seconds": self.t0.elapsed().as_secs(), "nanazip": self.nanazip.get(), "foreground_at_start": self.fg_ok.get()},
            "checks": cj, "menus": mj, "gdi_series": gj, "shots": sj, "probe": *self.probe.borrow(),
        });
        let _ = std::fs::create_dir_all(&self.out_dir);
        let _ = std::fs::write(
            self.out_dir.join("selftest.json"),
            serde_json::to_string_pretty(&doc).unwrap_or_default(),
        );
        // 菜单树（便于核对第三方命令的文字；不含路径）
        let mut tree = String::new();
        for (id, items) in self.trees.borrow().iter() {
            if id.starts_with("G-") {
                continue;
            }
            tree.push_str(&format!("## {id}\n"));
            for i in items {
                let kind = if i.separator {
                    "----"
                } else if i.submenu {
                    "[+]"
                } else {
                    "   "
                };
                tree.push_str(&format!(
                    "{}{kind} {}{}\n",
                    "  ".repeat(i.depth),
                    i.text,
                    if i.disabled { "（灰）" } else { "" }
                ));
            }
            tree.push('\n');
        }
        let _ = std::fs::write(self.out_dir.join("menu-trees.txt"), tree);
        println!(
            "\n共 {} 项检查：失败 {}，跳过 {}；用时 {} 秒；结果 {}",
            checks.len(),
            fails.len(),
            skipped,
            self.t0.elapsed().as_secs(),
            self.out_dir.join("selftest.json").display()
        );
        for f in &fails {
            println!("  失败：{} {} {}", f.id, f.name, f.detail);
        }
    }
}

// ---------------------------------------------------------------------------
// 夹具与入口
// ---------------------------------------------------------------------------

fn make_fixture() -> std::io::Result<PathBuf> {
    let parent = std::env::temp_dir().join("sfm-menu-selftest");
    if parent.exists() {
        std::fs::remove_dir_all(&parent)?;
    }
    let root = parent.join("sample");
    std::fs::create_dir_all(root.join("dir1"))?;
    std::fs::create_dir_all(root.join("empty"))?;
    for (n, body) in [
        ("a.txt", "内容 A\r\nhello from a\r\n"),
        ("b.txt", "内容 B\r\n"),
        ("c.txt", "内容 C\r\n"),
    ] {
        std::fs::write(root.join(n), body)?;
    }
    std::fs::write(root.join("dir1").join("x.txt"), "x\r\n")?;
    Ok(root)
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .find_map(|a| a.strip_prefix(key).map(String::from))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let out_dir =
        PathBuf::from(arg(&args, "--out=").unwrap_or_else(|| "artifacts/issue-9-review".into()));
    let gdi_runs: u32 = arg(&args, "--gdi-runs=")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    std::fs::create_dir_all(&out_dir)?;
    let root = make_fixture()?;
    println!("夹具：{}", root.display());

    let win = MainWindow::new()?;
    let ctl = app::wire(
        &win,
        Env {
            start: Some(root.clone()),
            home: Some(root.clone()),
            loader: Loader::Threaded,
            opener: Opener::Record(Rc::new(RefCell::new(Vec::new()))),
            menu: MenuMode::Shell,
            store: None,
            fresh_split: false,
            live: slint_file_manager::app::LiveEnv::inert(),
            backend: std::sync::Arc::new(slint_file_manager::fileops::SystemBackend),
            search: slint_file_manager::app::SearchEnv::production(),
        },
    );
    win.window()
        .set_size(slint::LogicalSize::new(1000.0, 640.0));
    win.show()?;
    win.window()
        .set_position(slint::PhysicalPosition::new(40, 40));

    let t0 = Instant::now();
    let ticks = Rc::new(RefCell::new(Vec::new()));
    let frames = Rc::new(RefCell::new(Vec::new()));
    let posted = Arc::new(Mutex::new(Vec::new()));
    let t = Rc::new(T {
        win,
        ctl,
        // 事件循环开始后窗口才真正创建，句柄在第一次 tick 里取
        hwnd: Cell::new(0),
        root,
        out_dir,
        t0,
        gdi_runs,
        slint_input: args.iter().any(|a| a == "--slint-input"),
        queue: RefCell::new(VecDeque::new()),
        in_tick: Cell::new(false),
        done: Cell::new(false),
        fg_ok: Cell::new(false),
        nanazip: Cell::new(false),
        checks: RefCell::new(Vec::new()),
        menus: RefCell::new(Vec::new()),
        gdi_series: RefCell::new(Vec::new()),
        shots: RefCell::new(Vec::new()),
        trees: RefCell::new(Vec::new()),
        reentry: RefCell::new(Vec::new()),
        injected: RefCell::new(None),
        pre_t: Cell::new(None),
        menu_rect: Cell::new(None),
        ticks: ticks.clone(),
        frames: frames.clone(),
        posted: posted.clone(),
        probe: RefCell::new(Value::Null),
        exit_code: Cell::new(0),
    });

    // 渲染通知：记录重绘（若渲染器支持）
    {
        let f = frames.clone();
        let r = t.win.window().set_rendering_notifier(move |state, _| {
            if matches!(state, slint::RenderingState::AfterRendering) {
                f.borrow_mut().push((ms(t0), shell_menu::is_active()));
            }
        });
        if let Err(e) = r {
            println!("渲染通知不可用：{e}");
        }
    }
    // 探测定时器：每 50ms 一次，记录时刻与菜单是否正在显示
    let probe_timer = Timer::default();
    probe_timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        ticks.borrow_mut().push((ms(t0), shell_menu::is_active()));
    });
    // 跨线程投递探测：每 40ms 从工作线程 invoke_from_event_loop 一次，记录排队到执行的延迟
    {
        let posted = posted.clone();
        std::thread::spawn(move || {
            loop {
                let p = ms(t0);
                let posted = posted.clone();
                if slint::invoke_from_event_loop(move || {
                    posted
                        .lock()
                        .unwrap()
                        .push((p, ms(t0), shell_menu::is_active()));
                })
                .is_err()
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
        });
    }
    // 看门狗
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let last = PROGRESS_MS.load(Ordering::Relaxed) as u128;
            if ms(t0).saturating_sub(last) > (WATCHDOG_SECS * 1000) as u128 {
                eprintln!("看门狗：{WATCHDOG_SECS} 秒没有进展，疑似菜单或事件循环卡死，强制退出");
                std::process::exit(3);
            }
        }
    });

    script(&t);
    let tt = t.clone();
    let runner = Timer::default();
    runner.start(TimerMode::Repeated, Duration::from_millis(30), move || {
        tick(&tt)
    });
    slint::run_event_loop()?;
    drop(runner);
    drop(probe_timer);
    let code = t.exit_code.get();
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}
