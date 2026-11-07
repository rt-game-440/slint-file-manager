//! 搜索引擎：选择后端、失败时回退、产出事件流。
//!
//! 后端选择（ADR 0007）：
//! - 偏好 `Auto` 且有增强后端时先探测：可用就用增强，否则用基本搜索，并给出区分原因的提示
//!   （未安装／未运行／索引未就绪／无法通信），不能一律表现为“无结果”。
//! - 增强查询中途失败：先发 [`Event::Reset`] 让界面丢弃已收到的结果，再用基本搜索重新完成同一个查询，
//!   因此不会混合两套结果，也不会出现假的空列表。
//! - 增强结果按本应用自己的查询规则二次过滤、按范围裁剪、剔除磁盘上已不存在的陈旧索引项
//!   （已移除磁盘的离线历史条目不会伪装成在线文件）。

use super::everything::{Availability, Enhancer, EnhancerError};
use super::walk::{Counters, WalkOpts, walk_roots};
use super::{Cancel, Hit, MAX_HITS, Query, Scope};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Basic,
    Everything,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendPref {
    /// 有可用增强就用，否则基本搜索
    Auto,
    /// 只用基本搜索
    BasicOnly,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub query: Query,
    pub scope: Scope,
    pub pref: BackendPref,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeCode {
    /// 增强未安装
    NotInstalled,
    /// 增强已安装但未运行
    NotRunning,
    /// 增强索引尚未载入完成
    IndexLoading,
    /// 增强无法通信
    Unreachable,
    /// 增强查询中途失败，已回退
    QueryFailed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub code: NoticeCode,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub backend: Option<BackendKind>,
    pub hits: usize,
    pub dirs: u64,
    pub entries: u64,
    pub denied: u64,
    pub cloud_unexpanded: u64,
    pub links_skipped: u64,
    /// 搜索开始时就不可用的根目录（不存在、磁盘已移除、设备未就绪）以及遍历中读取失败的根
    pub roots_unavailable: u64,
    pub roots_denied: u64,
    /// 增强后端返回、但磁盘上已不存在而剔除的陈旧条目
    pub stale_dropped: u64,
    pub truncated: bool,
    pub cancelled: bool,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// 搜索开始，说明采用的后端；回退后会再发一次
    Started {
        backend: BackendKind,
        notice: Option<Notice>,
    },
    /// 丢弃此前收到的全部结果（增强后端失败回退时）
    Reset,
    Hits(Vec<Hit>),
    Progress {
        dirs: u64,
        entries: u64,
    },
    Done(Summary),
}

fn unavailable_notice(a: &Availability, name: &str) -> Option<Notice> {
    let (kind, code, text) = match a {
        Availability::Ready(_) => return None,
        Availability::NotInstalled => (
            NoticeKind::Info,
            NoticeCode::NotInstalled,
            format!("未检测到 {name}，使用基本搜索（安装并运行 {name} 后可获得更快的搜索）"),
        ),
        Availability::NotRunning => (
            NoticeKind::Warn,
            NoticeCode::NotRunning,
            format!("{name} 已安装但未运行，本次使用基本搜索（启动 {name} 后可加速）"),
        ),
        Availability::IndexLoading => (
            NoticeKind::Warn,
            NoticeCode::IndexLoading,
            format!("{name} 的索引尚未就绪，本次使用基本搜索（稍后重试可使用增强）"),
        ),
        Availability::Failed(why) => (
            NoticeKind::Warn,
            NoticeCode::Unreachable,
            format!("无法与 {name} 通信（{why}），本次使用基本搜索"),
        ),
    };
    Some(Notice { kind, code, text })
}

/// 阻塞执行一次搜索。事件通过 `sink` 交给调用方（可能来自多个线程）。结束时恰好发出一个 [`Event::Done`]。
pub fn run(
    req: &Request,
    enhancer: Option<&dyn Enhancer>,
    cancel: &Cancel,
    sink: &(dyn Fn(Event) + Sync),
) {
    let t0 = Instant::now();
    let mut sum = Summary::default();

    // 范围内当前在线的根；其余按“不可用”计数，不能把离线历史伪装成在线
    let all_roots = req.scope.roots();
    let online: Vec<_> = all_roots
        .iter()
        .filter(|r| std::fs::metadata(r).is_ok())
        .cloned()
        .collect();
    sum.roots_unavailable = (all_roots.len() - online.len()) as u64;
    let scope = match &req.scope {
        Scope::Dir(_) => Scope::Dir(online.first().cloned().unwrap_or_default()),
        Scope::AllDisks(_) => Scope::AllDisks(online.clone()),
    };

    let mut notice: Option<Notice> = None;
    if online.is_empty() {
        sink(Event::Started {
            backend: BackendKind::Basic,
            notice: None,
        });
        sum.backend = Some(BackendKind::Basic);
        sum.elapsed_ms = t0.elapsed().as_millis() as u64;
        sink(Event::Done(sum));
        return;
    }

    if req.pref == BackendPref::Auto
        && let Some(e) = enhancer
    {
        let avail = e.probe();
        match &avail {
            Availability::Ready(_) => {
                sink(Event::Started {
                    backend: BackendKind::Everything,
                    notice: None,
                });
                match e.query(&req.query, &scope, MAX_HITS * 4, cancel) {
                    Ok(raw) => {
                        sum.backend = Some(BackendKind::Everything);
                        finish_enhanced(req, &scope, raw, cancel, sink, &mut sum);
                        sum.cancelled = cancel.is_cancelled();
                        sum.elapsed_ms = t0.elapsed().as_millis() as u64;
                        sink(Event::Done(sum));
                        return;
                    }
                    Err(EnhancerError::Cancelled) => {
                        sum.backend = Some(BackendKind::Everything);
                        sum.cancelled = true;
                        sum.elapsed_ms = t0.elapsed().as_millis() as u64;
                        sink(Event::Done(sum));
                        return;
                    }
                    Err(err) => {
                        let why = match err {
                            EnhancerError::Failed(w) => w,
                            EnhancerError::Unavailable(a) => format!("{a:?}"),
                            EnhancerError::Cancelled => unreachable!(),
                        };
                        sink(Event::Reset);
                        notice = Some(Notice {
                            kind: NoticeKind::Warn,
                            code: NoticeCode::QueryFailed,
                            text: format!("{} 查询失败（{why}），已改用基本搜索重新搜索", e.name()),
                        });
                    }
                }
            }
            other => notice = unavailable_notice(other, e.name()),
        }
    }

    {
        sink(Event::Started {
            backend: BackendKind::Basic,
            notice,
        });
        let c = Counters::default();
        walk_roots(
            &online,
            &req.query,
            cancel,
            &WalkOpts::default(),
            &c,
            &|ev| sink(ev),
        );
        sum.backend = Some(BackendKind::Basic);
        sum.hits = (Counters::get(&c.hits) as usize).min(MAX_HITS);
        sum.dirs = Counters::get(&c.dirs);
        sum.entries = Counters::get(&c.entries);
        sum.denied = Counters::get(&c.denied);
        sum.cloud_unexpanded = Counters::get(&c.cloud_unexpanded);
        sum.links_skipped = Counters::get(&c.links_skipped);
        sum.roots_unavailable += Counters::get(&c.roots_unavailable);
        sum.roots_denied = Counters::get(&c.roots_denied);
        sum.truncated = c.truncated.load(Ordering::Relaxed);
        sum.cancelled = cancel.is_cancelled();
        sum.elapsed_ms = t0.elapsed().as_millis() as u64;
        sink(Event::Done(sum));
    }
}

/// 增强结果的二次过滤：范围、本应用的查询规则、磁盘上仍然存在、去重、数量上限。
fn finish_enhanced(
    req: &Request,
    scope: &Scope,
    raw: Vec<Hit>,
    cancel: &Cancel,
    sink: &(dyn Fn(Event) + Sync),
    sum: &mut Summary,
) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut chunk: Vec<Hit> = Vec::new();
    for h in raw {
        if cancel.is_cancelled() {
            break;
        }
        if !scope.contains(&h.path) {
            continue;
        }
        if !req.query.matches(&h.name(), h.is_dir, || {
            h.path.to_string_lossy().into_owned()
        }) {
            continue;
        }
        if !seen.insert(h.path.to_string_lossy().to_lowercase()) {
            continue;
        }
        // 陈旧的索引项（文件已删除、磁盘已移除）不展示
        if std::fs::symlink_metadata(&h.path).is_err() {
            sum.stale_dropped += 1;
            continue;
        }
        if sum.hits >= MAX_HITS {
            sum.truncated = true;
            break;
        }
        sum.hits += 1;
        chunk.push(h);
        if chunk.len() >= 512 {
            sink(Event::Hits(std::mem::take(&mut chunk)));
        }
    }
    if !chunk.is_empty() {
        sink(Event::Hits(chunk));
    }
}

#[cfg(test)]
mod tests {
    use super::super::everything::MockEnhancer;
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join("sfm-unit-search").join(format!(
            "eng-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    struct Out {
        events: Mutex<Vec<Event>>,
    }

    fn go(req: &Request, enh: Option<&dyn Enhancer>, cancel: &Cancel) -> Vec<Event> {
        let out = Out {
            events: Mutex::new(Vec::new()),
        };
        run(req, enh, cancel, &|e| out.events.lock().unwrap().push(e));
        out.events.into_inner().unwrap()
    }

    fn collect(events: &[Event]) -> (Vec<String>, Summary, Vec<Option<Notice>>, usize) {
        let mut hits = Vec::new();
        let mut done = Summary::default();
        let mut notices = Vec::new();
        let mut resets = 0;
        for e in events {
            match e {
                Event::Hits(h) => hits.extend(h.iter().map(|h| h.name())),
                Event::Done(s) => done = s.clone(),
                Event::Started { notice, .. } => notices.push(notice.clone()),
                Event::Reset => {
                    hits.clear();
                    resets += 1;
                }
                Event::Progress { .. } => {}
            }
        }
        hits.sort();
        (hits, done, notices, resets)
    }

    fn req(dir: &Path, q: &str, pref: BackendPref) -> Request {
        Request {
            query: Query::parse(q),
            scope: Scope::Dir(dir.to_path_buf()),
            pref,
        }
    }

    fn setup() -> PathBuf {
        let d = temp("data");
        fs::create_dir_all(d.join("子")).unwrap();
        fs::write(d.join("报告.txt"), b"x").unwrap();
        fs::write(d.join("子").join("报告二.txt"), b"x").unwrap();
        d
    }

    fn hit(p: PathBuf) -> Hit {
        Hit {
            path: p,
            is_dir: false,
        }
    }

    #[test]
    fn exactly_one_done_and_basic_without_enhancer() {
        let d = setup();
        let ev = go(&req(&d, "报告", BackendPref::Auto), None, &Cancel::new());
        let dones = ev.iter().filter(|e| matches!(e, Event::Done(_))).count();
        assert_eq!(dones, 1);
        let (hits, sum, notices, _) = collect(&ev);
        assert_eq!(hits, vec!["报告.txt", "报告二.txt"]);
        assert_eq!(sum.backend, Some(BackendKind::Basic));
        assert_eq!(sum.hits, 2);
        assert!(notices[0].is_none(), "没有增强后端时不提示依赖");
    }

    #[test]
    fn unavailable_states_are_distinguished_and_fall_back_to_basic() {
        let d = setup();
        let cases = [
            (
                Availability::NotInstalled,
                NoticeCode::NotInstalled,
                NoticeKind::Info,
            ),
            (
                Availability::NotRunning,
                NoticeCode::NotRunning,
                NoticeKind::Warn,
            ),
            (
                Availability::IndexLoading,
                NoticeCode::IndexLoading,
                NoticeKind::Warn,
            ),
            (
                Availability::Failed("无响应".into()),
                NoticeCode::Unreachable,
                NoticeKind::Warn,
            ),
        ];
        for (a, code, kind) in cases {
            let m = MockEnhancer::new(a, vec![]);
            let ev = go(
                &req(&d, "报告", BackendPref::Auto),
                Some(&m),
                &Cancel::new(),
            );
            let (hits, sum, notices, resets) = collect(&ev);
            assert_eq!(hits.len(), 2, "{code:?}：基本搜索仍能得到结果");
            assert_eq!(sum.backend, Some(BackendKind::Basic));
            assert_eq!(resets, 0);
            let n = notices[0].clone().expect("应有提示");
            assert_eq!((n.code, n.kind), (code, kind));
            assert_eq!(m.query_count(), 0, "不可用时不发查询");
        }
    }

    #[test]
    fn ready_enhancer_is_used_filtered_and_stale_dropped() {
        let d = setup();
        let gone = d.join("已删除的报告.txt"); // 磁盘上不存在：陈旧索引项
        let outside = temp("outside").join("报告外.txt"); // 范围之外
        fs::write(&outside, b"x").unwrap();
        let m = MockEnhancer::new(
            Availability::Ready("1.4.1".into()),
            vec![
                hit(d.join("报告.txt")),
                hit(d.join("子").join("报告二.txt")),
                hit(d.join("子").join("报告二.txt")), // 重复
                hit(gone),
                hit(outside),
                hit(d.join("子")), // 名称不含“报告”：二次过滤去掉
                hit(PathBuf::from(d.to_string_lossy().to_uppercase()).join("报告.txt")), // 大小写不同的同一文件
            ],
        );
        let ev = go(
            &req(&d, "报告", BackendPref::Auto),
            Some(&m),
            &Cancel::new(),
        );
        let (hits, sum, notices, _) = collect(&ev);
        assert_eq!(sum.backend, Some(BackendKind::Everything));
        assert_eq!(hits, vec!["报告.txt", "报告二.txt"]);
        assert_eq!(sum.stale_dropped, 1);
        assert!(notices[0].is_none());
        assert_eq!(m.query_count(), 1);
        assert!(m.last_query.lock().unwrap().contains("报告"));
    }

    #[test]
    fn enhancer_mid_query_failure_resets_and_falls_back_without_mixing() {
        let d = setup();
        let m = MockEnhancer::new(
            Availability::Ready("1.4.1".into()),
            vec![hit(d.join("报告.txt"))],
        );
        m.fail(Some(EnhancerError::Failed("连接中断".into())));
        let ev = go(
            &req(&d, "报告", BackendPref::Auto),
            Some(&m),
            &Cancel::new(),
        );
        let (hits, sum, notices, resets) = collect(&ev);
        assert_eq!(resets, 1);
        assert_eq!(sum.backend, Some(BackendKind::Basic));
        assert_eq!(
            hits,
            vec!["报告.txt", "报告二.txt"],
            "回退后的结果完整，不含增强的旧结果"
        );
        let started: Vec<_> = ev
            .iter()
            .filter(|e| matches!(e, Event::Started { .. }))
            .collect();
        assert_eq!(started.len(), 2, "增强开始一次、回退开始一次");
        assert_eq!(notices[1].as_ref().unwrap().code, NoticeCode::QueryFailed);
        // Reset 必须出现在回退的 Started 之前
        let pos = |f: &dyn Fn(&Event) -> bool| ev.iter().position(f).unwrap();
        assert!(
            pos(&|e| matches!(e, Event::Reset))
                < pos(&|e| matches!(
                    e,
                    Event::Started {
                        notice: Some(_),
                        ..
                    }
                ))
        );
    }

    #[test]
    fn enhancer_failing_between_probe_and_query_as_unavailable_also_falls_back() {
        let d = setup();
        let m = MockEnhancer::new(Availability::Ready("1".into()), vec![]);
        m.fail(Some(EnhancerError::Unavailable(Availability::NotRunning)));
        let ev = go(
            &req(&d, "报告", BackendPref::Auto),
            Some(&m),
            &Cancel::new(),
        );
        let (hits, sum, _, resets) = collect(&ev);
        assert_eq!(
            (hits.len(), sum.backend, resets),
            (2, Some(BackendKind::Basic), 1)
        );
    }

    #[test]
    fn empty_enhanced_result_is_a_real_empty_result() {
        let d = setup();
        let m = MockEnhancer::new(Availability::Ready("1".into()), vec![]);
        let ev = go(
            &req(&d, "报告", BackendPref::Auto),
            Some(&m),
            &Cancel::new(),
        );
        let (hits, sum, _, resets) = collect(&ev);
        assert_eq!((hits.len(), sum.hits, resets), (0, 0, 0));
        assert_eq!(sum.backend, Some(BackendKind::Everything));
    }

    #[test]
    fn basic_only_ignores_enhancer() {
        let d = setup();
        let m = MockEnhancer::new(Availability::Ready("1".into()), vec![]);
        let ev = go(
            &req(&d, "报告", BackendPref::BasicOnly),
            Some(&m),
            &Cancel::new(),
        );
        let (hits, sum, notices, _) = collect(&ev);
        assert_eq!(hits.len(), 2);
        assert_eq!(sum.backend, Some(BackendKind::Basic));
        assert!(notices[0].is_none());
        assert_eq!(m.query_count(), 0);
    }

    #[test]
    fn cancelled_enhanced_query_reports_cancelled() {
        let d = setup();
        let m = MockEnhancer::new(
            Availability::Ready("1".into()),
            vec![hit(d.join("报告.txt"))],
        );
        let c = Cancel::new();
        c.cancel();
        let ev = go(&req(&d, "报告", BackendPref::Auto), Some(&m), &c);
        let (hits, sum, _, _) = collect(&ev);
        assert!(hits.is_empty() && sum.cancelled);
    }

    #[test]
    fn unavailable_scope_is_reported() {
        let d = temp("gone").join("没有这个目录");
        let ev = go(&req(&d, "x", BackendPref::Auto), None, &Cancel::new());
        let (hits, sum, _, _) = collect(&ev);
        assert!(hits.is_empty());
        assert_eq!(sum.roots_unavailable, 1);
    }

    #[test]
    fn all_disks_with_one_offline_root() {
        let a = setup();
        let offline = PathBuf::from("Z:\\已移除的磁盘");
        let r = Request {
            query: Query::parse("报告"),
            scope: Scope::AllDisks(vec![a, offline]),
            pref: BackendPref::Auto,
        };
        let ev = go(&r, None, &Cancel::new());
        let (hits, sum, _, _) = collect(&ev);
        assert_eq!(hits.len(), 2);
        assert_eq!(sum.roots_unavailable, 1);
    }
}
