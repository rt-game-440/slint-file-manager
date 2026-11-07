//! 基本搜索：广度优先遍历目录树，按 [`Query`] 过滤。
//!
//! 边界（对应 Issue #11 的验收项）：
//! - 只使用目录枚举自带的元数据（`FindNextFileW`），从不打开文件，更不读取正文，因此不会触发云占位文件下载。
//! - 符号链接与联接点（目录重解析点）只作为条目参与匹配，不进入，因此不会因循环无限扫描；另有深度上限兜底。
//! - 带“按需下载”属性（`RECALL_ON_OPEN`／`RECALL_ON_DATA_ACCESS`／`OFFLINE`）的文件夹是尚未同步到本机的云文件夹，
//!   枚举它可能触发网络读取，所以只显示它的名称，不展开，并计入 `cloud_unexpanded`。
//! - 无权限目录安静跳过，只计数，不逐条报错；隐藏条目与浏览视图一致，不列出也不进入。
//! - 可取消：每处理一批条目检查一次；增量输出：每 128 个结果或每 80ms 输出一批。
//! - 全盘搜索时每个根目录一个线程（各盘互不拖慢）。

use super::engine::Event;
use super::{Cancel, Hit, Query};
use crate::browse::entry::{ListErrorKind, classify};
use std::collections::VecDeque;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const ATTR_HIDDEN: u32 = 0x2;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_OFFLINE: u32 = 0x1000;
const ATTR_RECALL_ON_OPEN: u32 = 0x4_0000;
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

const BATCH: usize = 128;
const FLUSH_EVERY: Duration = Duration::from_millis(80);
/// 目录深度上限（Windows 路径本身约 32k 字符，正常目录树远小于此）。
pub const MAX_DEPTH: usize = 128;

/// 文件夹是否带“尚未同步到本机”的属性。
pub fn cloud_unpopulated(attrs: u32) -> bool {
    attrs & (ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0
}

/// 各线程共享的统计。
#[derive(Default)]
pub struct Counters {
    pub dirs: AtomicU64,
    pub entries: AtomicU64,
    pub hits: AtomicU64,
    pub denied: AtomicU64,
    pub other_errors: AtomicU64,
    pub cloud_unexpanded: AtomicU64,
    pub links_skipped: AtomicU64,
    /// 根目录读取失败（不存在、磁盘已移除、设备未就绪）的个数
    pub roots_unavailable: AtomicU64,
    /// 根目录无权限的个数
    pub roots_denied: AtomicU64,
    pub truncated: AtomicBool,
}

impl Counters {
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

pub struct WalkOpts {
    pub max_hits: usize,
    pub max_depth: usize,
}

impl Default for WalkOpts {
    fn default() -> WalkOpts {
        WalkOpts {
            max_hits: super::MAX_HITS,
            max_depth: MAX_DEPTH,
        }
    }
}

struct Batch<'a> {
    hits: Vec<Hit>,
    last: Instant,
    sink: &'a (dyn Fn(Event) + Sync),
    c: &'a Counters,
}

impl Batch<'_> {
    fn push(&mut self, h: Hit) {
        self.hits.push(h);
        if self.hits.len() >= BATCH {
            self.flush();
        }
    }

    fn tick(&mut self) {
        if self.last.elapsed() >= FLUSH_EVERY {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if !self.hits.is_empty() {
            (self.sink)(Event::Hits(std::mem::take(&mut self.hits)));
        }
        (self.sink)(Event::Progress {
            dirs: Counters::get(&self.c.dirs),
            entries: Counters::get(&self.c.entries),
        });
        self.last = Instant::now();
    }
}

/// 遍历所有根目录（多个根并行）。阻塞到完成或取消。
pub fn walk_roots(
    roots: &[PathBuf],
    q: &Query,
    cancel: &Cancel,
    opts: &WalkOpts,
    c: &Counters,
    sink: &(dyn Fn(Event) + Sync),
) {
    if roots.len() <= 1 {
        for r in roots {
            walk_root(r, q, cancel, opts, c, sink);
        }
        return;
    }
    std::thread::scope(|s| {
        for r in roots {
            s.spawn(move || walk_root(r, q, cancel, opts, c, sink));
        }
    });
}

fn walk_root(
    root: &Path,
    q: &Query,
    cancel: &Cancel,
    opts: &WalkOpts,
    c: &Counters,
    sink: &(dyn Fn(Event) + Sync),
) {
    let mut b = Batch {
        hits: Vec::new(),
        last: Instant::now(),
        sink,
        c,
    };
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));
    let mut first = true;
    while let Some((dir, depth)) = queue.pop_front() {
        if cancel.is_cancelled() || c.truncated.load(Ordering::Relaxed) {
            break;
        }
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                let kind = classify(&e);
                if first {
                    // 根目录本身读不了：区分“无权限”与“不可用（不存在／磁盘已移除／未就绪）”
                    if kind == ListErrorKind::AccessDenied {
                        c.roots_denied.fetch_add(1, Ordering::Relaxed);
                    } else {
                        c.roots_unavailable.fetch_add(1, Ordering::Relaxed);
                    }
                } else if kind == ListErrorKind::AccessDenied {
                    c.denied.fetch_add(1, Ordering::Relaxed);
                } else {
                    c.other_errors.fetch_add(1, Ordering::Relaxed);
                }
                first = false;
                continue;
            }
        };
        first = false;
        c.dirs.fetch_add(1, Ordering::Relaxed);
        let mut n = 0u32;
        for de in rd {
            let Ok(de) = de else {
                continue;
            };
            n += 1;
            if n.is_multiple_of(256) && cancel.is_cancelled() {
                break;
            }
            let Ok(md) = de.metadata() else {
                c.other_errors.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let attrs = md.file_attributes();
            if attrs & ATTR_HIDDEN != 0 {
                continue;
            }
            c.entries.fetch_add(1, Ordering::Relaxed);
            let is_dir = attrs & ATTR_DIRECTORY != 0;
            let name = de.file_name();
            let lossy = name.to_string_lossy();
            if q.matches(&lossy, is_dir, || {
                dir.join(&name).to_string_lossy().into_owned()
            }) {
                let hits = c.hits.fetch_add(1, Ordering::Relaxed) + 1;
                if hits as usize > opts.max_hits {
                    c.truncated.store(true, Ordering::Relaxed);
                    break;
                }
                b.push(Hit {
                    path: dir.join(&name),
                    is_dir,
                });
            }
            if is_dir {
                let is_link = de.file_type().map(|t| t.is_symlink()).unwrap_or(false);
                if is_link {
                    c.links_skipped.fetch_add(1, Ordering::Relaxed);
                } else if cloud_unpopulated(attrs) {
                    c.cloud_unexpanded.fetch_add(1, Ordering::Relaxed);
                } else if depth < opts.max_depth {
                    queue.push_back((dir.join(&name), depth + 1));
                }
            }
        }
        b.tick();
    }
    b.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join("sfm-unit-search").join(format!(
            "{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn run(roots: &[PathBuf], q: &str, cancel: &Cancel) -> (Vec<Hit>, Counters, usize) {
        let out = Mutex::new((Vec::<Hit>::new(), 0usize));
        let c = Counters::default();
        walk_roots(
            roots,
            &Query::parse(q),
            cancel,
            &WalkOpts::default(),
            &c,
            &|e| {
                if let Event::Hits(h) = e {
                    let mut g = out.lock().unwrap();
                    g.1 += 1;
                    g.0.extend(h);
                }
            },
        );
        let (h, batches) = out.into_inner().unwrap();
        (h, c, batches)
    }

    fn names(h: &[Hit]) -> Vec<String> {
        let mut v: Vec<String> = h.iter().map(Hit::name).collect();
        v.sort();
        v
    }

    #[test]
    fn finds_in_nested_dirs_by_name_ext_and_path() {
        let d = temp("nested");
        fs::create_dir_all(d.join("a").join("b").join("c")).unwrap();
        fs::write(d.join("报告1.txt"), b"x").unwrap();
        fs::write(d.join("a").join("报告2.TXT"), b"x").unwrap();
        fs::write(d.join("a").join("b").join("c").join("深层报告.md"), b"x").unwrap();
        fs::write(d.join("a").join("b").join("other.txt"), b"x").unwrap();
        let (h, c, _) = run(std::slice::from_ref(&d), "报告", &Cancel::new());
        assert_eq!(names(&h), vec!["报告1.txt", "报告2.TXT", "深层报告.md"]);
        assert_eq!(Counters::get(&c.dirs), 4);
        let (h, _, _) = run(std::slice::from_ref(&d), "报告 .txt", &Cancel::new());
        assert_eq!(names(&h), vec!["报告1.txt", "报告2.TXT"]);
        let (h, _, _) = run(
            std::slice::from_ref(&d),
            "path:a\\b ext:txt",
            &Cancel::new(),
        );
        assert_eq!(names(&h), vec!["other.txt"]);
        let (h, _, _) = run(&[d], "B", &Cancel::new());
        assert_eq!(names(&h), vec!["b"], "文件夹也参与名称匹配");
    }

    #[test]
    fn hidden_entries_are_not_listed_or_entered() {
        let d = temp("hidden");
        fs::create_dir(d.join("隐藏夹")).unwrap();
        fs::write(d.join("隐藏夹").join("里面.txt"), b"x").unwrap();
        let st = std::process::Command::new("attrib")
            .arg("+h")
            .arg(d.join("隐藏夹"))
            .status()
            .unwrap();
        assert!(st.success());
        let (h, _, _) = run(&[d], "里面", &Cancel::new());
        assert!(h.is_empty());
    }

    #[test]
    fn cancel_stops_early_and_emits_what_was_found() {
        let d = temp("cancel");
        for i in 0..40 {
            let sub = d.join(format!("d{i:02}"));
            fs::create_dir(&sub).unwrap();
            for j in 0..50 {
                fs::write(sub.join(format!("f{j}.txt")), b"x").unwrap();
            }
        }
        // 一开始就取消：不遍历
        let cancel = Cancel::new();
        cancel.cancel();
        let (h, c, _) = run(std::slice::from_ref(&d), "f", &cancel);
        assert!(h.is_empty());
        assert_eq!(Counters::get(&c.dirs), 0);
        // 收到第一批结果后取消：只处理了一部分目录
        let cancel = Cancel::new();
        let c = Counters::default();
        let seen = Mutex::new(0usize);
        walk_roots(
            &[d],
            &Query::parse("f"),
            &cancel,
            &WalkOpts::default(),
            &c,
            &|e| {
                if let Event::Hits(h) = e {
                    *seen.lock().unwrap() += h.len();
                    cancel.cancel();
                }
            },
        );
        let seen = seen.into_inner().unwrap();
        assert!(seen > 0 && seen < 40 * 50, "取消后应只得到部分结果：{seen}");
        assert!(Counters::get(&c.dirs) < 41);
    }

    #[test]
    fn results_are_incremental_in_batches() {
        let d = temp("batches");
        for i in 0..300 {
            fs::write(d.join(format!("x{i}.txt")), b"x").unwrap();
        }
        let (h, _, batches) = run(&[d], "x", &Cancel::new());
        assert_eq!(h.len(), 300);
        assert!(batches >= 3, "300 个结果应分多批输出：{batches}");
    }

    #[test]
    fn denied_dir_is_skipped_quietly_and_counted() {
        let d = temp("denied");
        let locked = d.join("锁住");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("秘密.txt"), b"x").unwrap();
        fs::write(d.join("公开.txt"), b"x").unwrap();
        let p = locked.to_string_lossy().to_string();
        let ok = |args: &[&str]| {
            std::process::Command::new("icacls")
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(ok(&[&p, "/deny", "*S-1-1-0:(OI)(CI)(RD)", "/Q"]));
        let (h, c, _) = run(std::slice::from_ref(&d), "txt", &Cancel::new());
        ok(&[&p, "/reset", "/T", "/Q"]);
        assert_eq!(names(&h), vec!["公开.txt"]);
        assert_eq!(Counters::get(&c.denied), 1);
        assert_eq!(Counters::get(&c.other_errors), 0);
    }

    #[test]
    fn junction_loop_is_not_followed() {
        let d = temp("loop");
        fs::create_dir_all(d.join("real")).unwrap();
        fs::write(d.join("real").join("文件.txt"), b"x").unwrap();
        // 联接点指回上级，形成环
        let link = d.join("real").join("回环");
        let st = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&d)
            .output()
            .unwrap();
        assert!(st.status.success(), "mklink /J 失败");
        let (h, c, _) = run(std::slice::from_ref(&d), "文件", &Cancel::new());
        assert_eq!(names(&h), vec!["文件.txt"], "联接点不进入，文件只出现一次");
        assert_eq!(Counters::get(&c.links_skipped), 1);
        // 联接点本身可按名称找到
        let (h, _, _) = run(&[d], "回环", &Cancel::new());
        assert_eq!(names(&h), vec!["回环"]);
    }

    #[test]
    fn missing_root_is_reported_not_empty_result() {
        let d = temp("missing");
        let (h, c, _) = run(&[d.join("不存在")], "x", &Cancel::new());
        assert!(h.is_empty());
        assert_eq!(Counters::get(&c.roots_unavailable), 1);
    }

    #[test]
    fn multiple_roots_run_in_parallel_and_merge() {
        let a = temp("rootA");
        let b = temp("rootB");
        fs::write(a.join("同名.txt"), b"x").unwrap();
        fs::write(b.join("同名.md"), b"x").unwrap();
        let (h, _, _) = run(
            &[a, b, PathBuf::from("Z:\\不存在的盘")],
            "同名",
            &Cancel::new(),
        );
        assert_eq!(names(&h), vec!["同名.md", "同名.txt"]);
    }

    #[test]
    fn hit_cap_truncates() {
        let d = temp("cap");
        for i in 0..50 {
            fs::write(d.join(format!("c{i}.txt")), b"x").unwrap();
        }
        let c = Counters::default();
        let out = Mutex::new(0usize);
        walk_roots(
            &[d],
            &Query::parse("c"),
            &Cancel::new(),
            &WalkOpts {
                max_hits: 10,
                max_depth: 8,
            },
            &c,
            &|e| {
                if let Event::Hits(h) = e {
                    *out.lock().unwrap() += h.len();
                }
            },
        );
        assert_eq!(out.into_inner().unwrap(), 10);
        assert!(c.truncated.load(Ordering::Relaxed));
    }

    #[test]
    fn cloud_attribute_classification() {
        assert!(cloud_unpopulated(0x10 | ATTR_RECALL_ON_DATA_ACCESS));
        assert!(cloud_unpopulated(0x10 | ATTR_RECALL_ON_OPEN));
        assert!(cloud_unpopulated(0x10 | ATTR_OFFLINE));
        assert!(!cloud_unpopulated(0x10 | 0x400)); // 仅重解析点（如已固定到本机的云文件夹）仍可进入
    }
}
