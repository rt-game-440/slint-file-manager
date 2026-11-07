//! 搜索的无头场景与脚本（Issue #11）。命令：`harness search`、`harness search-bench`。
//!
//! - [`showcase_env`] 与 `setup_*`：静态截图场景（名称以 `search-` 开头，登记在 scenarios.rs 的 STATIC），
//!   使用固定的样例目录与模拟增强后端，截图可确定复现。
//! - [`run`]：交互与故障注入脚本。搜索任务在手动模式下排队（`Controller::search_pump` 驱动），所以能确定地停在
//!   “扫描中”并取消；增强后端用 `MockEnhancer` 注入不同状态与故障。全部数据在 `%TEMP%\sfm-harness\scratch\…`。
//! - [`bench`]：基本搜索的真实规模与耗时（不经界面），只输出统计，不记录路径与文件名。

use crate::ctx::{Ctx, Opts, Res};
use crate::fixture;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use serde_json::{Value, json};
use slint::platform::Key;
use slint_file_manager::app::SearchEnv;
use slint_file_manager::browse::places::enumerate_drives;
use slint_file_manager::search::everything::MockEnhancer;
use slint_file_manager::search::{
    Availability, BackendKind, BackendPref, Cancel, Enhancer, EnhancerError, Event, Hit, Query,
    Request, Scope, run as engine_run,
};
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SIZE: (u32, u32) = (1100, 720);

// ---------- 静态场景用的环境 ----------

fn hit(p: PathBuf) -> Hit {
    Hit {
        path: p,
        is_dir: false,
    }
}

/// 展示场景的增强后端：返回样例目录里三个“报告”文件，外加一条已不存在的陈旧索引项。
fn showcase_mock(a: Availability) -> Arc<MockEnhancer> {
    let hits = vec![
        hit(fixture::path("报告2.txt")),
        hit(fixture::path("报告10.txt")),
        hit(fixture::path(fixture::LONG_PDF)),
        hit(fixture::path("报告已删除.txt")),
    ];
    Arc::new(MockEnhancer::new(a, hits))
}

/// 静态场景的搜索环境；非搜索场景返回 `None`（用惰性环境）。
pub fn showcase_env(name: &str) -> Option<SearchEnv> {
    if !name.starts_with("search-") {
        return None;
    }
    let mut env = SearchEnv::inert();
    match name {
        "search-all-disks" => {
            env.all_roots = Some(Rc::new(RefCell::new(vec![
                fixture::root(),
                fixture::scratch_dir("offline-disk"),
            ])));
        }
        "search-enhanced" => {
            env.enhancer = Some(showcase_mock(Availability::Ready(
                "1.4.1.1032（模拟）".into(),
            )));
        }
        "search-fallback-not-running" => {
            env.enhancer = Some(showcase_mock(Availability::NotRunning));
        }
        "search-fallback-failed" => {
            let m = showcase_mock(Availability::Ready("1.4.1.1032（模拟）".into()));
            m.fail(Some(EnhancerError::Failed("等待回复超时".into())));
            env.enhancer = Some(m);
        }
        // 其余场景：没装增强（最常见的真实情况）
        _ => {
            env.enhancer = Some(Arc::new(MockEnhancer::new(
                Availability::NotInstalled,
                vec![],
            )));
        }
    }
    Some(env)
}

fn start_search(c: &Ctx, text: &str) {
    c.ctrl("f");
    c.type_text(text);
    c.special(Key::Return, &[]);
}

pub fn setup_scanning(c: &Ctx) -> Res<()> {
    start_search(c, "报告");
    // 只送达“开始”和第一批结果：搜索仍在进行
    c.ctl.search_pump(2);
    c.advance(16);
    Ok(())
}

pub fn setup_results(c: &Ctx) -> Res<()> {
    start_search(c, "报告");
    c.pump();
    c.special(Key::DownArrow, &[]);
    c.advance(16);
    Ok(())
}

pub fn setup_empty(c: &Ctx) -> Res<()> {
    start_search(c, "不存在的名称zzz");
    c.pump();
    Ok(())
}

pub fn setup_denied(c: &Ctx) -> Res<()> {
    start_search(c, "秘密");
    c.pump();
    Ok(())
}

pub fn setup_all_disks(c: &Ctx) -> Res<()> {
    start_search(c, "报告");
    c.pump();
    c.click(&c.button("全盘")?);
    c.pump();
    Ok(())
}

pub fn setup_no_disks(c: &Ctx) -> Res<()> {
    start_search(c, "报告");
    c.pump();
    c.click(&c.button("全盘")?);
    c.pump();
    Ok(())
}

/// 空闲（未搜索）时在工具栏点“范围：全盘”：范围入口始终可见，全盘段为选中态。
pub fn setup_idle_scope_all(c: &Ctx) -> Res<()> {
    c.click(&c.button("范围：全盘")?);
    c.advance(16);
    Ok(())
}

/// 150% 文字缩放：范围入口折叠成单个切换按钮，点一下切到“全盘”；搜索框仍保留图标与占位文字。
pub fn setup_idle_scope_compact(c: &Ctx) -> Res<()> {
    c.click(&c.button("搜索范围")?);
    c.advance(16);
    Ok(())
}

pub fn setup_cancelled(c: &Ctx) -> Res<()> {
    start_search(c, "报告");
    c.ctl.search_pump(2);
    c.advance(16);
    c.click(&c.button("取消搜索")?);
    c.advance(16);
    Ok(())
}

// ---------- 脚本 ----------

struct Rep {
    items: Vec<Value>,
    fails: Vec<String>,
}

impl Rep {
    fn ok(&mut self, name: &str, cond: bool, detail: impl Into<String>) {
        let detail = detail.into();
        if !cond {
            eprintln!("  未通过：{name}：{detail}");
            self.fails.push(format!("{name}：{detail}"));
        }
        self.items
            .push(json!({"step": name, "passed": cond, "detail": detail}));
    }
}

struct Tree {
    root: PathBuf,
}

impl Tree {
    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

fn put(p: &Path) -> Res<()> {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d)?;
    }
    fs::write(p, b"x")?;
    Ok(())
}

/// 搜索专用目录树：多层目录、混合大小写、中文名、无权限目录、指回根目录的联接点（循环）。
fn build_tree(name: &str) -> Res<Tree> {
    let root = fixture::scratch_dir(name);
    if root.exists() {
        let _ = fixture::icacls(&[&root.join("locked").to_string_lossy(), "/reset", "/T", "/Q"]);
    }
    fixture::fresh_dir(&root)?;
    for f in [
        "alpha/report-2024.txt",
        "alpha/notes.md",
        "alpha/deep/deeper/Report-final.pdf",
        "beta/readme.md",
        "beta/REPORT.docx",
        "图片/海边.jpg",
        "图片/报告封面.png",
        "locked/secret-report.txt",
    ] {
        put(&root.join(f))?;
    }
    let st = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(root.join("loop"))
        .arg(&root)
        .output()?;
    if !st.status.success() {
        return Err("创建联接点失败".into());
    }
    fixture::icacls(&[
        &root.join("locked").to_string_lossy(),
        "/deny",
        "*S-1-1-0:(OI)(CI)(RD)",
        "/Q",
    ])?;
    Ok(Tree { root })
}

fn release_tree(t: &Tree) {
    let _ = fixture::icacls(&[&t.p("locked").to_string_lossy(), "/reset", "/T", "/Q"]);
}

struct Rig {
    c: Ctx,
    mock: Arc<MockEnhancer>,
    roots: Rc<RefCell<Vec<PathBuf>>>,
}

fn rig(start: &Path, availability: Availability, mock_hits: Vec<Hit>) -> Res<Rig> {
    let mock = Arc::new(MockEnhancer::new(availability, mock_hits));
    let roots = Rc::new(RefCell::new(Vec::new()));
    let c = Ctx::open(
        SIZE,
        1.0,
        Opts {
            start: start.to_path_buf(),
            search: Some(SearchEnv {
                manual: true,
                enhancer: Some(mock.clone() as Arc<dyn Enhancer>),
                pref: BackendPref::Auto,
                all_roots: Some(roots.clone()),
                debounce: Duration::from_millis(250),
            }),
            ..Opts::default()
        },
    )?;
    Ok(Rig { c, mock, roots })
}

fn rels(c: &Ctx, root: &Path) -> Vec<String> {
    let mut v: Vec<String> = c
        .ctl
        .search_hit_paths()
        .iter()
        .map(|p| {
            p.strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    v.sort();
    v
}

fn svec(v: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = v.iter().map(|s| s.to_string()).collect();
    v.sort();
    v
}

fn search_row(c: &Ctx, name: &str) -> Res<ElementHandle> {
    c.find_all(AccessibleRole::ListItem, name)
        .into_iter()
        .next()
        .ok_or_else(|| format!("找不到搜索结果行：{name}").into())
}

/// 键入查询并立即搜索（回车），执行并送达全部事件。
fn search_now(c: &Ctx, text: &str) {
    c.ctrl("f");
    c.type_text(text);
    c.special(Key::Return, &[]);
    c.pump();
}

fn base_hits() -> Vec<String> {
    svec(&[
        "alpha/report-2024.txt",
        "alpha/deep/deeper/Report-final.pdf",
        "beta/REPORT.docx",
    ])
}

pub fn run() -> Res<(Vec<Value>, Vec<String>)> {
    let mut r = Rep {
        items: Vec::new(),
        fails: Vec::new(),
    };
    let t = build_tree("search-tree")?;
    let res = (|| -> Res<()> {
        basic_search(&mut r, &t)?;
        incremental_cancel_stale(&mut r, &t)?;
        selection_open_locate(&mut r, &t)?;
        scope_and_disks(&mut r, &t)?;
        stale_results(&mut r, &t)?;
        enhanced_and_faults(&mut r, &t)?;
        Ok(())
    })();
    release_tree(&t);
    res?;
    Ok((r.items, r.fails))
}

/// A. 目录搜索：名称／扩展名／路径、大小写、权限跳过、不进联接点、不读内容、空结果与不可访问的范围。
fn basic_search(r: &mut Rep, t: &Tree) -> Res<()> {
    let g = rig(&t.root, Availability::NotInstalled, vec![])?;
    let c = &g.c;
    // 输入防抖：快速连打只发起一次搜索
    let s0 = c.ctl.search_session();
    c.ctrl("f");
    for ch in "report".chars() {
        c.type_text(&ch.to_string());
        c.advance(50);
    }
    r.ok(
        "A1 连续输入期间不重复发起搜索",
        c.ctl.search_session() == s0 && !c.win.get_search_view().active,
        format!("会话 {} → {}", s0, c.ctl.search_session()),
    );
    c.advance(300);
    c.pump();
    let got = rels(c, &t.root);
    r.ok(
        "A2 名称子串、不区分大小写；不进无权限目录与联接点循环",
        got == base_hits(),
        format!("{got:?}"),
    );
    let snap = c.ctl.search_snapshot();
    let sum = snap.summary.clone().unwrap_or_default();
    r.ok(
        "A3 无权限目录被跳过且计数、联接点未进入",
        sum.denied == 1 && sum.links_skipped >= 1 && snap.phase == "done",
        format!(
            "denied={} links={} phase={}",
            sum.denied, sum.links_skipped, snap.phase
        ),
    );
    r.ok(
        "A4 状态行说明跳过了无权限文件夹",
        snap.status_text.contains("跳过 1 个无权限"),
        snap.status_text.clone(),
    );
    r.ok(
        "A5 范围可见：说明里含具体目录与“全部子文件夹”",
        snap.scope_text.contains(&*t.root.to_string_lossy())
            && snap.scope_text.contains("子文件夹"),
        snap.scope_text.clone(),
    );
    r.ok(
        "A6 增强未安装：用基本搜索，只在能力说明里写明（不弹提示条打扰）",
        snap.backend == Some(BackendKind::Basic)
            && snap.notice_kind == "none"
            && snap.backend_text.contains("未检测到 Everything"),
        format!("{:?} {}", snap.backend, snap.backend_text),
    );

    // 扩展名与路径
    c.special(Key::Escape, &[]);
    search_now(c, ".md");
    r.ok(
        "A7 扩展名查询（.md）",
        rels(c, &t.root) == svec(&["alpha/notes.md", "beta/readme.md"]),
        format!("{:?}", rels(c, &t.root)),
    );
    c.special(Key::Escape, &[]);
    search_now(c, "path:deep");
    r.ok(
        "A8 路径查询（path:deep）匹配路径中的目录名",
        rels(c, &t.root)
            == svec(&[
                "alpha/deep",
                "alpha/deep/deeper",
                "alpha/deep/deeper/Report-final.pdf",
            ]),
        format!("{:?}", rels(c, &t.root)),
    );
    c.special(Key::Escape, &[]);
    search_now(c, "海边 .jpg");
    r.ok(
        "A9 多条件 AND（名称 + 扩展名）",
        rels(c, &t.root) == svec(&["图片/海边.jpg"]),
        format!("{:?}", rels(c, &t.root)),
    );
    c.special(Key::Escape, &[]);
    search_now(c, "nomatch-zzz");
    let snap = c.ctl.search_snapshot();
    r.ok(
        "A10 无匹配：明确显示“没有找到匹配项”且已扫描完毕",
        snap.phase == "empty" && snap.count == 0 && snap.status_text.contains("没有找到匹配项"),
        format!("{} / {}", snap.phase, snap.status_text),
    );
    c.special(Key::Escape, &[]);
    r.ok(
        "A11 Esc 关闭搜索并清空搜索框，焦点回到面板",
        !c.win.get_search_view().active
            && c.win.get_search_text().is_empty()
            && c.win.get_focused_pane() == 0,
        format!("focused={}", c.win.get_focused_pane()),
    );

    // 范围不可访问：目录在搜索前被删除
    let gone = fixture::scratch_dir("search-gone");
    fixture::fresh_dir(&gone)?;
    c.ctl.navigate_path(0, gone.clone());
    c.pump();
    fs::remove_dir_all(&gone)?;
    search_now(c, "x");
    let snap = c.ctl.search_snapshot();
    r.ok(
        "A12 范围目录已不存在：显示“搜索范围不可访问”，不是空结果",
        snap.phase == "unavailable" && snap.status_text.contains("不可访问"),
        format!("{} / {}", snap.phase, snap.status_text),
    );
    c.special(Key::Escape, &[]);
    Ok(())
}

/// B. 增量结果、取消、旧结果不混入新查询。
fn incremental_cancel_stale(r: &mut Rep, t: &Tree) -> Res<()> {
    let g = rig(&t.root, Availability::NotInstalled, vec![])?;
    let c = &g.c;
    c.ctrl("f");
    c.type_text("report");
    c.special(Key::Return, &[]);
    c.ctl.search_pump(2);
    let snap = c.ctl.search_snapshot();
    r.ok(
        "B1 搜索进行中：已有结果且仍显示“扫描中”（增量出结果）",
        snap.phase == "scanning" && snap.count > 0,
        format!("{} {}", snap.phase, snap.count),
    );
    r.ok(
        "B2 扫描中可见取消按钮",
        c.button("取消搜索").is_ok(),
        String::new(),
    );
    c.click(&c.button("取消搜索")?);
    let snap = c.ctl.search_snapshot();
    let before = snap.count;
    r.ok(
        "B3 取消后状态为“已取消”，说明结果不完整",
        snap.phase == "cancelled" && snap.status_text.contains("不完整"),
        format!("{} / {}", snap.phase, snap.status_text),
    );
    c.ctl.search_pump(usize::MAX);
    r.ok(
        "B4 取消后迟到的事件不再改变结果",
        c.ctl.search_snapshot().count == before && c.ctl.search_snapshot().phase == "cancelled",
        format!("{} → {}", before, c.ctl.search_snapshot().count),
    );

    // 改查询：旧会话的迟到事件被丢弃
    c.special(Key::Escape, &[]);
    search_now(c, "report");
    let old = c.ctl.search_session();
    search_now_replace(c, "readme");
    let new_paths = rels(c, &t.root);
    c.ctl
        .search_event(old, Event::Hits(vec![hit(t.p("alpha/report-2024.txt"))]));
    r.ok(
        "B5 旧查询的迟到结果不会混进新查询",
        new_paths == svec(&["beta/readme.md"]) && rels(c, &t.root) == new_paths,
        format!("{new_paths:?} / {:?}", rels(c, &t.root)),
    );
    r.ok(
        "B6 新查询开始时清空旧结果（改查询后第一帧不含旧项）",
        {
            c.ctl.search_pump(usize::MAX);
            c.ctrl("f");
            c.type_text("report");
            c.special(Key::Return, &[]);
            let n = c.ctl.search_snapshot().count;
            n == 0
        },
        "发起搜索后、送达事件前结果应为空".to_string(),
    );
    c.ctl.search_pump(usize::MAX);
    c.special(Key::Escape, &[]);
    Ok(())
}

/// 在已有搜索的搜索框里替换整个查询并立即搜索（全选后输入）。
fn search_now_replace(c: &Ctx, text: &str) {
    c.ctrl("f"); // 聚焦并全选
    c.type_text(text);
    c.special(Key::Return, &[]);
    c.pump();
}

/// C. 选择、默认打开、定位所在目录、目录结果进入、导航离开搜索。
fn selection_open_locate(r: &mut Rep, t: &Tree) -> Res<()> {
    let g = rig(&t.root, Availability::NotInstalled, vec![])?;
    let c = &g.c;
    search_now(c, "report-2024");
    c.click(&search_row(c, "report-2024.txt")?);
    r.ok(
        "C1 点击选中结果，出现“打开”“定位所在目录”",
        c.ctl.search_snapshot().selected == Some(0)
            && c.button("打开").is_ok()
            && c.button("定位所在目录").is_ok(),
        format!("{:?}", c.ctl.search_snapshot().selected),
    );
    c.special(Key::Return, &[]);
    c.advance(16);
    let opened = c.opened.borrow().clone();
    r.ok(
        "C2 回车用默认应用打开选中的文件（记录路径，不真正启动）",
        opened == vec![t.p("alpha/report-2024.txt")],
        format!("{opened:?}"),
    );
    c.click(&c.button("定位所在目录")?);
    c.pump();
    let loc = c.ctl.location();
    r.ok(
        "C3 定位：搜索关闭、进入所在目录并选中该文件",
        !c.win.get_search_view().active
            && loc == Some(t.p("alpha"))
            && c.ctl.selected_names() == vec!["report-2024.txt".to_string()],
        format!("{loc:?} {:?}", c.ctl.selected_names()),
    );
    r.ok(
        "C4 定位后搜索框已清空",
        c.win.get_search_text().is_empty(),
        String::new(),
    );

    // 目录结果：双击进入该目录
    c.ctl.navigate_path(0, t.root.clone());
    c.pump();
    search_now(c, "deeper");
    c.double_click(&search_row(c, "deeper")?);
    c.pump();
    r.ok(
        "C5 双击目录结果：进入该目录并关闭搜索",
        c.ctl.location() == Some(t.p("alpha/deep/deeper")) && !c.win.get_search_view().active,
        format!("{:?}", c.ctl.location()),
    );

    // 导航离开搜索
    c.ctl.navigate_path(0, t.root.clone());
    c.pump();
    search_now(c, "report");
    c.ctl.navigate_path(0, t.p("beta"));
    c.pump();
    r.ok(
        "C6 导航到别处会关闭搜索，不留下过期结果面板",
        !c.win.get_search_view().active && c.win.get_search_text().is_empty(),
        String::new(),
    );

    // 键盘：向下选择
    c.ctl.navigate_path(0, t.root.clone());
    c.pump();
    search_now(c, "report");
    c.special(Key::DownArrow, &[]);
    c.special(Key::DownArrow, &[]);
    r.ok(
        "C7 搜索框内上下键移动结果选择",
        c.ctl.search_snapshot().selected == Some(1),
        format!("{:?}", c.ctl.search_snapshot().selected),
    );
    c.special(Key::Escape, &[]);
    Ok(())
}

/// D. 全盘范围、离线磁盘、范围切换、没有可搜索磁盘。
fn scope_and_disks(r: &mut Rep, t: &Tree) -> Res<()> {
    let g = rig(&t.root, Availability::NotInstalled, vec![])?;
    let c = &g.c;
    let offline = fixture::scratch_dir("search-offline-disk");
    let _ = fs::remove_dir_all(&offline);
    *g.roots.borrow_mut() = vec![t.root.clone(), offline.clone()];
    search_now(c, "report");
    c.click(&c.button("全盘")?);
    c.pump();
    let snap = c.ctl.search_snapshot();
    let sum = snap.summary.clone().unwrap_or_default();
    r.ok(
        "D1 一键全盘：搜索全部根目录，范围说明列出磁盘且声明不读内容",
        snap.scope_all
            && rels(c, &t.root) == base_hits()
            && snap.scope_text.contains("全部磁盘")
            && snap.scope_text.contains("不读取文件内容"),
        format!("{} {:?}", snap.scope_text, rels(c, &t.root)),
    );
    r.ok(
        "D2 离线／不可访问的磁盘被计数并在状态行说明，结果不冒充完整",
        sum.roots_unavailable == 1 && snap.status_text.contains("个位置不可访问"),
        format!("unavailable={} {}", sum.roots_unavailable, snap.status_text),
    );
    c.click(&c.button("当前目录及子目录")?);
    c.pump();
    let snap = c.ctl.search_snapshot();
    r.ok(
        "D3 切回目录范围：重新搜索，不再报告不可访问磁盘",
        !snap.scope_all
            && snap
                .summary
                .as_ref()
                .is_some_and(|s| s.roots_unavailable == 0)
            && rels(c, &t.root) == base_hits(),
        format!("{} {:?}", snap.status_text, rels(c, &t.root)),
    );
    // 磁盘列表变化：全盘搜索按发起时的磁盘为准，之后再搜才包含新磁盘
    g.roots.borrow_mut().clear();
    c.click(&c.button("全盘")?);
    c.pump();
    let snap = c.ctl.search_snapshot();
    r.ok(
        "D4 没有可搜索的磁盘：显示“不可访问”而不是空结果",
        snap.phase == "unavailable" && snap.status_text.contains("没有识别到可用的磁盘"),
        format!("{} {}", snap.phase, snap.status_text),
    );
    c.special(Key::Escape, &[]);
    // D5 空闲状态的范围入口（Issue #40）：未开始搜索时工具栏就有“当前目录／全盘”，点选只记录范围，不触发搜索
    let snap = c.ctl.search_snapshot();
    r.ok(
        "D5a 关闭搜索后范围选择保留，面板已收起",
        !snap.active && snap.scope_all,
        format!("active={} all={}", snap.active, snap.scope_all),
    );
    c.click(&c.button("范围：当前目录及子目录")?);
    let a = c.ctl.search_snapshot();
    let dir_checked = c.button("范围：当前目录及子目录")?.accessible_checked();
    c.click(&c.button("范围：全盘")?);
    let b = c.ctl.search_snapshot();
    let all_checked = c.button("范围：全盘")?.accessible_checked();
    r.ok(
        "D5b 空闲时点选“当前目录／全盘”：范围切换且勾选态同步，不开始搜索",
        !a.active
            && !a.scope_all
            && dir_checked == Some(true)
            && !b.active
            && b.scope_all
            && all_checked == Some(true),
        format!(
            "a={}/{} b={}/{} {:?} {:?}",
            a.active, a.scope_all, b.active, b.scope_all, dir_checked, all_checked
        ),
    );
    *g.roots.borrow_mut() = vec![t.root.clone()];
    search_now(c, "report");
    let snap = c.ctl.search_snapshot();
    r.ok(
        "D5c 空闲时选好的“全盘”在随后的搜索里生效",
        snap.active && snap.scope_all && snap.scope_text.contains("全部磁盘"),
        format!("{} {}", snap.active, snap.scope_text),
    );
    c.special(Key::Escape, &[]);
    Ok(())
}

/// E. 结果过期：文件在搜索后被删除，打开时识别并标注，不假装成功。
fn stale_results(r: &mut Rep, t: &Tree) -> Res<()> {
    let g = rig(&t.root, Availability::NotInstalled, vec![])?;
    let c = &g.c;
    search_now(c, "report");
    fs::remove_file(t.p("beta/REPORT.docx"))?;
    c.click(&search_row(c, "REPORT.docx")?);
    c.special(Key::Return, &[]);
    c.advance(16);
    r.ok(
        "E1 打开已被删除的结果：不打开任何程序",
        c.opened.borrow().is_empty(),
        format!("{:?}", c.opened.borrow()),
    );
    let snap = c.ctl.search_snapshot();
    r.ok(
        "E2 该行标为已过期，状态行说明原因",
        c.ctl.search_missing_rows() == 1 && snap.status_text.contains("已不存在"),
        format!("{} {}", c.ctl.search_missing_rows(), snap.status_text),
    );
    c.special(Key::Escape, &[]);
    put(&t.p("beta/REPORT.docx"))?;
    Ok(())
}

/// F. 增强后端：就绪时的结果过滤，以及未安装／未运行／索引加载中／通信失败／查询中途失败的区分与回退。
fn enhanced_and_faults(r: &mut Rep, t: &Tree) -> Res<()> {
    let mock_hits = vec![
        hit(t.p("alpha/report-2024.txt")),
        hit(t.p("alpha/notes.md")),           // 不匹配查询：必须被二次过滤
        hit(t.p("alpha/report-deleted.txt")), // 陈旧索引项：必须被剔除
        hit(fixture::scratch_dir("elsewhere").join("report.txt")), // 范围之外
    ];
    let g = rig(
        &t.root,
        Availability::Ready("1.4.1（模拟）".into()),
        mock_hits,
    )?;
    let c = &g.c;
    search_now(c, "report");
    let snap = c.ctl.search_snapshot();
    let sum = snap.summary.clone().unwrap_or_default();
    r.ok(
        "F1 增强就绪：采用增强，结果按本应用规则二次过滤（不匹配、范围外、已不存在的都不显示）",
        snap.backend == Some(BackendKind::Everything)
            && rels(c, &t.root) == svec(&["alpha/report-2024.txt"])
            && sum.stale_dropped == 1,
        format!(
            "{:?} {:?} stale={}",
            snap.backend,
            rels(c, &t.root),
            sum.stale_dropped
        ),
    );
    r.ok(
        "F2 能力说明标明“Everything（增强）”，且没有降级提示",
        snap.backend_text.contains("Everything") && snap.notice_kind == "none",
        format!("{} / {}", snap.backend_text, snap.notice_kind),
    );
    r.ok(
        "F3 增强只被查询一次",
        g.mock.query_count() == 1,
        format!("{}", g.mock.query_count()),
    );

    let mut texts = HashSet::new();
    let cases: [(&str, Availability, &str); 4] = [
        ("未安装", Availability::NotInstalled, "none"),
        ("未运行", Availability::NotRunning, "warn"),
        ("索引加载中", Availability::IndexLoading, "warn"),
        ("通信失败", Availability::Failed("无响应".into()), "warn"),
    ];
    for (label, a, kind) in cases {
        g.mock.set_availability(a);
        c.special(Key::Escape, &[]);
        search_now(c, "report");
        let snap = c.ctl.search_snapshot();
        r.ok(
            &format!("F4 增强{label}：回退基本搜索，结果完整且不混入增强数据"),
            snap.backend == Some(BackendKind::Basic) && rels(c, &t.root) == base_hits(),
            format!("{:?} {:?}", snap.backend, rels(c, &t.root)),
        );
        r.ok(
            &format!("F5 增强{label}：给出对应级别的提示"),
            snap.notice_kind == kind && (kind == "none" || !snap.notice_text.is_empty()),
            format!(
                "{} {} {}",
                snap.notice_kind, snap.backend_text, snap.notice_text
            ),
        );
        texts.insert(format!("{}|{}", snap.backend_text, snap.notice_text));
    }
    r.ok(
        "F6 四种不可用状态的提示文字互不相同（可区分）",
        texts.len() == 4,
        format!("{}", texts.len()),
    );

    // 查询中途失败：回退，不显示“增强的空结果”，也不混入增强数据
    g.mock
        .set_availability(Availability::Ready("1.4.1（模拟）".into()));
    g.mock
        .fail(Some(EnhancerError::Failed("等待回复超时".into())));
    c.special(Key::Escape, &[]);
    search_now(c, "report");
    let snap = c.ctl.search_snapshot();
    r.ok(
        "F7 增强查询中途失败：回退基本搜索并给出“查询失败”提示，结果为基本搜索的完整结果",
        snap.backend == Some(BackendKind::Basic)
            && rels(c, &t.root) == base_hits()
            && snap.notice_kind == "warn"
            && snap.notice_text.contains("超时"),
        format!(
            "{:?} {:?} {}",
            snap.backend,
            rels(c, &t.root),
            snap.notice_text
        ),
    );
    r.ok(
        "F8 回退后不显示“没有找到匹配项”的假空结果",
        snap.phase == "done" && snap.count == 3,
        format!("{} {}", snap.phase, snap.count),
    );
    g.mock.fail(None);
    c.special(Key::Escape, &[]);
    Ok(())
}

// ---------- 规模与耗时（只输出统计） ----------

#[derive(Default, Clone)]
struct Stat {
    first_hit_ms: Option<u64>,
    done_ms: u64,
    hits: usize,
    dirs: u64,
    entries: u64,
    denied: u64,
    cloud: u64,
    links: u64,
    unavailable: u64,
    truncated: bool,
}

fn measure(scope: Scope, query: &str) -> Stat {
    let req = Request {
        query: Query::parse(query),
        scope,
        pref: BackendPref::BasicOnly,
    };
    let t0 = Instant::now();
    let first = Mutex::new(None::<u64>);
    let hits = Mutex::new(0usize);
    let sum = Mutex::new(None);
    engine_run(&req, None, &Cancel::new(), &|e| match e {
        Event::Hits(h) => {
            let mut f = first.lock().unwrap();
            if f.is_none() && !h.is_empty() {
                *f = Some(t0.elapsed().as_millis() as u64);
            }
            *hits.lock().unwrap() += h.len();
        }
        Event::Done(s) => *sum.lock().unwrap() = Some(s),
        _ => {}
    });
    let done_ms = t0.elapsed().as_millis() as u64;
    let s = sum.into_inner().unwrap().unwrap_or_default();
    Stat {
        first_hit_ms: first.into_inner().unwrap(),
        done_ms,
        hits: hits.into_inner().unwrap(),
        dirs: s.dirs,
        entries: s.entries,
        denied: s.denied,
        cloud: s.cloud_unexpanded,
        links: s.links_skipped,
        unavailable: s.roots_unavailable,
        truncated: s.truncated,
    }
}

fn stat_json(label: &str, scope_kind: &str, s: &Stat, q: &str) -> Value {
    json!({
        "label": label, "scope": scope_kind, "query_len": q.chars().count(),
        "first_hit_ms": s.first_hit_ms, "done_ms": s.done_ms, "hits": s.hits,
        "dirs": s.dirs, "entries": s.entries, "denied_dirs": s.denied,
        "cloud_unexpanded": s.cloud, "links_skipped": s.links, "roots_unavailable": s.unavailable,
        "truncated": s.truncated,
        "entries_per_sec": if s.done_ms > 0 { (s.entries as f64 * 1000.0 / s.done_ms as f64).round() } else { 0.0 },
    })
}

/// 基本搜索的真实规模与耗时。`dir` 为目录范围；`all` 为全部本机磁盘；每项重复 `repeat` 次（第一次通常是冷缓存）。
/// 只输出统计数字，不记录任何路径或文件名。
pub fn bench(dir: Option<PathBuf>, all: bool, query: &str, repeat: usize) -> Res<Vec<Value>> {
    let mut out = Vec::new();
    if let Some(d) = dir {
        for i in 0..repeat {
            let s = measure(Scope::Dir(d.clone()), query);
            let label = format!("目录范围 第{}次", i + 1);
            println!(
                "{label}：{} 个文件夹、{} 个条目，命中 {}，首个结果 {:?}ms，总耗时 {}ms",
                s.dirs, s.entries, s.hits, s.first_hit_ms, s.done_ms
            );
            out.push(stat_json(&label, "dir", &s, query));
        }
    }
    if all {
        let roots: Vec<PathBuf> = enumerate_drives()
            .into_iter()
            .filter(|d| d.ready())
            .map(|d| d.root)
            .collect();
        println!("全盘范围：{} 个磁盘根目录", roots.len());
        for i in 0..repeat {
            let s = measure(Scope::AllDisks(roots.clone()), query);
            let label = format!("全盘范围 第{}次（{} 个磁盘）", i + 1, roots.len());
            println!(
                "{label}：{} 个文件夹、{} 个条目，命中 {}，首个结果 {:?}ms，总耗时 {}ms，无权限 {} 个文件夹，云文件夹未展开 {}",
                s.dirs, s.entries, s.hits, s.first_hit_ms, s.done_ms, s.denied, s.cloud
            );
            out.push(stat_json(&label, "all", &s, query));
        }
    }
    Ok(out)
}
