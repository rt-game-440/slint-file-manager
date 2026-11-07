//! 缩略图磁盘缓存：一张缩略图一个文件，有容量上限与 LRU 淘汰，可清空；损坏条目自动丢弃重建。
//!
//! - 位置：`<根目录>\v1\<键16位十六进制>.thumb`。根目录默认 `%LOCALAPPDATA%\slint-file-manager\thumbs`，
//!   可用 `SFM_THUMB_CACHE_DIR` 覆盖（测试一律用临时目录）。清空只删除本格式的文件（`*.thumb`、`*.tmp`），
//!   即使根目录被误设为别的目录也不会删其他文件。
//! - 文件格式（小端）：`"SFMT"`｜版本 u32｜宽 u32｜高 u32｜身份长度 u32｜身份（路径小写｜大小｜修改时间，UTF-8）｜
//!   载荷长度 u32｜载荷（有损 WebP q80，保留 alpha）｜FNV-1a 校验 u64（覆盖前面全部字节）。
//! - 失效：键由“路径＋大小＋修改时间＋缩略图尺寸＋算法版本”哈希而来（[`super::Source::key`]）；命中时还会核对文件里
//!   保存的身份字符串，防止哈希碰撞。重命名、改内容后旧条目不再被命中，最终被 LRU 淘汰。
//! - 损坏：魔数、版本、长度、校验或 WebP 解码任一不符即视为未命中并删除该文件，随后重新生成；不影响原文件。
//! - 写入：先写 `*.tmp` 再改名，写失败（空间不足、无权限）只计数并跳过，缩略图照常显示。
//! - 容量：超过上限时按最近使用时间淘汰到上限的 90%。最近使用时间在进程内按访问更新，跨启动依据文件修改时间
//!   （命中时若文件超过 1 小时未被标记就更新一次修改时间）。
//! - 清空与生成并发：清空持有写锁并使“代号”加一；写入持有读锁并核对开始生成时的代号，代号不同就不写。
//!   因此清空完成后，缓存里只会有清空之后才开始生成的缩略图；清空期间完成的生成不写盘（拿不到读锁就跳过）。

use super::{Fail, Fnv, Source, Thumb, webp};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 4] = b"SFMT";
const VERSION: u32 = 1;
const SUBDIR: &str = "v1";
const EXT: &str = "thumb";
/// 磁盘缓存载荷的 WebP 质量
pub const QUALITY: f32 = 80.0;
/// 默认容量上限（MiB），依据见 docs/thumbnails.md 的测量
pub const DEFAULT_CAPACITY_MB: u64 = 512;
/// 设置界面提供的容量选项（MiB）
pub const CAPACITY_CHOICES_MB: [u64; 4] = [256, 512, 1024, 2048];
/// 命中时最多每隔多久更新一次文件修改时间（跨启动的 LRU 依据）
const TOUCH_AFTER: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug)]
struct Rec {
    bytes: u64,
    /// 最近使用序号（越大越新）
    used: u64,
    /// 文件修改时间（秒）
    mtime_s: u64,
}

#[derive(Default)]
struct Index {
    files: HashMap<u64, Rec>,
    total: u64,
    tick: u64,
}

impl Index {
    fn insert(&mut self, key: u64, bytes: u64, mtime_s: u64) {
        self.tick += 1;
        let rec = Rec {
            bytes,
            used: self.tick,
            mtime_s,
        };
        if let Some(old) = self.files.insert(key, rec) {
            self.total -= old.bytes;
        }
        self.total += bytes;
    }

    fn remove(&mut self, key: u64) {
        if let Some(old) = self.files.remove(&key) {
            self.total -= old.bytes;
        }
    }
}

/// 缓存占用统计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub bytes: u64,
    pub files: u64,
    pub capacity: u64,
    /// 启动时的目录扫描是否已完成（未完成时 bytes 只含本次运行写入的部分）
    pub scanned: bool,
}

pub struct DiskCache {
    dir: PathBuf,
    cap: AtomicU64,
    cgen: AtomicU64,
    io: RwLock<()>,
    index: Mutex<Index>,
    scanned: AtomicBool,
    write_failures: AtomicU64,
    corrupt_dropped: AtomicU64,
}

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn file_mtime_s(m: &fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

fn parse_name(p: &Path) -> Option<u64> {
    if p.extension()? != EXT {
        return None;
    }
    let stem = p.file_stem()?.to_str()?;
    if stem.len() != 16 {
        return None;
    }
    u64::from_str_radix(stem, 16).ok()
}

impl DiskCache {
    /// 打开（必要时创建）缓存目录。`background` 为 true 时在后台线程扫描已有文件（产品），否则当场扫描（测试）。
    pub fn open(root: &Path, capacity: u64, background: bool) -> std::io::Result<Arc<DiskCache>> {
        let dir = root.join(SUBDIR);
        fs::create_dir_all(&dir)?;
        let c = Arc::new(DiskCache {
            dir,
            cap: AtomicU64::new(capacity),
            cgen: AtomicU64::new(0),
            io: RwLock::new(()),
            index: Mutex::new(Index::default()),
            scanned: AtomicBool::new(false),
            write_failures: AtomicU64::new(0),
            corrupt_dropped: AtomicU64::new(0),
        });
        if background {
            let c2 = c.clone();
            std::thread::Builder::new()
                .name("thumb-cache-scan".into())
                .spawn(move || c2.scan())?;
        } else {
            c.scan();
        }
        Ok(c)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 扫描已有文件建立索引，清理残留的临时文件，然后按容量淘汰。
    fn scan(&self) {
        let mut found: Vec<(u64, u64, u64)> = Vec::new();
        if let Ok(rd) = fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "tmp") {
                    let _ = fs::remove_file(&p);
                    continue;
                }
                let Some(key) = parse_name(&p) else { continue };
                if let Ok(m) = e.metadata() {
                    found.push((key, m.len(), file_mtime_s(&m)));
                }
            }
        }
        // 旧的在前：按修改时间分配“最近使用”序号
        found.sort_by_key(|f| f.2);
        {
            let mut ix = self.lock_index();
            let mut fresh = Index::default();
            for (key, bytes, m) in found {
                fresh.insert(key, bytes, m);
            }
            // 扫描期间本进程写入的条目更新
            let pending: Vec<(u64, Rec)> = ix.files.iter().map(|(k, r)| (*k, *r)).collect();
            for (k, r) in pending {
                fresh.insert(k, r.bytes, r.mtime_s);
            }
            *ix = fresh;
        }
        self.scanned.store(true, Ordering::SeqCst);
        self.evict();
    }

    fn lock_index(&self) -> std::sync::MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn path_of(&self, key: u64) -> PathBuf {
        self.dir.join(format!("{key:016x}.{EXT}"))
    }

    /// 当前代号（生成开始时取得，写入时核对）。
    pub fn generation(&self) -> u64 {
        self.cgen.load(Ordering::SeqCst)
    }

    /// 查找缩略图。未命中、身份不符或条目损坏返回 `None`（损坏条目被删除）。
    pub fn get(&self, src: &Source) -> Option<Thumb> {
        let key = src.key();
        let path = self.path_of(key);
        let bytes = fs::read(&path).ok()?;
        match parse(&bytes, &src.identity()) {
            Ok(t) => {
                self.touch(key, &path);
                Some(t)
            }
            Err(Mismatch::Identity) => None,
            Err(Mismatch::Corrupt) => {
                self.corrupt_dropped.fetch_add(1, Ordering::Relaxed);
                let _guard = self.io.read().unwrap_or_else(|e| e.into_inner());
                let _ = fs::remove_file(&path);
                self.lock_index().remove(key);
                None
            }
        }
    }

    fn touch(&self, key: u64, path: &Path) {
        let now = now_s();
        let stale = {
            let mut ix = self.lock_index();
            ix.tick += 1;
            let tick = ix.tick;
            match ix.files.get_mut(&key) {
                Some(r) => {
                    r.used = tick;
                    let stale = now.saturating_sub(r.mtime_s) > TOUCH_AFTER.as_secs();
                    if stale {
                        r.mtime_s = now;
                    }
                    stale
                }
                None => false,
            }
        };
        if stale && let Ok(f) = fs::OpenOptions::new().write(true).open(path) {
            let _ = f.set_modified(SystemTime::now());
        }
    }

    /// 写入缩略图。`cgen` 为生成开始时的 [`DiskCache::generation`]；清空进行中或代号已变时不写。
    pub fn put(&self, src: &Source, t: &Thumb, cgen: u64) -> Result<(), Fail> {
        let Some(payload) = webp::encode(t, QUALITY) else {
            return Err(Fail::Corrupt);
        };
        let data = serialize(t, &src.identity(), &payload);
        let Ok(_guard) = self.io.try_read() else {
            return Ok(()); // 正在清空：跳过
        };
        if self.generation() != cgen {
            return Ok(());
        }
        let key = src.key();
        let path = self.path_of(key);
        let tmp = self
            .dir
            .join(format!("{key:016x}.{:?}.tmp", std::thread::current().id()));
        let written = (|| -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&data)?;
            f.flush()?;
            drop(f);
            fs::rename(&tmp, &path)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
            self.write_failures.fetch_add(1, Ordering::Relaxed);
            return Err(Fail::Io);
        }
        self.lock_index().insert(key, data.len() as u64, now_s());
        drop(_guard);
        self.evict();
        Ok(())
    }

    /// 超过容量时按最近使用淘汰到容量的 90%。扫描完成前不淘汰（索引不完整）。
    fn evict(&self) {
        if !self.scanned.load(Ordering::SeqCst) {
            return;
        }
        let cap = self.cap.load(Ordering::SeqCst);
        let victims: Vec<u64> = {
            let mut ix = self.lock_index();
            if ix.total <= cap {
                return;
            }
            let target = cap / 10 * 9;
            let mut order: Vec<(u64, u64, u64)> = ix
                .files
                .iter()
                .map(|(k, r)| (r.used, *k, r.bytes))
                .collect();
            order.sort_unstable();
            let mut v = Vec::new();
            let mut total = ix.total;
            for (_, k, b) in order {
                if total <= target {
                    break;
                }
                total -= b;
                v.push(k);
            }
            for &k in &v {
                ix.remove(k);
            }
            v
        };
        let _guard = self.io.read().unwrap_or_else(|e| e.into_inner());
        for k in victims {
            let _ = fs::remove_file(self.path_of(k));
        }
    }

    pub fn set_capacity(&self, bytes: u64) {
        self.cap.store(bytes, Ordering::SeqCst);
        self.evict();
    }

    pub fn capacity(&self) -> u64 {
        self.cap.load(Ordering::SeqCst)
    }

    /// 清空缓存（只删本格式的文件）。返回删除的字节数。
    pub fn clear(&self) -> u64 {
        let _guard = self.io.write().unwrap_or_else(|e| e.into_inner());
        self.cgen.fetch_add(1, Ordering::SeqCst);
        let mut freed = 0;
        if let Ok(rd) = fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let p = e.path();
                let ours = parse_name(&p).is_some() || p.extension().is_some_and(|x| x == "tmp");
                if !ours {
                    continue;
                }
                let len = e.metadata().map_or(0, |m| m.len());
                if fs::remove_file(&p).is_ok() {
                    freed += len;
                }
            }
        }
        *self.lock_index() = Index::default();
        freed
    }

    pub fn usage(&self) -> Usage {
        let ix = self.lock_index();
        Usage {
            bytes: ix.total,
            files: ix.files.len() as u64,
            capacity: self.capacity(),
            scanned: self.scanned.load(Ordering::SeqCst),
        }
    }

    pub fn write_failures(&self) -> u64 {
        self.write_failures.load(Ordering::Relaxed)
    }

    pub fn corrupt_dropped(&self) -> u64 {
        self.corrupt_dropped.load(Ordering::Relaxed)
    }
}

fn serialize(t: &Thumb, identity: &str, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(32 + identity.len() + payload.len());
    v.extend_from_slice(MAGIC);
    v.extend_from_slice(&VERSION.to_le_bytes());
    v.extend_from_slice(&t.w.to_le_bytes());
    v.extend_from_slice(&t.h.to_le_bytes());
    v.extend_from_slice(&(identity.len() as u32).to_le_bytes());
    v.extend_from_slice(identity.as_bytes());
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    let mut h = Fnv::new();
    h.write(&v);
    v.extend_from_slice(&h.finish().to_le_bytes());
    v
}

enum Mismatch {
    /// 条目完好但属于另一个文件（哈希碰撞）
    Identity,
    Corrupt,
}

fn parse(b: &[u8], identity: &str) -> Result<Thumb, Mismatch> {
    use Mismatch::Corrupt;
    if b.len() < 32 || &b[0..4] != MAGIC {
        return Err(Corrupt);
    }
    let (body, sum) = b.split_at(b.len() - 8);
    let mut h = Fnv::new();
    h.write(body);
    if h.finish().to_le_bytes() != sum {
        return Err(Corrupt);
    }
    let u32_at = |o: usize| -> Result<u32, Mismatch> {
        body.get(o..o + 4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
            .ok_or(Corrupt)
    };
    if u32_at(4)? != VERSION {
        return Err(Corrupt);
    }
    let (w, h_) = (u32_at(8)?, u32_at(12)?);
    let id_len = u32_at(16)? as usize;
    let id = body.get(20..20 + id_len).ok_or(Corrupt)?;
    let p_off = 20 + id_len;
    let p_len = u32_at(p_off)? as usize;
    let payload = body.get(p_off + 4..p_off + 4 + p_len).ok_or(Corrupt)?;
    if p_off + 4 + p_len != body.len() {
        return Err(Corrupt);
    }
    if id != identity.as_bytes() {
        return Err(Mismatch::Identity);
    }
    let t = webp::decode_full(payload).map_err(|_| Corrupt)?;
    if (t.w, t.h) != (w, h_) {
        return Err(Corrupt);
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thumbs::decode::samples::pattern;
    use std::time::Duration;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sfm-thumb-cache-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn src(i: u32) -> Source {
        Source::new(
            PathBuf::from(format!(r"C:\样例\{i}.webp")),
            1000 + i as u64,
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000 + i as u64)),
        )
    }

    /// 测量真实图片的缓存条目大小（默认忽略，用来确定默认容量）。只输出统计，不输出文件名：
    /// `$env:SFM_THUMB_MEASURE_DIR='C:\Windows\Web'; cargo test --release --lib cache_entry_size -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn cache_entry_size() {
        let Some(dir) = std::env::var_os("SFM_THUMB_MEASURE_DIR") else {
            return;
        };
        let mut files = Vec::new();
        let mut stack = vec![PathBuf::from(dir)];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| {
                    crate::thumbs::wants_thumbnail(&x.to_string_lossy().to_lowercase())
                }) {
                    files.push(p);
                }
            }
        }
        let root = tmp("measure");
        let c = DiskCache::open(&root, u64::MAX, false).unwrap();
        let mut sizes = Vec::new();
        for p in files {
            let m = fs::metadata(&p).unwrap();
            let s = Source::new(p, m.len(), m.modified().ok());
            let Ok(g) =
                crate::thumbs::decode::generate(&s, &crate::thumbs::decode::Limits::default())
            else {
                continue;
            };
            c.put(&s, &g.thumb, c.generation()).unwrap();
            sizes.push(fs::metadata(c.path_of(s.key())).unwrap().len());
        }
        sizes.sort_unstable();
        let n = sizes.len();
        assert!(n > 0, "目录里没有可生成缩略图的图片");
        let mean = sizes.iter().sum::<u64>() as f64 / n as f64;
        let q = |f: f64| sizes[((n - 1) as f64 * f).round() as usize];
        println!(
            "缓存条目：{n} 个，平均 {:.1} KiB，中位 {:.1} KiB，p90 {:.1} KiB，最大 {:.1} KiB；512 MiB 约可容纳 {:.0} 张（按平均）",
            mean / 1024.0,
            q(0.5) as f64 / 1024.0,
            q(0.9) as f64 / 1024.0,
            sizes[n - 1] as f64 / 1024.0,
            (512u64 << 20) as f64 / mean
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn roundtrip_and_restart() {
        let root = tmp("rt");
        let c = DiskCache::open(&root, 1 << 30, false).unwrap();
        let t = pattern(256, 170, 3);
        assert!(c.get(&src(1)).is_none());
        c.put(&src(1), &t, c.generation()).unwrap();
        let got = c.get(&src(1)).expect("命中");
        assert_eq!((got.w, got.h), (256, 170));
        assert_eq!(c.usage().files, 1);
        drop(c);
        // “重启”：新实例扫描已有文件
        let c2 = DiskCache::open(&root, 1 << 30, false).unwrap();
        assert_eq!(c2.usage().files, 1);
        assert!(c2.get(&src(1)).is_some(), "跨实例复用");
        // 文件变化（大小或修改时间）后不命中
        let mut changed = src(1);
        changed.size += 1;
        assert!(c2.get(&changed).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_entry_is_dropped_and_regenerated() {
        let root = tmp("corrupt");
        let c = DiskCache::open(&root, 1 << 30, false).unwrap();
        c.put(&src(2), &pattern(100, 80, 1), 0).unwrap();
        let p = c.path_of(src(2).key());
        let mut b = fs::read(&p).unwrap();
        let mid = b.len() / 2;
        b[mid] ^= 0xFF;
        fs::write(&p, &b).unwrap();
        assert!(c.get(&src(2)).is_none(), "校验失败视为未命中");
        assert!(!p.exists(), "损坏条目被删除");
        assert_eq!(c.corrupt_dropped(), 1);
        // 截断、空文件
        c.put(&src(2), &pattern(100, 80, 1), 0).unwrap();
        fs::write(&p, &fs::read(&p).unwrap()[..20]).unwrap();
        assert!(c.get(&src(2)).is_none());
        fs::write(&p, b"").unwrap();
        assert!(c.get(&src(2)).is_none());
        // 重新写入后正常
        c.put(&src(2), &pattern(100, 80, 1), 0).unwrap();
        assert!(c.get(&src(2)).is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn identity_mismatch_is_a_miss() {
        let root = tmp("ident");
        let c = DiskCache::open(&root, 1 << 30, false).unwrap();
        c.put(&src(3), &pattern(64, 64, 1), 0).unwrap();
        // 把 src(3) 的条目复制到 src(4) 的文件名下，模拟哈希碰撞
        fs::copy(c.path_of(src(3).key()), c.path_of(src(4).key())).unwrap();
        assert!(c.get(&src(4)).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn capacity_evicts_least_recently_used() {
        let root = tmp("cap");
        let c = DiskCache::open(&root, 1 << 30, false).unwrap();
        for i in 0..10 {
            c.put(&src(i), &pattern(200, 150, i as u8 * 20), 0).unwrap();
        }
        let per = c.usage().bytes / 10;
        // 最早写入的 0 号刚被访问过，应保留
        assert!(c.get(&src(0)).is_some());
        c.set_capacity(per * 5);
        let u = c.usage();
        assert!(u.bytes <= per * 5, "淘汰后不超过容量：{u:?}");
        assert!(c.get(&src(0)).is_some(), "最近访问的保留");
        assert!(c.get(&src(1)).is_none(), "最久未用的被淘汰");
        let on_disk = fs::read_dir(c.dir()).unwrap().count() as u64;
        assert_eq!(on_disk, c.usage().files);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_only_removes_own_files_and_blocks_stale_writes() {
        let root = tmp("clear");
        let c = DiskCache::open(&root, 1 << 30, false).unwrap();
        let other = c.dir().join("不是缓存.txt");
        fs::write(&other, b"keep").unwrap();
        let cgen_before = c.generation();
        c.put(&src(5), &pattern(64, 64, 1), cgen_before).unwrap();
        assert!(c.clear() > 0);
        assert_eq!(c.usage().files, 0);
        assert!(other.exists(), "非缓存文件不删");
        // 清空前开始的生成在清空后才完成：不写盘
        c.put(&src(6), &pattern(64, 64, 1), cgen_before).unwrap();
        assert!(c.get(&src(6)).is_none());
        // 清空后开始的生成正常写入
        c.put(&src(7), &pattern(64, 64, 1), c.generation()).unwrap();
        assert!(c.get(&src(7)).is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_failure_is_counted_not_fatal() {
        let root = tmp("wfail");
        let c = DiskCache::open(&root, 1 << 30, false).unwrap();
        // 让目标文件名被一个目录占住，改名必然失败
        fs::create_dir_all(c.path_of(src(8).key())).unwrap();
        assert_eq!(c.put(&src(8), &pattern(64, 64, 1), 0), Err(Fail::Io));
        assert_eq!(c.write_failures(), 1);
        assert_eq!(c.usage().files, 0);
        let leftovers = fs::read_dir(c.dir())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .count();
        assert_eq!(leftovers, 0, "临时文件已清理");
        let _ = fs::remove_dir_all(&root);
    }
}
