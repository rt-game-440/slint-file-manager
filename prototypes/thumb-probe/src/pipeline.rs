//! 缩略图调度链路（原型）：可见项优先、视口变化时取消排队中的旧请求、单个慢文件不占住全部工作线程。
//!
//! 形状与 #10 要接入的一致：界面线程只做“告诉服务当前目录与可见范围”和“轮询结果”，两者都只持锁做内存操作；
//! 解码在后台线程；结果带 `epoch`（目录代号）与 `key`（文件身份），界面应用前必须核对。
//!
//! 取消语义：
//! - 排队中的请求：`set_viewport` 用新视口重建队列，不在新视口（含预取）内的立即作废，不会开始。
//! - 已经在解码的请求：无法中断（libwebp 没有取消点），完成后照常入内存缓存，但标记为“视口外完成”；
//!   换目录（`enter_dir`）后它们的结果带旧 epoch，界面丢弃。
//! - 慢文件：运行超过 `slow_after` 时补一个临时工作线程（上限 `max_extra`），运行超过 `hard_timeout` 时向界面报告
//!   `Fail::Timeout`（显示回退图标）；它之后若真的完成，作为“迟到成功”再投递一次。

use crate::cache::DiskCache;
use crate::common::{Fail, Limits, Thumb, Timing};
use crate::decode::{self, Route};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct Item {
    pub path: PathBuf,
    /// 文件身份：路径 + 大小 + 修改时间 + 缩略图边长的哈希
    pub key: u64,
}

#[derive(Clone)]
pub struct Config {
    pub workers: usize,
    pub size: u32,
    pub route: Route,
    pub limits: Limits,
    pub slow_after: Duration,
    pub hard_timeout: Duration,
    pub max_extra: usize,
    /// 可见范围之后（以及之前一半）预取的项数
    pub prefetch: usize,
    /// 慢文件注入：目录内下标 → 额外延迟（模拟慢盘／网络位置）
    pub inject: HashMap<usize, Duration>,
    pub disk: Option<Arc<DiskCache>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Memory,
    Disk,
    Decoded,
}

pub struct Done {
    pub epoch: u64,
    pub index: usize,
    pub key: u64,
    pub outcome: Result<Arc<Thumb>, Fail>,
    pub source: Source,
    pub timing: Timing,
    /// 超时之后才完成的成功结果
    pub late: bool,
    pub at: Instant,
}

#[derive(Default, Clone, Debug)]
pub struct Counters {
    pub started: u64,
    pub started_unwanted: u64,
    pub ok: u64,
    pub failed: HashMap<&'static str, u64>,
    pub cancelled_queued: u64,
    pub stale_inflight: u64,
    pub completed_out_of_view: u64,
    pub extra_workers_spawned: u64,
    pub timeouts: u64,
    pub late_ready: u64,
    pub from_memory: u64,
    pub from_disk: u64,
    pub decoded: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum St {
    Idle,
    Queued,
    Running,
    Ready,
    Failed,
}

struct Run {
    epoch: u64,
    index: usize,
    start: Instant,
    slow: bool,
    timed_out: bool,
    stale_counted: bool,
}

struct Inner {
    epoch: u64,
    items: Arc<Vec<Item>>,
    status: Vec<St>,
    wanted: Vec<bool>,
    queue: VecDeque<usize>,
    runs: HashMap<u64, Run>,
    next_job: u64,
    live_workers: usize,
    slow_running: usize,
    extra_live: usize,
    shutdown: bool,
    c: Counters,
}

struct Shared {
    m: Mutex<Inner>,
    cv: Condvar,
    tx: Sender<Done>,
    mem: Mutex<HashMap<u64, Arc<Thumb>>>,
    /// 已投递但磁盘缓存尚未写完的条目数（进程退出前要等它归零）
    pending_puts: AtomicUsize,
    cfg: Config,
}

pub struct Service {
    sh: Arc<Shared>,
}

impl Service {
    pub fn new(cfg: Config) -> (Service, Receiver<Done>) {
        let (tx, rx) = channel();
        let sh = Arc::new(Shared {
            m: Mutex::new(Inner {
                epoch: 0,
                items: Arc::new(Vec::new()),
                status: Vec::new(),
                wanted: Vec::new(),
                queue: VecDeque::new(),
                runs: HashMap::new(),
                next_job: 0,
                live_workers: 0,
                slow_running: 0,
                extra_live: 0,
                shutdown: false,
                c: Counters::default(),
            }),
            cv: Condvar::new(),
            tx,
            mem: Mutex::new(HashMap::new()),
            pending_puts: AtomicUsize::new(0),
            cfg,
        });
        {
            let mut g = sh.m.lock().unwrap();
            g.live_workers = sh.cfg.workers;
        }
        for _ in 0..sh.cfg.workers {
            spawn_worker(sh.clone());
        }
        let w = sh.clone();
        std::thread::spawn(move || watchdog(w));
        (Service { sh }, rx)
    }

    /// 进入新目录：新 epoch，作废全部排队请求。运行中的旧请求结果会带旧 epoch，由界面丢弃。
    pub fn enter_dir(&self, items: Vec<Item>) -> u64 {
        let mut g = self.sh.m.lock().unwrap();
        g.epoch += 1;
        let queued = g.queue.len() as u64;
        g.c.cancelled_queued += queued;
        g.queue.clear();
        let n = items.len();
        g.items = Arc::new(items);
        g.status = vec![St::Idle; n];
        g.wanted = vec![false; n];
        g.epoch
    }

    /// 可见范围 `[first, last)` 变化（滚动、缩放窗口）。返回命中内存缓存而立即可用的项数。
    pub fn set_viewport(&self, epoch: u64, first: usize, last: usize) -> usize {
        let mut immediate: Vec<Done> = Vec::new();
        {
            let mut g = self.sh.m.lock().unwrap();
            if g.epoch != epoch {
                return 0;
            }
            let n = g.items.len();
            let (first, last) = (first.min(n), last.min(n));
            let pf = self.sh.cfg.prefetch;
            // 优先级：可见（自上而下）→ 其后预取 → 其前预取（由近及远）
            let mut order: Vec<usize> = (first..last).collect();
            order.extend(last..(last + pf).min(n));
            order.extend((first.saturating_sub(pf / 2)..first).rev());
            for w in g.wanted.iter_mut() {
                *w = false;
            }
            for &i in &order {
                g.wanted[i] = true;
            }
            let old: Vec<usize> = g.queue.drain(..).collect();
            let mut newq = VecDeque::new();
            for &i in &order {
                match g.status[i] {
                    St::Idle | St::Queued => {
                        let key = g.items[i].key;
                        let hit = self.sh.mem.lock().unwrap().get(&key).cloned();
                        if let Some(t) = hit {
                            g.status[i] = St::Ready;
                            g.c.from_memory += 1;
                            g.c.ok += 1;
                            immediate.push(Done {
                                epoch,
                                index: i,
                                key,
                                outcome: Ok(t),
                                source: Source::Memory,
                                timing: Timing::default(),
                                late: false,
                                at: Instant::now(),
                            });
                        } else {
                            g.status[i] = St::Queued;
                            newq.push_back(i);
                        }
                    }
                    _ => {}
                }
            }
            for i in old {
                if !g.wanted[i] && g.status[i] == St::Queued {
                    g.status[i] = St::Idle;
                    g.c.cancelled_queued += 1;
                }
            }
            g.queue = newq;
            // 视口外但仍在解码的请求：无法中断，计数
            let mut stale = 0;
            let wanted = std::mem::take(&mut g.wanted);
            for r in g.runs.values_mut() {
                if r.epoch == epoch
                    && !wanted.get(r.index).copied().unwrap_or(false)
                    && !r.stale_counted
                {
                    r.stale_counted = true;
                    stale += 1;
                }
            }
            g.wanted = wanted;
            g.c.stale_inflight += stale;
            self.sh.cv.notify_all();
        }
        let n = immediate.len();
        for d in immediate {
            let _ = self.sh.tx.send(d);
        }
        n
    }

    /// 等待磁盘缓存写完（最多 10 秒）。测量“重启后”条件前必须调用，否则缓存可能不完整。
    pub fn flush(&self) {
        let t = Instant::now();
        while self.sh.pending_puts.load(Ordering::SeqCst) > 0
            && t.elapsed() < Duration::from_secs(10)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn counters(&self) -> Counters {
        self.sh.m.lock().unwrap().c.clone()
    }

    pub fn mem_entries(&self) -> usize {
        self.sh.mem.lock().unwrap().len()
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // 不 join：卡在慢 I/O 里的线程无法回收，进程退出时一并结束
        self.sh.m.lock().unwrap().shutdown = true;
        self.sh.cv.notify_all();
    }
}

fn spawn_worker(sh: Arc<Shared>) {
    std::thread::spawn(move || worker(sh));
}

fn watchdog(sh: Arc<Shared>) {
    loop {
        std::thread::sleep(Duration::from_millis(10));
        let mut timeouts: Vec<Done> = Vec::new();
        {
            let mut g = sh.m.lock().unwrap();
            if g.shutdown {
                return;
            }
            let now = Instant::now();
            let cfg = &sh.cfg;
            let epoch_now = g.epoch;
            let mut newly_slow = 0usize;
            let mut timed: Vec<(usize, u64)> = Vec::new();
            for r in g.runs.values_mut() {
                let age = now - r.start;
                if age > cfg.slow_after && !r.slow {
                    r.slow = true;
                    newly_slow += 1;
                }
                if age > cfg.hard_timeout && !r.timed_out {
                    r.timed_out = true;
                    if r.epoch == epoch_now {
                        timed.push((r.index, r.epoch));
                    }
                }
            }
            g.slow_running += newly_slow;
            for _ in 0..newly_slow {
                if g.extra_live < cfg.max_extra {
                    g.extra_live += 1;
                    g.live_workers += 1;
                    g.c.extra_workers_spawned += 1;
                    spawn_worker(sh.clone());
                }
            }
            for (index, epoch) in timed {
                if g.status[index] == St::Running {
                    g.status[index] = St::Failed;
                    g.c.timeouts += 1;
                    *g.c.failed.entry("timeout").or_insert(0) += 1;
                    timeouts.push(Done {
                        epoch,
                        index,
                        key: g.items[index].key,
                        outcome: Err(Fail::Timeout),
                        source: Source::Decoded,
                        timing: Timing::default(),
                        late: false,
                        at: now,
                    });
                }
            }
        }
        for d in timeouts {
            let _ = sh.tx.send(d);
        }
    }
}

fn worker(sh: Arc<Shared>) {
    loop {
        // 取任务
        let (job, epoch, index, key, path) = {
            let mut g = sh.m.lock().unwrap();
            loop {
                if g.shutdown {
                    return;
                }
                if let Some(i) = g.queue.pop_front() {
                    if g.status[i] != St::Queued {
                        continue;
                    }
                    g.status[i] = St::Running;
                    let job = g.next_job;
                    g.next_job += 1;
                    let epoch = g.epoch;
                    if !g.wanted[i] {
                        g.c.started_unwanted += 1;
                    }
                    g.c.started += 1;
                    g.runs.insert(
                        job,
                        Run {
                            epoch,
                            index: i,
                            start: Instant::now(),
                            slow: false,
                            timed_out: false,
                            stale_counted: false,
                        },
                    );
                    let it = &g.items[i];
                    break (job, epoch, i, it.key, it.path.clone());
                }
                g = sh.cv.wait(g).unwrap();
            }
        };

        // 干活（不持锁）
        if let Some(d) = sh.cfg.inject.get(&index) {
            std::thread::sleep(*d);
        }
        let (outcome, source, timing) = run_job(&sh, &path, key);

        // 收尾：先登记、再投递，最后（锁外）写磁盘缓存
        let mut put_disk: Option<Arc<Thumb>> = None;
        let exit;
        {
            let mut g = sh.m.lock().unwrap();
            let run = g.runs.remove(&job).expect("run 记录存在");
            if run.slow {
                g.slow_running -= 1;
            }
            let cur = g.epoch == epoch;
            let in_view = cur && g.wanted.get(index).copied().unwrap_or(false);
            let late = run.timed_out && outcome.is_ok();
            match &outcome {
                Ok(t) => {
                    g.c.ok += 1;
                    match source {
                        Source::Disk => g.c.from_disk += 1,
                        Source::Decoded => {
                            g.c.decoded += 1;
                            if sh.cfg.disk.is_some() {
                                sh.pending_puts.fetch_add(1, Ordering::SeqCst);
                                put_disk = Some(t.clone());
                            }
                        }
                        Source::Memory => g.c.from_memory += 1,
                    }
                    sh.mem.lock().unwrap().insert(key, t.clone());
                    if late {
                        g.c.late_ready += 1;
                    }
                }
                Err(f) => {
                    // 超时已报告过的不重复计数（运行中失败仍是真实失败，照常计数）
                    if !run.timed_out {
                        *g.c.failed.entry(f.as_str()).or_insert(0) += 1;
                    }
                }
            }
            if !in_view {
                g.c.completed_out_of_view += 1;
            }
            if cur {
                g.status[index] = if outcome.is_ok() {
                    St::Ready
                } else {
                    St::Failed
                };
            }
            // 已报告超时、本次又失败：界面已有超时状态，不再重复投递
            let suppress = run.timed_out && outcome.is_err();
            if !suppress {
                let _ = sh.tx.send(Done {
                    epoch,
                    index,
                    key,
                    outcome,
                    source,
                    timing,
                    late,
                    at: Instant::now(),
                });
            }
            // 多出来的临时线程在慢任务结束后退役
            exit = g.live_workers > sh.cfg.workers + g.slow_running;
            if exit {
                g.live_workers -= 1;
                if g.extra_live > 0 {
                    g.extra_live -= 1;
                }
            }
        }
        if let Some(t) = put_disk
            && let Some(d) = &sh.cfg.disk
        {
            d.put(key, &t);
            sh.pending_puts.fetch_sub(1, Ordering::SeqCst);
        }
        if exit {
            return;
        }
    }
}

fn run_job(
    sh: &Shared,
    path: &std::path::Path,
    key: u64,
) -> (Result<Arc<Thumb>, Fail>, Source, Timing) {
    if let Some(d) = &sh.cfg.disk {
        let t = Instant::now();
        if let Some(th) = d.get(key) {
            let tm = Timing {
                read_us: t.elapsed().as_micros() as u64,
                ..Default::default()
            };
            return (Ok(Arc::new(th)), Source::Disk, tm);
        }
    }
    match decode::generate(sh.cfg.route, path, sh.cfg.size, &sh.cfg.limits) {
        Ok((th, tm, _)) => (Ok(Arc::new(th)), Source::Decoded, tm),
        Err(f) => (Err(f), Source::Decoded, Timing::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不存在的路径：解码立即以 Io 失败；`inject` 用延迟模拟慢文件，所以测试不依赖任何夹具文件。
    fn svc(workers: usize, inject_ms: &[(usize, u64)]) -> (Service, Receiver<Done>) {
        Service::new(Config {
            workers,
            size: 64,
            route: Route::Auto,
            limits: Limits::default(),
            slow_after: Duration::from_millis(50),
            hard_timeout: Duration::from_millis(300),
            max_extra: 2,
            prefetch: 0,
            inject: inject_ms
                .iter()
                .map(|&(i, m)| (i, Duration::from_millis(m)))
                .collect(),
            disk: None,
        })
    }

    fn items(n: usize) -> Vec<Item> {
        (0..n)
            .map(|i| Item {
                path: PathBuf::from(format!("Z:\\不存在\\{i}.webp")),
                key: i as u64 + 1,
            })
            .collect()
    }

    fn collect(rx: &Receiver<Done>, until: Duration) -> Vec<Done> {
        let t = Instant::now();
        let mut v = Vec::new();
        while t.elapsed() < until {
            if let Ok(d) = rx.recv_timeout(Duration::from_millis(5)) {
                v.push(d);
            }
        }
        v
    }

    #[test]
    fn scroll_cancels_queued_and_never_starts_stale_work() {
        // 单线程、每项 30ms：滚动时队列里仍有大量旧请求
        let inj: Vec<(usize, u64)> = (0..100).map(|i| (i, 30)).collect();
        let (s, rx) = svc(1, &inj);
        let e = s.enter_dir(items(100));
        s.set_viewport(e, 0, 10);
        std::thread::sleep(Duration::from_millis(40)); // 约完成 1 项
        s.set_viewport(e, 50, 55);
        let done = collect(&rx, Duration::from_millis(500));
        let c = s.counters();
        assert!(c.cancelled_queued >= 8, "旧请求应被取消：{c:?}");
        assert_eq!(c.started_unwanted, 0, "取消后不应再开始视口外的任务：{c:?}");
        let got = done.iter().filter(|d| d.index >= 50).count();
        assert_eq!(got, 5, "新视口 5 项都应有结果");
        // 在途的那一项无法中断，最多多完成 1~2 个旧项
        assert!(done.iter().filter(|d| d.index < 10).count() <= 3);
    }

    #[test]
    fn old_epoch_results_carry_their_epoch() {
        let inj: Vec<(usize, u64)> = (0..10).map(|i| (i, 60)).collect();
        let (s, rx) = svc(2, &inj);
        let e1 = s.enter_dir(items(10));
        s.set_viewport(e1, 0, 4);
        std::thread::sleep(Duration::from_millis(20)); // 两项在途
        let e2 = s.enter_dir(items(10));
        assert_ne!(e1, e2);
        s.set_viewport(e2, 0, 4);
        let done = collect(&rx, Duration::from_millis(500));
        assert!(
            done.iter().any(|d| d.epoch == e1),
            "在途的旧请求会带旧 epoch 回来，由界面丢弃"
        );
        assert_eq!(done.iter().filter(|d| d.epoch == e2).count(), 4);
        assert_eq!(s.counters().started_unwanted, 0);
    }

    #[test]
    fn one_slow_file_neither_blocks_others_nor_hangs_forever() {
        // 单个基础线程 + 慢项：慢项超过 slow_after 后补线程，其余项不必等它；超过硬超时报告 Timeout
        let (s, rx) = svc(1, &[(0, 700)]);
        let e = s.enter_dir(items(6));
        let t = Instant::now();
        s.set_viewport(e, 0, 6);
        let mut seen = std::collections::HashMap::new();
        while t.elapsed() < Duration::from_millis(1200) {
            if let Ok(d) = rx.recv_timeout(Duration::from_millis(5)) {
                seen.entry(d.index)
                    .or_insert((t.elapsed(), d.outcome.err()));
            }
        }
        for i in 1..6 {
            let (at, _) = seen.get(&i).unwrap_or_else(|| panic!("项 {i} 没有结果"));
            assert!(
                *at < Duration::from_millis(250),
                "项 {i} 不应等慢文件：{at:?}"
            );
        }
        let c = s.counters();
        assert_eq!(c.timeouts, 1);
        assert!(c.extra_workers_spawned >= 1);
        assert_eq!(seen[&0].1, Some(Fail::Timeout), "慢项先以超时状态出现");
    }
}
