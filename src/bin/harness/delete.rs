//! 删除、回收站浏览与还原的交互脚本（Issue #8）。
//!
//! 全部在 `%TEMP%\sfm-harness\scratch\delete` 的一次性数据上进行，后端是临时目录“回收站”（[`StashBackend`]，
//! 可注入失败），**绝不触碰系统真实回收站**。覆盖：删入、取消、Shift+Delete 的危险确认、回收站浏览、
//! 单项与多项还原、同名冲突循环、原目录消失、失败注入下的部分成功汇报、不支持回收站时不退化为永久删除。
//! 窗口为双栏：左栏做操作，右栏停在同一个草稿目录，用来证明“相关目录随操作同步更新”。

use crate::ctx::{Ctx, Opts, Res};
use crate::dual::Rec;
use crate::fixture;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use serde_json::Value;
use slint::platform::Key;
use slint_file_manager::browse::location;
use slint_file_manager::fileops::{Backend, FailKind, Inject};
use std::fs;
use std::path::{Path, PathBuf};

/// 对话框里的按钮。对话框遮罩覆盖整个窗口，工具栏也有同名的“永久删除”，
/// 所以取位于对话框标题下方的那个（对话框按钮在卡片底部）。
pub(crate) fn dlg_btn(c: &Ctx, label: &str) -> Res<ElementHandle> {
    let t = c.find_all(AccessibleRole::Text, "对话框标题");
    let ty = t.first().ok_or("对话框没有显示")?.absolute_position().y;
    c.find_all(AccessibleRole::Button, label)
        .into_iter()
        .find(|e| e.absolute_position().y > ty)
        .ok_or_else(|| {
            format!(
                "对话框里找不到按钮：{label}（对话框：{}，标题“{}”，消息“{}”）",
                c.win.get_dialog_visible(),
                c.win.get_dialog_title(),
                c.win.get_status_message()
            )
            .into()
        })
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
fn in_bin(c: &Ctx, p: usize) -> bool {
    c.ctl
        .location_in(p)
        .is_some_and(|l| location::is_recycle_bin(&l))
}
fn left_state(c: &Ctx) -> String {
    format!("{:?}", c.win.get_left_view().state)
}
fn enabled(e: &ElementHandle) -> Option<bool> {
    e.accessible_enabled()
}
fn text(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_default()
}

/// 选中左栏的若干项（第一个普通单击，其余 Ctrl 单击）。
fn select(c: &Ctx, items: &[&str]) -> Res<()> {
    for (i, n) in items.iter().enumerate() {
        let e = c.item_in(0, n)?;
        if i == 0 {
            c.click(&e);
        } else {
            c.ctrl_click(&e);
        }
    }
    Ok(())
}

fn go_bin(c: &Ctx) -> Res<()> {
    c.click(&c.button("回收站")?);
    c.pump();
    Ok(())
}

fn go_back(c: &Ctx) -> Res<()> {
    c.click(&c.button_in(0, "后退")?);
    c.pump();
    Ok(())
}

/// 对话框的“确定”用 Enter，随后让排队的刷新执行完。
fn enter(c: &Ctx) {
    c.special(Key::Return, &[]);
    c.pump();
}

pub fn delete(size: (u32, u32), scale: f32) -> Res<(Vec<Value>, Vec<String>)> {
    let dir: PathBuf = fixture::fresh_dir(&fixture::scratch_dir("delete"))?;
    for n in [
        "甲.txt", "乙.txt", "丙.txt", "丁.txt", "戊.txt", "己.txt", "庚.txt",
    ] {
        fs::write(dir.join(n), n)?;
    }
    fs::create_dir(dir.join("目录甲"))?;
    fs::write(dir.join("目录甲").join("内.txt"), "内")?;
    let c = &Ctx::open(
        size,
        scale,
        Opts {
            start: dir.clone(),
            split: true,
            ..Opts::default()
        },
    )?;
    let mut r = Rec::new();
    let st = c.stash.clone();

    // 右栏也进入同一个草稿目录（F6 切到右栏输入地址，再切回左栏）
    c.special(Key::F6, &[]);
    c.enter_address(&dir.to_string_lossy())?;
    c.pump();
    c.special(Key::F6, &[]);
    r.step(
        c,
        "A 初始：两栏同在草稿目录，未选择时删除不可用",
        vec![
            (
                names(c, 0).len() == 8 && names(c, 1).len() == 8,
                "两栏都应显示 8 项",
            ),
            (
                enabled(&c.button("删除")?) == Some(false),
                "未选择时“删除”应不可用",
            ),
            (
                enabled(&c.button("永久删除")?) == Some(false),
                "未选择时“永久删除”应不可用",
            ),
        ],
    );
    c.special(Key::Delete, &[]);
    r.step(
        c,
        "A2 未选择按 Delete：只提示，不弹确认",
        vec![
            (!dlg(c), "不应弹确认框"),
            (msg(c).contains("请先选择"), "应提示先选择项目"),
        ],
    );

    // ---------- B. 点“删除”→ 取消：文件不变 ----------
    select(c, &["甲.txt"])?;
    r.step(
        c,
        "B1 选中后“删除”可用",
        vec![(enabled(&c.button("删除")?) == Some(true), "选中后应可用")],
    );
    c.click(&c.button("删除")?);
    r.step(
        c,
        "B2 “删除”弹出移到回收站的确认（非危险样式）",
        vec![
            (dlg(c), "应弹确认框"),
            (
                title(c) == "将“甲.txt”移到回收站？",
                "标题应点名并说明是移到回收站",
            ),
            (
                c.win.get_dialog_confirm_label() == "移到回收站",
                "确认按钮文字",
            ),
            (c.win.get_dialog_cancel_label() == "取消", "应有取消按钮"),
            (!c.win.get_dialog_danger(), "移到回收站不是危险样式"),
            (dir.join("甲.txt").exists(), "确认前文件不变"),
        ],
    );
    c.click(&dlg_btn(c, "取消")?);
    c.pump();
    r.step(
        c,
        "B3 点取消：什么都没变",
        vec![
            (!dlg(c), "对话框应关闭"),
            (dir.join("甲.txt").exists(), "文件仍在"),
            (
                st.bin_len() == 0 && st.recycled_count() == 0,
                "回收站没有收到任何东西",
            ),
            (msg(c).contains("已取消"), "应提示已取消"),
            (has(c, 0, "甲.txt"), "列表仍有该项"),
        ],
    );

    // ---------- C. Delete 键 → Esc 取消 ----------
    c.special(Key::Delete, &[]);
    let opened = dlg(c);
    c.special(Key::Escape, &[]);
    c.pump();
    r.step(
        c,
        "C Delete 键弹确认，Esc 取消",
        vec![
            (opened, "Delete 键应弹确认框"),
            (!dlg(c), "Esc 应关闭对话框"),
            (dir.join("甲.txt").exists() && st.bin_len() == 0, "文件不变"),
        ],
    );

    // ---------- D. Delete → Enter 确认：进回收站，两栏同步 ----------
    c.special(Key::Delete, &[]);
    enter(c);
    r.step(
        c,
        "D 确认移到回收站：原位置消失、进入回收站、两栏同步刷新",
        vec![
            (!dlg(c), "对话框应关闭"),
            (!dir.join("甲.txt").exists(), "原位置应已不存在"),
            (st.bin_len() == 1, "回收站应收到 1 项"),
            (
                !has(c, 0, "甲.txt") && !has(c, 1, "甲.txt"),
                "左右两栏列表都应不再显示",
            ),
            (msg(c) == "已将“甲.txt”移到回收站", "状态栏应如实汇报"),
            (c.win.get_status_message_ok(), "成功提示用常规色"),
        ],
    );

    // ---------- E. Shift+Delete：危险确认，Enter 无效，必须点按钮 ----------
    select(c, &["乙.txt"])?;
    c.special(Key::Delete, &[Key::Shift]);
    r.step(
        c,
        "E1 Shift+Delete：永久删除确认（危险样式）",
        vec![
            (dlg(c), "应弹确认框"),
            (
                title(c) == "永久删除“乙.txt”？",
                "标题应点名并说明是永久删除",
            ),
            (c.win.get_dialog_danger(), "永久删除应是危险样式"),
            (
                c.win.get_dialog_confirm_label() == "永久删除",
                "确认按钮文字",
            ),
            (
                body(c).contains("不会进入回收站"),
                "正文应说明不进回收站、无法恢复",
            ),
        ],
    );
    c.special(Key::Return, &[]);
    r.step(
        c,
        "E2 危险确认按 Enter 无效",
        vec![
            (dlg(c), "对话框应仍打开"),
            (dir.join("乙.txt").exists(), "文件不应被删除"),
        ],
    );
    c.click(&dlg_btn(c, "永久删除")?);
    c.pump();
    r.step(
        c,
        "E3 点“永久删除”：直接删除，不进回收站",
        vec![
            (!dir.join("乙.txt").exists(), "文件应已删除"),
            (
                st.bin_len() == 1 && st.recycled_count() == 1,
                "回收站不应多出项目",
            ),
            (msg(c) == "已永久删除“乙.txt”", "状态栏应如实汇报"),
            (!has(c, 0, "乙.txt"), "列表应刷新"),
        ],
    );

    // ---------- F. 多项（含文件夹） ----------
    select(c, &["丙.txt", "目录甲"])?;
    c.special(Key::Delete, &[]);
    let t = title(c);
    enter(c);
    r.step(
        c,
        "F 多选（含文件夹）移到回收站",
        vec![
            (t == "将 2 项移到回收站？", "标题应说明数量"),
            (
                !dir.join("丙.txt").exists() && !dir.join("目录甲").exists(),
                "原位置都应消失",
            ),
            (st.bin_len() == 3, "回收站共 3 项"),
            (msg(c) == "已将 2 项移到回收站", "汇报数量"),
        ],
    );

    // ---------- G. 回收站视图 ----------
    go_bin(c)?;
    let banner = c.find_all(AccessibleRole::Groupbox, "回收站提示").len();
    let mut bin_names = names(c, 0);
    bin_names.sort();
    r.step(
        c,
        "G1 侧栏进入回收站：与普通目录区分，列出被删项目",
        vec![
            (in_bin(c, 0), "位置应是回收站虚拟位置"),
            (banner == 1, "应有“回收站提示”条"),
            (
                bin_names == ["丙.txt", "甲.txt", "目录甲"],
                "应列出 3 个被删项目",
            ),
            (!c.win.get_can_up(), "回收站没有上一级"),
            (
                c.win.get_sidebar_current() == "recycle-bin",
                "侧栏应高亮回收站",
            ),
            (
                c.button("回收站")?.accessible_checked() == Some(true),
                "侧栏回收站按钮应为选中态",
            ),
            (
                enabled(&c.button("还原所选项目")?) == Some(false),
                "未选择时“还原所选项目”应不可用",
            ),
            (
                enabled(&c.button("删除")?) == Some(false),
                "回收站里不提供删除",
            ),
            (has(c, 1, "丁.txt"), "右栏仍是普通目录"),
        ],
    );
    c.double_click(&c.item_in(0, "甲.txt")?);
    r.step(
        c,
        "G2 双击回收站项目：不能直接打开",
        vec![
            (msg(c).contains("不能直接打开"), "应提示先还原"),
            (c.opened.borrow().is_empty(), "不应启动任何程序"),
        ],
    );
    c.click(&c.item_in(0, "甲.txt")?);
    c.special(Key::Delete, &[]);
    r.step(
        c,
        "G3 回收站里按 Delete：不弹确认，只提示",
        vec![
            (!dlg(c), "不应弹确认框"),
            (msg(c).contains("不能在这里删除"), "应提示用还原"),
            (st.bin_len() == 3, "回收站不变"),
        ],
    );

    // ---------- H. 还原单项，相关目录同步 ----------
    c.click(&c.button("还原所选项目")?);
    c.pump();
    let mut left = names(c, 0);
    left.sort();
    r.step(
        c,
        "H 还原单项：回到原位置，回收站与右栏同步更新",
        vec![
            (
                text(&dir.join("甲.txt")) == "甲.txt",
                "文件应回到原位置且内容完整",
            ),
            (st.bin_len() == 2, "回收站剩 2 项"),
            (left == ["丙.txt", "目录甲"], "回收站视图不再显示已还原项"),
            (has(c, 1, "甲.txt"), "右栏（原目录）应立即出现该文件"),
            (msg(c) == "已将“甲.txt”还原到原位置", "状态栏应如实汇报"),
        ],
    );

    // ---------- I. 冲突循环 ----------
    fs::write(dir.join("丙.txt"), "新的丙")?;
    select(c, &["丙.txt", "目录甲"])?;
    c.click(&c.button("还原所选项目")?);
    c.pump();
    r.step(
        c,
        "I1 多项还原遇同名冲突：不覆盖，冲突项留在回收站，如实汇报部分成功",
        vec![
            (
                text(&dir.join("丙.txt")) == "新的丙",
                "原位置的同名文件不得被覆盖",
            ),
            (
                dir.join("目录甲").join("内.txt").is_file(),
                "无冲突的文件夹应已还原",
            ),
            (st.bin_len() == 1, "只剩冲突的 1 项"),
            (
                names(c, 0) == ["丙.txt"],
                "回收站视图只剩冲突项，失败项不假消失",
            ),
            (
                dlg(c) && title(c) == "部分项目没有还原",
                "应弹出部分成功说明",
            ),
            (
                body(c).contains("丙.txt") && body(c).contains("同名"),
                "说明里应点名失败项与原因",
            ),
            (c.win.get_dialog_cancel_label() == "", "说明框只有“知道了”"),
            (
                msg(c) == "部分完成：1 项已还原到原位置，1 项未能还原",
                "状态栏应如实写出成功与失败数量",
            ),
            (!c.win.get_status_message_ok(), "有失败时用警示色"),
        ],
    );
    enter(c);
    r.step(c, "I2 说明框按 Enter 关闭", vec![(!dlg(c), "应关闭")]);
    fs::remove_file(dir.join("丙.txt"))?;
    c.click(&c.item_in(0, "丙.txt")?);
    c.click(&c.button("还原所选项目")?);
    c.pump();
    r.step(
        c,
        "I3 清除冲突后再还原成功，回收站变空并显示空状态",
        vec![
            (text(&dir.join("丙.txt")) == "丙.txt", "原内容应回来"),
            (st.bin_len() == 0, "回收站应已空"),
            (left_state(c) == "Empty", "应显示“回收站是空的”空状态"),
            (!dlg(c), "全部成功不弹说明框"),
        ],
    );

    // ---------- J. 失败注入：权限不足的部分成功 ----------
    go_back(c)?;
    st.inject(dir.join("丁.txt"), Inject::Delete(FailKind::AccessDenied));
    select(c, &["丁.txt", "戊.txt"])?;
    c.special(Key::Delete, &[]);
    enter(c);
    r.step(
        c,
        "J1 批量删除中一项权限不足：成功的进回收站，失败的保持原样并如实汇报",
        vec![
            (
                !dir.join("戊.txt").exists() && st.bin_len() == 1,
                "戊.txt 应进回收站",
            ),
            (dir.join("丁.txt").exists(), "丁.txt 应保持原样"),
            (
                has(c, 0, "丁.txt") && !has(c, 0, "戊.txt"),
                "列表不得把失败项也刷掉",
            ),
            (
                dlg(c) && title(c) == "部分项目没有删除",
                "应弹出部分成功说明",
            ),
            (
                body(c).contains("丁.txt") && body(c).contains("没有权限"),
                "说明里应点名失败项与原因",
            ),
            (
                msg(c) == "部分完成：1 项已移到回收站，1 项未能移到回收站",
                "状态栏应如实写出数量",
            ),
        ],
    );
    enter(c);
    select(c, &["丁.txt"])?;
    c.special(Key::Delete, &[Key::Shift]);
    c.click(&dlg_btn(c, "永久删除")?);
    c.pump();
    r.step(
        c,
        "J2 永久删除失败：文件仍在，不假装成功",
        vec![
            (dir.join("丁.txt").exists(), "文件应仍在"),
            (has(c, 0, "丁.txt"), "列表仍显示"),
            (
                dlg(c) && title(c) == "没有删除任何项目",
                "应说明没有删除任何项目",
            ),
            (msg(c).starts_with("未能删除“丁.txt”"), "状态栏应说明失败"),
        ],
    );
    enter(c);
    st.clear_injections();

    // ---------- K. 不支持回收站：绝不退化为永久删除 ----------
    st.inject(dir.join("己.txt"), Inject::Unsupported);
    let before = st.bin_len();
    select(c, &["己.txt"])?;
    c.special(Key::Delete, &[]);
    r.step(
        c,
        "K1 位置不支持回收站：只给说明，不执行、不退化为永久删除",
        vec![
            (
                dlg(c) && title(c) == "无法移到回收站",
                "应说明无法移到回收站",
            ),
            (c.win.get_dialog_cancel_label() == "", "只有“知道了”"),
            (
                body(c).contains("没有删除任何项目") && body(c).contains("Shift+Delete"),
                "应说明后果与永久删除的入口",
            ),
            (dir.join("己.txt").exists(), "文件必须仍在"),
            (st.bin_len() == before, "回收站不变"),
        ],
    );
    enter(c);
    select(c, &["己.txt", "庚.txt"])?;
    c.special(Key::Delete, &[]);
    let (t2, b2) = (title(c), body(c));
    enter(c);
    r.step(
        c,
        "K2 混合：确认框写明不支持的项目不会被删；执行后它们保持原样并汇报",
        vec![
            (t2 == "将 2 项移到回收站？", "确认框标题"),
            (
                b2.contains("不支持回收站") && b2.contains("也不会被永久删除"),
                "确认框应写明后果",
            ),
            (
                !dir.join("庚.txt").exists() && st.bin_len() == before + 1,
                "庚.txt 应进回收站",
            ),
            (
                dir.join("己.txt").exists() && has(c, 0, "己.txt"),
                "己.txt 保持原样",
            ),
            (
                dlg(c) && title(c) == "部分项目没有删除",
                "应弹出部分成功说明",
            ),
            (
                body(c).contains("己.txt") && body(c).contains("不支持回收站"),
                "说明里点名失败项与原因",
            ),
        ],
    );
    enter(c);
    st.clear_injections();

    // ---------- L. 还原失败与原目录消失 ----------
    go_bin(c)?;
    let n_before = st.bin_len();
    st.inject(dir.join("戊.txt"), Inject::Restore(FailKind::AccessDenied));
    c.click(&c.item_in(0, "戊.txt")?);
    c.click(&c.button("还原所选项目")?);
    c.pump();
    r.step(
        c,
        "L1 还原权限不足：留在回收站，不假消失",
        vec![
            (!dir.join("戊.txt").exists(), "不应出现在原位置"),
            (
                st.bin_len() == n_before && has(c, 0, "戊.txt"),
                "仍在回收站且仍显示",
            ),
            (
                dlg(c) && title(c) == "没有还原任何项目",
                "应说明没有还原任何项目",
            ),
            (body(c).contains("没有权限"), "应写明原因"),
        ],
    );
    enter(c);
    st.clear_injections();
    c.click(&c.item_in(0, "戊.txt")?);
    c.click(&c.button("还原所选项目")?);
    c.pump();
    r.step(
        c,
        "L2 放开后还原成功",
        vec![
            (dir.join("戊.txt").exists(), "应回到原位置"),
            (st.bin_len() == n_before - 1, "回收站少 1 项"),
        ],
    );

    let gone = dir.join("临时夹");
    fs::create_dir(&gone)?;
    fs::write(gone.join("孤.txt"), "孤")?;
    st.recycle(&gone.join("孤.txt")).map_err(|e| e.text())?;
    fs::remove_dir(&gone)?;
    c.click(&c.button("回收站")?); // 已在回收站：再次点击即刷新
    c.pump();
    c.click(&c.item_in(0, "孤.txt")?);
    c.click(&c.button("还原所选项目")?);
    c.pump();
    r.step(
        c,
        "L3 原文件夹已消失：不自动重建，项目留在回收站",
        vec![
            (!gone.exists(), "不得自动重建原文件夹"),
            (has(c, 0, "孤.txt"), "项目仍在回收站"),
            (dlg(c) && body(c).contains("原文件夹已不存在"), "应写明原因"),
        ],
    );
    enter(c);

    r.step(
        c,
        "M 汇总：全部操作只经过临时回收站",
        vec![(
            st.recycled_count() == 6,
            "临时回收站共收到 6 次回收（甲、丙、目录甲、戊、庚、孤）",
        )],
    );
    Ok((r.steps, r.fails))
}
