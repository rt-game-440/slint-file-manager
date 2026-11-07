//! 后退／前进历史。属于单个浏览上下文。

use std::path::PathBuf;

#[derive(Default, Debug)]
pub struct History {
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
}

impl History {
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

#[cfg(test)]
mod tests {
    use super::*;

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
