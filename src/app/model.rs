//! 把浏览上下文的条目暴露为 Slint 模型。按需生成显示文字：大目录只为可见行格式化，
//! 不在界面线程为全部条目构造 `FileItem`。缩略图状态同样按需从内存缓存查询（见 [`super::thumbs`]）。

use super::thumbs::Thumbs;
use crate::browse::{ViewMode as CoreView, Workspace, format};
use crate::thumbs::{Source, wants_any_thumbnail};
use crate::{FileItem, ThumbState, browse::tab::Row};
use slint::{Model, ModelNotify, ModelTracker};
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::OsString;
use std::rc::Rc;

/// 叠加在条目上的临时标记：剪切（变淡）、正在重命名、拖放目标高亮。由控制器写入，模型读取。
#[derive(Default)]
pub struct Marks {
    /// 被剪切的路径（[`crate::xfer::key`] 归一化）。
    pub cut: HashSet<String>,
    /// 正在编辑名称的条目：（面板，系统名称）。
    pub editing: Option<(usize, OsString)>,
    /// 拖放的当前目标文件夹：（面板，行号）。
    pub drop: Option<(usize, usize)>,
}

/// 一个浏览面板当前标签的条目。切换标签时调用 [`ItemsModel::reset`]，模型随即指向新标签的行。
pub struct ItemsModel {
    ws: Rc<RefCell<Workspace>>,
    thumbs: Rc<Thumbs>,
    pane: usize,
    marks: Rc<RefCell<Marks>>,
    notify: ModelNotify,
}

impl ItemsModel {
    pub fn new(
        ws: Rc<RefCell<Workspace>>,
        thumbs: Rc<Thumbs>,
        pane: usize,
        marks: Rc<RefCell<Marks>>,
    ) -> ItemsModel {
        ItemsModel {
            ws,
            thumbs,
            pane,
            marks,
            notify: ModelNotify::default(),
        }
    }

    /// 条目整体变化（换目录、排序、切换标签）。
    pub fn reset(&self) {
        self.notify.reset();
    }

    /// 单行变化（选择、缩略图结果）。
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
        thumb_state: ThumbState::None,
        thumb: Default::default(),
        editing: false,
        cut: false,
        drop_target: false,
        cloud: badge(e.cloud),
    }
}

/// 云盘状态 → 界面标记。
pub fn badge(c: crate::cloud::CloudState) -> crate::CloudBadge {
    use crate::cloud::CloudState as S;
    match c {
        S::None => crate::CloudBadge::None,
        S::OnlineOnly => crate::CloudBadge::Online,
        S::OnlineUnavailable => crate::CloudBadge::Unavailable,
        S::Local => crate::CloudBadge::Local,
        S::AlwaysKeep => crate::CloudBadge::Pinned,
    }
}

impl Model for ItemsModel {
    type Data = FileItem;

    fn row_count(&self) -> usize {
        self.ws.borrow().active_tab(self.pane).rows().len()
    }

    fn row_data(&self, row: usize) -> Option<FileItem> {
        let ws = self.ws.borrow();
        let t = ws.active_tab(self.pane);
        let r = t.rows().get(row)?;
        let mut item = to_item(r);
        let e = &r.entry;
        if self.thumbs.enabled()
            && t.view() == CoreView::Grid
            && !e.is_dir
            && e.bin.is_none()
            && wants_any_thumbnail(&e.ext)
            && let Some(loc) = t.location()
        {
            let key = Source::new(loc.join(&e.os_name), e.size, e.modified).key();
            (item.thumb_state, item.thumb) = self.thumbs.lookup(key);
        }
        let m = self.marks.borrow();
        if let Some((p, name)) = &m.editing
            && *p == self.pane
            && r.entry.os_name == *name
        {
            item.editing = true;
        }
        if !m.cut.is_empty()
            && !t.is_recycle_bin()
            && let Some(path) = t.path_of(row)
        {
            item.cut = m.cut.contains(&crate::xfer::key(&path));
        }
        item.drop_target = m.drop == Some((self.pane, row));
        Some(item)
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}
