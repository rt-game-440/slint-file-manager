//! 真实窗口的帧耗时与关键界面事件日志（调试开关，默认关闭）。
//!
//! - 开启：命令行 `--debug-perf` 或环境变量 `SFM_DEBUG_PERF=1`。关闭时不创建文件、不注册渲染通知，
//!   `event()` 只做一次 `OnceLock` 读取即返回。
//! - 位置：`$SFM_LOG_DIR`（缺省为当前目录下的 `logs`），文件名 `yyyy-MM-dd HH-mm-ss.log`，一次启动一个文件。
//! - 隐私：`event()` 只接受 `&'static str` 事件名和一个整数，调用方没有途径写入路径、文件名或内容。

use slint::{ComponentHandle, RenderingState};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// 动画期间，相邻两帧间隔超过该毫秒数记为掉帧。可用 `SFM_PERF_DROP_MS` 覆盖。
const DEFAULT_DROP_MS: f64 = 25.0;

struct Inner {
    out: BufWriter<File>,
    start: Instant,
    animating: bool,
    last_before: Option<Instant>,
    last_animating: bool,
    frame_no: u64,
    dropped: u64,
    intervals: Vec<f64>,
    drop_ms: f64,
    /// 等待下一帧渲染完成后记录的事件：(名称, 起点)
    pending: Vec<(&'static str, Instant)>,
}

static LOG: OnceLock<Mutex<Inner>> = OnceLock::new();

/// 命令行参数或环境变量是否要求开启。
pub fn requested(args: &[String]) -> bool {
    args.iter().any(|a| a == "--debug-perf")
        || std::env::var("SFM_DEBUG_PERF").is_ok_and(|v| v == "1")
}

impl Inner {
    fn line(&mut self, text: &str) {
        let wall = chrono::Local::now().format("%H:%M:%S%.3f");
        let ms = self.start.elapsed().as_secs_f64() * 1000.0;
        let _ = writeln!(self.out, "{wall} +{ms:.3}ms {text}");
    }
}

/// 创建日志文件并注册渲染通知。返回日志路径。
pub fn init(window: &slint::Window) -> std::io::Result<PathBuf> {
    let dir = PathBuf::from(std::env::var("SFM_LOG_DIR").unwrap_or_else(|_| "logs".into()));
    fs::create_dir_all(&dir)?;
    let name = chrono::Local::now().format("%Y-%m-%d %H-%M-%S").to_string();
    let path = dir.join(format!("{name}.log"));
    let drop_ms = std::env::var("SFM_PERF_DROP_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_DROP_MS);
    let inner = Inner {
        out: BufWriter::new(File::create(&path)?),
        start: Instant::now(),
        animating: false,
        last_before: None,
        last_animating: false,
        frame_no: 0,
        dropped: 0,
        intervals: Vec::new(),
        drop_ms,
        pending: Vec::new(),
    };
    if LOG.set(Mutex::new(inner)).is_err() {
        return Ok(path); // 同一进程只初始化一次，复用同一文件
    }
    with(|l| {
        l.line(&format!(
            "start version={} drop_threshold_ms={drop_ms}",
            env!("CARGO_PKG_VERSION")
        ));
    });

    let mut before: Option<Instant> = None;
    let res = window.set_rendering_notifier(move |state, api| match state {
        // 实际使用的图形接口（Issue #39：区分 femtovg/OpenGL 与软件渲染；软件渲染不会触发这一步）
        RenderingState::RenderingSetup => {
            let name = match api {
                slint::GraphicsAPI::NativeOpenGL { .. } => "opengl",
                _ => "other",
            };
            with(|l| l.line(&format!("renderer graphics_api={name}")));
        }
        RenderingState::BeforeRendering => before = Some(Instant::now()),
        RenderingState::AfterRendering => {
            if let Some(b) = before.take() {
                frame(b);
            }
        }
        _ => {}
    });
    if let Err(e) = res {
        with(|l| l.line(&format!("warn rendering-notifier-unavailable reason={e:?}")));
    }
    Ok(path)
}

fn with(f: impl FnOnce(&mut Inner)) {
    if let Some(m) = LOG.get()
        && let Ok(mut g) = m.lock()
    {
        f(&mut g);
    }
}

fn frame(before: Instant) {
    with(|l| {
        let now = Instant::now();
        let render_ms = (now - before).as_secs_f64() * 1000.0;
        let interval = l.last_before.map(|p| (before - p).as_secs_f64() * 1000.0);
        l.last_before = Some(before);
        l.frame_no += 1;
        let animating = l.animating;
        // 只有连续两帧都处在动画期间，间隔才有意义；动画前后的空闲间隔不算掉帧。
        let eligible = animating && l.last_animating;
        let dropped = eligible && interval.is_some_and(|i| i > l.drop_ms);
        l.last_animating = animating;
        if dropped {
            l.dropped += 1;
        }
        if eligible && let Some(i) = interval {
            l.intervals.push(i);
        }
        let n = l.frame_no;
        let iv = interval.map_or("na".to_string(), |i| format!("{i:.2}"));
        l.line(&format!(
            "frame n={n} interval_ms={iv} render_ms={render_ms:.2} animating={} dropped={}",
            animating as u8, dropped as u8
        ));
        for (name, since) in std::mem::take(&mut l.pending) {
            let us = (now - since).as_micros() as i64;
            l.line(&format!("event {name} value={us}"));
        }
        if n % 64 == 0 {
            let _ = l.out.flush();
        }
    });
}

/// 调试日志是否开启。
pub fn enabled() -> bool {
    LOG.get().is_some()
}

/// 在下一帧渲染完成时记录 `event 名称 value=微秒`（自 `since` 起）。用于“进入目录到可操作”这类
/// 需要以画面实际更新为终点的计时。日志关闭时直接返回。
pub fn mark_next_frame(name: &'static str, since: Instant) {
    with(|l| l.pending.push((name, since)));
}

/// 记录关键界面事件。名称必须是静态字符串，数值可选。
pub fn event(name: &'static str, value: Option<i64>) {
    with(|l| {
        match value {
            Some(v) => l.line(&format!("event {name} value={v}")),
            None => l.line(&format!("event {name}")),
        }
        let _ = l.out.flush();
    });
}

/// 标记动画期间，用于判定掉帧。
pub fn set_animating(on: bool) {
    with(|l| {
        if on {
            // 每次动画从头开始：与上一次动画最后一帧之间的空闲间隔不参与掉帧判定
            l.last_animating = false;
        }
        l.animating = on;
    });
}

/// 写出汇总并刷新文件，进程退出前调用。
pub fn finish() {
    with(|l| {
        let mut v = l.intervals.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pick = |q: f64| {
            v.get(((v.len() as f64 - 1.0) * q).round() as usize)
                .copied()
        };
        let fmt = |x: Option<f64>| x.map_or("na".to_string(), |x| format!("{x:.2}"));
        let (frames, dropped) = (l.frame_no, l.dropped);
        let (p50, p95, max) = (fmt(pick(0.5)), fmt(pick(0.95)), fmt(v.last().copied()));
        let n = v.len();
        l.line(&format!(
            "summary frames={frames} animating_intervals={n} p50_ms={p50} p95_ms={p95} max_ms={max} dropped={dropped}"
        ));
        let _ = l.out.flush();
    });
}

/// 供 main 使用：按需初始化并连接窗口。
pub fn setup_if_requested<C: ComponentHandle>(component: &C, args: &[String]) -> Option<PathBuf> {
    if !requested(args) {
        return None;
    }
    match init(component.window()) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("debug-perf: 无法创建日志文件：{e}");
            None
        }
    }
}
