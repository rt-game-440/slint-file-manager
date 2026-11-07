//! 无头场景：用“模拟界面线程”驱动调度链路，测首屏、冷／热／重启条件、滚动取消、换目录和慢文件隔离。
//!
//! 取舍：无头场景不含 Slint 的图像上传与渲染，所以得到的是“结果送达界面线程”的时间，是整机首屏的下界，
//! 不能当作整机 UI 验收成绩（Issue #5 明确不得如此宣称）。真实窗口里的“可操作”时间由主程序的
//! `--bench-enter` 计时（scripts/perf-thumbs.ps1 一并采集）。
//!
//! 输出只含统计与类别：不含路径、文件名、图像。

use crate::bench::list_files;
use crate::cache::{DiskCache, Format, fnv1a};
use crate::common::*;
use crate::decode::Route;
use crate::pipeline::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

#[derive(Clone)]
enum Slot {
    Pending,
    Ready {
        at: Instant,
        src: Source,
        late: bool,
    },
    Failed {
        at: Instant,
        why: Fail,
    },
}

/// 模拟界面线程：持有“当前目录各项的绑定”，只应用 epoch 与文件身份都匹配的结果。
struct Ui {
    epoch: u64,
    keys: Vec<u64>,
    slots: Vec<Slot>,
    applied: u64,
    dropped_old_epoch: u64,
    key_mismatch: u64,
    /// 界面线程调用服务的耗时（微秒）：证明界面线程不被调度链路阻塞
    call_us: Vec<u64>,
    decode_stage: Vec<Timing>,
}

impl Ui {
    fn new() -> Ui {
        Ui {
            epoch: 0,
            keys: vec![],
            slots: vec![],
            applied: 0,
            dropped_old_epoch: 0,
            key_mismatch: 0,
            call_us: vec![],
            decode_stage: vec![],
        }
    }

    /// 每个场景开始前清零按场景统计的量
    fn reset(&mut self) {
        self.applied = 0;
        self.dropped_old_epoch = 0;
        self.key_mismatch = 0;
        self.call_us.clear();
        self.decode_stage.clear();
    }

    fn timed<R>(&mut self, f: impl FnOnce() -> R) -> R {
        let t = Instant::now();
        let r = f();
        self.call_us.push(t.elapsed().as_micros() as u64);
        r
    }

    fn apply(&mut self, d: Done) {
        if d.epoch != self.epoch {
            self.dropped_old_epoch += 1;
            return;
        }
        if self.keys.get(d.index) != Some(&d.key) {
            self.key_mismatch += 1;
            return;
        }
        self.applied += 1;
        if d.source == Source::Decoded && d.outcome.is_ok() {
            self.decode_stage.push(d.timing);
        }
        self.slots[d.index] = match d.outcome {
            Ok(_) => Slot::Ready {
                at: d.at,
                src: d.source,
                late: d.late,
            },
            Err(why) => Slot::Failed { at: d.at, why },
        };
    }

    fn poll(&mut self, rx: &Receiver<Done>, wait: Duration) -> bool {
        match rx.recv_timeout(wait) {
            Ok(d) => {
                self.apply(d);
                while let Ok(d) = rx.try_recv() {
                    self.apply(d);
                }
                true
            }
            Err(RecvTimeoutError::Timeout) => true,
            Err(RecvTimeoutError::Disconnected) => false,
        }
    }

    fn resolved(&self, r: std::ops::Range<usize>) -> bool {
        r.into_iter()
            .all(|i| !matches!(self.slots[i], Slot::Pending))
    }
}

struct Setup {
    items: Vec<(PathBuf, u64)>,
}

fn build_items(dir: &Path, size: u32, reverse: bool) -> Setup {
    let mut files = list_files(dir);
    if reverse {
        files.reverse();
    }
    let items = files
        .into_iter()
        .map(|p| {
            let (len, mt) = std::fs::metadata(&p)
                .map(|m| {
                    (
                        m.len(),
                        m.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map_or(0, |d| d.as_nanos()),
                    )
                })
                .unwrap_or((0, 0));
            let key = fnv1a(&[
                p.to_string_lossy().as_bytes(),
                &len.to_le_bytes(),
                &mt.to_le_bytes(),
                &size.to_le_bytes(),
            ]);
            (p, key)
        })
        .collect();
    Setup { items }
}

fn to_items(s: &Setup) -> Vec<Item> {
    s.items
        .iter()
        .map(|(p, k)| Item {
            path: p.clone(),
            key: *k,
        })
        .collect()
}

fn bind(ui: &mut Ui, epoch: u64, s: &Setup) {
    ui.epoch = epoch;
    ui.keys = s.items.iter().map(|(_, k)| *k).collect();
    ui.slots = vec![Slot::Pending; s.items.len()];
}

struct PassOpts {
    first: usize,
    inject: HashSet<usize>,
    wait_cap: Duration,
    /// 需要逐项报告状态的下标（合成夹具里的异常文件位置）
    report: Vec<usize>,
    /// 注入的最大延迟：迟到结果要等到它过去才能观察到
    max_inject: Duration,
}

/// 一次“进入目录并等首屏”。返回该次的指标。
fn enter_pass(
    svc: &Service,
    rx: &Receiver<Done>,
    ui: &mut Ui,
    s: &Setup,
    o: &PassOpts,
    name: &str,
) -> serde_json::Value {
    let c0 = svc.counters();
    ui.reset();
    let t0 = Instant::now();
    let epoch = ui.timed(|| svc.enter_dir(to_items(s)));
    bind(ui, epoch, s);
    let first = o.first.min(s.items.len());
    let hits = ui.timed(|| svc.set_viewport(epoch, 0, first));
    let _ = hits;
    let mut guard = 0;
    while !ui.resolved(0..first) && t0.elapsed() < o.wait_cap {
        ui.poll(rx, Duration::from_micros(300));
        guard += 1;
        if guard > 50_000_000 {
            break;
        }
    }
    let report = first_screen_report(ui, t0, first, &o.inject);
    // 继续等预取完成，便于下一个条件使用磁盘缓存，并得到“后台完成”时间
    let want_end = (first + 28).min(s.items.len());
    while !ui.resolved(0..want_end) && t0.elapsed() < o.wait_cap + Duration::from_secs(20) {
        ui.poll(rx, Duration::from_millis(1));
    }
    let bg = ui.slots[..want_end]
        .iter()
        .filter_map(slot_at)
        .max()
        .map(|t| ms(t - t0));
    // 慢文件注入：等注入的延迟过去，才能观察到“超时之后才完成”的迟到结果
    let mut final_states = Vec::new();
    if !o.inject.is_empty() {
        while t0.elapsed() < o.max_inject + Duration::from_millis(300) {
            ui.poll(rx, Duration::from_millis(5));
        }
        for &i in o.inject.iter().filter(|&&i| i < s.items.len()) {
            final_states.push(slot_json(&ui.slots[i], t0));
        }
    }
    let index_states: Vec<_> = o
        .report
        .iter()
        .filter(|&&i| i < s.items.len())
        .map(|&i| {
            let mut j = slot_json(&ui.slots[i], t0);
            j["index"] = i.into();
            j
        })
        .collect();
    svc.flush();
    let c1 = svc.counters();
    let mut j = report;
    j["pass"] = name.into();
    j["injected_slow_final_states"] = final_states.into();
    j["index_states"] = index_states.into();
    j["background_done_ms"] = bg.map(round2).into();
    j["counters_delta"] = counters_delta(&c0, &c1);
    j["stage_ms_decoded_items"] = stage_json(&ui.decode_stage);
    j
}

fn slot_json(s: &Slot, t0: Instant) -> serde_json::Value {
    match s {
        Slot::Ready { at, late, src } => serde_json::json!({
            "state": if *late { "ready_late" } else { "ready" },
            "source": match src { Source::Memory => "memory", Source::Disk => "disk", Source::Decoded => "decoded" },
            "at_ms": round2(ms(*at - t0)),
        }),
        Slot::Failed { at, why } => {
            serde_json::json!({"state": why.as_str(), "at_ms": round2(ms(*at - t0))})
        }
        Slot::Pending => serde_json::json!({"state": "pending"}),
    }
}

fn slot_at(s: &Slot) -> Option<Instant> {
    match s {
        Slot::Ready { at, .. } | Slot::Failed { at, .. } => Some(*at),
        Slot::Pending => None,
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn first_screen_report(
    ui: &Ui,
    t0: Instant,
    first: usize,
    inject: &HashSet<usize>,
) -> serde_json::Value {
    let mut normal_ready: Option<Instant> = None;
    let (mut ready, mut failed, mut pending) = (0, BTreeMap::<&str, u32>::new(), 0);
    let mut src = BTreeMap::<&str, u32>::new();
    let mut slow_states = Vec::new();
    for (i, slot) in ui.slots[..first].iter().enumerate() {
        match slot {
            Slot::Ready { at, src: s, late } => {
                ready += 1;
                *src.entry(match s {
                    Source::Memory => "memory",
                    Source::Disk => "disk",
                    Source::Decoded => "decoded",
                })
                .or_insert(0) += 1;
                if inject.contains(&i) {
                    slow_states.push(serde_json::json!({"state": if *late {"ready_late"} else {"ready"}, "at_ms": round2(ms(*at - t0))}));
                } else if normal_ready.is_none_or(|n| *at > n) {
                    normal_ready = Some(*at);
                }
            }
            Slot::Failed { at, why } => {
                *failed.entry(why.as_str()).or_insert(0) += 1;
                if inject.contains(&i) {
                    slow_states.push(
                        serde_json::json!({"state": why.as_str(), "at_ms": round2(ms(*at - t0))}),
                    );
                }
            }
            Slot::Pending => {
                pending += 1;
                if inject.contains(&i) {
                    slow_states.push(serde_json::json!({"state": "pending"}));
                }
            }
        }
    }
    let all_resolved = ui.slots[..first].iter().filter_map(slot_at).max();
    serde_json::json!({
        "first_screen_items": first,
        "ready": ready,
        "failed": failed,
        "pending": pending,
        "sources": src,
        // 非注入的正常文件：最后一张送达界面线程的时间
        "normal_ready_ms": (pending == 0 || normal_ready.is_some()).then(|| normal_ready.map(|t| round2(ms(t - t0)))).flatten(),
        "all_resolved_ms": (pending == 0).then(|| all_resolved.map(|t| round2(ms(t - t0)))).flatten(),
        "injected_slow_items": slow_states,
        "ui_thread": {
            "calls": ui.call_us.len(),
            "call_max_us": ui.call_us.iter().max(),
            "call_total_us": ui.call_us.iter().sum::<u64>(),
        },
        "applied": ui.applied,
        "dropped_old_epoch": ui.dropped_old_epoch,
        "key_mismatch": ui.key_mismatch,
    })
}

fn stage_json(v: &[Timing]) -> serde_json::Value {
    if v.is_empty() {
        return serde_json::Value::Null;
    }
    // 全为 0 说明该阶段不可单独测量（例如解码时缩放），输出 null 而不是 0
    let col = |f: fn(&Timing) -> u64| {
        // 0 表示“该条目没有这一阶段”，不计入统计；一个样本也没有则为 null
        let c: Vec<u64> = v.iter().map(f).filter(|&x| x > 0).collect();
        stats_ms(&c)
    };
    serde_json::json!({
        "read": col(|t| t.read_us),
        "decode": col(|t| t.decode_us),
        "scale": col(|t| t.scale_us),
        "total": col(|t| t.total_us()),
    })
}

fn counters_json(c: &Counters) -> serde_json::Value {
    serde_json::json!({
        "started": c.started, "started_unwanted": c.started_unwanted, "ok": c.ok, "failed": c.failed,
        "cancelled_queued": c.cancelled_queued, "stale_inflight": c.stale_inflight,
        "completed_out_of_view": c.completed_out_of_view,
        "extra_workers_spawned": c.extra_workers_spawned, "timeouts": c.timeouts, "late_ready": c.late_ready,
        "from_memory": c.from_memory, "from_disk": c.from_disk, "decoded": c.decoded,
    })
}

fn counters_delta(a: &Counters, b: &Counters) -> serde_json::Value {
    let mut f = b.failed.clone();
    for (k, v) in &a.failed {
        if let Some(x) = f.get_mut(k) {
            *x -= v;
        }
    }
    f.retain(|_, v| *v > 0);
    serde_json::json!({
        "started": b.started - a.started, "ok": b.ok - a.ok, "failed": f,
        "cancelled_queued": b.cancelled_queued - a.cancelled_queued,
        "stale_inflight": b.stale_inflight - a.stale_inflight,
        "completed_out_of_view": b.completed_out_of_view - a.completed_out_of_view,
        "extra_workers_spawned": b.extra_workers_spawned - a.extra_workers_spawned,
        "timeouts": b.timeouts - a.timeouts, "late_ready": b.late_ready - a.late_ready,
        "from_memory": b.from_memory - a.from_memory, "from_disk": b.from_disk - a.from_disk,
        "decoded": b.decoded - a.decoded,
        "started_unwanted": b.started_unwanted - a.started_unwanted,
    })
}

pub fn cmd(args: &[String]) -> i32 {
    let Some(dir) = arg(args, "dir") else {
        eprintln!("需要 --dir=目录");
        return 2;
    };
    let dir = PathBuf::from(dir);
    let kind = arg(args, "kind").unwrap_or_else(|| "enter".into());
    let condition = arg(args, "condition").unwrap_or_else(|| "cold".into());
    let size: u32 = arg_parse(args, "size", 256);
    let workers: usize = arg_parse(args, "workers", 4);
    let first: usize = arg_parse(args, "first-screen", 28);
    let route =
        Route::parse(&arg(args, "route").unwrap_or_else(|| "auto".into())).unwrap_or(Route::Auto);
    let cache_fmt = arg(args, "cache-format").unwrap_or_else(|| "webp".into());
    let cache_dir = arg(args, "cache-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| temp_root().join(format!("cache-{cache_fmt}")));
    let hard_timeout = Duration::from_millis(arg_parse(args, "timeout-ms", 2000));
    let slow_after = Duration::from_millis(arg_parse(args, "slow-after-ms", 300));
    let prefetch: usize = arg_parse(args, "prefetch", 28);
    let max_extra: usize = arg_parse(args, "max-extra", 4);
    let mut inject = HashMap::new();
    if let Some(sp) = arg(args, "slow") {
        for part in sp.split(',').filter(|s| !s.is_empty()) {
            if let Some((i, m)) = part.split_once(':')
                && let (Ok(i), Ok(m)) = (i.parse::<usize>(), m.parse::<u64>())
            {
                inject.insert(i, Duration::from_millis(m));
            }
        }
    }
    let disk = if cache_fmt == "none" {
        None
    } else {
        let f = Format::parse(&cache_fmt).unwrap_or(Format::Webp);
        Some(Arc::new(DiskCache::new(cache_dir, f)))
    };

    let setup = build_items(&dir, size, false);
    if setup.items.is_empty() {
        eprintln!("目录里没有文件");
        return 2;
    }
    let make = |disk: &Option<Arc<DiskCache>>| {
        Service::new(Config {
            workers,
            size,
            route,
            limits: Limits::default(),
            slow_after,
            hard_timeout,
            max_extra,
            prefetch,
            inject: inject.clone(),
            disk: disk.clone(),
        })
    };

    let inject_set: HashSet<usize> = inject.keys().copied().collect();
    let base_mem = mem();
    let o = PassOpts {
        first,
        inject: inject_set,
        wait_cap: hard_timeout + Duration::from_secs(3),
        report: arg(args, "report-indices")
            .map(|s| s.split(',').filter_map(|x| x.parse().ok()).collect())
            .unwrap_or_default(),
        max_inject: inject.values().copied().max().unwrap_or_default(),
    };
    let mut passes = Vec::new();
    let mut checks = serde_json::Map::new();
    let mut valid = true;
    let mut note = String::new();
    let cache_before;
    let mut final_counters: Counters;

    match condition.as_str() {
        "cold" | "warm" => {
            if let Some(d) = &disk {
                d.clear();
            }
            cache_before = disk.as_ref().map(|d| d.entries());
            let (svc, rx) = make(&disk);
            let mut ui = Ui::new();
            match kind.as_str() {
                "scroll" => {
                    passes.push(scroll_pass(&svc, &rx, &mut ui, &setup, &o, args));
                }
                "dirswitch" => {
                    passes.push(dirswitch_pass(&svc, &rx, &mut ui, &setup, &o, size));
                }
                _ => {
                    passes.push(enter_pass(&svc, &rx, &mut ui, &setup, &o, "cold"));
                    if condition == "warm" {
                        // 同一进程再次进入：内存缓存命中
                        passes.push(enter_pass(
                            &svc,
                            &rx,
                            &mut ui,
                            &setup,
                            &o,
                            "warm_same_process",
                        ));
                    }
                }
            }
            final_counters = svc.counters();
            checks.insert("mem_cache_entries".into(), svc.mem_entries().into());
        }
        "restart" => {
            // 不清缓存：依赖之前某次进程写入的磁盘缓存；新进程内存缓存为空
            cache_before = disk.as_ref().map(|d| d.entries());
            if cache_before.is_none_or(|(n, _)| n == 0) {
                valid = false;
                note = "磁盘缓存为空：这不是“重启后”条件，请先运行一次 cold/warm 并保持同一 --cache-dir".into();
            }
            let (svc, rx) = make(&disk);
            let mut ui = Ui::new();
            passes.push(enter_pass(&svc, &rx, &mut ui, &setup, &o, "restart"));
            final_counters = svc.counters();
        }
        other => {
            eprintln!("未知条件 {other}");
            return 2;
        }
    }
    let m = mem();
    final_counters.failed.retain(|_, v| *v > 0);
    let out = serde_json::json!({
        "kind": format!("scenario-{kind}"),
        "condition": condition,
        "valid": valid,
        "note": note,
        "config": {
            "route": route.name(), "workers": workers, "thumb_size": size, "first_screen_items": first,
            "prefetch": prefetch, "slow_after_ms": ms(slow_after), "hard_timeout_ms": ms(hard_timeout),
            "max_extra_workers": max_extra, "cache_format": cache_fmt,
            "injected_slow": inject.iter().map(|(i, d)| serde_json::json!({"index": i, "delay_ms": ms(*d)})).collect::<Vec<_>>(),
        },
        "cache_before": cache_before.map(|(n, b)| serde_json::json!({"entries": n, "bytes": b})),
        "cache_after": disk.as_ref().map(|d| { let (n, b) = d.entries(); serde_json::json!({"entries": n, "bytes": b}) }),
        "os_file_cache": "未控制：夹具刚生成或刚读过，原始文件大概率在系统文件缓存里；真正冷读需要管理员清备用列表，本次未测量",
        "dir_items": setup.items.len(),
        "passes": passes,
        "counters_total": counters_json(&final_counters),
        "checks": checks,
        "memory": {
            "baseline_commit_mib": base_mem.commit,
            "end": mem_json(&m),
            "peak_commit_over_baseline_mib": ((m.peak_commit - base_mem.commit) * 10.0).round() / 10.0,
        },
        "scope": "无头：不含 Slint 图像上传与渲染，是整机首屏的下界，不是整机 UI 验收成绩",
    });
    println!("{out}");
    if let Some(p) = arg(args, "out") {
        let _ = std::fs::write(p, serde_json::to_string_pretty(&out).unwrap());
    }
    if valid { 0 } else { 3 }
}

/// 滚动：进入目录后很快跳到远处。验证排队中的旧请求被取消、新可见项优先、没有旧请求在滚动后才开始。
fn scroll_pass(
    svc: &Service,
    rx: &Receiver<Done>,
    ui: &mut Ui,
    s: &Setup,
    o: &PassOpts,
    args: &[String],
) -> serde_json::Value {
    ui.reset();
    let after = Duration::from_millis(arg_parse(args, "scroll-after-ms", 40));
    let n = s.items.len();
    let to: usize = arg_parse(args, "scroll-to", (n / 2).min(n.saturating_sub(o.first)));
    let first = o.first;
    let t0 = Instant::now();
    let epoch = ui.timed(|| svc.enter_dir(to_items(s)));
    bind(ui, epoch, s);
    ui.timed(|| svc.set_viewport(epoch, 0, first));
    while t0.elapsed() < after {
        ui.poll(rx, Duration::from_micros(300));
    }
    let c_before = svc.counters();
    let old_resolved_at_scroll = ui.slots[..first]
        .iter()
        .filter(|x| !matches!(x, Slot::Pending))
        .count();
    let t_scroll = Instant::now();
    ui.timed(|| svc.set_viewport(epoch, to, to + first));
    let cap = Duration::from_secs(20);
    while !ui.resolved(to..(to + first).min(n)) && t_scroll.elapsed() < cap {
        ui.poll(rx, Duration::from_micros(300));
    }
    let end = (to + first).min(n);
    let new_ready = ui.slots[to..end]
        .iter()
        .filter_map(|x| match x {
            Slot::Ready { at, .. } => Some(*at),
            _ => None,
        })
        .max()
        .map(|t| round2(ms(t - t_scroll)));
    // 等剩余在途请求落地，便于核对计数
    let t_idle = Instant::now();
    while t_idle.elapsed() < Duration::from_millis(600) {
        ui.poll(rx, Duration::from_millis(5));
    }
    svc.flush();
    let c_after = svc.counters();
    // 旧首屏里没有在滚动时刻完成、也不在新视口的项：理应从未开始
    let old_never_started = ui.slots[..first]
        .iter()
        .enumerate()
        .filter(|(i, x)| matches!(x, Slot::Pending) && *i < to.saturating_sub(0))
        .count();
    serde_json::json!({
        "pass": "scroll",
        "scroll_after_ms": ms(after),
        "scroll_to_index": to,
        "old_first_screen_resolved_at_scroll": old_resolved_at_scroll,
        "old_first_screen_still_pending": old_never_started,
        "new_first_screen_ready_ms_after_scroll": new_ready,
        "new_first_screen_ready_count": ui.slots[to..end].iter().filter(|x| matches!(x, Slot::Ready{..})).count(),
        "new_first_screen_items": end - to,
        "counters_at_scroll": counters_delta(&c_before, &c_after),
        "started_unwanted_total": c_after.started_unwanted,
        "applied": ui.applied, "dropped_old_epoch": ui.dropped_old_epoch, "key_mismatch": ui.key_mismatch,
        "ui_thread": {"calls": ui.call_us.len(), "call_max_us": ui.call_us.iter().max()},
        "stage_ms_decoded_items": stage_json(&ui.decode_stage),
    })
}

/// 换目录：旧目录的在途请求完成后带旧 epoch，必须丢弃；新目录同下标项的绑定必须是新文件（用反序列表保证下标对应不同文件）。
fn dirswitch_pass(
    svc: &Service,
    rx: &Receiver<Done>,
    ui: &mut Ui,
    s: &Setup,
    o: &PassOpts,
    size: u32,
) -> serde_json::Value {
    let first = o.first.min(s.items.len());
    ui.reset();
    let epoch1 = ui.timed(|| svc.enter_dir(to_items(s)));
    bind(ui, epoch1, s);
    ui.timed(|| svc.set_viewport(epoch1, 0, first));
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(30) {
        ui.poll(rx, Duration::from_micros(300));
    }
    let s2 = build_items(&PathBuf::from(dir_of(s)), size, true);
    let t2 = Instant::now();
    let epoch2 = ui.timed(|| svc.enter_dir(to_items(&s2)));
    bind(ui, epoch2, &s2);
    let c_before = svc.counters();
    ui.timed(|| svc.set_viewport(epoch2, 0, first));
    while !ui.resolved(0..first) && t2.elapsed() < o.wait_cap {
        ui.poll(rx, Duration::from_micros(300));
    }
    let ready2 = ui.slots[..first]
        .iter()
        .filter_map(|x| match x {
            Slot::Ready { at, .. } => Some(*at),
            _ => None,
        })
        .max()
        .map(|t| round2(ms(t - t2)));
    let t_idle = Instant::now();
    while t_idle.elapsed() < Duration::from_millis(600) {
        ui.poll(rx, Duration::from_millis(5));
    }
    svc.flush();
    let c_after = svc.counters();
    serde_json::json!({
        "pass": "dirswitch",
        "second_dir_first_screen_ready_ms": ready2,
        "second_dir_ready_count": ui.slots[..first].iter().filter(|x| matches!(x, Slot::Ready{..})).count(),
        "first_screen_items": first,
        "counters_after_switch": counters_delta(&c_before, &c_after),
        "dropped_old_epoch": ui.dropped_old_epoch,
        "key_mismatch": ui.key_mismatch,
        "applied": ui.applied,
        "started_unwanted_total": c_after.started_unwanted,
    })
}

fn dir_of(s: &Setup) -> String {
    s.items[0]
        .0
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}
