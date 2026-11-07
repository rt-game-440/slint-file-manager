//! 缩略图的界面侧：内存缓存、按面板可见范围生成请求、把结果按文件身份（键）写回对应行。
//!
//! - 请求：只为可见面板、网格视图、已读取完成的当前标签生成。可见项优先（活动面板 > 另一面板），
//!   其后是可见范围之后一屏、之前半屏的预取。视口变化即用新列表替换该面板的排队请求（旧的排队请求作废）。
//! - 绑定：结果只带键（路径 + 大小 + 修改时间）。界面按键查行，换目录、滚动、文件被改名或修改后，
//!   旧结果找不到对应行，只进内存缓存，不会贴到别的文件上。
//! - 状态：`loading`（排队或生成中）、`ready`、`failed`（格式不支持、损坏、过大、云端占位）、
//!   `timeout`（超过 2 秒，先显示图标；之后成功会补图）。
//! - 内存缓存上限 [`MEM_BUDGET`]，按最近使用淘汰，当前请求窗口内的条目不淘汰。
//! - 首屏计时（`--debug-perf`）：进入目录后，活动面板首屏可见的图片全部有结果（成功、失败或超时）后，
//!   下一帧渲染完成时记录 `thumbs-first-screen-us`（自导航请求起算），并记录首屏数量与来源。
//! - 持久缓存与容量设置见 [`crate::thumbs::cache`]；容量写在数据目录的 `settings.json`。

use crate::browse::{LoadState, ViewMode as CoreView, Workspace};
use crate::thumbs::cache::{self, DiskCache};
use crate::thumbs::queue::{Job, rank};
use crate::thumbs::sched::{self, Done, Manual, Scheduler};
use crate::thumbs::{self, Fail, Source, Thumb};
use crate::{ThumbState, perf};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// 内存缓存上限（字节）。256×256 RGBA 约 256 KiB，约可容纳 500 张。
pub const MEM_BUDGET: usize = 128 << 20;

const MIB: u64 = 1 << 20;

/// 生成方式。
pub enum ThumbMode {
    /// 不生成缩略图（所有图片显示类型图标）。
    Off,
    /// 工作线程池（产品）。
    Threaded,
    /// 单线程、由 [`super::Controller::pump_thumbs`] 驱动（harness）。参数判定“慢文件”（先超时，之后可补图）。
    Manual(Option<sched::SlowFn>),
}

/// 缩略图相关的外部环境。
pub struct ThumbEnv {
    pub mode: ThumbMode,
    /// 持久缓存目录；`None` 时只用内存缓存。
    pub cache_dir: Option<PathBuf>,
    /// 保存容量设置的文件（数据目录下的 `settings.json`）；`None` 时不保存。
    pub settings_file: Option<PathBuf>,
}

impl ThumbEnv {
    pub fn off() -> ThumbEnv {
        ThumbEnv {
            mode: ThumbMode::Off,
            cache_dir: None,
            settings_file: None,
        }
    }

    /// 产品：缓存目录为 `SFM_THUMB_CACHE_DIR` 或 `数据目录\thumbs`，设置在 `数据目录\settings.json`。
    pub fn production(data_dir: Option<&Path>) -> ThumbEnv {
        ThumbEnv {
            mode: ThumbMode::Threaded,
            cache_dir: thumbs::cache_dir(data_dir),
            settings_file: data_dir.map(|d| d.join("settings.json")),
        }
    }
}

// 只有一个实例，手动模式（harness）的体积无关紧要
#[allow(clippy::large_enum_variant)]
enum Backend {
    Off,
    Threaded(Scheduler),
    Manual(RefCell<Manual>),
}

enum Slot {
    Ready(slint::Image),
    Failed,
    Timeout,
}

struct Entry {
    slot: Slot,
    bytes: usize,
    tick: u64,
}

struct FirstScreen {
    t0: Instant,
    keys: HashSet<u64>,
}

/// 读取 `settings.json` 里的缓存容量（MiB）；缺失或无效时用默认值。
fn load_capacity(file: Option<&Path>) -> u64 {
    file.and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("thumbnail_cache_mb")?.as_u64())
        .filter(|mb| cache::CAPACITY_CHOICES_MB.contains(mb))
        .unwrap_or(cache::DEFAULT_CAPACITY_MB)
}

/// 写入容量设置，保留文件中的其他字段。先写临时文件再改名。
fn save_capacity(file: &Path, mb: u64) -> std::io::Result<()> {
    let mut v = std::fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    v["thumbnail_cache_mb"] = serde_json::json!(mb);
    if let Some(d) = file.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&v).unwrap_or_default())?;
    std::fs::rename(&tmp, file)
}

/// 字节数的简短文字。
pub fn bytes_text(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.1} GB", b as f64 / (1u64 << 30) as f64)
    } else if b >= MIB {
        format!("{:.1} MB", b as f64 / MIB as f64)
    } else {
        format!("{} KB", b.div_ceil(1024))
    }
}

fn to_image(t: &Thumb) -> slint::Image {
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&t.rgba, t.w, t.h);
    slint::Image::from_rgba8(buf)
}

pub struct Thumbs {
    backend: Backend,
    disk: Option<Arc<DiskCache>>,
    disk_error: Option<String>,
    settings_file: Option<PathBuf>,
    capacity_mb: Cell<u64>,
    mem: RefCell<HashMap<u64, Entry>>,
    mem_bytes: Cell<usize>,
    tick: Cell<u64>,
    /// 每个面板当前请求窗口（可见 + 预取）里图片的键 → 行号
    rows: [RefCell<HashMap<u64, usize>>; 2],
    /// 每个面板最近一次交给调度器的请求键（相同则不重复提交）
    last: [RefCell<Vec<u64>>; 2],
    /// 已进入目录、等待首屏结果的计时起点
    armed: Cell<Option<Instant>>,
    /// 最近一次 [`Thumbs::arm`] 的起点：同一次导航的再次读取（目录监视的静默刷新）不重新计时
    last_armed: Cell<Option<Instant>>,
    first: RefCell<Option<FirstScreen>>,
}

impl Thumbs {
    /// `sink` 只在线程模式下使用：结果从工作线程投递回界面线程。
    pub fn new(env: ThumbEnv, sink: sched::Sink) -> Thumbs {
        let capacity_mb = load_capacity(env.settings_file.as_deref());
        let enabled = !matches!(env.mode, ThumbMode::Off);
        let background = matches!(env.mode, ThumbMode::Threaded);
        let (disk, disk_error) = match env.cache_dir.filter(|_| enabled) {
            Some(d) => match DiskCache::open(&d, capacity_mb * MIB, background) {
                Ok(c) => (Some(c), None),
                Err(e) => (None, Some(format!("持久缓存不可用（{e}），只使用内存缓存"))),
            },
            None => (None, None),
        };
        let backend = match env.mode {
            ThumbMode::Off => Backend::Off,
            ThumbMode::Threaded => {
                Backend::Threaded(Scheduler::new(sched::Config::product(disk.clone()), sink))
            }
            ThumbMode::Manual(slow) => {
                Backend::Manual(RefCell::new(Manual::new(disk.clone(), slow)))
            }
        };
        Thumbs {
            backend,
            disk,
            disk_error,
            settings_file: env.settings_file,
            capacity_mb: Cell::new(capacity_mb),
            mem: RefCell::new(HashMap::new()),
            mem_bytes: Cell::new(0),
            tick: Cell::new(0),
            rows: Default::default(),
            last: Default::default(),
            armed: Cell::new(None),
            last_armed: Cell::new(None),
            first: RefCell::new(None),
        }
    }

    pub fn enabled(&self) -> bool {
        !matches!(self.backend, Backend::Off)
    }

    pub fn disk(&self) -> Option<&Arc<DiskCache>> {
        self.disk.as_ref()
    }

    /// 内存缓存中的条目数与字节数（harness 与日志）。
    pub fn mem_usage(&self) -> (usize, usize) {
        (self.mem.borrow().len(), self.mem_bytes.get())
    }

    /// 某个文件在界面上的缩略图状态。
    pub fn lookup(&self, key: u64) -> (ThumbState, slint::Image) {
        let mut mem = self.mem.borrow_mut();
        match mem.get_mut(&key) {
            None => (ThumbState::Loading, slint::Image::default()),
            Some(e) => {
                let t = self.tick.get() + 1;
                self.tick.set(t);
                e.tick = t;
                match &e.slot {
                    Slot::Ready(img) => (ThumbState::Ready, img.clone()),
                    Slot::Failed => (ThumbState::Failed, slint::Image::default()),
                    Slot::Timeout => (ThumbState::Timeout, slint::Image::default()),
                }
            }
        }
    }

    /// 进入目录后开始等待首屏结果（下一次 [`Thumbs::refresh`] 确定首屏范围）。
    pub fn arm(&self, t0: Instant) {
        if self.last_armed.replace(Some(t0)) == Some(t0) {
            return;
        }
        self.armed.set(Some(t0));
        *self.first.borrow_mut() = None;
    }

    /// 按两个面板的可见范围（`[first, end)`，来自界面）重新生成请求。
    pub fn refresh(&self, ws: &Workspace, ranges: [(i32, i32); 2]) {
        if !self.enabled() {
            return;
        }
        let active = ws.active_pane();
        let mut first_keys: Option<HashSet<u64>> = None;
        for (p, &(f, e)) in ranges.iter().enumerate() {
            let mut rows = HashMap::new();
            let mut jobs = Vec::new();
            let mut visible_keys = HashSet::new();
            let t = ws.active_tab(p);
            let shown = (ws.is_split() || active == p)
                && t.view() == CoreView::Grid
                && *t.state() == LoadState::Ready;
            if shown && let Some(loc) = t.location() {
                let all = t.rows();
                let n = all.len();
                let f = usize::try_from(f).unwrap_or(0).min(n);
                let e = usize::try_from(e).unwrap_or(0).clamp(f, n);
                let span = e - f;
                let vis_rank = if p == active {
                    rank::VISIBLE_ACTIVE
                } else {
                    rank::VISIBLE_OTHER
                };
                let order = (f..e)
                    .map(|i| (i, vis_rank))
                    .chain((e..(e + span).min(n)).map(|i| (i, rank::AHEAD)))
                    .chain(
                        (f.saturating_sub(span / 2)..f)
                            .rev()
                            .map(|i| (i, rank::BEHIND)),
                    );
                let mem = self.mem.borrow();
                for (i, r) in order {
                    let en = &all[i].entry;
                    if en.is_dir || en.bin.is_some() || !thumbs::wants_thumbnail(&en.ext) {
                        continue;
                    }
                    let src = Source::new(loc.join(&en.os_name), en.size, en.modified);
                    let key = src.key();
                    rows.insert(key, i);
                    if r <= rank::VISIBLE_OTHER {
                        visible_keys.insert(key);
                    }
                    if !mem.contains_key(&key) {
                        jobs.push(Job {
                            key,
                            src,
                            pane: p,
                            rank: r,
                        });
                    }
                }
            }
            if p == active && shown {
                first_keys = Some(visible_keys);
            }
            *self.rows[p].borrow_mut() = rows;
            let keys: Vec<u64> = jobs.iter().map(|j| j.key).collect();
            if *self.last[p].borrow() != keys {
                *self.last[p].borrow_mut() = keys;
                match &self.backend {
                    Backend::Off => {}
                    Backend::Threaded(s) => s.set_wanted(p, jobs),
                    Backend::Manual(m) => m.borrow_mut().set_wanted(p, jobs),
                }
            }
        }
        // 首屏：进入目录后第一次确定活动面板的可见范围；完成前范围变化（例如布局稳定）则以新范围为准
        let tracking = self.armed.get().is_some() || self.first.borrow().is_some();
        if tracking {
            match first_keys {
                Some(keys) if !keys.is_empty() => {
                    let t0 = self
                        .armed
                        .take()
                        .or_else(|| self.first.borrow().as_ref().map(|f| f.t0));
                    if let Some(t0) = t0 {
                        *self.first.borrow_mut() = Some(FirstScreen { t0, keys });
                    }
                    self.check_first_screen();
                }
                // 活动面板没有图片（或不是网格视图）：本次导航不计首屏
                _ => {
                    self.armed.set(None);
                    *self.first.borrow_mut() = None;
                }
            }
        }
    }

    /// 首屏全部有结果时记录计时与数量，结束跟踪。
    fn check_first_screen(&self) {
        let done = {
            let first = self.first.borrow();
            let Some(f) = first.as_ref() else { return };
            let mem = self.mem.borrow();
            if !f.keys.iter().all(|k| mem.contains_key(k)) {
                return;
            }
            let (mut ready, mut failed, mut timeout) = (0i64, 0i64, 0i64);
            for k in &f.keys {
                match mem.get(k).map(|e| &e.slot) {
                    Some(Slot::Ready(_)) => ready += 1,
                    Some(Slot::Failed) => failed += 1,
                    Some(Slot::Timeout) => timeout += 1,
                    None => {}
                }
            }
            (f.t0, f.keys.len() as i64, ready, failed, timeout)
        };
        *self.first.borrow_mut() = None;
        if perf::enabled() {
            let (t0, n, ready, failed, timeout) = done;
            perf::event("thumbs-first-screen-items", Some(n));
            perf::event("thumbs-first-screen-ready", Some(ready));
            perf::event("thumbs-first-screen-failed", Some(failed));
            perf::event("thumbs-first-screen-timeout", Some(timeout));
            perf::mark_next_frame("thumbs-first-screen-us", t0);
        }
    }

    /// 应用一个生成结果，返回需要刷新的 (面板, 行)。
    pub fn apply(&self, d: Done) -> Vec<(usize, usize)> {
        {
            let mut mem = self.mem.borrow_mut();
            let ready_already = matches!(
                mem.get(&d.key),
                Some(Entry {
                    slot: Slot::Ready(_),
                    ..
                })
            );
            let (slot, bytes) = match &d.outcome {
                Ok(t) => (Slot::Ready(to_image(t)), t.bytes()),
                Err(_) if ready_already => return Vec::new(),
                Err(Fail::Timeout) => (Slot::Timeout, 64),
                Err(_) => (Slot::Failed, 64),
            };
            let t = self.tick.get() + 1;
            self.tick.set(t);
            if let Some(old) = mem.insert(
                d.key,
                Entry {
                    slot,
                    bytes,
                    tick: t,
                },
            ) {
                self.mem_bytes.set(self.mem_bytes.get() - old.bytes);
            }
            self.mem_bytes.set(self.mem_bytes.get() + bytes);
        }
        self.evict();
        let out = (0..2)
            .filter_map(|p| self.rows[p].borrow().get(&d.key).map(|&i| (p, i)))
            .collect();
        self.check_first_screen();
        out
    }

    /// 超过内存上限时按最近使用淘汰到 90%，当前请求窗口内的条目保留。
    fn evict(&self) {
        if self.mem_bytes.get() <= MEM_BUDGET {
            return;
        }
        let keep: HashSet<u64> = self
            .rows
            .iter()
            .flat_map(|r| r.borrow().keys().copied().collect::<Vec<_>>())
            .collect();
        let mut mem = self.mem.borrow_mut();
        let mut order: Vec<(u64, u64)> = mem
            .iter()
            .filter(|(k, _)| !keep.contains(k))
            .map(|(k, e)| (e.tick, *k))
            .collect();
        order.sort_unstable();
        let target = MEM_BUDGET / 10 * 9;
        for (_, k) in order {
            if self.mem_bytes.get() <= target {
                break;
            }
            if let Some(e) = mem.remove(&k) {
                self.mem_bytes.set(self.mem_bytes.get() - e.bytes);
            }
        }
    }

    /// 手动模式：执行全部排队请求，返回结果（线程模式返回空）。
    pub fn run_manual(&self) -> Vec<Done> {
        match &self.backend {
            Backend::Manual(m) => m.borrow_mut().run(),
            _ => Vec::new(),
        }
    }

    /// 手动模式：完成之前超时的慢请求（迟到补图）。
    pub fn finish_slow_manual(&self) -> Vec<Done> {
        match &self.backend {
            Backend::Manual(m) => m.borrow_mut().finish_slow(),
            _ => Vec::new(),
        }
    }

    /// 手动模式下排队中的请求数。
    pub fn pending_manual(&self) -> usize {
        match &self.backend {
            Backend::Manual(m) => m.borrow().pending(),
            _ => 0,
        }
    }

    /// 调度计数（日志与 harness）。
    pub fn counters(&self) -> sched::Counters {
        match &self.backend {
            Backend::Off => sched::Counters::default(),
            Backend::Threaded(s) => s.counters(),
            Backend::Manual(m) => m.borrow().counters.clone(),
        }
    }

    /// 清空内存缓存并忘记已提交的请求（之后的 refresh 会重新请求可见项）。
    pub fn clear_memory(&self) {
        self.mem.borrow_mut().clear();
        self.mem_bytes.set(0);
        for l in &self.last {
            l.borrow_mut().clear();
        }
    }

    pub fn capacity_mb(&self) -> u64 {
        self.capacity_mb.get()
    }

    /// 设置持久缓存容量并保存设置。返回保存失败的说明。淘汰在后台线程执行（线程模式）。
    pub fn set_capacity_mb(&self, mb: u64) -> Result<(), String> {
        if !cache::CAPACITY_CHOICES_MB.contains(&mb) {
            return Err(format!("不支持的容量：{mb} MB"));
        }
        self.capacity_mb.set(mb);
        if let Some(d) = &self.disk {
            if matches!(self.backend, Backend::Threaded(_)) {
                let d = d.clone();
                std::thread::spawn(move || d.set_capacity(mb * MIB));
            } else {
                d.set_capacity(mb * MIB);
            }
        }
        match &self.settings_file {
            Some(f) => save_capacity(f, mb).map_err(|e| format!("缩略图缓存设置保存失败：{e}")),
            None => Ok(()),
        }
    }

    /// “更多”菜单里的占用说明。
    pub fn usage_text(&self) -> String {
        if !self.enabled() {
            return "缩略图已关闭".into();
        }
        if let Some(e) = &self.disk_error {
            return e.clone();
        }
        let Some(d) = &self.disk else {
            return "未启用持久缓存（只使用内存缓存）".into();
        };
        let u = d.usage();
        let used = if u.scanned {
            format!("已用 {}（{} 张）", bytes_text(u.bytes), u.files)
        } else {
            "正在统计占用…".to_string()
        };
        let mb = u.capacity / MIB;
        let cap = if mb >= 1024 && mb.is_multiple_of(1024) {
            format!("{} GB", mb / 1024)
        } else {
            format!("{mb} MB")
        };
        format!("{used}，上限 {cap}")
    }

    /// 是否在后台线程执行磁盘清空（线程模式）。
    pub fn threaded(&self) -> bool {
        matches!(self.backend, Backend::Threaded(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_setting_roundtrip_keeps_other_fields() {
        let dir = std::env::temp_dir().join(format!("sfm-thumb-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let f = dir.join("settings.json");
        assert_eq!(load_capacity(Some(&f)), cache::DEFAULT_CAPACITY_MB);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&f, br#"{"other": 1}"#).unwrap();
        save_capacity(&f, 1024).unwrap();
        assert_eq!(load_capacity(Some(&f)), 1024);
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&f).unwrap()).unwrap();
        assert_eq!(v["other"], 1);
        // 无效值回到默认
        std::fs::write(&f, br#"{"thumbnail_cache_mb": 3}"#).unwrap();
        assert_eq!(load_capacity(Some(&f)), cache::DEFAULT_CAPACITY_MB);
        std::fs::write(&f, b"not json").unwrap();
        assert_eq!(load_capacity(Some(&f)), cache::DEFAULT_CAPACITY_MB);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bytes_text_units() {
        assert_eq!(bytes_text(512), "1 KB");
        assert_eq!(bytes_text(3 * MIB / 2), "1.5 MB");
        assert_eq!(bytes_text(2 << 30), "2.0 GB");
    }
}
