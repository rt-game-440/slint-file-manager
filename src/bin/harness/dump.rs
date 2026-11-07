//! 元素树与状态导出为 JSON。

use crate::ctx::{Ctx, r3, root};
use i_slint_backend_testing::ElementHandle;
use serde_json::{Value, json};
use std::ops::ControlFlow;

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

/// 主窗口的浏览状态（Slint 生成的属性读取器 + 控制器）。元素级无法通用读取的状态（如焦点）通过这类属性导出。
/// 路径只记录相对样例目录的部分，使不同机器上的记录可比较。
pub fn extras(ctx: &Ctx) -> Value {
    let w = &ctx.win;
    let root = crate::fixture::root();
    let loc = ctx.ctl.location().map(|p| match p.strip_prefix(&root) {
        Ok(rel) => format!("<样例目录>\\{}", rel.display())
            .trim_end_matches('\\')
            .to_string(),
        Err(_) => p.display().to_string(),
    });
    json!({
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
        "status_message": w.get_status_message().to_string(),
        "pending_loads": ctx.ctl.pending(),
        "opened": ctx.opened.borrow().iter().map(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).collect::<Vec<_>>(),
    })
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
