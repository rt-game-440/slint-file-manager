//! 缩略图调度：工作线程池按 [`Queue`] 优先级取请求；先查磁盘缓存，未命中再解码并（投递之后）写入缓存。
//!
//! - 取消：视口变化只重排队列（[`Scheduler::set_wanted`]），排队中的旧请求作废、不会开始；正在解码的无法中断
//!   （libwebp／image 没有取消点），完成后结果照常投递，由界面按文件身份（键）决定是否显示。
//! - 慢文件：运行超过 `slow_after` 临时增援一个工作线程（最多 `max_extra` 个），慢任务结束后增援线程退役；
//!   运行超过 `hard_timeout` 先投递 [`Fail::Timeout`]（界面显示回退图标、不再等待），之后若成功再投递一次
//!   （`late = true`，“迟到补图”），之后若失败则不再投递。
//! - 投递：通过 [`Sink`] 回调，产品里转到界面线程；结果只带键，不带面板／下标，界面按键绑定，旧结果不会贴错位置。
//!
//! [`Manual`] 是同一语义的单线程版本，供 harness 确定性地截到“加载中”“超时”“迟到补图”等状态。

use super::cache::DiskCache;
use super::decode::{self, Limits};
use super::lane::{self, LaneCfg, ShellLane, execute_shell, merge_counters};
use super::queue::{Job, Queue};
use super::shell::{ShellProvider, path_is_shell};
use super::{Fail, Source, Thumb};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

/// 记住“需要系统缩略图”的文件键的上限（超过就清空重来，最坏多做一次原生判定）。
const SHELL_KEYS_MAX: usize = 4096;

/// 一次生成的结果。
#[derive(Clone, Debug)]
pub struct Done {
    pub key: u64,
    pub outcome: Result<Arc<Thumb>, Fail>,
    /// 超时之后才完成的成功结果
    pub late: bool,
    pub from_disk: bool,
}

/// 结果投递回调（在工作线程或看门狗线程上调用）。
pub type Sink = Arc<dyn Fn(Done) + Send + Sync>;

/// 手动调度器判定“慢文件”的函数（harness）。
pub type SlowFn = Box<dyn Fn(&Path) -> bool>;

/// 一次执行的结果：生成结果、是否来自磁盘缓存、需要写入缓存的缩略图与缓存代号。
type Executed = (Result<Arc<Thumb>, Fail>, bool, Option<(Arc<Thumb>, u64)>);

/// 测试注入：对某个路径额外延迟（模拟慢盘、网络位置）。
pub type Inject = Arc<dyn Fn(&Path) -> Option<Duration> + Send + Sync>;

#[derive(Clone)]
pub struct Config {
    pub workers: usize,
    pub slow_after: Duration,
    pub hard_timeout: Duration,
    pub max_extra: usize,
    pub limits: Limits,
    pub disk: Option<Arc<DiskCache>>,
    pub inject: Option<Inject>,
    /// 系统缩略图提供者（视频、TIFF、HEIC 与云占位的缓存查询）；`None` 时这些文件显示类型图标
    pub shell: Option<Arc<dyn ShellProvider>>,
    /// Shell 车道的硬超时、让路上限、允许卡住的线程数（见 [`LaneCfg`]）
    pub shell_hard_timeout: Duration,
    pub shell_defer: Duration,
    pub shell_max_stuck: usize,
}

impl Config {
    /// 研究 0005 选定的参数：300 ms 增援、2 s 硬超时、最多增援 4 个线程。
    pub fn product(disk: Option<Arc<DiskCache>>, shell: Option<Arc<dyn ShellProvider>>) -> Config {
        Config {
            workers: super::default_workers(),
            slow_after: Duration::from_millis(300),
            hard_timeout: Duration::from_secs(2),
            max_extra: 4,
            limits: Limits::default(),
            disk,
            inject: None,
            shell,
            shell_hard_timeout: lane::HARD_TIMEOUT,
            shell_defer: lane::DEFER,
            shell_max_stuck: lane::MAX_STUCK,
        }
    }
}

/// 计数（只含数值，可写日志）。
#[derive(Clone, Debug, Default)]
pub struct Counters {
    pub started: u64,
    pub ok: u64,
    pub failed: HashMap<&'static str, u64>,
    pub cancelled_queued: u64,
    pub timeouts: u64,
    pub late_ready: u64,
    pub extra_workers_spawned: u64,
    pub from_disk: u64,
    pub decoded: u64,
    pub cache_write_failed: u64,
    /// 其中由系统缩略图（Shell）生成的数量（已计入 `decoded`）
    pub shell_ok: u64,
    /// 原生解码判定“需要系统缩略图”而转入 Shell 车道的请求数
    pub shell_handoff: u64,
    /// Shell 车道累计卡住（被替换）的工作线程数
    pub shell_stuck: u64,
}

/// 执行一个请求：磁盘缓存 → 解码。返回结果与（需要时）待写入缓存的缩略图与代号。
fn execute(src: &Source, limits: &Limits, disk: Option<&DiskCache>) -> Executed {
    let cgen = disk.map(DiskCache::generation);
    if let Some(d) = disk
        && let Some(t) = d.get(src)
    {
        return (Ok(Arc::new(t)), true, None);
    }
    match decode::generate(src, limits) {
        Ok(g) => {
            let t = Arc::new(g.thumb);
            let put = match (cgen, g.identity_ok) {
                (Some(cgen), true) => Some((t.clone(), cgen)),
                _ => None,
            };
            (Ok(t), false, put)
        }
        Err(f) => (Err(f), false, None),
    }
}

struct Run {
    key: u64,
    start: Instant,
    slow: bool,
    timed_out: bool,
}

struct Inner {
    queue: Queue,
    runs: HashMap<u64, Run>,
    running_keys: HashMap<u64, usize>,
    next_job: u64,
    live_workers: usize,
    slow_running: usize,
    extra_live: usize,
    shutdown: bool,
    c: Counters,
    /// 每个面板当前想要的全部请求键（含在途的），用于判断转交 Shell 车道的请求是否还有人需要
    wanted: [HashSet<u64>; 2],
    /// 已知需要系统缩略图的文件键（原生解码判定为不支持或云占位）：再次请求时直接走 Shell 车道
    shell_keys: HashSet<u64>,
}

struct Shared {
    m: Mutex<Inner>,
    cv: Condvar,
    sink: Sink,
    cfg: Config,
    /// 系统缩略图车道（没有提供者时为空）
    lane: OnceLock<ShellLane>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 多线程调度器（产品）。
pub struct Scheduler {
    sh: Arc<Shared>,
}

impl Scheduler {
    pub fn new(cfg: Config, sink: Sink) -> Scheduler {
        let workers = cfg.workers.max(1);
        let sh = Arc::new(Shared {
            m: Mutex::new(Inner {
                queue: Queue::default(),
                runs: HashMap::new(),
                running_keys: HashMap::new(),
                next_job: 0,
                live_workers: workers,
                slow_running: 0,
                extra_live: 0,
                shutdown: false,
                c: Counters::default(),
                wanted: Default::default(),
                shell_keys: HashSet::new(),
            }),
            cv: Condvar::new(),
            sink,
            cfg,
            lane: OnceLock::new(),
        });
        if let Some(provider) = sh.cfg.shell.clone() {
            let weak: Weak<Shared> = Arc::downgrade(&sh);
            let busy = Box::new(move || {
                weak.upgrade().is_some_and(|s| {
                    let g = s.lock();
                    !g.queue.is_empty() || !g.runs.is_empty()
                })
            });
            let cfg = LaneCfg {
                provider,
                disk: sh.cfg.disk.clone(),
                hard_timeout: sh.cfg.shell_hard_timeout,
                defer: sh.cfg.shell_defer,
                max_stuck: sh.cfg.shell_max_stuck,
                inject: sh.cfg.inject.clone(),
            };
            let _ = sh.lane.set(ShellLane::new(cfg, sh.sink.clone(), busy));
        }
        for _ in 0..workers {
            spawn_worker(sh.clone());
        }
        let w = sh.clone();
        let _ = std::thread::Builder::new()
            .name("thumb-watchdog".into())
            .spawn(move || watchdog(w));
        Scheduler { sh }
    }

    /// 替换面板 `pane` 想要的请求（已按优先级排序）。正在生成的同一文件不会重复排队。
    ///
    /// 请求按路线分流：视频、TIFF、HEIC 等（以及已知需要系统缩略图的文件）进 Shell 车道，其余进原生工作线程。
    pub fn set_wanted(&self, pane: usize, jobs: Vec<Job>) {
        let lane = self.sh.lane.get();
        let (native, shell): (Vec<Job>, Vec<Job>) = {
            let g = self.sh.lock();
            jobs.iter().cloned().partition(|j| {
                lane.is_none() || !(path_is_shell(&j.src.path) || g.shell_keys.contains(&j.key))
            })
        };
        {
            let mut g = self.sh.lock();
            g.wanted[pane.min(1)] = jobs.iter().map(|j| j.key).collect();
            let Inner {
                queue,
                running_keys,
                c,
                ..
            } = &mut *g;
            let n = queue.set_wanted(pane, native, |k| running_keys.contains_key(&k));
            c.cancelled_queued += n as u64;
        }
        if let Some(l) = lane {
            l.set_wanted(pane, shell);
        }
        self.sh.cv.notify_all();
    }

    pub fn counters(&self) -> Counters {
        let mut c = self.sh.lock().c.clone();
        if let Some(l) = self.sh.lane.get() {
            merge_counters(&mut c, &l.counters());
            c.shell_stuck = l.stuck() as u64;
        }
        c
    }

    /// 排队中与生成中的请求数（含 Shell 车道）。
    pub fn pending(&self) -> (usize, usize) {
        let g = self.sh.lock();
        let (mut q, mut r) = (g.queue.len(), g.runs.len());
        if let Some(l) = self.sh.lane.get() {
            let (lq, lr) = l.pending();
            q += lq;
            r += lr;
        }
        (q, r)
    }

    /// Shell 车道的（排队，在途）请求数。
    pub fn shell_pending(&self) -> (usize, usize) {
        self.sh.lane.get().map_or((0, 0), ShellLane::pending)
    }

    /// Shell 车道是否因卡死线程过多而停用。
    pub fn shell_disabled(&self) -> bool {
        self.sh.lane.get().is_some_and(ShellLane::disabled)
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        // 不 join：卡在慢 I/O 里的线程无法回收，进程退出时一并结束
        self.sh.lock().shutdown = true;
        self.sh.cv.notify_all();
        if let Some(l) = self.sh.lane.get() {
            l.shutdown();
        }
    }
}

fn spawn_worker(sh: Arc<Shared>) {
    let _ = std::thread::Builder::new()
        .name("thumb-worker".into())
        .spawn(move || worker(sh));
}

fn watchdog(sh: Arc<Shared>) {
    loop {
        std::thread::sleep(Duration::from_millis(10));
        let mut timeouts: Vec<Done> = Vec::new();
        {
            let mut g = sh.lock();
            if g.shutdown {
                return;
            }
            let now = Instant::now();
            let cfg = &sh.cfg;
            let mut newly_slow = 0usize;
            for r in g.runs.values_mut() {
                let age = now - r.start;
                if age > cfg.slow_after && !r.slow {
                    r.slow = true;
                    newly_slow += 1;
                }
                if age > cfg.hard_timeout && !r.timed_out {
                    r.timed_out = true;
                    timeouts.push(Done {
                        key: r.key,
                        outcome: Err(Fail::Timeout),
                        late: false,
                        from_disk: false,
                    });
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
            g.c.timeouts += timeouts.len() as u64;
        }
        for d in timeouts {
            (sh.sink)(d);
        }
    }
}

fn worker(sh: Arc<Shared>) {
    loop {
        let (job_id, job) = {
            let mut g = sh.lock();
            loop {
                if g.shutdown {
                    return;
                }
                let Inner {
                    queue,
                    running_keys,
                    ..
                } = &mut *g;
                if let Some(job) = queue.pop(|k| running_keys.contains_key(&k)) {
                    let id = g.next_job;
                    g.next_job += 1;
                    g.c.started += 1;
                    *g.running_keys.entry(job.key).or_insert(0) += 1;
                    g.runs.insert(
                        id,
                        Run {
                            key: job.key,
                            start: Instant::now(),
                            slow: false,
                            timed_out: false,
                        },
                    );
                    break (id, job);
                }
                g = sh.cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
        };

        if let Some(inject) = &sh.cfg.inject
            && let Some(d) = inject(&job.src.path)
        {
            std::thread::sleep(d);
        }
        let (outcome, from_disk, put) = execute(&job.src, &sh.cfg.limits, sh.cfg.disk.as_deref());

        let (deliver, exit, handoff) = {
            let mut g = sh.lock();
            let run = g.runs.remove(&job_id).expect("运行记录存在");
            // 原生解码判定“需要系统缩略图”（动图 WebP、其他不支持的内容、云占位文件）：转入 Shell 车道，
            // 不在这里投递失败；仍被某个面板需要时才转交（滚走了就不浪费系统提取）
            let handoff = matches!(outcome, Err(Fail::Unsupported | Fail::Cloud))
                && sh.lane.get().is_some()
                && g.wanted.iter().any(|w| w.contains(&job.key));
            if handoff {
                if g.shell_keys.len() >= SHELL_KEYS_MAX {
                    g.shell_keys.clear();
                }
                g.shell_keys.insert(job.key);
                g.c.shell_handoff += 1;
            }
            if let Some(n) = g.running_keys.get_mut(&job.key) {
                *n -= 1;
                if *n == 0 {
                    g.running_keys.remove(&job.key);
                }
            }
            if run.slow {
                g.slow_running -= 1;
            }
            let late = run.timed_out && outcome.is_ok();
            match &outcome {
                Ok(_) => {
                    g.c.ok += 1;
                    if from_disk {
                        g.c.from_disk += 1;
                    } else {
                        g.c.decoded += 1;
                    }
                    if late {
                        g.c.late_ready += 1;
                    }
                }
                Err(f) => {
                    if !run.timed_out && !handoff {
                        *g.c.failed.entry(f.as_str()).or_insert(0) += 1;
                    }
                }
            }
            // 已报告超时、本次又失败：界面已有超时状态，不再投递；转交 Shell 车道的由车道投递结果
            let deliver = !(run.timed_out && outcome.is_err()) && !handoff;
            let exit = g.live_workers > sh.cfg.workers.max(1) + g.slow_running;
            if exit {
                g.live_workers -= 1;
                g.extra_live = g.extra_live.saturating_sub(1);
            }
            (deliver.then_some((outcome, late)), exit, handoff)
        };
        if handoff && let Some(l) = sh.lane.get() {
            l.handoff(job.clone());
        }
        if let Some((outcome, late)) = deliver {
            (sh.sink)(Done {
                key: job.key,
                outcome,
                late,
                from_disk,
            });
        }
        // 先投递再写盘：写缓存不拖慢显示
        if let (Some((t, cgen)), Some(d)) = (put, &sh.cfg.disk)
            && d.put(&job.src, &t, cgen).is_err()
        {
            sh.lock().c.cache_write_failed += 1;
        }
        sh.cv.notify_all();
        if exit {
            return;
        }
    }
}

/// 单线程、由调用方驱动的调度器（harness）。语义与 [`Scheduler`] 相同：
/// [`Manual::run`] 按优先级执行全部排队请求；`slow` 判定为慢的请求先以超时结束并挂起，
/// [`Manual::finish_slow`] 再真正执行它们（迟到补图）。
pub struct Manual {
    queue: Queue,
    limits: Limits,
    disk: Option<Arc<DiskCache>>,
    slow: Option<SlowFn>,
    shell: Option<Arc<dyn ShellProvider>>,
    held: Vec<Job>,
    pub counters: Counters,
}

impl Manual {
    pub fn new(
        disk: Option<Arc<DiskCache>>,
        slow: Option<SlowFn>,
        shell: Option<Arc<dyn ShellProvider>>,
    ) -> Manual {
        Manual {
            queue: Queue::default(),
            limits: Limits::default(),
            disk,
            slow,
            shell,
            held: Vec::new(),
            counters: Counters::default(),
        }
    }

    pub fn set_wanted(&mut self, pane: usize, jobs: Vec<Job>) {
        let held: Vec<u64> = self.held.iter().map(|j| j.key).collect();
        let n = self.queue.set_wanted(pane, jobs, |k| held.contains(&k));
        self.counters.cancelled_queued += n as u64;
    }

    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// 执行全部排队请求，返回结果（慢请求返回超时）。
    pub fn run(&mut self) -> Vec<Done> {
        let mut out = Vec::new();
        while let Some(job) = self.queue.pop(|_| false) {
            self.counters.started += 1;
            if self.slow.as_ref().is_some_and(|f| f(&job.src.path)) {
                self.counters.timeouts += 1;
                out.push(Done {
                    key: job.key,
                    outcome: Err(Fail::Timeout),
                    late: false,
                    from_disk: false,
                });
                self.held.push(job);
                continue;
            }
            out.push(self.exec(&job, false));
        }
        out
    }

    /// 完成之前超时的慢请求（成功的作为迟到补图投递，失败的不投递）。
    pub fn finish_slow(&mut self) -> Vec<Done> {
        let held = std::mem::take(&mut self.held);
        held.iter()
            .map(|j| self.exec(j, true))
            .filter(|d| d.outcome.is_ok())
            .collect()
    }

    fn exec(&mut self, job: &Job, late: bool) -> Done {
        let disk = self.disk.as_deref();
        let (outcome, from_disk, put, via_shell) = match &self.shell {
            Some(p) if path_is_shell(&job.src.path) => {
                let (o, d, put) = execute_shell(&job.src, p.as_ref(), disk);
                (o, d, put, true)
            }
            Some(p) => match execute(&job.src, &self.limits, disk) {
                // 原生解码判定需要系统缩略图（动图 WebP、云占位……）：与线程模式一样转交 Shell
                (Err(Fail::Unsupported | Fail::Cloud), ..) => {
                    self.counters.shell_handoff += 1;
                    let (o, d, put) = execute_shell(&job.src, p.as_ref(), disk);
                    (o, d, put, true)
                }
                (o, d, put) => (o, d, put, false),
            },
            None => {
                let (o, d, put) = execute(&job.src, &self.limits, disk);
                (o, d, put, false)
            }
        };
        match &outcome {
            Ok(_) => {
                self.counters.ok += 1;
                if from_disk {
                    self.counters.from_disk += 1;
                } else {
                    self.counters.decoded += 1;
                    if via_shell {
                        self.counters.shell_ok += 1;
                    }
                }
                if late {
                    self.counters.late_ready += 1;
                }
            }
            Err(f) => *self.counters.failed.entry(f.as_str()).or_insert(0) += 1,
        }
        if let (Some((t, cgen)), Some(d)) = (put, &self.disk)
            && d.put(&job.src, &t, cgen).is_err()
        {
            self.counters.cache_write_failed += 1;
        }
        Done {
            key: job.key,
            outcome,
            late,
            from_disk,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thumbs::queue::rank;
    use std::path::PathBuf;
    use std::sync::mpsc::{Receiver, channel};

    /// 不存在的路径：解码立即以 Io 失败；`inject` 用延迟模拟慢文件，测试不依赖夹具文件。
    fn svc(workers: usize, delays: Vec<(usize, u64)>) -> (Scheduler, Receiver<Done>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let delays: HashMap<String, Duration> = delays
            .into_iter()
            .map(|(i, ms)| (format!("{i}.webp"), Duration::from_millis(ms)))
            .collect();
        let inject: Inject =
            Arc::new(move |p: &Path| delays.get(p.file_name()?.to_str()?).copied());
        let s = Scheduler::new(
            Config {
                workers,
                slow_after: Duration::from_millis(50),
                hard_timeout: Duration::from_millis(300),
                max_extra: 2,
                limits: Limits::default(),
                disk: None,
                inject: Some(inject),
                shell: None,
                shell_hard_timeout: lane::HARD_TIMEOUT,
                shell_defer: lane::DEFER,
                shell_max_stuck: lane::MAX_STUCK,
            },
            Arc::new(move |d| {
                let _ = tx.lock().unwrap().send(d);
            }),
        );
        (s, rx)
    }

    fn src(i: usize) -> Source {
        Source::new(PathBuf::from(format!("Z:\\不存在\\{i}.webp")), 1, None)
    }

    fn jobs(r: std::ops::Range<usize>, pane: usize) -> Vec<Job> {
        r.map(|i| {
            let s = src(i);
            Job {
                key: s.key(),
                src: s,
                pane,
                rank: rank::VISIBLE_ACTIVE,
            }
        })
        .collect()
    }

    fn index_of(key: u64) -> usize {
        (0..1000).find(|&i| src(i).key() == key).unwrap()
    }

    fn collect(rx: &Receiver<Done>, until: Duration) -> Vec<(Duration, usize, Done)> {
        let t = Instant::now();
        let mut v = Vec::new();
        while t.elapsed() < until {
            if let Ok(d) = rx.recv_timeout(Duration::from_millis(5)) {
                v.push((t.elapsed(), index_of(d.key), d));
            }
        }
        v
    }

    #[test]
    fn scroll_cancels_queued_and_never_starts_stale_work() {
        let (s, rx) = svc(1, (0..100).map(|i| (i, 30)).collect());
        s.set_wanted(0, jobs(0..10, 0));
        std::thread::sleep(Duration::from_millis(40));
        s.set_wanted(0, jobs(50..55, 0));
        let done = collect(&rx, Duration::from_millis(500));
        let c = s.counters();
        assert!(c.cancelled_queued >= 7, "旧请求应被取消：{c:?}");
        assert_eq!(done.iter().filter(|d| d.1 >= 50).count(), 5);
        // 在途的旧项无法中断，最多 2 个
        assert!(done.iter().filter(|d| d.1 < 10).count() <= 2, "{c:?}");
        assert!(c.started <= 7, "取消后的旧请求不应开始：{c:?}");
    }

    #[test]
    fn one_slow_file_neither_blocks_others_nor_hangs_forever() {
        let (s, rx) = svc(1, vec![(0, 700)]);
        s.set_wanted(0, jobs(0..6, 0));
        let done = collect(&rx, Duration::from_millis(1200));
        for i in 1..6 {
            let at = done.iter().find(|d| d.1 == i).map(|d| d.0);
            let at = at.unwrap_or_else(|| panic!("项 {i} 没有结果"));
            assert!(
                at < Duration::from_millis(250),
                "项 {i} 不应等慢文件：{at:?}"
            );
        }
        let slow: Vec<&Done> = done.iter().filter(|d| d.1 == 0).map(|d| &d.2).collect();
        assert_eq!(
            slow[0].outcome.as_ref().err(),
            Some(&Fail::Timeout),
            "慢项先超时"
        );
        // 慢项最终以 Io 失败（路径不存在）：超时后再失败不重复投递
        assert_eq!(slow.len(), 1);
        let c = s.counters();
        assert_eq!(c.timeouts, 1);
        assert!(c.extra_workers_spawned >= 1);
    }

    #[test]
    fn late_success_is_delivered_after_timeout() {
        let dir = std::env::temp_dir().join(format!("sfm-sched-late-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("0.webp");
        let t = crate::thumbs::decode::samples::pattern(300, 200, 1);
        std::fs::write(&p, crate::thumbs::decode::samples::webp_lossy(&t)).unwrap();
        let m = std::fs::metadata(&p).unwrap();
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let s = Scheduler::new(
            Config {
                workers: 1,
                slow_after: Duration::from_millis(50),
                hard_timeout: Duration::from_millis(150),
                max_extra: 1,
                limits: Limits::default(),
                disk: None,
                inject: Some(Arc::new(|_| Some(Duration::from_millis(400)))),
                shell: None,
                shell_hard_timeout: lane::HARD_TIMEOUT,
                shell_defer: lane::DEFER,
                shell_max_stuck: lane::MAX_STUCK,
            },
            Arc::new(move |d| {
                let _ = tx.lock().unwrap().send(d);
            }),
        );
        let src = Source::new(p.clone(), m.len(), m.modified().ok());
        s.set_wanted(
            0,
            vec![Job {
                key: src.key(),
                src,
                pane: 0,
                rank: 0,
            }],
        );
        let a = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(a.outcome.err(), Some(Fail::Timeout));
        let b = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(b.late && b.outcome.is_ok(), "迟到成功照常补上");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn both_panes_share_work_for_the_same_file() {
        let (s, rx) = svc(2, vec![]);
        s.set_wanted(0, jobs(0..4, 0));
        s.set_wanted(1, jobs(0..4, 1));
        let done = collect(&rx, Duration::from_millis(300));
        assert_eq!(done.len(), 4, "同一文件只生成一次");
    }

    #[test]
    fn manual_runs_in_priority_order_and_holds_slow() {
        let mut m = Manual::new(
            None,
            Some(Box::new(|p: &Path| {
                p.to_string_lossy().contains("\\2.webp")
            })),
            None,
        );
        m.set_wanted(0, jobs(0..4, 0));
        let r = m.run();
        assert_eq!(r.len(), 4);
        assert_eq!(index_of(r[2].key), 2);
        assert_eq!(r[2].outcome.as_ref().err(), Some(&Fail::Timeout));
        assert_eq!(m.held(), 1);
        // 路径不存在：迟到的也是失败，不投递
        assert!(m.finish_slow().is_empty());
    }
}
