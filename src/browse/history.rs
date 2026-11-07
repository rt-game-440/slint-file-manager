//! 后退／前进历史。属于单个浏览上下文。

use std::path::PathBuf;

#[derive(Default, Debug)]
pub struct History {
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
}

/// 持久化时每个方向最多保留的历史条数。
pub const MAX_PERSISTED: usize = 50;

impl History {
    /// 由持久化的两个栈构造（`back` 末尾是最近一次后退目标）。
    pub fn from_parts(mut back: Vec<PathBuf>, mut forward: Vec<PathBuf>) -> History {
        trim_front(&mut back);
        trim_front(&mut forward);
        History { back, forward }
    }

    /// 两个栈的副本，超过上限时只保留最近的部分。
    pub fn parts(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut b = self.back.clone();
        let mut f = self.forward.clone();
        trim_front(&mut b);
        trim_front(&mut f);
        (b, f)
    }

    /// 从 `from` 进入新位置：`from` 进入后退栈，前进栈清空。
    pub fn visit(&mut self, from: PathBuf) {
        self.back.push(from);
        self.forward.clear();
    }

    /// 后退：返回目标位置，`current` 进入前进栈。
    pub fn back(&mut self, current: PathBuf) -> Option<PathBuf> {
        let p = self.back.pop()?;
        self.forward.push(current);
        Some(p)
    }

    /// 前进：返回目标位置，`current` 进入后退栈。
    pub fn forward(&mut self, current: PathBuf) -> Option<PathBuf> {
        let p = self.forward.pop()?;
        self.back.push(current);
        Some(p)
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

fn trim_front(v: &mut Vec<PathBuf>) {
    if v.len() > MAX_PERSISTED {
        v.drain(..v.len() - MAX_PERSISTED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_roundtrip_and_cap() {
        let back: Vec<PathBuf> = (0..80).map(|i| format!("B{i}").into()).collect();
        let h = History::from_parts(back, vec!["F".into()]);
        let (b, f) = h.parts();
        assert_eq!(b.len(), MAX_PERSISTED);
        assert_eq!(b.last(), Some(&PathBuf::from("B79")));
        assert_eq!(b.first(), Some(&PathBuf::from("B30")));
        assert_eq!(f, [PathBuf::from("F")]);
    }

    #[test]
    fn back_forward_roundtrip() {
        let mut h = History::default();
        h.visit("A".into()); // A -> B
        h.visit("B".into()); // B -> C
        assert_eq!(h.back("C".into()), Some("B".into()));
        assert_eq!(h.back("B".into()), Some("A".into()));
        assert!(!h.can_back());
        assert_eq!(h.forward("A".into()), Some("B".into()));
        h.visit("B".into()); // 从 B 去新位置，前进栈清空
        assert!(!h.can_forward());
    }
}
