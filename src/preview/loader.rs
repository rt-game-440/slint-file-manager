//! 预览加载器：一个后台线程，只处理“最新的一个请求”。
//!
//! 用户连按左右键时，旧请求在轮到之前就被新请求取代而跳过；正在进行的解码无法中断，但同一时刻至多一个，
//! 完成后若已过期（代号不是最新）由接收方丢弃。这样快速切换既不堆积线程也不堆积内存。
//! 转成 Slint 像素缓冲在后台线程完成，界面线程只做包装。

use super::{Decoded, Failure, Limits, decode_file};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

/// 可以直接交给界面的结果。
pub struct Prepared {
    pub frames: Vec<SharedPixelBuffer<Rgba8Pixel>>,
    pub delays_ms: Vec<u32>,
    pub note: Option<String>,
    pub w: u32,
    pub h: u32,
}

pub type Outcome = Result<Prepared, Failure>;

/// 请求完成的回调：`(请求代号, 路径, 结果)`，在后台线程调用。
pub type Sink = Arc<dyn Fn(u64, PathBuf, Outcome) + Send + Sync>;

pub fn prepare(d: Decoded) -> Prepared {
    let (w, h) = (d.width(), d.height());
    let frames = d
        .frames
        .into_iter()
        .map(|f| SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&f.rgba, f.w, f.h))
        .collect();
    Prepared {
        frames,
        delays_ms: d.delays_ms,
        note: d.note,
        w,
        h,
    }
}

/// 同步执行一个请求（harness 的手动模式与后台线程共用）。
pub fn run(path: &std::path::Path, lim: &Limits) -> Outcome {
    decode_file(path, lim).map(prepare)
}

struct Req {
    gen_: u64,
    path: PathBuf,
}

struct State {
    req: Option<Req>,
    shutdown: bool,
    /// 已开始的解码数（测试用）
    started: u64,
    /// 因被新请求取代而跳过的请求数（测试用）
    skipped: u64,
    busy: bool,
}

struct Shared {
    m: Mutex<State>,
    cv: Condvar,
}

/// 后台加载器。丢弃时通知线程退出（不等待正在进行的解码）。
pub struct Loader {
    sh: Arc<Shared>,
}

impl Loader {
    pub fn new(sink: Sink, lim: Limits) -> Loader {
        let sh = Arc::new(Shared {
            m: Mutex::new(State {
                req: None,
                shutdown: false,
                started: 0,
                skipped: 0,
                busy: false,
            }),
            cv: Condvar::new(),
        });
        let w = sh.clone();
        let _ = std::thread::Builder::new()
            .name("preview-loader".into())
            .spawn(move || worker(w, sink, lim));
        Loader { sh }
    }

    /// 提交请求；取代尚未开始的旧请求。
    pub fn request(&self, gen_: u64, path: PathBuf) {
        let mut g = self.sh.m.lock().unwrap_or_else(|e| e.into_inner());
        if g.req.is_some() {
            g.skipped += 1;
        }
        g.req = Some(Req { gen_, path });
        drop(g);
        self.sh.cv.notify_all();
    }

    /// 撤销尚未开始的请求（关闭预览时）。
    pub fn cancel(&self) {
        let mut g = self.sh.m.lock().unwrap_or_else(|e| e.into_inner());
        if g.req.take().is_some() {
            g.skipped += 1;
        }
    }

    /// `(已开始, 被跳过)`。
    pub fn counters(&self) -> (u64, u64) {
        let g = self.sh.m.lock().unwrap_or_else(|e| e.into_inner());
        (g.started, g.skipped)
    }

    /// 有请求在排队或解码在进行。
    pub fn busy(&self) -> bool {
        let g = self.sh.m.lock().unwrap_or_else(|e| e.into_inner());
        g.req.is_some() || g.busy
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        let mut g = self.sh.m.lock().unwrap_or_else(|e| e.into_inner());
        g.shutdown = true;
        g.req = None;
        drop(g);
        self.sh.cv.notify_all();
    }
}

fn worker(sh: Arc<Shared>, sink: Sink, lim: Limits) {
    loop {
        let req = {
            let mut g = sh.m.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if g.shutdown {
                    return;
                }
                if let Some(r) = g.req.take() {
                    g.started += 1;
                    g.busy = true;
                    break r;
                }
                g = sh.cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
        };
        let out = run(&req.path, &lim);
        sh.m.lock().unwrap_or_else(|e| e.into_inner()).busy = false;
        sink(req.gen_, req.path, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thumbs::decode::samples as s;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    fn dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("sfm-preview-loader-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn delivers_result_and_failure() {
        let d = dir("basic");
        std::fs::write(d.join("a.png"), s::png(&s::pattern(30, 20, 1))).unwrap();
        std::fs::write(d.join("bad.png"), b"nope").unwrap();
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let l = Loader::new(
            Arc::new(move |g, p, o| {
                let _ = tx.lock().unwrap().send((g, p, o));
            }),
            Limits::default(),
        );
        l.request(1, d.join("a.png"));
        let (g, _, o) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let p = o.ok().unwrap();
        assert_eq!((g, p.w, p.h, p.frames.len()), (1, 30, 20, 1));
        l.request(2, d.join("bad.png"));
        let (g, _, o) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(g, 2);
        assert!(matches!(o, Err(Failure::Unsupported)));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn newest_request_supersedes_queued_ones() {
        let d = dir("latest");
        // 一张较大的图让第一个请求持续一会儿
        std::fs::write(d.join("big.bmp"), s::bmp(&s::pattern(3000, 3000, 1))).unwrap();
        for i in 0..20 {
            std::fs::write(d.join(format!("{i}.png")), s::png(&s::pattern(10, 10, 1))).unwrap();
        }
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let l = Loader::new(
            Arc::new(move |g, _, _| {
                let _ = tx.lock().unwrap().send(g);
            }),
            Limits::default(),
        );
        l.request(1, d.join("big.bmp"));
        std::thread::sleep(Duration::from_millis(20));
        for i in 0..20u64 {
            l.request(2 + i, d.join(format!("{i}.png")));
        }
        let mut got = Vec::new();
        while let Ok(g) = rx.recv_timeout(Duration::from_millis(1500)) {
            got.push(g);
        }
        assert_eq!(*got.last().unwrap(), 21, "最新请求一定会完成");
        assert!(got.len() <= 3, "中间请求被跳过，实际完成 {got:?}");
        let (started, skipped) = l.counters();
        assert!(
            started <= 3 && skipped >= 18,
            "started={started} skipped={skipped}"
        );
        assert!(!l.busy());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn cancel_drops_queued_request() {
        let d = dir("cancel");
        std::fs::write(d.join("a.png"), s::png(&s::pattern(10, 10, 1))).unwrap();
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let l = Loader::new(
            Arc::new(move |g, _, _| {
                let _ = tx.lock().unwrap().send(g);
            }),
            Limits::default(),
        );
        // 线程可能已取走请求；只验证：取消不会 panic，且最终不再有请求在排队
        l.request(1, d.join("a.png"));
        l.cancel();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!l.busy());
        drop(rx);
        let _ = std::fs::remove_dir_all(d);
    }
}
