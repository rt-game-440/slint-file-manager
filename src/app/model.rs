//! 把浏览上下文的条目暴露为 Slint 模型。按需生成显示文字：大目录只为可见行格式化，
//! 不在界面线程为全部条目构造 `FileItem`。

use crate::browse::{Workspace, format};
use crate::{FileItem, browse::tab::Row};
use slint::{Model, ModelNotify, ModelTracker};
use std::cell::RefCell;
use std::rc::Rc;

/// 一个浏览面板当前标签的条目。切换标签时调用 [`ItemsModel::reset`]，模型随即指向新标签的行。
pub struct ItemsModel {
    ws: Rc<RefCell<Workspace>>,
    pane: usize,
    notify: ModelNotify,
}

impl ItemsModel {
    pub fn new(ws: Rc<RefCell<Workspace>>, pane: usize) -> ItemsModel {
        ItemsModel {
            ws,
            pane,
            notify: ModelNotify::default(),
        }
    }

    /// 条目整体变化（换目录、排序、切换标签）。
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
        // 回收站里这一列是“原位置”（列标题随之变为“原位置”），普通目录里是类型
        kind_text: match &e.bin {
            Some(b) => b.original_dir.as_str().into(),
            None => format::kind_text(e).into(),
        },
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
        self.ws.borrow().active_tab(self.pane).rows().len()
    }

    fn row_data(&self, row: usize) -> Option<FileItem> {
        self.ws
            .borrow()
            .active_tab(self.pane)
            .rows()
            .get(row)
            .map(to_item)
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}
