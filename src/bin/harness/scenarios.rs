//! 命名场景：截图与动画采集的统一入口。新增界面时在这里追加场景，不要为场景另建抽象层。

use crate::ctx::{Ctx, Res};
use serde_json::{Value, json};

pub struct Scenario {
    pub name: &'static str,
    /// 场景内容（给 3 级审阅材料用）
    pub content: &'static str,
    /// 预期效果（给 3 级审阅材料用）
    pub expected: &'static str,
    pub setup: fn(&Ctx) -> Res<()>,
}

pub const TOGGLE_LABEL: &str = "切换示例面板";
pub const LIST_LABEL: &str = "示例列表";
/// 逐帧采集时被跟踪的元素。
pub const TRACKED: &[(&str, &str)] = &[("panel", "MainWindow::demo-panel")];

fn idle(_: &Ctx) -> Res<()> {
    Ok(())
}

fn panel_open(c: &Ctx) -> Res<()> {
    c.click(&c.by_label(TOGGLE_LABEL)?);
    c.advance(1000);
    Ok(())
}

fn list_scrolled(c: &Ctx) -> Res<()> {
    c.by_label(LIST_LABEL)?.scroll(0.0, -60.0);
    c.advance(500);
    Ok(())
}

pub const STATIC: &[Scenario] = &[
    Scenario {
        name: "idle",
        content: "初始状态：标题居中，左上角蓝色按钮，浅橙色半透明面板在左侧，下方列表顶部",
        expected: "按钮文字为“切换面板（0）”且无焦点边框；面板 x=24、不透明度 0.3；列表首行为“行 0”",
        setup: idle,
    },
    Scenario {
        name: "panel-open",
        content: "点击按钮并等待动画结束后的状态",
        expected: "面板移到 x=400 且不透明；按钮文字“切换面板（1）”并带深蓝焦点边框",
        setup: panel_open,
    },
    Scenario {
        name: "list-scrolled",
        content: "对列表滚动 60 像素后的状态",
        expected: "列表内容上移 60px，可见首行为“行 2”",
        setup: list_scrolled,
    },
];

/// 动画场景：`setup` 在采集 t=0 之前执行，随后 `start` 触发动画。
pub struct AnimScenario {
    pub name: &'static str,
    pub content: &'static str,
    pub expected: &'static str,
    pub start: fn(&Ctx) -> Res<()>,
}

fn toggle(c: &Ctx) -> Res<()> {
    c.click(&c.by_label(TOGGLE_LABEL)?);
    Ok(())
}

pub const ANIM: &[AnimScenario] = &[AnimScenario {
    name: "panel-toggle",
    content: "点击按钮后面板 400ms 的位置与透明度动画",
    expected: "面板 x 由 24 经 ease-out-back 到 400（会过冲后回落），透明度 0.3 到 1；t≥400ms 后终态不再变化",
    start: toggle,
}];

pub fn find_static(name: &str) -> Res<&'static Scenario> {
    STATIC.iter().find(|s| s.name == name).ok_or_else(|| {
        format!(
            "未知静态场景 {name}，可选：{}",
            names(STATIC.iter().map(|s| s.name))
        )
        .into()
    })
}

pub fn find_anim(name: &str) -> Res<&'static AnimScenario> {
    ANIM.iter().find(|s| s.name == name).ok_or_else(|| {
        format!(
            "未知动画场景 {name}，可选：{}",
            names(ANIM.iter().map(|s| s.name))
        )
        .into()
    })
}

fn names<'a>(it: impl Iterator<Item = &'a str>) -> String {
    it.collect::<Vec<_>>().join("、")
}

/// 交互脚本：按标识查找、点击、按键、滚动，并对状态断言。返回每步状态与失败列表。
pub fn interaction(c: &Ctx) -> Res<(Vec<Value>, Vec<String>)> {
    use crate::dump::extras;
    let mut steps = Vec::new();
    let mut fails = Vec::new();
    let mut check = |ok: bool, msg: &str| {
        if !ok {
            fails.push(msg.to_string());
        }
    };
    let panel_x = |c: &Ctx| {
        c.by_id("MainWindow::demo-panel")
            .map(|e| e.absolute_position().x)
    };
    let mut record = |c: &Ctx, name: &str| {
        steps.push(json!({
            "step": name,
            "t_ms": c.elapsed(),
            "state": extras(c),
            "panel_x": panel_x(c).ok().map(crate::ctx::r3),
        }));
    };

    record(c, "初始");
    let button = c.by_label(TOGGLE_LABEL)?;
    check(
        button.accessible_checked() == Some(false),
        "初始按钮应为未选中",
    );
    c.click(&button);
    record(c, "点击按钮后");
    check(
        c.win.get_click_count() == 1 && c.win.get_panel_open(),
        "点击后 click_count=1 且面板打开",
    );
    check(
        button.accessible_checked() == Some(true),
        "点击后按钮应为选中",
    );
    check(c.win.get_key_focused(), "点击后焦点应在示例焦点域");

    c.advance(500);
    record(c, "动画结束后");
    check((panel_x(c)? - 400.0).abs() < 0.5, "动画结束后面板 x=400");

    c.key(" ");
    c.advance(500);
    record(c, "空格键后");
    check(
        c.win.get_click_count() == 2 && !c.win.get_panel_open(),
        "空格后 click_count=2 且面板关闭",
    );
    check((panel_x(c)? - 24.0).abs() < 0.5, "关闭动画结束后面板 x=24");

    c.by_label(LIST_LABEL)?.scroll(0.0, -60.0);
    c.advance(100);
    record(c, "滚动后");
    check(
        (c.win.get_list_scroll_y() as f64 + 60.0).abs() < 0.5,
        "列表滚动后 list_scroll_y=-60",
    );
    Ok((steps, fails))
}
