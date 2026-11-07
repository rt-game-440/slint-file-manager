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

/// 主窗口的示例状态（Slint 生成的属性读取器）。元素级无法通用读取的状态（如焦点）通过这类属性导出。
pub fn extras(ctx: &Ctx) -> Value {
    json!({
        "click_count": ctx.win.get_click_count(),
        "panel_open": ctx.win.get_panel_open(),
        "key_focused": ctx.win.get_key_focused(),
        "list_scroll_y": r3(ctx.win.get_list_scroll_y()),
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
