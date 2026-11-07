//! 系统缩略图车道（Issue #13）：Shell 提取在系统侧串行、较慢（研究 0005：约 113 ms／张，多线程不增吞吐），
//! 所以单独一条车道，不占用普通图片的工作线程，也不参与首屏计时。
//!
//! - **单工作线程**，优先级与取消语义同原生车道（[`Queue`]）：视口变化只重排队列，作废的排队请求不会开始。
//! - **让路**：开始一个请求前，若原生车道还有排队或在途的请求，最多让 [`LaneCfg::defer`]（默认 300 ms），
//!   让普通图片先出图；超过上限后照常开始，避免被无穷的预取饿死。
//! - **硬超时与替换线程**：一个请求超过 [`LaneCfg::hard_timeout`] 先投递 [`Fail::Timeout`]（界面显示图标），
//!   并换一个新的工作线程继续处理队列，卡住的旧线程无法中断（COM 调用没有取消点），之后若返回成功仍会“迟到补图”。
//!   累计卡住的线程超过 [`LaneCfg::max_stuck`] 个后，本次运行不再向系统提取（其余请求直接按“不支持”回退图标），
//!   防止线程无限泄漏。
//! - **云端占位文件**：只允许 `cache_only`（`SIIGBF_INCACHEONLY`），拿不到返回 [`Fail::Cloud`]，不触发下载。
//! - 结果先投递再写盘缓存；磁盘缓存与原生车道共用，键相同。

use super::cache::DiskCache;
use super::decode::is_cloud_placeholder;
use super::queue::{Job, Queue};
use super::sched::{Counters, Done, Inject, Sink};
use super::shell::ShellProvider;
use super::{Fail, Source, Thumb, mtime_ns};
use std::os::windows::fs::MetadataExt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// 一次执行：结果、是否来自磁盘缓存、需要写入缓存的缩略图与缓存代号。
pub type Executed = (Result<Arc<Thumb>, Fail>, bool, Option<(Arc<Thumb>, u64)>);

/// 执行一个系统缩略图请求：磁盘缓存 → Shell。
pub fn execute_shell(
    src: &Source,
    provider: &dyn ShellProvider,
    disk: Option<&DiskCache>,
) -> Executed {
    let cgen = disk.map(DiskCache::generation);
    if let Some(d) = disk
        && let Some(t) = d.get(src)
    {
        return (Ok(Arc::new(t)), true, None);
    }
    let Ok(meta) = std::fs::metadata(&src.path) else {
        return (Err(Fail::Io), false, None);
    };
    if !meta.is_file() {
        return (Err(Fail::Unsupported), false, None);
    }
    // 云端占位：只查系统缓存，绝不触发提取或下载
    let cloud = is_cloud_placeholder(meta.file_attributes());
    match provider.thumbnail(&src.path, cloud) {
        Ok(t) => {
            // 取图期间文件被改动：照常显示，但不写盘（缓存键对应的是旧身份）
            let identity_ok = std::fs::metadata(&src.path)
                .map(|m| m.len() == src.size && mtime_ns(m.modified().ok()) == src.mtime_ns)
                .unwrap_or(false);
            let t = Arc::new(t);
            let put = match (cgen, identity_ok) {
                (Some(cgen), true) => Some((t.clone(), cgen)),
                _ => None,
            };
            (Ok(t), false, put)
        }
        Err(Fail::Io) => (Err(Fail::Io), false, None),
        Err(_) if cloud => (Err(Fail::Cloud), false, None),
        Err(f) => (Err(f), false, None),
    }
}

#[derive(Clone)]
pub struct LaneCfg {
    pub provider: Arc<dyn ShellProvider>,
    pub disk: Option<Arc<DiskCache>>,
    /// 超过它先投递超时（之后成功仍会补图），并换新线程
    pub hard_timeout: Duration,
    /// 原生车道有活时的最长让路时间
    pub defer: Duration,
    /// 允许累计卡住（被替换）的线程数；超过后不再向系统提取
    pub max_stuck: usize,
    pub inject: Option<Inject>,
}

/// 产品参数：4 秒硬超时（视频首次提取要初始化媒体管线，比图片慢）、让路 300 ms、最多放弃 3 个线程。
pub const HARD_TIMEOUT: Duration = Duration::from_secs(4);
pub const DEFER: Duration = Duration::from_millis(300);
pub const MAX_STUCK: usize = 3;

struct Run {
    id: u64,
    key: u64,
    start: Instant,
    timed_out: bool,
}

struct Inner {
    queue: Queue,
    run: Option<Run>,
    /// 当前有效的工作线程代号；被替换的旧线程返回后只投递迟到结果然后退出
    gen_: u64,
    next_run: u64,
    stuck: usize,
    disabled: bool,
    shutdown: bool,
    c: Counters,
}

struct Shared {
    m: Mutex<Inner>,
    cv: Condvar,
    sink: Sink,
    cfg: LaneCfg,
    /// 原生车道是否还有排队或在途的请求
    native_busy: Box<dyn Fn() -> bool + Send + Sync>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct ShellLane {
    sh: Arc<Shared>,
}

impl ShellLane {
    pub fn new(
        cfg: LaneCfg,
        sink: Sink,
        native_busy: Box<dyn Fn() -> bool + Send + Sync>,
    ) -> ShellLane {
        let sh = Arc::new(Shared {
            m: Mutex::new(Inner {
                queue: Queue::default(),
                run: None,
                gen_: 0,
                next_run: 0,
                stuck: 0,
                disabled: false,
                shutdown: false,
                c: Counters::default(),
            }),
            cv: Condvar::new(),
            sink,
            cfg,
            native_busy,
        });
        spawn_worker(sh.clone(), 0);
        let w = sh.clone();
        let _ = std::thread::Builder::new()
            .name("thumb-shell-watchdog".into())
            .spawn(move || watchdog(w));
        ShellLane { sh }
    }

    /// 替换面板 `pane` 想要的系统缩略图请求。车道已停用时直接按“不支持”回退。
    pub fn set_wanted(&self, pane: usize, jobs: Vec<Job>) {
        let mut disabled_done = Vec::new();
        {
            let mut g = self.sh.lock();
            if g.disabled {
                disabled_done = jobs
                    .iter()
                    .map(|j| Done {
                        key: j.key,
                        outcome: Err(Fail::Unsupported),
                        late: false,
                        from_disk: false,
                    })
                    .collect();
                g.queue.set_wanted(pane, Vec::new(), |_| false);
            } else {
                let running = g.run.as_ref().map(|r| r.key);
                let n = g.queue.set_wanted(pane, jobs, |k| Some(k) == running);
                g.c.cancelled_queued += n as u64;
            }
        }
        for d in disabled_done {
            (self.sh.sink)(d);
        }
        self.sh.cv.notify_all();
    }

    /// 原生解码判定需要系统缩略图的请求（动图 WebP、云占位……）转入本车道。
    pub fn handoff(&self, job: Job) {
        let done = {
            let mut g = self.sh.lock();
            if g.disabled {
                Some(Done {
                    key: job.key,
                    outcome: Err(Fail::Unsupported),
                    late: false,
                    from_disk: false,
                })
            } else {
                g.queue.add(job);
                None
            }
        };
        if let Some(d) = done {
            (self.sh.sink)(d);
        }
        self.sh.cv.notify_all();
    }

    pub fn counters(&self) -> Counters {
        self.sh.lock().c.clone()
    }

    /// 排队中与生成中的请求数。
    pub fn pending(&self) -> (usize, usize) {
        let g = self.sh.lock();
        (g.queue.len(), usize::from(g.run.is_some()))
    }

    /// 已被替换（卡住）的线程数；超过上限后车道停用。
    pub fn stuck(&self) -> usize {
        self.sh.lock().stuck
    }

    pub fn disabled(&self) -> bool {
        self.sh.lock().disabled
    }
}

impl ShellLane {
    /// 通知工作线程与看门狗退出（不 join：卡在系统调用里的线程无法回收，进程退出时一并结束）。
    pub fn shutdown(&self) {
        self.sh.lock().shutdown = true;
        self.sh.cv.notify_all();
    }
}

impl Drop for ShellLane {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn spawn_worker(sh: Arc<Shared>, gen_: u64) {
    let _ = std::thread::Builder::new()
        .name("thumb-shell".into())
        .spawn(move || worker(sh, gen_));
}

fn watchdog(sh: Arc<Shared>) {
    loop {
        std::thread::sleep(Duration::from_millis(20));
        let mut timeout: Option<Done> = None;
        let mut flush: Vec<Done> = Vec::new();
        {
            let mut g = sh.lock();
            if g.shutdown {
                return;
            }
            let hard = sh.cfg.hard_timeout;
            let expired = g
                .run
                .as_ref()
                .is_some_and(|r| !r.timed_out && r.start.elapsed() > hard);
            if expired {
                let key = {
                    let r = g.run.as_mut().expect("在途请求存在");
                    r.timed_out = true;
                    r.key
                };
                g.c.timeouts += 1;
                timeout = Some(Done {
                    key,
                    outcome: Err(Fail::Timeout),
                    late: false,
                    from_disk: false,
                });
                // 换新线程继续处理队列；旧线程卡在系统调用里，返回后自行退出
                g.stuck += 1;
                g.gen_ += 1;
                let new_gen = g.gen_;
                // 在途记录留给旧线程收尾（迟到补图的判定依赖它）；新线程从空闲开始
                g.run = None;
                if g.stuck > sh.cfg.max_stuck {
                    g.disabled = true;
                    flush = drain(&mut g.queue);
                } else {
                    spawn_worker(sh.clone(), new_gen);
                }
            }
        }
        if let Some(d) = timeout {
            (sh.sink)(d);
        }
        for d in flush {
            (sh.sink)(d);
        }
        sh.cv.notify_all();
    }
}

fn drain(q: &mut Queue) -> Vec<Done> {
    let mut v = Vec::new();
    while let Some(j) = q.pop(|_| false) {
        v.push(Done {
            key: j.key,
            outcome: Err(Fail::Unsupported),
            late: false,
            from_disk: false,
        });
    }
    v
}

fn worker(sh: Arc<Shared>, my_gen: u64) {
    loop {
        // 等到有请求
        {
            let mut g = sh.lock();
            loop {
                if g.shutdown || g.gen_ != my_gen {
                    return;
                }
                if !g.queue.is_empty() {
                    break;
                }
                g = sh.cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
        }
        // 让路：原生车道有活时先等一等（有上限），普通图片优先出图
        let t0 = Instant::now();
        while t0.elapsed() < sh.cfg.defer && (sh.native_busy)() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let (run_id, job) = {
            let mut g = sh.lock();
            if g.shutdown || g.gen_ != my_gen {
                return;
            }
            let Some(job) = g.queue.pop(|_| false) else {
                continue;
            };
            let id = g.next_run;
            g.next_run += 1;
            g.c.started += 1;
            g.run = Some(Run {
                id,
                key: job.key,
                start: Instant::now(),
                timed_out: false,
            });
            (id, job)
        };

        if let Some(inject) = &sh.cfg.inject
            && let Some(d) = inject(&job.src.path)
        {
            std::thread::sleep(d);
        }
        let (outcome, from_disk, put) =
            execute_shell(&job.src, sh.cfg.provider.as_ref(), sh.cfg.disk.as_deref());

        let (deliver, stale) = {
            let mut g = sh.lock();
            let stale = g.gen_ != my_gen;
            // 当前线程：在途记录归自己；被替换的旧线程：记录已被清掉，视为“已超时”
            let timed_out = if stale {
                true
            } else {
                let r = g.run.take();
                debug_assert!(r.as_ref().is_some_and(|r| r.id == run_id));
                r.is_some_and(|r| r.timed_out)
            };
            let late = timed_out && outcome.is_ok();
            match &outcome {
                Ok(_) => {
                    g.c.ok += 1;
                    if from_disk {
                        g.c.from_disk += 1;
                    } else {
                        g.c.decoded += 1;
                        g.c.shell_ok += 1;
                    }
                    if late {
                        g.c.late_ready += 1;
                    }
                }
                Err(f) => {
                    if !timed_out {
                        *g.c.failed.entry(f.as_str()).or_insert(0) += 1;
                    }
                }
            }
            // 已报告超时、本次又失败：界面已有超时状态，不再投递
            (
                (!(timed_out && outcome.is_err())).then_some((outcome, late)),
                stale,
            )
        };
        if let Some((outcome, late)) = deliver {
            (sh.sink)(Done {
                key: job.key,
                outcome,
                late,
                from_disk,
            });
        }
        if let (Some((t, cgen)), Some(d)) = (put, &sh.cfg.disk)
            && d.put(&job.src, &t, cgen).is_err()
        {
            sh.lock().c.cache_write_failed += 1;
        }
        sh.cv.notify_all();
        if stale {
            return;
        }
    }
}

/// 把车道的计数合并进原生车道的计数。
pub fn merge_counters(into: &mut Counters, lane: &Counters) {
    into.started += lane.started;
    into.ok += lane.ok;
    for (k, v) in &lane.failed {
        *into.failed.entry(k).or_insert(0) += v;
    }
    into.cancelled_queued += lane.cancelled_queued;
    into.timeouts += lane.timeouts;
    into.late_ready += lane.late_ready;
    into.from_disk += lane.from_disk;
    into.decoded += lane.decoded;
    into.cache_write_failed += lane.cache_write_failed;
    into.shell_ok += lane.shell_ok;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thumbs::queue::rank;
    use crate::thumbs::sched::Done;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, channel};

    /// 替身提供者：按文件名决定行为，并记录 `(文件名, cache_only)`。
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<(String, bool)>>,
        /// 取图时先睡眠的毫秒数（文件名 → 毫秒）
        sleeps: Vec<(String, u64)>,
    }

    impl ShellProvider for Fake {
        fn thumbnail(&self, path: &Path, cache_only: bool) -> Result<Thumb, Fail> {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            self.calls.lock().unwrap().push((name.clone(), cache_only));
            if let Some((_, ms)) = self.sleeps.iter().find(|(n, _)| *n == name) {
                std::thread::sleep(Duration::from_millis(*ms));
            }
            if cache_only {
                return if name.contains("hit") {
                    Ok(Thumb {
                        w: 1,
                        h: 1,
                        rgba: vec![9, 9, 9, 255],
                    })
                } else {
                    Err(Fail::Unsupported)
                };
            }
            if name.starts_with("bad") {
                Err(Fail::Unsupported)
            } else {
                Ok(Thumb {
                    w: 2,
                    h: 1,
                    rgba: vec![1, 2, 3, 255, 4, 5, 6, 255],
                })
            }
        }
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sfm-lane-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn job(dir: &Path, name: &str, pane: usize, r: u8) -> Job {
        let p = dir.join(name);
        std::fs::write(&p, b"x").unwrap();
        let m = std::fs::metadata(&p).unwrap();
        let src = Source::new(p, m.len(), m.modified().ok());
        Job {
            key: src.key(),
            src,
            pane,
            rank: r,
        }
    }

    fn lane(
        p: Arc<dyn ShellProvider>,
        hard_ms: u64,
        defer_ms: u64,
        max_stuck: usize,
        busy: Arc<AtomicBool>,
    ) -> (ShellLane, Receiver<Done>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let sink: Sink = Arc::new(move |d| {
            let _ = tx.lock().unwrap().send(d);
        });
        let l = ShellLane::new(
            LaneCfg {
                provider: p,
                disk: None,
                hard_timeout: Duration::from_millis(hard_ms),
                defer: Duration::from_millis(defer_ms),
                max_stuck,
                inject: None,
            },
            sink,
            Box::new(move || busy.load(Ordering::SeqCst)),
        );
        (l, rx)
    }

    fn recv(rx: &Receiver<Done>, ms: u64) -> Option<Done> {
        rx.recv_timeout(Duration::from_millis(ms)).ok()
    }

    #[test]
    fn executes_and_reports_ok_and_unsupported() {
        let d = dir("basic");
        let (l, rx) = lane(
            Arc::new(Fake::default()),
            2000,
            0,
            3,
            Arc::new(AtomicBool::new(false)),
        );
        let a = job(&d, "a.mp4", 0, rank::VISIBLE_ACTIVE);
        let b = job(&d, "bad.mp4", 0, rank::VISIBLE_ACTIVE);
        let (ka, kb) = (a.key, b.key);
        l.set_wanted(0, vec![a, b]);
        let mut got = [recv(&rx, 2000).unwrap(), recv(&rx, 2000).unwrap()];
        got.sort_by_key(|g| g.key != ka);
        assert_eq!(got[0].key, ka);
        assert!(got[0].outcome.is_ok());
        assert_eq!(got[1].key, kb);
        assert_eq!(got[1].outcome.as_ref().err(), Some(&Fail::Unsupported));
        let c = l.counters();
        assert_eq!((c.ok, c.shell_ok), (1, 1));
        assert_eq!(c.failed.get("unsupported"), Some(&1));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn yields_to_native_lane_but_not_forever() {
        let d = dir("defer");
        let busy = Arc::new(AtomicBool::new(true));
        let (l, rx) = lane(Arc::new(Fake::default()), 2000, 250, 3, busy.clone());
        let t0 = Instant::now();
        l.set_wanted(0, vec![job(&d, "a.mp4", 0, rank::VISIBLE_ACTIVE)]);
        assert!(recv(&rx, 100).is_none(), "原生车道忙时先让路");
        let done = recv(&rx, 1500).expect("让路有上限，之后照常开始");
        assert!(done.outcome.is_ok());
        assert!(
            t0.elapsed() >= Duration::from_millis(240),
            "至少让路接近上限"
        );
        // 原生车道空闲：不让路
        busy.store(false, Ordering::SeqCst);
        let t1 = Instant::now();
        l.set_wanted(0, vec![job(&d, "b.mp4", 0, rank::VISIBLE_ACTIVE)]);
        assert!(recv(&rx, 1000).is_some());
        assert!(t1.elapsed() < Duration::from_millis(200));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn hard_timeout_replaces_thread_and_late_fill() {
        let d = dir("timeout");
        let p = Fake {
            sleeps: vec![("slow.mp4".into(), 600)],
            ..Fake::default()
        };
        let (l, rx) = lane(Arc::new(p), 150, 0, 3, Arc::new(AtomicBool::new(false)));
        let slow = job(&d, "slow.mp4", 0, rank::VISIBLE_ACTIVE);
        let fast = job(&d, "fast.mp4", 0, rank::AHEAD);
        let (ks, kf) = (slow.key, fast.key);
        l.set_wanted(0, vec![slow, fast]);
        let first = recv(&rx, 1000).unwrap();
        assert_eq!(
            (first.key, first.outcome.as_ref().err()),
            (ks, Some(&Fail::Timeout))
        );
        // 卡住的线程被替换：后面的请求不必等它
        let second = recv(&rx, 1000).unwrap();
        assert_eq!(second.key, kf);
        assert!(second.outcome.is_ok() && !second.late);
        // 旧线程最终返回：迟到补图
        let third = recv(&rx, 2000).unwrap();
        assert_eq!(third.key, ks);
        assert!(third.outcome.is_ok() && third.late);
        assert_eq!(l.stuck(), 1);
        let c = l.counters();
        assert_eq!((c.timeouts, c.late_ready), (1, 1));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn disabled_after_too_many_stuck_threads() {
        let d = dir("disable");
        let p = Fake {
            sleeps: (0..4).map(|i| (format!("h{i}.mp4"), 2500)).collect(),
            ..Fake::default()
        };
        let (l, rx) = lane(Arc::new(p), 80, 0, 2, Arc::new(AtomicBool::new(false)));
        // 逐个卡住：超过 max_stuck（2）个后停用
        for i in 0..3 {
            l.set_wanted(
                0,
                vec![job(&d, &format!("h{i}.mp4"), 0, rank::VISIBLE_ACTIVE)],
            );
            let r = recv(&rx, 1500).unwrap();
            assert_eq!(r.outcome.as_ref().err(), Some(&Fail::Timeout));
        }
        assert!(l.disabled(), "卡住线程超过上限后车道停用");
        // 停用后新请求立即按不支持回退，不再调用系统
        l.set_wanted(0, vec![job(&d, "after.mp4", 0, rank::VISIBLE_ACTIVE)]);
        let r = recv(&rx, 500).unwrap();
        assert_eq!(r.outcome.as_ref().err(), Some(&Fail::Unsupported));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn viewport_change_cancels_queued() {
        let d = dir("cancel");
        let p = Fake {
            sleeps: vec![("first.mp4".into(), 200)],
            ..Fake::default()
        };
        let (l, rx) = lane(Arc::new(p), 2000, 0, 3, Arc::new(AtomicBool::new(false)));
        let first = job(&d, "first.mp4", 0, rank::VISIBLE_ACTIVE);
        let gone = job(&d, "gone.mp4", 0, rank::AHEAD);
        let (kf, kg) = (first.key, gone.key);
        l.set_wanted(0, vec![first.clone(), gone]);
        std::thread::sleep(Duration::from_millis(60));
        // first 已在生成，gone 还在排队：新视口不再需要 gone
        l.set_wanted(0, vec![first]);
        let r = recv(&rx, 1000).unwrap();
        assert_eq!(r.key, kf);
        assert!(
            recv(&rx, 400).is_none_or(|d| d.key != kg),
            "作废的排队请求不应开始"
        );
        assert_eq!(l.counters().cancelled_queued, 1);
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn cloud_placeholder_only_queries_cache() {
        let d = dir("cloud");
        let miss = job(&d, "cloud-miss.mp4", 0, rank::VISIBLE_ACTIVE);
        let hit = job(&d, "cloud-hit.mp4", 0, rank::VISIBLE_ACTIVE);
        // FILE_ATTRIBUTE_OFFLINE：与云同步客户端的“仅在线”占位文件同类
        for j in [&miss, &hit] {
            let ok = std::process::Command::new("attrib")
                .arg("+O")
                .arg(&j.src.path)
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok, "设置脱机属性失败");
        }
        let f = Fake::default();
        let (r1, _, put1) = execute_shell(&miss.src, &f, None);
        assert_eq!(
            r1.err(),
            Some(Fail::Cloud),
            "云占位且系统缓存没有：回退图标，不下载"
        );
        assert!(put1.is_none());
        let (r2, _, _) = execute_shell(&hit.src, &f, None);
        assert!(r2.is_ok(), "系统缓存里已有则显示");
        let calls = f.calls.lock().unwrap().clone();
        assert!(
            calls.iter().all(|(_, cache_only)| *cache_only),
            "云占位只允许仅缓存查询"
        );
        // 普通文件走完整提取
        let normal = job(&d, "n.mp4", 0, rank::VISIBLE_ACTIVE);
        let f2 = Fake::default();
        assert!(execute_shell(&normal.src, &f2, None).0.is_ok());
        assert!(!f2.calls.lock().unwrap()[0].1);
        std::fs::remove_dir_all(d).ok();
    }
}
