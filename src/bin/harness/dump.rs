//! 元素树与状态导出为 JSON。

use crate::ctx::{Ctx, r3, root};
use i_slint_backend_testing::ElementHandle;
use serde_json::{Value, json};
use std::ops::ControlFlow;
use std::path::Path;

fn element(e: &ElementHandle) -> Value {
    let pos = e.absolute_position();
    let size = e.size();
    json!({
        "id": e.id().map(|s| s.to_string()),
        "type": e.type_name().map(|s| s.to_string()),
        "role": e.accessible_role().map(|r| format!("{r:?}")),
        "label": e.accessible_label().map(|s| s.to_string()),
        "value": e.accessible_value().map(|s| s.to_string()),
        "enabled": e.accessible_enabled(),
        "checked": e.accessible_checked(),
        "selected": e.accessible_item_selected(),
        "expanded": e.accessible_expanded(),
        "x": r3(pos.x), "y": r3(pos.y), "w": r3(size.width), "h": r3(size.height),
        "opacity": r3(e.computed_opacity()),
    })
}

/// 路径只保留相对样例目录的部分，其余原样。
pub fn rel_path(p: &Path) -> String {
    if slint_file_manager::browse::location::is_recycle_bin(p) {
        return "<回收站>".into();
    }
    let scratch = crate::fixture::parent().join("scratch");
    if let Ok(rel) = p.strip_prefix(&scratch) {
        return format!("<草稿>\\{}", rel.display())
            .trim_end_matches('\\')
            .to_string();
    }
    let root = crate::fixture::root();
    match p.strip_prefix(&root) {
        Ok(rel) => format!("<样例目录>\\{}", rel.display())
            .trim_end_matches('\\')
            .to_string(),
        Err(_) => p.display().to_string(),
    }
}

/// 一个面板的标签组与当前标签状态（来自工作区，与界面是否显示无关）。
fn pane_state(ctx: &Ctx, p: usize) -> Value {
    let ws = ctx.ctl.ws();
    let pane = ws.pane(p);
    json!({
        "visible": ws.is_visible(p),
        "active_tab": pane.active_index(),
        "tabs": pane.tabs().iter().map(|t| json!({
            "location": t.location().map(rel_path),
            "view": format!("{:?}", t.view()),
            "sort": format!("{:?}", t.sort()),
            "unavailable": t.is_unavailable(),
            "can_back": t.can_back(),
            "can_forward": t.can_forward(),
        })).collect::<Vec<_>>(),
        "selected": ctx.ctl.selected_in(p),
        "item_count": pane.active_tab().rows().len(),
    })
}

/// 主窗口的浏览状态（Slint 生成的属性读取器 + 控制器）。元素级无法通用读取的状态（如焦点）通过这类属性导出。
/// 路径只记录相对样例目录的部分，使不同机器上的记录可比较。
pub fn extras(ctx: &Ctx) -> Value {
    let w = &ctx.win;
    let loc = ctx.ctl.location().map(|p| rel_path(&p));
    json!({
        "active_pane": w.get_active_pane(),
        "split": w.get_split(),
        "focused_pane": w.get_focused_pane(),
        "panes": [pane_state(ctx, 0), pane_state(ctx, 1)],
        "location": loc,
        "content_state": format!("{:?}", w.get_content_state()),
        "item_count": w.get_item_count(),
        "selected_count": w.get_selected_count(),
        "selected": ctx.ctl.selected_names(),
        "cursor_index": w.get_cursor_index(),
        "view": format!("{:?}", w.get_view()),
        "sort_key": format!("{:?}", w.get_sort_key()),
        "sort_ascending": w.get_sort_ascending(),
        "focus_area": w.get_focus_area().to_string(),
        "editing_address": w.get_editing_address(),
        "can_back": w.get_can_back(),
        "can_forward": w.get_can_forward(),
        "can_up": w.get_can_up(),
        "status_text": w.get_status_text().to_string(),
        "status_message": mask_time(&w.get_status_message()),
        "status_message_ok": w.get_status_message_ok(),
        "dialog": {
            "visible": w.get_dialog_visible(),
            "title": w.get_dialog_title().to_string(),
            "danger": w.get_dialog_danger(),
            "confirm": w.get_dialog_confirm_label().to_string(),
            "cancel": w.get_dialog_cancel_label().to_string(),
        },
        "settings": {
            "visible": w.get_settings_visible(),
            "entry_state": format!("{:?}", w.get_entry_view().state),
            "entry_title": w.get_entry_view().title.to_string(),
        },
        "bin_items": ctx.stash.bin_len(),
        "pending_loads": ctx.ctl.pending(),
        "opened": ctx.opened.borrow().iter().map(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).collect::<Vec<_>>(),
    })
}

/// 备份文件名里的时间戳（`session.backup-yyyyMMdd-HHmmss-…`）每次不同，记录里替换成占位符，保证重复运行的记录一致。
fn mask_time(s: &str) -> String {
    const KEY: &str = "session.backup-";
    match s.find(KEY) {
        Some(i) if s.len() >= i + KEY.len() + 15 => {
            format!("{}{}<时间>{}", &s[..i], KEY, &s[i + KEY.len() + 15..])
        }
        _ => s.to_string(),
    }
}

/// 元素按深度优先先序平铺。`visible` 恒为 true：不可见元素不会被遍历到。
pub fn tree(ctx: &Ctx) -> Value {
    let mut elems = vec![element(&root(ctx))];
    root(ctx).visit_descendants(|e| {
        elems.push(element(&e));
        ControlFlow::<()>::Continue(())
    });
    for (i, e) in elems.iter_mut().enumerate() {
        e["index"] = json!(i);
        e["visible"] = json!(true);
    }
    json!({ "extras": extras(ctx), "elements": elems })
}
