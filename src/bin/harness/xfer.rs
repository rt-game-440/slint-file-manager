//! 新建、重命名、剪贴板与拖放的交互脚本（Issue #12）。
//!
//! 全部在 `%TEMP%\sfm-harness\scratch\xfer` 的一次性数据上进行，后端是临时目录后端（[`StashBackend`]，可注入失败），
//! 剪贴板是内存剪贴板、拖出窗口只记录路径，**绝不触碰系统剪贴板、回收站或用户文件**。
//! 窗口为双栏：左栏 `src`、右栏 `dst`。覆盖：新建文件夹与文件（默认进入重命名、选中范围不含扩展名）、F2 重命名
//! （重名与非法名称留在编辑、大小写变更、Esc 取消、点别处提交）、复制／剪切／粘贴与剪切淡化标记、同名冲突三个选项、
//! 失败注入（权限、磁盘满、设备断开、用户取消）下的部分成功汇报、跨卷移动、应用内拖放（默认语义、Ctrl／Shift、
//! 提示与实际一致、拖到文件夹、拖出窗口、Esc 取消、多选不拖动时收拢）、资源管理器拖入（模拟 DropHost）。

use crate::ctx::{Ctx, Opts, Res};
use crate::dual::Rec;
use crate::fixture;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use serde_json::Value;
use slint::ComponentHandle;
use slint::Model;
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint_file_manager::clipboard::{Clipboard, Effect};
use slint_file_manager::dragdrop::{EFFECT_COPY, EFFECT_MOVE, MK_CONTROL, MK_SHIFT};
use slint_file_manager::fileops::{FailKind, Inject};
use slint_file_manager::{DragBus, FileItem, RenameBus};
use std::fs;
use std::path::{Path, PathBuf};

fn disk(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}
fn has_disk(dir: &Path, n: &str) -> bool {
    dir.join(n).symlink_metadata().is_ok()
}
fn dlg(c: &Ctx) -> bool {
    c.win.get_dialog_visible()
}
fn title(c: &Ctx) -> String {
    c.win.get_dialog_title().to_string()
}
fn body(c: &Ctx) -> String {
    c.win.get_dialog_body().to_string()
}
fn msg(c: &Ctx) -> String {
    c.win.get_status_message().to_string()
}
fn names(c: &Ctx, p: usize) -> Vec<String> {
    c.ctl.names_in(p)
}
fn has(c: &Ctx, p: usize, n: &str) -> bool {
    names(c, p).iter().any(|x| x == n)
}
fn sel(c: &Ctx, p: usize) -> Vec<String> {
    c.ctl.selected_in(p)
}
fn dlg_btn(c: &Ctx, label: &str) -> Res<ElementHandle> {
    let t = c.find_all(AccessibleRole::Text, "对话框标题");
    let ty = t.first().ok_or("对话框没有显示")?.absolute_position().y;
    c.find_all(AccessibleRole::Button, label)
        .into_iter()
        .find(|e| e.absolute_position().y > ty)
        .ok_or_else(|| {
            format!(
                "对话框里找不到按钮：{label}（标题“{}”）",
                c.win.get_dialog_title()
            )
            .into()
        })
}

fn center(e: &ElementHandle) -> (f32, f32) {
    let (p, s) = (e.absolute_position(), e.size());
    (p.x + s.width / 2.0, p.y + s.height / 2.0)
}

/// 指针按下 → 经过若干点移动（不释放）。
fn press_and_move(c: &Ctx, from: (f32, f32), via: &[(f32, f32)]) {
    let w = c.win.window();
    let pos = |p: (f32, f32)| slint::LogicalPosition::new(p.0, p.1);
    w.dispatch_event(WindowEvent::PointerMoved {
        position: pos(from),
    });
    w.dispatch_event(WindowEvent::PointerPressed {
        position: pos(from),
        button: PointerEventButton::Left,
    });
    c.advance(20);
    for p in via {
        w.dispatch_event(WindowEvent::PointerMoved { position: pos(*p) });
        c.advance(20);
    }
}

fn release(c: &Ctx, p: (f32, f32)) {
    c.win.window().dispatch_event(WindowEvent::PointerReleased {
        position: slint::LogicalPosition::new(p.0, p.1),
        button: PointerEventButton::Left,
    });
    c.advance(20);
}

fn pick(c: &Ctx, p: usize, items: &[&str]) -> Res<()> {
    for (i, n) in items.iter().enumerate() {
        let e = c.item_in(p, n)?;
        if i == 0 {
            c.click(&e);
        } else {
            c.ctrl_click(&e);
        }
    }
    Ok(())
}

fn row(c: &Ctx, p: usize, name: &str) -> Option<FileItem> {
    let m = if p == 0 {
        c.win.get_left_items()
    } else {
        c.win.get_right_items()
    };
    (0..m.row_count())
        .filter_map(|i| m.row_data(i))
        .find(|r| r.name == name)
}

fn ctrl_shift_n(c: &Ctx) {
    c.with_mods(&[Key::Control, Key::Shift], || c.key("N"));
    c.pump();
}

fn editor_count(c: &Ctx) -> usize {
    c.find_all(AccessibleRole::TextInput, "重命名").len()
}

fn setup(dir: &Path, src: &Path, dst: &Path) -> Res<()> {
    let _ = dir;
    for n in ["甲.txt", "乙.txt", "丙.txt", "丁.txt", "readme.txt"] {
        fs::write(src.join(n), format!("内容：{n}"))?;
    }
    fs::create_dir(src.join("子目录"))?;
    fs::write(src.join("子目录").join("内.txt"), "内")?;
    fs::write(dst.join("同名.txt"), "目标里原有的同名文件")?;
    fs::write(src.join("同名.txt"), "来源里的同名文件")?;
    fs::create_dir(dst.join("目标子目录"))?;
    Ok(())
}

pub fn xfer(size: (u32, u32), scale: f32) -> Res<(Vec<Value>, Vec<String>)> {
    let base: PathBuf = fixture::fresh_dir(&fixture::scratch_dir("xfer"))?;
    let (src, dst) = (base.join("src"), base.join("dst"));
    fs::create_dir_all(&src)?;
    fs::create_dir_all(&dst)?;
    setup(&base, &src, &dst)?;
    let c = &Ctx::open(
        size,
        scale,
        Opts {
            start: src.clone(),
            split: true,
            ..Opts::default()
        },
    )?;
    let mut r = Rec::new();
    let st = c.stash.clone();

    // 右栏进入 dst（F6 切到右栏输入地址，再切回左栏）
    c.special(Key::F6, &[]);
    c.enter_address(&dst.to_string_lossy())?;
    c.pump();
    c.special(Key::F6, &[]);
    r.step(
        c,
        "A0 初始：左 src、右 dst，没有编辑框、没有对话框",
        vec![
            (
                c.ctl.location_in(0).as_deref() == Some(src.as_path()),
                "左栏应在 src",
            ),
            (
                c.ctl.location_in(1).as_deref() == Some(dst.as_path()),
                "右栏应在 dst",
            ),
            (editor_count(c) == 0, "不应有重命名输入框"),
            (!dlg(c), "不应有对话框"),
        ],
    );

    // ---------- B. 新建文件夹：创建、选中、立刻进入重命名 ----------
    ctrl_shift_n(c);
    let sel_end = c.win.global::<RenameBus>().get_select_end();
    r.step(
        c,
        "B1 Ctrl+Shift+N：磁盘上建出“新建文件夹”，选中它并进入重命名（整段选中）",
        vec![
            (src.join("新建文件夹").is_dir(), "磁盘上应有“新建文件夹”"),
            (c.ctl.rename_active(), "应进入重命名"),
            (c.ctl.rename_text() == "新建文件夹", "编辑框默认文本"),
            (
                sel(c, 0) == vec!["新建文件夹".to_string()],
                "应选中新文件夹",
            ),
            (editor_count(c) == 1, "应显示一个输入框"),
            (sel_end == "新建文件夹".len() as i32, "文件夹默认全选名称"),
            (msg(c) == "已新建“新建文件夹”", "状态栏应如实汇报"),
        ],
    );
    c.type_text("资料");
    r.step(
        c,
        "B2 输入框接收键盘输入",
        vec![(c.ctl.rename_text() == "资料", "输入应替换选中的默认名称")],
    );
    c.special(Key::Return, &[]);
    c.pump();
    r.step(
        c,
        "B3 Enter 确定：改名为“资料”，退出编辑，选中改名结果",
        vec![
            (
                src.join("资料").is_dir() && !has_disk(&src, "新建文件夹"),
                "磁盘上应已改名",
            ),
            (!c.ctl.rename_active() && editor_count(c) == 0, "应退出编辑"),
            (sel(c, 0) == vec!["资料".to_string()], "应选中“资料”"),
            (msg(c) == "已重命名为“资料”", "状态栏应汇报改名"),
        ],
    );

    // ---------- C. 新建文件（模板菜单）：默认选中不含扩展名的部分 ----------
    c.click(&c.button("新建")?);
    c.advance(50);
    let menu_ok = c.find_all(AccessibleRole::Button, "文本文档").len() == 1;
    if menu_ok {
        c.click(&c.find_all(AccessibleRole::Button, "文本文档")[0]);
        c.pump();
    } else {
        c.ctl.new_file(0, 0);
        c.pump();
    }
    let sel_end = c.win.global::<RenameBus>().get_select_end();
    r.step(
        c,
        "C1 工具栏“新建 → 文本文档”：建出“新建 文本文档.txt”，只选中不含扩展名的部分",
        vec![
            (menu_ok, "“新建”菜单里应有“文本文档”项"),
            (src.join("新建 文本文档.txt").is_file(), "磁盘上应有新文件"),
            (c.ctl.rename_active(), "应进入重命名"),
            (
                c.ctl.rename_text() == "新建 文本文档.txt",
                "默认文本含扩展名",
            ),
            (
                sel_end == "新建 文本文档".len() as i32,
                "选中范围应不含扩展名",
            ),
        ],
    );
    c.special(Key::Escape, &[]);
    c.pump();
    r.step(
        c,
        "C2 Esc 取消：文件保留默认名称，退出编辑",
        vec![
            (!c.ctl.rename_active() && editor_count(c) == 0, "应退出编辑"),
            (has_disk(&src, "新建 文本文档.txt"), "文件应保留默认名称"),
        ],
    );
    // 再建一个：名称自动避开已有的
    ctrl_shift_n(c);
    r.step(
        c,
        "C3 再次新建文件夹：名称自动编号“新建文件夹”（上一个已改名，可再用）",
        vec![(src.join("新建文件夹").is_dir(), "应建出“新建文件夹”")],
    );
    ctrl_shift_n(c); // 编辑中再次新建：先按失去焦点提交（名称未改动）
    r.step(
        c,
        "C4 编辑中再次 Ctrl+Shift+N：上一个按失去焦点提交，新建“新建文件夹 (2)”",
        vec![
            (
                src.join("新建文件夹 (2)").is_dir(),
                "应建出“新建文件夹 (2)”",
            ),
            (c.ctl.rename_active(), "应进入新的重命名"),
        ],
    );
    c.special(Key::Escape, &[]);
    c.pump();
    // 清理两个多余的空文件夹，让后续场景简单
    let _ = fs::remove_dir(src.join("新建文件夹"));
    let _ = fs::remove_dir(src.join("新建文件夹 (2)"));
    c.special(Key::F5, &[]);
    c.pump();

    // ---------- D. F2 重命名 ----------
    pick(c, 0, &["甲.txt"])?;
    c.special(Key::F2, &[]);
    let sel_end = c.win.global::<RenameBus>().get_select_end();
    r.step(
        c,
        "D1 F2：进入重命名，选中“甲”（不含 .txt）",
        vec![
            (
                c.ctl.rename_active() && editor_count(c) == 1,
                "应进入重命名",
            ),
            (c.ctl.rename_text() == "甲.txt", "初始文本"),
            (sel_end == "甲".len() as i32, "选中范围不含扩展名"),
        ],
    );
    c.ctl.set_rename_text("乙.txt");
    c.special(Key::Return, &[]);
    let err = c.win.global::<RenameBus>().get_error().to_string();
    r.step(
        c,
        "D2 改成已有名称：留在编辑状态，输入框报错，文件不变",
        vec![
            (c.ctl.rename_active(), "应仍在编辑"),
            (err.contains("已存在"), "输入框应说明重名"),
            (
                has_disk(&src, "甲.txt")
                    && fs::read_to_string(src.join("乙.txt"))? == "内容：乙.txt",
                "文件不变、乙.txt 未被覆盖",
            ),
            (msg(c).contains("已存在"), "状态栏也应说明"),
        ],
    );
    c.ctl.set_rename_text("a/b.txt");
    c.special(Key::Return, &[]);
    let err = c.win.global::<RenameBus>().get_error().to_string();
    r.step(
        c,
        "D3 非法字符：留在编辑状态并说明",
        vec![
            (c.ctl.rename_active(), "应仍在编辑"),
            (!err.is_empty(), "输入框应说明原因"),
            (has_disk(&src, "甲.txt"), "文件不变"),
        ],
    );
    c.ctl.set_rename_text("CON");
    c.special(Key::Return, &[]);
    r.step(
        c,
        "D4 保留名称 CON：留在编辑状态",
        vec![
            (c.ctl.rename_active(), "应仍在编辑"),
            (has_disk(&src, "甲.txt"), "文件不变"),
        ],
    );
    c.ctl.set_rename_text("  ");
    c.special(Key::Return, &[]);
    r.step(
        c,
        "D5 空名称：留在编辑状态",
        vec![
            (c.ctl.rename_active(), "应仍在编辑"),
            (has_disk(&src, "甲.txt"), "文件不变"),
        ],
    );
    c.ctl.set_rename_text("甲甲.txt");
    c.special(Key::Return, &[]);
    c.pump();
    r.step(
        c,
        "D6 合法新名称：改名成功并选中",
        vec![
            (!c.ctl.rename_active(), "应退出编辑"),
            (
                has_disk(&src, "甲甲.txt") && !has_disk(&src, "甲.txt"),
                "磁盘上已改名",
            ),
            (sel(c, 0) == vec!["甲甲.txt".to_string()], "应选中新名称"),
        ],
    );
    pick(c, 0, &["readme.txt"])?;
    c.special(Key::F2, &[]);
    c.ctl.set_rename_text("README.txt");
    c.special(Key::Return, &[]);
    c.pump();
    r.step(
        c,
        "D7 只改大小写：不会被当成重名，磁盘名称真的变化",
        vec![
            (
                disk(&src).iter().any(|n| n == "README.txt")
                    && !disk(&src).iter().any(|n| n == "readme.txt"),
                "磁盘上应是 README.txt",
            ),
            (!c.ctl.rename_active(), "应退出编辑"),
        ],
    );
    pick(c, 0, &["丁.txt"])?;
    c.special(Key::F2, &[]);
    c.ctl.set_rename_text("不会生效.txt");
    c.special(Key::Escape, &[]);
    c.pump();
    r.step(
        c,
        "D8 Esc 取消：名称不变",
        vec![
            (!c.ctl.rename_active(), "应退出编辑"),
            (
                has_disk(&src, "丁.txt") && !has_disk(&src, "不会生效.txt"),
                "文件名不变",
            ),
        ],
    );
    // 点别处提交
    c.special(Key::F2, &[]);
    c.ctl.set_rename_text("丁丁.txt");
    c.click(&c.item_in(0, "丙.txt")?);
    c.pump();
    r.step(
        c,
        "D9 编辑中点击别的项目：按失去焦点提交（合法名称改名成功）",
        vec![
            (!c.ctl.rename_active() && editor_count(c) == 0, "应退出编辑"),
            (
                has_disk(&src, "丁丁.txt") && !has_disk(&src, "丁.txt"),
                "应已改名",
            ),
        ],
    );
    c.special(Key::F2, &[]);
    c.ctl.set_rename_text("坏/名.txt");
    c.click(&c.item_in(0, "乙.txt")?);
    c.pump();
    r.step(
        c,
        "D10 编辑中点击别处且名称不合法：放弃本次编辑并说明，不改任何文件",
        vec![
            (!c.ctl.rename_active(), "应退出编辑"),
            (has_disk(&src, "丙.txt"), "丙.txt 名称不变"),
            (msg(c).contains("没有重命名"), "状态栏应说明没有重命名"),
        ],
    );
    // 多选 F2：只提示
    pick(c, 0, &["乙.txt", "丙.txt"])?;
    c.special(Key::F2, &[]);
    r.step(
        c,
        "D11 多选按 F2：只提示，不进入编辑",
        vec![
            (!c.ctl.rename_active(), "不应进入编辑"),
            (msg(c).contains("只选择一项"), "应提示"),
        ],
    );
    // 重命名失败注入：权限不足
    pick(c, 0, &["乙.txt"])?;
    st.inject(src.join("乙.txt"), Inject::Rename(FailKind::AccessDenied));
    c.special(Key::F2, &[]);
    c.ctl.set_rename_text("乙乙.txt");
    c.special(Key::Return, &[]);
    c.pump();
    r.step(
        c,
        "D12 重命名遇到权限不足：如实报告失败，名称不变",
        vec![
            (dlg(c) && title(c) == "没有重命名成功", "应弹说明框"),
            (body(c).contains("没有权限"), "说明原因"),
            (
                has_disk(&src, "乙.txt") && !has_disk(&src, "乙乙.txt"),
                "名称不变",
            ),
            (!c.win.get_status_message_ok(), "失败用警示色"),
        ],
    );
    c.special(Key::Return, &[]);
    c.pump();
    st.clear_injections();

    // ---------- E. 复制 / 粘贴 / 冲突 ----------
    pick(c, 0, &["丙.txt"])?;
    c.ctrl("c");
    let clip = c.clip.read();
    r.step(
        c,
        "E1 Ctrl+C：剪贴板里是该文件（复制），没有淡化标记",
        vec![
            (
                clip.as_ref().is_some_and(|f| {
                    f.effect == Effect::Copy && f.paths == vec![src.join("丙.txt")]
                }),
                "剪贴板应含丙.txt（复制）",
            ),
            (msg(c).contains("已复制"), "状态栏提示"),
            (!c.ctl.cut_active(), "复制不是剪切"),
        ],
    );
    c.special(Key::F6, &[]);
    c.ctrl("v");
    c.pump();
    r.step(
        c,
        "E2 在右栏 Ctrl+V：复制到 dst，来源保留，选中粘贴结果",
        vec![
            (
                has_disk(&dst, "丙.txt") && has_disk(&src, "丙.txt"),
                "两边都应有",
            ),
            (
                fs::read_to_string(dst.join("丙.txt"))? == "内容：丙.txt",
                "内容一致",
            ),
            (has(c, 1, "丙.txt"), "右栏列表已刷新"),
            (sel(c, 1) == vec!["丙.txt".to_string()], "应选中粘贴结果"),
            (msg(c) == "已复制“丙.txt”", "如实汇报"),
        ],
    );
    c.ctrl("v");
    c.pump();
    r.step(
        c,
        "E3 再粘贴一次：同名冲突，弹出三选项，尚未改动任何文件",
        vec![
            (dlg(c) && title(c) == "目标位置已有同名项目", "应弹冲突框"),
            (
                c.win.get_dialog_confirm_label() == "保留两者",
                "主按钮“保留两者”",
            ),
            (
                c.win.get_dialog_secondary_label() == "跳过",
                "第二按钮“跳过”",
            ),
            (c.win.get_dialog_cancel_label() == "取消", "取消按钮"),
            (body(c).contains("丙.txt"), "点名冲突项"),
            (
                disk(&dst) == vec!["丙.txt", "同名.txt", "目标子目录"],
                "冲突框打开时 dst 不变",
            ),
        ],
    );
    c.click(&dlg_btn(c, "保留两者")?);
    c.pump();
    r.step(
        c,
        "E4 保留两者：新来的改名为“丙 (2).txt”，已有的丙.txt 原样",
        vec![
            (
                has_disk(&dst, "丙 (2).txt") && has_disk(&dst, "丙.txt"),
                "两个都在",
            ),
            (!dlg(c), "对话框应关闭"),
        ],
    );
    c.ctrl("v");
    c.click(&dlg_btn(c, "跳过")?);
    c.pump();
    r.step(
        c,
        "E5 再次冲突选“跳过”：什么都不新增，如实说明",
        vec![
            (
                disk(&dst) == vec!["丙 (2).txt", "丙.txt", "同名.txt", "目标子目录"],
                "dst 不变",
            ),
            (msg(c).contains("没有复制任何项目"), "状态栏如实说明"),
            (!dlg(c), "对话框应关闭"),
        ],
    );
    c.ctrl("v");
    c.click(&dlg_btn(c, "取消")?);
    c.pump();
    r.step(
        c,
        "E6 冲突框点取消：不做任何改动",
        vec![
            (
                disk(&dst) == vec!["丙 (2).txt", "丙.txt", "同名.txt", "目标子目录"],
                "dst 不变",
            ),
            (msg(c).contains("已取消"), "提示已取消"),
        ],
    );
    c.ctrl("v");
    c.special(Key::Escape, &[]);
    c.pump();
    r.step(
        c,
        "E7 冲突框按 Esc = 取消",
        vec![
            (!dlg(c), "对话框应关闭"),
            (disk(&dst).len() == 4, "dst 不变"),
        ],
    );

    // ---------- F. 剪切 / 粘贴 ----------
    c.special(Key::F6, &[]);
    pick(c, 0, &["乙.txt"])?;
    c.ctrl("x");
    let faded = row(c, 0, "乙.txt").is_some_and(|r| r.cut);
    r.step(
        c,
        "F1 Ctrl+X：该项变淡（剪切标记），文件没有移动",
        vec![
            (c.ctl.cut_active(), "应有剪切状态"),
            (faded, "条目应带剪切标记"),
            (has_disk(&src, "乙.txt"), "文件仍在原处"),
            (
                c.clip.read().is_some_and(|f| f.effect == Effect::Move),
                "剪贴板为移动",
            ),
        ],
    );
    c.special(Key::F6, &[]);
    c.ctrl("v");
    c.pump();
    r.step(
        c,
        "F2 在右栏粘贴：移动成功，来源消失，剪贴板与标记清空",
        vec![
            (
                has_disk(&dst, "乙.txt") && !has_disk(&src, "乙.txt"),
                "文件应已移动",
            ),
            (
                !has(c, 0, "乙.txt") && has(c, 1, "乙.txt"),
                "两栏列表都已刷新",
            ),
            (!c.ctl.cut_active(), "剪切状态应清除"),
            (c.clip.read().is_none(), "剪贴板应已清空"),
            (msg(c) == "已移动“乙.txt”", "如实汇报"),
        ],
    );
    c.special(Key::F6, &[]);
    pick(c, 0, &["丙.txt"])?;
    c.ctrl("x");
    c.clip.put_external(&[dst.join("丙 (2).txt")], Effect::Copy);
    pick(c, 0, &["丁丁.txt"])?;
    let faded = row(c, 0, "丙.txt").is_some_and(|r| r.cut);
    r.step(
        c,
        "F3 剪切后别处改写了剪贴板：剪切标记自动失效",
        vec![
            (!c.ctl.cut_active(), "剪切状态应失效"),
            (!faded, "淡化标记应消失"),
        ],
    );

    // ---------- G. 失败注入：部分成功、取消、磁盘满、设备断开 ----------
    pick(c, 0, &["丙.txt", "丁丁.txt", "README.txt"])?;
    c.ctrl("c");
    st.inject(
        src.join("丁丁.txt"),
        Inject::Transfer(FailKind::AccessDenied),
    );
    c.special(Key::F6, &[]);
    c.pump();
    // 右栏进入 目标子目录 做粘贴目标
    c.open_folder("目标子目录")?;
    let sub = dst.join("目标子目录");
    c.ctrl("v");
    c.pump();
    r.step(
        c,
        "G1 三项复制，其中一项权限不足：部分完成，如实列出失败项，成功的不回滚",
        vec![
            (
                has_disk(&sub, "丙.txt")
                    && has_disk(&sub, "README.txt")
                    && !has_disk(&sub, "丁丁.txt"),
                "成功两项、失败一项",
            ),
            (dlg(c) && title(c) == "部分项目没有复制", "应弹说明框"),
            (
                body(c).contains("丁丁.txt") && body(c).contains("没有权限"),
                "点名失败项与原因",
            ),
            (msg(c).starts_with("部分完成"), "状态栏为部分完成"),
            (!c.win.get_status_message_ok(), "警示色"),
            (has_disk(&src, "丁丁.txt"), "失败项来源保持原样"),
        ],
    );
    c.special(Key::Return, &[]);
    c.pump();
    st.clear_injections();
    let _ = fs::remove_file(sub.join("丙.txt"));
    let _ = fs::remove_file(sub.join("README.txt"));
    c.special(Key::F5, &[]);
    c.pump();
    // 用户在系统提示里取消（Aborted）：后续项目不再执行，只留状态栏，不弹错误框
    pick(c, 0, &["丙.txt", "丁丁.txt", "README.txt"])?;
    c.ctrl("c");
    // 按列表顺序第一个就取消：之后的项目一个都不应再执行
    st.inject(src.join("README.txt"), Inject::Transfer(FailKind::Aborted));
    c.special(Key::F6, &[]);
    c.ctrl("v");
    c.pump();
    r.step(
        c,
        "G2 用户取消（系统提示里点取消）：不再继续后面的项目，不弹错误框，如实说明",
        vec![
            (disk(&sub).is_empty(), "一项都没有复制"),
            (!dlg(c), "取消不弹错误框"),
            (msg(c).contains("已取消"), "状态栏说明已取消"),
        ],
    );
    st.clear_injections();
    pick(c, 0, &["丙.txt"])?;
    c.ctrl("c");
    st.inject(src.join("丙.txt"), Inject::Transfer(FailKind::DiskFull));
    c.special(Key::F6, &[]);
    c.ctrl("v");
    c.pump();
    r.step(
        c,
        "G3 磁盘空间不足：如实报告",
        vec![
            (!has_disk(&sub, "丙.txt"), "没有半截文件"),
            (dlg(c) && body(c).contains("磁盘空间不足"), "说明原因"),
        ],
    );
    c.special(Key::Return, &[]);
    c.pump();
    st.clear_injections();
    st.inject(src.join("丙.txt"), Inject::Transfer(FailKind::DeviceGone));
    c.ctrl("v");
    c.pump();
    r.step(
        c,
        "G4 设备断开：如实报告",
        vec![
            (!has_disk(&sub, "丙.txt"), "没有半截文件"),
            (dlg(c) && body(c).contains("设备已断开"), "说明原因"),
        ],
    );
    c.special(Key::Return, &[]);
    c.pump();
    st.clear_injections();
    // 跨卷移动：复制校验后才删除源；复制中途失败源原样保留
    st.set_force_cross_volume(true);
    pick(c, 0, &["丙.txt"])?;
    c.ctrl("x");
    st.inject(
        src.join("丙.txt"),
        Inject::TransferFile("丙.txt".into(), FailKind::DiskFull),
    );
    c.special(Key::F6, &[]);
    c.ctrl("v");
    c.pump();
    r.step(
        c,
        "G5 跨卷移动复制中途失败：源原样保留，目标没有残留",
        vec![
            (has_disk(&src, "丙.txt"), "源应保留"),
            (!has_disk(&sub, "丙.txt"), "目标不应留下不完整副本"),
            (dlg(c), "应说明失败"),
        ],
    );
    c.special(Key::Return, &[]);
    c.pump();
    st.clear_injections();
    pick(c, 0, &["丙.txt"])?;
    c.ctrl("x");
    c.special(Key::F6, &[]);
    c.ctrl("v");
    c.pump();
    r.step(
        c,
        "G6 跨卷移动成功：目标内容一致后才删除源",
        vec![
            (
                has_disk(&sub, "丙.txt") && !has_disk(&src, "丙.txt"),
                "已移动",
            ),
            (
                fs::read_to_string(sub.join("丙.txt"))? == "内容：丙.txt",
                "内容一致",
            ),
        ],
    );
    st.set_force_cross_volume(false);
    // 文件夹放进自己里面
    c.special(Key::F6, &[]);
    pick(c, 0, &["子目录"])?;
    c.ctrl("c");
    c.double_click(&c.item_in(0, "子目录")?);
    c.pump();
    c.ctrl("v");
    c.pump();
    r.step(
        c,
        "G7 把文件夹粘贴进它自己里面：拒绝并说明，没有任何改动",
        vec![
            (
                disk(&src.join("子目录")) == vec!["内.txt"],
                "子目录内容不变",
            ),
            (
                dlg(c) && body(c).contains("不能把文件夹放进它自己里面"),
                "说明原因",
            ),
        ],
    );
    c.special(Key::Return, &[]);
    c.pump();
    c.click(&c.button_in(0, "上一级")?);
    c.pump();
    c.special(Key::F6, &[]);
    c.click(&c.button_in(1, "上一级")?);
    c.pump();
    c.special(Key::F6, &[]);

    // ---------- H. 应用内拖放 ----------
    c.special(Key::F5, &[]);
    c.pump();
    let a = center(&c.item_in(0, "丁丁.txt")?);
    let right = c.pane_rect(1)?;
    let to_right = (right.x + right.w / 2.0, right.y + right.h * 0.8);
    press_and_move(c, a, &[(a.0 + 12.0, a.1 + 12.0), to_right]);
    let bus = c.win.global::<DragBus>();
    let (active, hint, ok, tp) = (
        bus.get_active(),
        bus.get_hint().to_string(),
        bus.get_hint_ok(),
        bus.get_target_pane(),
    );
    r.step(
        c,
        "H1 拖动过程：跟随提示写明“移动到 dst”（同一卷默认移动），右栏被标为目标",
        vec![
            (active, "拖动中"),
            (hint == "移动到 dst", "提示应是“移动到 dst”"),
            (ok, "可放置"),
            (tp == 1, "目标面板是右栏"),
            (c.ctl.drag_active(), "控制器处于拖动状态"),
        ],
    );
    release(c, to_right);
    c.pump();
    r.step(
        c,
        "H2 松开：与提示一致地移动到 dst",
        vec![
            (
                has_disk(&dst, "丁丁.txt") && !has_disk(&src, "丁丁.txt"),
                "文件应已移动",
            ),
            (!c.win.global::<DragBus>().get_active(), "提示应消失"),
            (!c.ctl.drag_active(), "拖动状态应结束"),
        ],
    );
    // Ctrl 拖 = 复制
    c.ctl.set_keys_override(Some(MK_CONTROL));
    c.special(Key::F5, &[]);
    c.pump();
    let a = center(&c.item_in(0, "README.txt")?);
    press_and_move(c, a, &[(a.0 + 12.0, a.1 + 12.0), to_right]);
    let hint = c.win.global::<DragBus>().get_hint().to_string();
    release(c, to_right);
    c.pump();
    r.step(
        c,
        "H3 按住 Ctrl 拖动：提示“复制到 dst”，实际复制，来源保留",
        vec![
            (hint == "复制到 dst", "提示应是“复制到 dst”"),
            (
                has_disk(&dst, "README.txt") && has_disk(&src, "README.txt"),
                "两边都有",
            ),
        ],
    );
    // Shift 拖 = 移动（强制，即使默认也是移动；这里用跨卷模拟默认复制再强制移动）
    c.ctl.set_keys_override(Some(MK_SHIFT));
    st.set_force_cross_volume(true);
    let _ = fs::remove_file(dst.join("README.txt"));
    c.special(Key::F5, &[]);
    c.pump();
    let a = center(&c.item_in(0, "README.txt")?);
    press_and_move(c, a, &[(a.0 + 12.0, a.1 + 12.0), to_right]);
    let hint = c.win.global::<DragBus>().get_hint().to_string();
    release(c, to_right);
    c.pump();
    r.step(
        c,
        "H4 按住 Shift 拖动：提示“移动到 dst”，实际移动",
        vec![
            (hint == "移动到 dst", "提示应是“移动到 dst”"),
            (
                has_disk(&dst, "README.txt") && !has_disk(&src, "README.txt"),
                "已移动",
            ),
        ],
    );
    st.set_force_cross_volume(false);
    c.ctl.set_keys_override(None);
    // 拖到文件夹行上
    c.special(Key::F5, &[]);
    c.pump();
    let a = center(&c.item_in(0, "甲甲.txt")?);
    let folder = center(&c.item_in(1, "目标子目录")?);
    press_and_move(c, a, &[(a.0 + 12.0, a.1 + 12.0), folder]);
    let bus = c.win.global::<DragBus>();
    let (hint, ti) = (bus.get_hint().to_string(), bus.get_target_index());
    let hl = row(c, 1, "目标子目录").is_some_and(|r| r.drop_target);
    r.step(
        c,
        "H5 拖到右栏的文件夹上：该文件夹高亮，提示点名它",
        vec![
            (hint == "移动到 目标子目录", "提示应点名文件夹"),
            (hl, "文件夹条目应带目标高亮"),
            (ti >= 0, "目标条目序号"),
        ],
    );
    release(c, folder);
    c.pump();
    r.step(
        c,
        "H6 松开：放进了该文件夹",
        vec![
            (
                has_disk(&sub, "甲甲.txt") && !has_disk(&src, "甲甲.txt"),
                "已移动进文件夹",
            ),
            (
                row(c, 1, "目标子目录").is_some_and(|r| !r.drop_target),
                "高亮已清除",
            ),
        ],
    );
    // Esc 取消
    c.special(Key::F5, &[]);
    c.pump();
    let first = names(c, 0)[0].clone();
    let a2 = center(&c.item_in(0, &first)?);
    press_and_move(c, a2, &[(a2.0 + 12.0, a2.1 + 12.0), to_right]);
    let was = c.ctl.drag_active();
    c.special(Key::Escape, &[]);
    release(c, to_right);
    c.pump();
    r.step(
        c,
        "H7 拖动中按 Esc：取消，松开后什么都没发生",
        vec![
            (was, "取消前在拖动"),
            (
                !c.ctl.drag_active() && !c.win.global::<DragBus>().get_active(),
                "拖动与提示应结束",
            ),
            (
                has_disk(&src, &first) && !has_disk(&dst, &first),
                "文件没有移动",
            ),
        ],
    );
    // 拖出窗口 → 交给系统拖放（harness 只记录）
    let a3 = center(&c.item_in(0, &first)?);
    press_and_move(c, a3, &[(a3.0 + 12.0, a3.1 + 12.0), (-20.0, a3.1)]);
    release(c, (-20.0, a3.1));
    c.pump();
    let out = c.dragged_out.borrow().clone();
    r.step(
        c,
        "H8 指针拖出窗口：改用系统拖放，把该文件交给系统（harness 只记录路径）",
        vec![
            (
                out.len() == 1 && out[0] == vec![src.join(&first)],
                "应记录一次拖出",
            ),
            (!c.win.global::<DragBus>().get_active(), "窗口内提示应消失"),
        ],
    );
    // 多选里按下已选项不拖动：收拢为单选；拖动则整组移动
    let (n0, n1) = (names(c, 0)[0].clone(), names(c, 0)[1].clone());
    pick(c, 0, &[&n0, &n1])?;
    let n = sel(c, 0);
    c.click(&c.item_in(0, &n0)?);
    c.pump();
    r.step(
        c,
        "H9 多选后单击其中一项（没有拖动）：松开时收拢为只选这一项",
        vec![
            (n.len() == 2, "先选中两项"),
            (sel(c, 0) == vec![n0.clone()], "收拢为单选"),
        ],
    );

    // ---------- I. 资源管理器拖入（模拟 DropHost） ----------
    let ext_file = base.join("外部文件.txt");
    fs::write(&ext_file, "来自资源管理器")?;
    c.ctl.ext_enter(std::slice::from_ref(&ext_file));
    // ext_* 的坐标是物理像素（OLE 给的是客户区像素）
    let rp = (
        (to_right.0 * scale).round() as i32,
        (to_right.1 * scale).round() as i32,
    );
    let eff = c.ctl.ext_over(rp, 0, EFFECT_COPY | EFFECT_MOVE);
    let hint = c.win.global::<DragBus>().get_hint().to_string();
    r.step(
        c,
        "I1 资源管理器拖入悬停：同卷默认移动，提示与回答给系统的效果一致",
        vec![
            (eff == EFFECT_MOVE, "回答“移动”"),
            (hint == "移动到 dst", "提示“移动到 dst”"),
        ],
    );
    let eff_c = c.ctl.ext_over(rp, MK_CONTROL, EFFECT_COPY | EFFECT_MOVE);
    let hint_c = c.win.global::<DragBus>().get_hint().to_string();
    let eff_only_copy = c.ctl.ext_over(rp, 0, EFFECT_COPY);
    let hint_only_copy = c.win.global::<DragBus>().get_hint().to_string();
    r.step(
        c,
        "I2 按住 Ctrl 或来源只允许复制：改为复制，提示同步变化",
        vec![
            (eff_c == EFFECT_COPY && hint_c == "复制到 dst", "Ctrl：复制"),
            (
                eff_only_copy == EFFECT_COPY && hint_only_copy == "复制到 dst",
                "来源只允许复制",
            ),
        ],
    );
    let ret = c.ctl.ext_drop(rp, MK_CONTROL, EFFECT_COPY | EFFECT_MOVE);
    c.pump();
    r.step(
        c,
        "I3 释放（Ctrl 复制）：复制到 dst，来源保留",
        vec![
            (ret == EFFECT_COPY, "回答“复制”"),
            (
                has_disk(&dst, "外部文件.txt") && ext_file.exists(),
                "复制成功、来源保留",
            ),
            (!c.win.global::<DragBus>().get_active(), "提示消失"),
        ],
    );
    let ext2 = base.join("外部二.txt");
    fs::write(&ext2, "来自资源管理器二")?;
    c.ctl.ext_enter(std::slice::from_ref(&ext2));
    c.ctl.ext_over(rp, 0, EFFECT_COPY | EFFECT_MOVE);
    let ret = c.ctl.ext_drop(rp, 0, EFFECT_COPY | EFFECT_MOVE);
    c.pump();
    r.step(
        c,
        "I4 释放（默认同卷移动）：本应用自己完成移动，并回答“没有效果”，来源就不会再删一次",
        vec![
            (ret == 0, "移动时回答 DROPEFFECT_NONE"),
            (has_disk(&dst, "外部二.txt") && !ext2.exists(), "已移动"),
        ],
    );
    c.ctl.ext_enter(&[base.join("外部文件.txt")]);
    c.ctl.ext_over(rp, 0, EFFECT_COPY | EFFECT_MOVE);
    let shown = c.win.global::<DragBus>().get_active();
    c.ctl.ext_leave();
    r.step(
        c,
        "I5 拖入后离开窗口：提示与状态复位",
        vec![
            (shown, "悬停时应显示提示"),
            (!c.win.global::<DragBus>().get_active(), "提示消失"),
            (!c.ctl.drag_active(), "拖动状态结束"),
        ],
    );
    Ok((r.steps, r.fails))
}
