//! 系统入口（Issue #17）的交互脚本与截图场景：设置面板里的“用本应用打开文件夹”开关、状态行、
//! 修复／重新接管／恢复原设置、启动自检，以及外部打开请求（复用或新建标签、选中文件、路径不可用）。
//!
//! 注册表一律用测试根 `HKCU\Software\slint-file-manager-test\<pid>-<纳秒>`（[`TestRoot`]，结束时整棵删除），
//! 截图场景用内存实现 [`MemStore`]；**绝不触碰真实的 `HKCU\Software\Classes`**。程序路径是虚构的，
//! 备份文件放在 `%TEMP%\sfm-harness\entry\<名称>`。

use crate::ctx::{Ctx, Opts, Res};
use crate::dual::Rec;
use crate::fixture;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use serde_json::Value;
use slint::platform::Key;
use slint_file_manager::app::EntryEnv;
use slint_file_manager::sysentry::manage;
use slint_file_manager::sysentry::request::{Reply, Request};
use slint_file_manager::sysentry::takeover::{self, MemStore, RegStore, RegValue};
use slint_file_manager::sysentry::winreg::{TestRoot, WinReg};
use std::path::{Path, PathBuf};

/// 虚构的当前程序路径（不需要存在）。
pub const EXE: &str = r"C:\Program Files\Slint 文件管理器\slint-file-manager.exe";
/// 虚构的旧位置（不存在，用来制造“关联失效”）。
pub const OLD_EXE: &str = r"D:\旧位置\Slint 文件管理器\slint-file-manager.exe";
/// 冲突步骤里“别的程序”写进 `Directory\shell` 默认值的动词。
const FOREIGN_VERB: &str = "othertool.open";

fn backup_in(name: &str) -> Res<PathBuf> {
    Ok(fixture::fresh_dir(&fixture::parent().join("entry").join(name))?.join(manage::BACKUP_FILE))
}

fn settings_el(c: &Ctx) -> Res<ElementHandle> {
    let mut v = c.find_all(AccessibleRole::Groupbox, "设置");
    match v.len() {
        1 => Ok(v.remove(0)),
        0 => Err("设置面板没有显示".into()),
        n => Err(format!("设置面板不唯一（{n} 个）").into()),
    }
}

/// 某个容器里按角色与标签查找唯一元素。
fn within(root: &ElementHandle, role: AccessibleRole, label: &str) -> Res<ElementHandle> {
    let mut v: Vec<_> = root
        .query_descendants()
        .match_accessible_role(role)
        .find_all()
        .into_iter()
        .filter(|e| e.accessible_label().is_some_and(|l| l == label))
        .collect();
    match v.len() {
        1 => Ok(v.remove(0)),
        0 => Err(format!("找不到 {role:?}：{label}").into()),
        n => Err(format!("{role:?}“{label}”不唯一（{n} 个）").into()),
    }
}

fn settings_btn(c: &Ctx, label: &str) -> Res<ElementHandle> {
    within(&settings_el(c)?, AccessibleRole::Button, label)
}

fn switch(c: &Ctx) -> Res<ElementHandle> {
    within(
        &settings_el(c)?,
        AccessibleRole::Switch,
        "用本应用打开文件夹",
    )
}

fn switch_on(c: &Ctx) -> Option<bool> {
    switch(c).ok().and_then(|e| e.accessible_checked())
}

/// 对话框里的按钮（设置面板在对话框下面也有同名按钮，所以只在对话框元素内查找）。
fn dlg_btn(c: &Ctx, label: &str) -> Res<ElementHandle> {
    let d = c
        .find_all(AccessibleRole::Groupbox, "对话框")
        .into_iter()
        .next()
        .ok_or("对话框没有显示")?;
    within(&d, AccessibleRole::Button, label)
}

fn state(c: &Ctx) -> String {
    format!("{:?}", c.win.get_entry_view().state)
}
fn title(c: &Ctx) -> String {
    c.win.get_entry_view().title.to_string()
}
fn dlg(c: &Ctx) -> bool {
    c.win.get_dialog_visible()
}
fn dlg_title(c: &Ctx) -> String {
    c.win.get_dialog_title().to_string()
}
fn msg(c: &Ctx) -> String {
    c.win.get_status_message().to_string()
}
fn visible(c: &Ctx) -> bool {
    c.win.get_settings_visible()
}
fn tabs(c: &Ctx) -> usize {
    let ws = c.ctl.ws();
    ws.pane(ws.active_pane()).tabs().len()
}
fn active_tab(c: &Ctx) -> usize {
    let ws = c.ctl.ws();
    ws.pane(ws.active_pane()).active_index()
}

/// 通过“更多 → 设置…”打开设置面板。
fn open_settings(c: &Ctx) -> Res<()> {
    c.click(&c.button("更多")?);
    c.pump();
    c.click(&c.button("设置…")?);
    c.pump();
    Ok(())
}

/// 关闭再打开（面板只在打开时读取状态，外部改动后用它刷新）。
fn reopen_settings(c: &Ctx) -> Res<()> {
    c.special(Key::Escape, &[]);
    c.pump();
    open_settings(c)
}

fn own_cmd(reg: &WinReg) -> Option<String> {
    reg.get(r"Directory\shell\slintfm.open\command", "")
        .ok()
        .flatten()
        .and_then(|v| v.text())
}

fn request(c: &Ctx, req: Request) -> Reply {
    let r = c.ctl.handle_request(req);
    c.pump();
    r
}

fn open_req(folder: PathBuf) -> Request {
    Request::Open {
        folder,
        select: None,
    }
}

/// 交互脚本。
pub fn entry(size: (u32, u32), scale: f32) -> Res<(Vec<Value>, Vec<String>)> {
    let root = TestRoot::new()?;
    let mut reg = WinReg::hkcu(root.reg.base());
    takeover::seed_rehearsal(&mut reg)?;
    let seed = reg.snapshot("")?;
    let backup = backup_in("settings")?;
    let exe = PathBuf::from(EXE);
    let c = Ctx::open(
        size,
        scale,
        Opts {
            entry: Some(EntryEnv::with_store(
                Box::new(WinReg::hkcu(root.reg.base())),
                exe.clone(),
                backup.clone(),
            )),
            ..Opts::default()
        },
    )?;
    let mut r = Rec::new();
    let unchanged = |reg: &WinReg| reg.snapshot("").ok() == Some(seed.clone());

    // 1. 打开设置：未接管
    open_settings(&c)?;
    r.step(
        &c,
        "更多 → 设置… 打开设置面板",
        vec![
            (visible(&c), "设置面板应显示"),
            (state(&c) == "NotTaken", "状态应为未接管"),
            (title(&c) == "未接管", "状态行应为“未接管”"),
            (switch_on(&c) == Some(false), "开关应为关"),
            (settings_btn(&c, "修复").is_err(), "未接管时不应有“修复”"),
        ],
    );

    // 2. 打开开关：先确认并列出将被替换的现有设置
    c.click(&switch(&c)?);
    c.pump();
    let body = c.win.get_dialog_body().to_string();
    r.step(
        &c,
        "打开开关弹出确认",
        vec![
            (dlg(&c), "应弹出确认对话框"),
            (dlg_title(&c) == "用本应用打开文件夹？", "对话框标题"),
            (
                body.contains(r"Drive\shell\(默认) = none"),
                "应列出将被替换的 Drive\\shell 默认值 none",
            ),
            (
                !body.contains("Zed"),
                "旁边别的程序的动词不会被替换，不应列出",
            ),
            (unchanged(&reg), "确认前不应写注册表"),
        ],
    );

    // 3. 取消：什么都不改
    c.click(&dlg_btn(&c, "取消")?);
    c.pump();
    r.step(
        &c,
        "取消启用",
        vec![
            (!dlg(&c), "对话框应关闭"),
            (visible(&c), "设置面板仍显示"),
            (state(&c) == "NotTaken", "仍为未接管"),
            (switch_on(&c) == Some(false), "开关仍为关"),
            (unchanged(&reg), "注册表与布置时一致"),
            (!backup.exists(), "不应写备份文件"),
        ],
    );

    // 4. 启用
    c.click(&switch(&c)?);
    c.pump();
    c.click(&dlg_btn(&c, "启用")?);
    c.pump();
    let cmd = own_cmd(&reg).unwrap_or_default();
    r.step(
        &c,
        "确认启用",
        vec![
            (!dlg(&c), "对话框应关闭"),
            (state(&c) == "Taken", "状态应为已接管"),
            (title(&c) == "已接管", "状态行应为“已接管”"),
            (switch_on(&c) == Some(true), "开关应为开"),
            (backup.is_file(), "应在数据目录写备份文件"),
            (
                cmd == format!("\"{EXE}\" --entry \"%1\""),
                "文件夹动词命令应指向当前程序",
            ),
            (msg(&c).starts_with("已启用"), "状态栏提示已启用"),
        ],
    );

    // 5. 别的程序改动了一部分 → 冲突 → 重新接管
    reg.set(r"Directory\shell", "", &RegValue::sz(FOREIGN_VERB))?;
    reopen_settings(&c)?;
    r.step(
        &c,
        "外部改动后显示冲突",
        vec![
            (state(&c) == "Conflict", "状态应为冲突"),
            (title(&c) == "冲突", "状态行应为“冲突”"),
            (switch_on(&c) == Some(true), "冲突时开关仍为开"),
            (settings_btn(&c, "重新接管").is_ok(), "应提供“重新接管”"),
            (settings_btn(&c, "恢复原设置").is_ok(), "应提供“恢复原设置”"),
        ],
    );
    c.click(&settings_btn(&c, "重新接管")?);
    c.pump();
    r.step(
        &c,
        "重新接管",
        vec![
            (state(&c) == "Taken", "状态应回到已接管"),
            (settings_btn(&c, "重新接管").is_err(), "按钮随状态消失"),
        ],
    );

    // 6. 程序换了位置 → 关联失效 → 修复
    manage::take(&mut reg, Path::new(OLD_EXE), &backup)?;
    reopen_settings(&c)?;
    r.step(
        &c,
        "关联指向旧位置",
        vec![
            (state(&c) == "Stale", "状态应为关联失效"),
            (title(&c) == "关联失效", "状态行应为“关联失效”"),
            (settings_btn(&c, "修复").is_ok(), "应提供“修复”"),
        ],
    );
    c.click(&settings_btn(&c, "修复")?);
    c.pump();
    let cmd = own_cmd(&reg).unwrap_or_default();
    r.step(
        &c,
        "修复关联",
        vec![
            (state(&c) == "Taken", "状态应回到已接管"),
            (cmd.contains(EXE), "命令应改回当前程序"),
        ],
    );

    // 7. 关闭开关：恢复原设置
    c.click(&switch(&c)?);
    c.pump();
    let foreign_kept =
        reg.get(r"Directory\shell", "")?.and_then(|v| v.text()) == Some(FOREIGN_VERB.into());
    // 重新接管时别的程序的值被当作新的“原值”备份，恢复后应回到它；去掉它后应与布置时逐值一致
    reg.delete_value(r"Directory\shell", "")?;
    r.step(
        &c,
        "关闭开关恢复原设置",
        vec![
            (!dlg(&c), "关闭开关不需要确认"),
            (state(&c) == "NotTaken", "状态应为未接管"),
            (switch_on(&c) == Some(false), "开关应为关"),
            (foreign_kept, "别的程序后来写入的默认动词应被保留"),
            (unchanged(&reg), "其余与布置时逐值一致"),
            (!backup.exists(), "恢复完成后删除备份文件"),
            (msg(&c).starts_with("已恢复原设置"), "状态栏提示已恢复"),
        ],
    );

    // 8. Esc 关闭设置
    c.special(Key::Escape, &[]);
    c.pump();
    r.step(&c, "Esc 关闭设置", vec![(!visible(&c), "设置面板应关闭")]);

    // 9. 启动自检：关联失效时询问；“以后再说”不改动，“恢复原设置”撤销接管
    manage::take(&mut reg, Path::new(OLD_EXE), &backup)?;
    c.ctl.entry_self_check();
    c.pump();
    r.step(
        &c,
        "启动自检发现关联失效",
        vec![
            (dlg(&c), "应弹出自检对话框"),
            (dlg_title(&c) == "文件夹打开方式指向了旧位置", "对话框标题"),
            (
                c.win.get_dialog_body().contains(OLD_EXE),
                "说明中给出旧路径",
            ),
            (
                c.win.get_dialog_secondary_label() == "恢复原设置",
                "应提供“恢复原设置”",
            ),
        ],
    );
    c.click(&dlg_btn(&c, "以后再说")?);
    c.pump();
    r.step(
        &c,
        "自检选择以后再说",
        vec![
            (!dlg(&c), "对话框应关闭"),
            (
                own_cmd(&reg).is_some_and(|s| s.contains(OLD_EXE)),
                "不改动注册表",
            ),
        ],
    );
    c.ctl.entry_self_check();
    c.pump();
    c.click(&dlg_btn(&c, "恢复原设置")?);
    c.pump();
    r.step(
        &c,
        "自检选择恢复原设置",
        vec![
            (!dlg(&c), "对话框应关闭"),
            (unchanged(&reg), "与布置时逐值一致"),
            (!backup.exists(), "删除备份文件"),
        ],
    );

    // 10. 外部打开请求
    open_settings(&c)?;
    let photos = fixture::path(fixture::PHOTOS);
    let rep = request(&c, open_req(photos.clone()));
    r.step(
        &c,
        "外部请求打开新文件夹",
        vec![
            (rep.ok, "答复成功"),
            (!visible(&c), "打开文件夹前关闭设置面板"),
            (tabs(&c) == 2, "新开一个标签"),
            (active_tab(&c) == 1, "切到新标签"),
            (c.ctl.location() == Some(photos.clone()), "位置为照片"),
        ],
    );
    let rep = request(&c, open_req(fixture::root()));
    r.step(
        &c,
        "外部请求打开已打开的文件夹",
        vec![
            (rep.ok, "答复成功"),
            (tabs(&c) == 2, "复用已有标签，不新建"),
            (active_tab(&c) == 0, "切到样例目录所在标签"),
        ],
    );
    let rep = request(&c, open_req(photos.clone()));
    r.step(
        &c,
        "再次外部请求照片",
        vec![
            (rep.ok, "答复成功"),
            (tabs(&c) == 2, "仍复用"),
            (active_tab(&c) == 1, "切回照片标签"),
        ],
    );
    let rep = request(&c, open_req(fixture::path("说明.md")));
    r.step(
        &c,
        "外部请求打开文件",
        vec![
            (rep.ok, "答复成功"),
            (tabs(&c) == 2, "所在文件夹已打开，复用标签"),
            (
                c.ctl.location() == Some(fixture::root()),
                "打开文件所在文件夹",
            ),
            (
                c.ctl.selected_names() == vec!["说明.md".to_string()],
                "选中该文件",
            ),
        ],
    );
    let rep = request(
        &c,
        Request::Open {
            folder: fixture::root(),
            select: Some(fixture::path("数据表.xlsx")),
        },
    );
    r.step(
        &c,
        "外部请求打开并选中",
        vec![
            (rep.ok, "答复成功"),
            (
                c.ctl.selected_names() == vec!["数据表.xlsx".to_string()],
                "选中指定项",
            ),
        ],
    );
    let missing = fixture::path("不存在的文件夹");
    let rep = request(&c, open_req(missing.clone()));
    r.step(
        &c,
        "外部请求不存在的路径",
        vec![
            (!rep.ok, "答复失败"),
            (rep.message.contains("不存在的文件夹"), "答复说明路径"),
            (dlg(&c), "弹出说明"),
            (dlg_title(&c) == "无法打开文件夹", "对话框标题"),
            (tabs(&c) == 2, "不新建标签"),
        ],
    );
    let rep = request(&c, open_req(missing));
    r.step(
        &c,
        "对话框打开时再请求不存在的路径",
        vec![
            (!rep.ok, "答复失败"),
            (dlg_title(&c) == "无法打开文件夹", "不叠加对话框"),
            (msg(&c).starts_with("无法打开"), "改在状态栏提示"),
        ],
    );
    c.click(&dlg_btn(&c, "知道了")?);
    c.pump();
    let rep = request(&c, Request::Activate);
    r.step(
        &c,
        "外部请求激活窗口",
        vec![
            (rep.ok && rep.message == "已激活窗口", "答复已激活"),
            (tabs(&c) == 2, "不改动标签"),
        ],
    );
    drop(c);
    root.cleanup()?;
    Ok((r.steps, r.fails))
}

// ---------- 截图场景 ----------

/// 截图场景的系统入口环境：内存注册表（布置为与本机同构的初始状态）＋ 虚构程序路径。名称不以 settings- 开头时返回 `None`。
pub fn showcase_env(name: &str) -> Option<EntryEnv> {
    if !name.starts_with("settings-") {
        return None;
    }
    if name == "settings-unavailable" {
        return Some(EntryEnv::unavailable(
            "没有数据目录（--no-session 或无法确定 %LOCALAPPDATA%），无法保存备份",
        ));
    }
    let backup = backup_in(name).ok()?;
    let mut m = MemStore::new();
    takeover::seed_rehearsal(&mut m).ok()?;
    let exe = PathBuf::from(EXE);
    match name {
        "settings-on" => {
            manage::take(&mut m, &exe, &backup).ok()?;
        }
        "settings-conflict" => {
            manage::take(&mut m, &exe, &backup).ok()?;
            m.set(r"Directory\shell", "", &RegValue::sz(FOREIGN_VERB))
                .ok()?;
        }
        "settings-stale" => {
            manage::take(&mut m, Path::new(OLD_EXE), &backup).ok()?;
        }
        _ => {}
    }
    Some(EntryEnv::with_store(Box::new(m), exe, backup))
}

pub fn scene_settings(c: &Ctx) -> Res<()> {
    c.ctl.show_settings();
    c.pump();
    c.advance(300); // 越过开关动画
    Ok(())
}

pub fn scene_confirm(c: &Ctx) -> Res<()> {
    scene_settings(c)?;
    c.click(&switch(c)?);
    c.pump();
    c.advance(300);
    Ok(())
}
