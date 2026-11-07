//! 缩略图请求队列（纯逻辑）：两个面板各自声明“想要”的请求，按优先级合并；视口变化时只重排，
//! 不在新视口（含预取）里的排队请求立即作废，永远不会开始。

use super::Source;

/// 优先级：数值越小越先处理。
pub mod rank {
    /// 活动面板的可见项
    pub const VISIBLE_ACTIVE: u8 = 0;
    /// 另一面板的可见项（双栏同时可见）
    pub const VISIBLE_OTHER: u8 = 1;
    /// 可见范围之后的预取
    pub const AHEAD: u8 = 2;
    /// 可见范围之前的预取
    pub const BEHIND: u8 = 3;
}

#[derive(Clone, Debug)]
pub struct Job {
    pub key: u64,
    pub src: Source,
    pub pane: usize,
    pub rank: u8,
}

#[derive(Default)]
pub struct Queue {
    items: Vec<(Job, u64)>,
    seq: u64,
}

impl Queue {
    /// 用面板 `pane` 的新请求列表（已按面板内优先级排好）替换它原先排队的请求。
    /// `busy(key)` 为真的请求（正在生成）不再排队。返回作废的排队请求数。
    pub fn set_wanted(&mut self, pane: usize, jobs: Vec<Job>, busy: impl Fn(u64) -> bool) -> usize {
        let old: Vec<u64> = self
            .items
            .iter()
            .filter(|(j, _)| j.pane == pane)
            .map(|(j, _)| j.key)
            .collect();
        self.items.retain(|(j, _)| j.pane != pane);
        let mut kept = 0;
        for j in jobs {
            if busy(j.key) {
                continue;
            }
            if old.contains(&j.key) {
                kept += 1;
            }
            self.seq += 1;
            self.items.push((Job { pane, ..j }, self.seq));
        }
        self.items.sort_by_key(|(j, s)| (j.rank, *s));
        old.len() - kept.min(old.len())
    }

    /// 追加一个请求（原生解码判定需要系统缩略图时交给 Shell 车道）。同一文件已在队列里则忽略。
    pub fn add(&mut self, job: Job) {
        if self.contains(job.key) {
            return;
        }
        self.seq += 1;
        self.items.push((job, self.seq));
        self.items.sort_by_key(|(j, s)| (j.rank, *s));
    }

    /// 取出优先级最高、且不在生成中的请求；同一文件在另一面板的重复请求一并移除。
    pub fn pop(&mut self, busy: impl Fn(u64) -> bool) -> Option<Job> {
        let i = self.items.iter().position(|(j, _)| !busy(j.key))?;
        let (job, _) = self.items.remove(i);
        self.items.retain(|(j, _)| j.key != job.key);
        Some(job)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 是否仍有面板想要该文件（视口外完成的统计用）。
    pub fn contains(&self, key: u64) -> bool {
        self.items.iter().any(|(j, _)| j.key == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn job(key: u64, pane: usize, rank: u8) -> Job {
        Job {
            key,
            src: Source::new(PathBuf::from(format!("Z:\\{key}.webp")), 1, None),
            pane,
            rank,
        }
    }

    #[test]
    fn visible_before_prefetch_and_active_before_other() {
        let mut q = Queue::default();
        q.set_wanted(
            0,
            vec![job(1, 0, rank::VISIBLE_ACTIVE), job(2, 0, rank::AHEAD)],
            |_| false,
        );
        q.set_wanted(1, vec![job(3, 1, rank::VISIBLE_OTHER)], |_| false);
        let order: Vec<u64> = std::iter::from_fn(|| q.pop(|_| false))
            .map(|j| j.key)
            .collect();
        assert_eq!(order, vec![1, 3, 2]);
    }

    #[test]
    fn scroll_replaces_only_that_pane() {
        let mut q = Queue::default();
        q.set_wanted(0, (1..=5).map(|k| job(k, 0, 0)).collect(), |_| false);
        q.set_wanted(1, vec![job(100, 1, 1)], |_| false);
        // 滚动：面板 0 的新视口只有 4、5、6
        let cancelled = q.set_wanted(0, (4..=6).map(|k| job(k, 0, 0)).collect(), |_| false);
        assert_eq!(cancelled, 3);
        let order: Vec<u64> = std::iter::from_fn(|| q.pop(|_| false))
            .map(|j| j.key)
            .collect();
        assert_eq!(order, vec![4, 5, 6, 100]);
    }

    #[test]
    fn busy_and_duplicates_skipped() {
        let mut q = Queue::default();
        q.set_wanted(0, vec![job(1, 0, 0), job(2, 0, 0)], |k| k == 1);
        q.set_wanted(1, vec![job(2, 1, 1)], |_| false);
        assert_eq!(q.len(), 2);
        let a = q.pop(|_| false).unwrap();
        assert_eq!(a.key, 2);
        assert!(q.is_empty(), "另一面板的同一文件不重复生成");
    }
}
