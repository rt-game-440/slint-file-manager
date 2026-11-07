//! 模拟提供方：单元测试与 harness 用，不触碰系统云文件 API。
//!
//! 可以登记同步根（含提供方状态）与占位信息，并统计被询问的次数，用来断言“哪些入口没有向提供方发问”。

use super::{CloudProbe, Placeholder, ProviderStatus, SyncRoot};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Inner {
    /// （小写目录前缀，同步根）；取最长前缀。
    roots: Vec<(String, SyncRoot)>,
    placeholders: HashMap<String, Placeholder>,
}

#[derive(Default)]
pub struct MockCloud {
    inner: Mutex<Inner>,
    root_calls: AtomicUsize,
    placeholder_calls: AtomicUsize,
}

fn key(p: &str) -> String {
    p.replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

impl MockCloud {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 登记一个同步根；同一路径重复登记则覆盖（用来模拟提供方状态变化）。
    pub fn add_root(&self, dir: &str, provider: &str, status: ProviderStatus) {
        let k = key(dir);
        let mut g = self.lock();
        g.roots.retain(|(p, _)| *p != k);
        g.roots.push((
            k,
            SyncRoot {
                provider: provider.into(),
                status,
            },
        ));
    }

    pub fn set_status(&self, dir: &str, status: ProviderStatus) {
        let k = key(dir);
        if let Some((_, r)) = self.lock().roots.iter_mut().find(|(p, _)| *p == k) {
            r.status = status;
        }
    }

    pub fn set_placeholder(&self, path: &str, ph: Placeholder) {
        self.lock().placeholders.insert(key(path), ph);
    }

    pub fn clear_placeholder(&self, path: &str) {
        self.lock().placeholders.remove(&key(path));
    }

    pub fn root_calls(&self) -> usize {
        self.root_calls.load(Ordering::SeqCst)
    }

    pub fn placeholder_calls(&self) -> usize {
        self.placeholder_calls.load(Ordering::SeqCst)
    }

    pub fn reset_calls(&self) {
        self.root_calls.store(0, Ordering::SeqCst);
        self.placeholder_calls.store(0, Ordering::SeqCst);
    }
}

impl CloudProbe for MockCloud {
    fn sync_root(&self, dir: &Path) -> Option<SyncRoot> {
        self.root_calls.fetch_add(1, Ordering::SeqCst);
        let k = key(&dir.to_string_lossy());
        self.lock()
            .roots
            .iter()
            .filter(|(p, _)| k == *p || k.starts_with(&format!("{p}\\")))
            .max_by_key(|(p, _)| p.len())
            .map(|(_, r)| r.clone())
    }

    fn placeholder(&self, path: &Path) -> Option<Placeholder> {
        self.placeholder_calls.fetch_add(1, Ordering::SeqCst);
        self.lock()
            .placeholders
            .get(&key(&path.to_string_lossy()))
            .copied()
    }
}
