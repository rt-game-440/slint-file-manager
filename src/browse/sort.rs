//! 排序：名称排序与资源管理器一致（`StrCmpLogicalW`，数字按数值比较、不区分大小写）。
//!
//! 规则：升序时文件夹在前；降序时整体反转（与资源管理器相同，文件夹随之排到最后）。
//! 主键相同的条目再按名称、最后按原始字符串排序，保证结果确定。

use super::entry::Entry;
use std::cmp::Ordering;
use windows_sys::Win32::UI::Shell::StrCmpLogicalW;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Name,
    Modified,
    Size,
    Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortSpec {
    pub key: SortKey,
    pub ascending: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        SortSpec {
            key: SortKey::Name,
            ascending: true,
        }
    }
}

/// 资源管理器同款名称比较。
pub fn logical_cmp(a: &Entry, b: &Entry) -> Ordering {
    // SAFETY: 两个缓冲区都由 Entry::new 构造并以 NUL 结尾，生命周期覆盖本次调用。
    let r = unsafe { StrCmpLogicalW(a.wide.as_ptr(), b.wide.as_ptr()) };
    r.cmp(&0)
}

/// 升序比较。
pub fn compare(a: &Entry, b: &Entry, key: SortKey) -> Ordering {
    b.is_dir
        .cmp(&a.is_dir)
        .then_with(|| match key {
            SortKey::Name => Ordering::Equal,
            SortKey::Modified => a.modified.cmp(&b.modified),
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Kind => a.ext.cmp(&b.ext),
        })
        .then_with(|| logical_cmp(a, b))
        .then_with(|| a.os_name.cmp(&b.os_name))
}

/// 按规格排序，`get` 从元素取出条目。
pub fn sort_by<T>(v: &mut [T], spec: SortSpec, get: impl Fn(&T) -> &Entry) {
    v.sort_by(|x, y| {
        let o = compare(get(x), get(y), spec.key);
        if spec.ascending { o } else { o.reverse() }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn f(name: &str, size: u64, secs: u64) -> Entry {
        Entry::new(
            name.into(),
            false,
            size,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
        )
    }
    fn d(name: &str) -> Entry {
        Entry::new(name.into(), true, 0, None)
    }
    fn names(v: &[Entry]) -> Vec<&str> {
        v.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn name_sort_is_logical_and_folders_first() {
        let mut v = vec![
            f("报告10.txt", 1, 1),
            d("照片"),
            f("报告2.txt", 1, 1),
            f("B.txt", 1, 1),
            f("a.txt", 1, 1),
        ];
        sort_by(&mut v, SortSpec::default(), |e| e);
        assert_eq!(
            names(&v),
            ["照片", "a.txt", "B.txt", "报告2.txt", "报告10.txt"]
        );
        sort_by(
            &mut v,
            SortSpec {
                key: SortKey::Name,
                ascending: false,
            },
            |e| e,
        );
        assert_eq!(
            names(&v),
            ["报告10.txt", "报告2.txt", "B.txt", "a.txt", "照片"]
        );
    }

    #[test]
    fn size_and_date_sort() {
        let mut v = vec![
            f("大.bin", 300, 1),
            f("小.bin", 10, 3),
            d("目录"),
            f("中.bin", 100, 2),
        ];
        sort_by(
            &mut v,
            SortSpec {
                key: SortKey::Size,
                ascending: true,
            },
            |e| e,
        );
        assert_eq!(names(&v), ["目录", "小.bin", "中.bin", "大.bin"]);
        sort_by(
            &mut v,
            SortSpec {
                key: SortKey::Modified,
                ascending: false,
            },
            |e| e,
        );
        assert_eq!(names(&v), ["小.bin", "中.bin", "大.bin", "目录"]);
    }

    #[test]
    fn kind_sort_groups_extensions() {
        let mut v = vec![
            f("b.txt", 1, 1),
            f("a.zip", 1, 1),
            f("c.md", 1, 1),
            f("a.txt", 1, 1),
        ];
        sort_by(
            &mut v,
            SortSpec {
                key: SortKey::Kind,
                ascending: true,
            },
            |e| e,
        );
        assert_eq!(names(&v), ["c.md", "a.txt", "b.txt", "a.zip"]);
    }
}
