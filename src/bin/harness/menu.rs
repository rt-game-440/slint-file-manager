//! 右键菜单的选择语义与请求目标（Issue #9）。
//!
//! 无头后端不弹出系统菜单（`MenuMode::Record` 只记录请求）：这里证明界面的右键／键盘事件
//! 经过真实回调后，选择状态与“菜单作用于谁”正确——未选中项右键先单选，已选项右键保持选择，
//! 空白处右键清空选择并作用于文件夹，键盘（菜单键、Shift+F10）按选择决定，空文件夹也能呼出，
//! 双栏时右键在非活动面板上同时切换活动面板。真实系统菜单、NanaZip 命令见 `menu-selftest`。

use crate::ctx::{Ctx, Opts, Res};
use crate::dual::Rec;
use crate::fixture;
use i_slint_backend_testing::AccessibleRole;
use serde_json::Value;
use slint::ComponentHandle;
use slint::platform::{Key, PointerEventButton};
use slint_file_manager::shell_menu::Target;

impl Ctx {
    /// 右键单击一个元素（按下、松开都在元素中心）。
    pub fn right_click(&self, e: &i_slint_backend_testing::ElementHandle) {
        e.mock_single_click(PointerEventButton::Right);
    }

    /// 右键单击窗口逻辑坐标处。
    pub fn right_click_at(&self, x: f32, y: f32) {
        use slint::platform::WindowEvent;
        let position = slint::LogicalPosition::new(x, y);
        let w = self.win.window();
        w.dispatch_event(WindowEvent::PointerMoved { position });
        w.dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Right,
        });
        self.advance(50);
        w.dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Right,
        });
        self.advance(16);
    }

    fn menu_count(&self) -> usize {
        self.menus.borrow().len()
    }

    fn last_menu(&self) -> Option<slint_file_manager::app::MenuCall> {
        self.menus.borrow().last().cloned()
    }
}

fn items(names: &[&str]) -> Target {
    Target::Items(names.iter().map(|n| fixture::path(n)).collect())
}

pub fn context(size: (u32, u32), scale: f32) -> Res<(Vec<Value>, Vec<String>)> {
    let c = &Ctx::open(size, scale, Opts::default())?;
    let mut r = Rec::new();
    let w = &c.win;
    let photos = fixture::path(fixture::PHOTOS);
    let sel0 = |c: &Ctx| c.ctl.selected_in(0);

    // A. 未选中的项上右键：先单选它，菜单作用于它
    c.right_click(&c.item("图片003.webp")?);
    r.step(
        c,
        "A 在未选中的图片003 上右键",
        vec![
            (c.menu_count() == 1, "请求了一次菜单"),
            (sel0(c) == ["图片003.webp"], "该项被单选"),
            (
                c.last_menu()
                    .is_some_and(|m| m.target == items(&["图片003.webp"]) && m.pane == 0),
                "菜单作用于图片003（左面板）",
            ),
            (
                c.last_menu().is_some_and(|m| !m.intercept_open),
                "文件不接管“打开”",
            ),
            (
                c.last_menu().is_some_and(|m| m.intercept_delete),
                "选中项时接管“删除”（Issue #41）",
            ),
            (w.get_focus_area() == "content", "焦点仍在内容区"),
        ],
    );

    // B. 多选后在已选项上右键：保持选择，菜单作用于全部已选项（按显示顺序）
    c.click(&c.item("图片001.webp")?);
    c.ctrl_click(&c.item("图片002.webp")?);
    c.ctrl_click(&c.item("图片004.webp")?);
    c.right_click(&c.item("图片002.webp")?);
    r.step(
        c,
        "B 在已选的图片002 上右键（共选 3 项）",
        vec![
            (c.menu_count() == 2, "请求了第二次菜单"),
            (c.ctl.selected_in(0).len() == 3, "选择保持 3 项"),
            (
                c.last_menu().is_some_and(|m| {
                    m.target == items(&["图片001.webp", "图片002.webp", "图片004.webp"])
                }),
                "菜单作用于 3 个已选项",
            ),
        ],
    );

    // C. 多选时在未选中的项上右键：改为只选它
    c.right_click(&c.item("图片005.webp")?);
    r.step(
        c,
        "C 在未选中的图片005 上右键",
        vec![
            (sel0(c) == ["图片005.webp"], "选择改为只有图片005"),
            (
                c.last_menu()
                    .is_some_and(|m| m.target == items(&["图片005.webp"])),
                "菜单只作用于图片005",
            ),
        ],
    );

    // D. 文件夹：单选文件夹时接管“打开”（在本应用内进入）
    c.right_click(&c.item(fixture::PHOTOS)?);
    r.step(
        c,
        "D 在文件夹“照片”上右键",
        vec![
            (
                c.last_menu()
                    .is_some_and(|m| m.target == items(&[fixture::PHOTOS])),
                "菜单作用于该文件夹",
            ),
            (
                c.last_menu().is_some_and(|m| m.intercept_open),
                "单选文件夹时由本应用接管“打开”",
            ),
        ],
    );

    // E. 键盘：菜单键与 Shift+F10 按当前选择决定
    let n = c.menu_count();
    c.special(Key::Menu, &[]);
    let by_menu_key = c.last_menu();
    c.special(Key::F10, &[Key::Shift]);
    r.step(
        c,
        "E 键盘呼出菜单（选中文件夹“照片”）",
        vec![
            (c.menu_count() == n + 2, "菜单键与 Shift+F10 各请求一次"),
            (
                by_menu_key.is_some_and(|m| m.target == items(&[fixture::PHOTOS])),
                "菜单键作用于当前选择",
            ),
            (
                c.last_menu()
                    .is_some_and(|m| m.target == items(&[fixture::PHOTOS])),
                "Shift+F10 作用于当前选择",
            ),
            (
                c.ctl.selected_in(0) == [fixture::PHOTOS],
                "键盘呼出不改变选择",
            ),
        ],
    );

    // F. 进入“照片”：空白处右键清空选择并作用于文件夹；键盘在无选择时也作用于文件夹
    c.open_folder(fixture::PHOTOS)?;
    c.click(&c.item("海边003.webp")?);
    let area = c.pane_rect(0)?;
    let before = c.menu_count();
    c.right_click_at(area.x + 80.0, area.y + area.h - 40.0);
    r.step(
        c,
        "F 在“照片”的空白处右键",
        vec![
            (c.menu_count() == before + 1, "请求了一次菜单"),
            (
                c.last_menu()
                    .is_some_and(|m| m.target == Target::Background(photos.clone())),
                "菜单作用于文件夹本身",
            ),
            (
                c.last_menu().is_some_and(|m| !m.intercept_delete),
                "空白处菜单不接管“删除”（没有删除项）",
            ),
            (c.ctl.selected_in(0).is_empty(), "空白处右键清空选择"),
        ],
    );
    c.special(Key::Menu, &[]);
    r.step(
        c,
        "G 无选择时按菜单键",
        vec![
            (c.menu_count() == before + 2, "请求了一次菜单"),
            (
                c.last_menu()
                    .is_some_and(|m| m.target == Target::Background(photos.clone())),
                "无选择时作用于文件夹",
            ),
        ],
    );

    // H. 空文件夹没有文件视图，也能呼出菜单（“新建”）
    c.ctl.navigate_path(0, fixture::path(fixture::EMPTY));
    c.pump();
    let before = c.menu_count();
    let area = c.pane_rect(0)?;
    c.right_click_at(area.x + area.w / 2.0, area.y + area.h * 0.7);
    r.step(
        c,
        "H 空文件夹中右键",
        vec![
            (c.menu_count() == before + 1, "请求了一次菜单"),
            (
                c.last_menu()
                    .is_some_and(|m| m.target == Target::Background(fixture::path(fixture::EMPTY))),
                "菜单作用于空文件夹",
            ),
        ],
    );

    // I. 无权限的文件夹显示错误面板：不弹出菜单（拒绝访问的位置没有可用的外壳菜单）
    c.ctl.navigate_path(0, fixture::path(fixture::DENIED));
    c.pump();
    let before = c.menu_count();
    let area = c.pane_rect(0)?;
    c.right_click_at(area.x + area.w / 2.0, area.y + area.h * 0.7);
    let after_mouse = c.menu_count();
    c.special(Key::Menu, &[]);
    r.step(
        c,
        "I 无权限文件夹的错误面板",
        vec![
            (after_mouse == before, "错误面板上鼠标右键不呼出菜单"),
            (
                c.menu_count() == before + 1,
                "菜单键仍作用于该文件夹（外壳自行处理拒绝访问）",
            ),
        ],
    );

    // K. 回收站是虚拟位置，没有磁盘路径可交给外壳菜单：鼠标与键盘都不呼出菜单
    c.ctl
        .navigate_path(0, slint_file_manager::browse::location::recycle_bin_path());
    c.pump();
    let before = c.menu_count();
    let area = c.pane_rect(0)?;
    c.right_click_at(area.x + area.w / 2.0, area.y + area.h * 0.7);
    c.special(Key::Menu, &[]);
    r.step(
        c,
        "K 回收站中右键与菜单键",
        vec![(c.menu_count() == before, "回收站里不呼出外壳菜单")],
    );
    // J. 双栏：在右面板（非活动）的项上右键，同时让右面板成为活动面板，菜单作用于右面板的项
    let c = &Ctx::open(
        size,
        scale,
        Opts {
            split: true,
            ..Opts::default()
        },
    )?;
    let item = c.find_in(1, AccessibleRole::ListItem, fixture::PHOTOS)?;
    c.right_click(&item);
    r.step(
        c,
        "J 在右面板的“照片”上右键",
        vec![
            (c.ctl.ws().active_pane() == 1, "右面板成为活动面板"),
            (
                c.last_menu()
                    .is_some_and(|m| m.pane == 1 && m.target == items(&[fixture::PHOTOS])),
                "菜单作用于右面板的“照片”",
            ),
            (c.ctl.selected_in(1) == [fixture::PHOTOS], "右面板单选该项"),
            (c.ctl.selected_in(0).is_empty(), "左面板选择不受影响"),
            (c.win.get_focused_pane() == 1, "键盘焦点移到右面板"),
        ],
    );
    Ok((r.steps, r.fails))
}
