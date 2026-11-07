//! 把浏览上下文的条目暴露为 Slint 模型。按需生成显示文字：大目录只为可见行格式化，
//! 不在界面线程为全部条目构造 `FileItem`。

use crate::browse::{Tab, format};
use crate::{FileItem, browse::tab::Row};
use slint::{Model, ModelNotify, ModelTracker};
use std::cell::RefCell;
use std::rc::Rc;

pub struct ItemsModel {
    tab: Rc<RefCell<Tab>>,
    notify: ModelNotify,
}

impl ItemsModel {
    pub fn new(tab: Rc<RefCell<Tab>>) -> ItemsModel {
        ItemsModel {
            tab,
            notify: ModelNotify::default(),
        }
    }

    /// 条目整体变化（换目录、排序）。
    pub fn reset(&self) {
        self.notify.reset();
    }

    /// 单行变化（选择）。
    pub fn row_changed(&self, row: usize) {
        self.notify.row_changed(row);
    }
}

pub fn to_item(r: &Row) -> FileItem {
    let e = &r.entry;
    FileItem {
        name: e.name.as_str().into(),
        kind_text: format::kind_text(e).into(),
        size_text: if e.is_dir {
            Default::default()
        } else {
            format::size_text(e.size).into()
        },
        date_text: format::date_text(e.modified).into(),
        is_dir: e.is_dir,
        icon: format::icon_name(e).into(),
        selected: r.selected,
    }
}

impl Model for ItemsModel {
    type Data = FileItem;

    fn row_count(&self) -> usize {
        self.tab.borrow().rows().len()
    }

    fn row_data(&self, row: usize) -> Option<FileItem> {
        self.tab.borrow().rows().get(row).map(to_item)
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}
