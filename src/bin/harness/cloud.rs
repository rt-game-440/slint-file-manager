//! 云盘无隐式下载浏览（Issue #14）：截图场景的准备步骤与交互脚本。
//!
//! 全部在 `%TEMP%\sfm-harness\scratch\cloud-*` 的一次性数据上进行，提供方是模拟的（[`MockCloud`]，经 [`StashBackend`] 注入），
//! **不触碰真实云盘，也不含任何私人文件名**。“仅在线”用真实文件加 `attrib +O`（属性与真实占位文件一致，内容其实在本机）；
//! “本地可用／始终保留”由模拟提供方的占位信息给出。覆盖：四种状态与提供方状态的显示（图标之外的文字说明）、
//! 缩略图只用允许的来源、打开／复制／拖出／移动／删除对用户的说明、同步客户端退出与断网时的显示与恢复、侧栏云盘入口。
//! 真实环境的只读核查见 `cloud-real` 子命令。

use crate::ctx::{Ctx, Opts, Res};
use crate::dual::Rec;
use crate::fixture;
use crate::preview::png_bytes;
use crate::thumbs::{FAILED, READY, desc};
use crate::xfer::{center, press_and_move, release};
use serde_json::Value;
use slint::platform::Key;
use slint_file_manager::app::{DriveSource, LiveEnv};
use slint_file_manager::cloud::{Pin, Placeholder, ProviderStatus};
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const ONLINE_DOC: &str = "季度报告.docx";
pub const ONLINE_PDF: &str = "会议纪要.pdf";
pub const ONLINE_PHOTO: &str = "旅行照片.png";
pub const LOCAL_SHEET: &str = "预算表.xlsx";
pub const LOCAL_PHOTO: &str = "家庭照片.png";
pub const PINNED_DOC: &str = "常用模板.docx";
pub const PLAIN_NOTE: &str = "本机笔记.txt";
pub const ONLINE_DIR: &str = "旧项目归档";
pub const LOCAL_DIR: &str = "本地资料";

const ONLINE_TEXT: &str = "云盘：仅在线（未下载到本机）";
const LOCAL_TEXT: &str = "云盘：本地可用";
const PINNED_TEXT: &str = "云盘：始终保留在本设备";
const DOWN_TEXT: &str = "云盘：仅在线，同步客户端当前不可用";

fn attrib(flag: &str, p: &Path) -> Res<()> {
    let ok = std::process::Command::new("attrib")
        .arg(flag)
        .arg(p)
        .status()?
        .success();
    if ok {
        Ok(())
    } else {
        Err(format!("attrib {flag} 失败").into())
    }
}

fn stamp(p: &Path, h: u64) -> Res<()> {
    fs::OpenOptions::new()
        .write(true)
        .open(p)?
        .set_modified(fixture::time(h))?;
    Ok(())
}

/// 生成模拟云盘目录（每次重建，修改时间固定，截图确定）。
pub fn build(name: &str) -> Res<PathBuf> {
    let work = fixture::fresh_dir(&fixture::scratch_dir(name))?;
    fixture::file(&work, ONLINE_DOC, 48_200, fixture::time(10))?;
    fixture::file(&work, ONLINE_PDF, 1_250_000, fixture::time(11))?;
    fs::write(work.join(ONLINE_PHOTO), png_bytes(64, 48, 5))?;
    stamp(&work.join(ONLINE_PHOTO), 12)?;
    fixture::file(&work, LOCAL_SHEET, 23_700, fixture::time(13))?;
    fs::write(work.join(LOCAL_PHOTO), png_bytes(64, 48, 9))?;
    stamp(&work.join(LOCAL_PHOTO), 14)?;
    fixture::file(&work, PINNED_DOC, 15_300, fixture::time(15))?;
    fixture::file(&work, PLAIN_NOTE, 900, fixture::time(16))?;
    for (d, h) in [(ONLINE_DIR, 17), (LOCAL_DIR, 18)] {
        fs::create_dir(work.join(d))?;
        fixture::dir_time(&work.join(d), fixture::time(h))?;
    }
    for n in [ONLINE_DOC, ONLINE_PDF, ONLINE_PHOTO, ONLINE_DIR] {
        attrib("+O", &work.join(n))?;
    }
    Ok(work)
}

/// 把 `work` 登记为模拟 OneDrive 同步根，并给本地可用／始终保留的文件登记占位信息。
pub fn attach(c: &Ctx, work: &Path) {
    let m = c.stash.cloud_mock();
    m.add_root(
        &work.to_string_lossy(),
        "OneDrive",
        ProviderStatus::Connected,
    );
    let ph = |pin| Placeholder {
        on_disk: 1,
        pin,
        in_sync: true,
    };
    m.set_placeholder(
        &work.join(LOCAL_SHEET).to_string_lossy(),
        ph(Pin::Unspecified),
    );
    m.set_placeholder(
        &work.join(LOCAL_PHOTO).to_string_lossy(),
        ph(Pin::Unspecified),
    );
    m.set_placeholder(&work.join(PINNED_DOC).to_string_lossy(), ph(Pin::Pinned));
    // 普通笔记：同步根内但没有占位信息（尚未被提供方接管），不登记
}

fn go(c: &Ctx, dir: &Path) -> Res<()> {
    c.enter_address(&dir.to_string_lossy())?;
    c.pump();
    Ok(())
}

fn set_status(c: &Ctx, work: &Path, s: ProviderStatus) {
    c.stash.cloud_mock().set_status(&work.to_string_lossy(), s);
}

/// 推进节拍直到触发一次提供方状态探测（每 20 拍一次），并让随之排队的静默重读完成。
fn probe(c: &Ctx) {
    for _ in 0..20 {
        c.ctl.tick();
    }
    c.pump();
    c.pump();
}

fn msg(c: &Ctx) -> String {
    c.win.get_status_message().to_string()
}

fn full(c: &Ctx, name: &str) -> String {
    c.item(name)
        .ok()
        .and_then(|e| e.accessible_description())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "<找不到>".into())
}

fn open_window(size: (u32, u32), scale: f32, work: &Path) -> Res<Ctx> {
    let c = Ctx::open(
        size,
        scale,
        Opts {
            start: work.to_path_buf(),
            load: false,
            ..Opts::default()
        },
    )?;
    attach(&c, work);
    c.pump();
    Ok(c)
}

// ---------- 截图场景 ----------

/// 网格视图：四种状态（仅在线、本地可用、始终保留、不在云盘里的普通文件）与仅在线文件夹。
pub fn scene_grid(c: &Ctx) -> Res<()> {
    let work = build("cloud-scene")?;
    attach(c, &work);
    go(c, &work)?;
    c.click(&c.item(ONLINE_DOC)?);
    Ok(())
}

/// 列表视图，选中一个“本地可用”文件（状态栏用文字说明）。
pub fn scene_list(c: &Ctx) -> Res<()> {
    let work = build("cloud-scene")?;
    attach(c, &work);
    go(c, &work)?;
    c.click(&c.button("列表视图")?);
    c.click(&c.item(LOCAL_SHEET)?);
    Ok(())
}

/// 同步客户端未运行：仅在线文件换成琥珀色标记，状态栏如实说明，已下载的文件不受影响。
pub fn scene_down(c: &Ctx) -> Res<()> {
    let work = build("cloud-scene")?;
    attach(c, &work);
    go(c, &work)?;
    set_status(c, &work, ProviderStatus::Disconnected);
    probe(c);
    Ok(())
}

/// 在云盘里按“删除”：确认框说明删除会同步到云端，只想腾空间应改用“释放空间”。
pub fn scene_delete(c: &Ctx) -> Res<()> {
    let work = build("cloud-scene")?;
    attach(c, &work);
    go(c, &work)?;
    c.click(&c.item(LOCAL_SHEET)?);
    c.click(&c.button("删除")?);
    Ok(())
}

// ---------- 交互脚本 ----------

/// 云盘交互脚本。
pub fn cloud(size: (u32, u32), scale: f32) -> Res<(Vec<Value>, Vec<String>)> {
    let mut r = Rec::new();
    browse_and_explicit(size, scale, &mut r)?;
    provider_down(size, scale, &mut r)?;
    sidebar(size, scale, &mut r)?;
    Ok((r.steps, r.fails))
}

fn browse_and_explicit(size: (u32, u32), scale: f32, r: &mut Rec) -> Res<()> {
    let work = build("cloud-work")?;
    let c = &open_window(size, scale, &work)?;

    // 1. 四种状态与文件夹
    let calls = c.stash.cloud_mock().placeholder_calls();
    r.step(
        c,
        "C1 进入云盘目录：状态来自属性与提供方，文字说明与真实情况一致",
        vec![
            (
                full(c, ONLINE_DOC).contains(ONLINE_TEXT),
                "未下载的文档为仅在线",
            ),
            (
                full(c, ONLINE_PDF).contains(ONLINE_TEXT),
                "未下载的 PDF 为仅在线",
            ),
            (
                full(c, ONLINE_DIR).contains(ONLINE_TEXT),
                "未填充的文件夹为仅在线",
            ),
            (
                full(c, LOCAL_SHEET).contains(LOCAL_TEXT),
                "已下载的表格为本地可用",
            ),
            (
                full(c, PINNED_DOC).contains(PINNED_TEXT),
                "固定的文档为始终保留",
            ),
            (
                !full(c, PLAIN_NOTE).contains("云盘"),
                "没有占位信息的普通文件不标记",
            ),
            (!full(c, LOCAL_DIR).contains("云盘"), "已下载的文件夹不标记"),
            // 4 个占位信息查询：本地表格、本地照片、固定文档、普通笔记；仅在线的 4 项只看属性，不向提供方询问
            (calls == 4, "仅在线项目只看属性，不向提供方询问占位信息"),
        ],
    );

    // 2. 缩略图：仅在线的图片不读内容（回退图标），本地的照常；系统缩略图只做仅缓存查询
    let shell = c.shell.calls();
    let online_calls: Vec<bool> = shell
        .iter()
        .filter(|(p, _)| p.ends_with(ONLINE_PHOTO))
        .map(|(_, cache_only)| *cache_only)
        .collect();
    r.step(
        c,
        "C2 缩略图只用允许的来源：仅在线图片回退图标，不触发下载",
        vec![
            (
                desc(c, ONLINE_PHOTO) == FAILED,
                "仅在线图片没有缩略图，显示图标",
            ),
            (
                full(c, ONLINE_PHOTO).contains(ONLINE_TEXT),
                "图标之外仍有仅在线说明",
            ),
            (desc(c, LOCAL_PHOTO) == READY, "已下载的图片显示缩略图"),
            (
                !online_calls.is_empty() && online_calls.iter().all(|x| *x),
                "对仅在线文件的系统缩略图查询只能是“仅缓存”",
            ),
            (
                shell
                    .iter()
                    .all(|(p, cache_only)| *cache_only || !p.ends_with(ONLINE_PHOTO)),
                "没有对仅在线文件发起会触发提供方的查询",
            ),
        ],
    );

    // 3. 状态栏：选中一项时用文字说明
    c.click(&c.item(ONLINE_DOC)?);
    let on = c.win.get_status_text().to_string();
    c.click(&c.item(LOCAL_SHEET)?);
    let loc = c.win.get_status_text().to_string();
    c.click(&c.item(PINNED_DOC)?);
    let pin = c.win.get_status_text().to_string();
    c.click(&c.item(PLAIN_NOTE)?);
    let plain = c.win.get_status_text().to_string();
    r.step(
        c,
        "C3 状态栏用文字说明所选项目的云盘状态",
        vec![
            (on.contains("云盘：仅在线"), "仅在线"),
            (loc.contains("云盘：本地可用"), "本地可用"),
            (pin.contains("云盘：始终保留"), "始终保留"),
            (!plain.contains("云盘"), "普通文件没有云盘说明"),
        ],
    );

    // 4. 打开：显式操作，允许下载，但要让人看出来
    c.double_click(&c.item(LOCAL_SHEET)?);
    let local_open = msg(c);
    c.double_click(&c.item(ONLINE_DOC)?);
    let online_open = msg(c);
    r.step(
        c,
        "C4 打开仅在线文件前说明将通过 OneDrive 下载；打开本地文件没有这类提示",
        vec![
            (!local_open.contains("只在云端"), "本地文件打开不提示下载"),
            (
                online_open.contains("只在云端")
                    && online_open.contains("OneDrive")
                    && online_open.contains("下载"),
                "仅在线文件打开时点名提供方并说明会下载",
            ),
            (
                c.win.get_status_message_ok(),
                "提供方正常时是普通提示而非警示",
            ),
            (
                c.opened.borrow().iter().any(|p| p.ends_with(ONLINE_DOC)),
                "仍然交给默认应用打开（用户的明确操作）",
            ),
        ],
    );

    // 5. 复制：提前说明；粘贴后汇报哪些被下载
    c.click(&c.item(ONLINE_DOC)?);
    c.ctrl_click(&c.item(LOCAL_SHEET)?);
    c.ctrl("c");
    let copy_msg = msg(c);
    c.open_folder(LOCAL_DIR)?;
    c.ctrl("v");
    c.pump();
    let pasted = msg(c);
    r.step(
        c,
        "C5 复制含仅在线文件：复制时提前说明，粘贴完成后汇报已下载的数量",
        vec![
            (
                copy_msg.contains("已复制")
                    && copy_msg.contains("仅在线")
                    && copy_msg.contains("下载"),
                "复制提示里说明含仅在线文件、将由云盘客户端下载",
            ),
            (
                pasted.contains("已复制")
                    && pasted.contains("1 个仅在线文件")
                    && pasted.contains("已由云盘客户端下载到本机"),
                "粘贴完成后汇报下载的仅在线文件数",
            ),
            (
                work.join(LOCAL_DIR).join(ONLINE_DOC).exists()
                    && work.join(LOCAL_DIR).join(LOCAL_SHEET).exists(),
                "文件确实复制过去了",
            ),
        ],
    );

    // 6. 同盘移动 = 改名，不读内容，不提下载
    go(c, &work)?;
    c.click(&c.item(ONLINE_PDF)?);
    c.ctrl("x");
    c.open_folder(LOCAL_DIR)?;
    c.ctrl("v");
    c.pump();
    let moved = msg(c);
    r.step(
        c,
        "C6 同一磁盘上移动仅在线文件只是改名：不读内容，也不说下载",
        vec![
            (work.join(LOCAL_DIR).join(ONLINE_PDF).exists(), "文件已移动"),
            (!work.join(ONLINE_PDF).exists(), "来源已不在"),
            (
                moved.contains("已移动") && !moved.contains("下载"),
                "汇报里没有下载说明",
            ),
        ],
    );

    // 7. 拖出窗口：对方读取文件会下载，拖出时就说明
    go(c, &work)?;
    let a = center(&c.item(ONLINE_DOC)?);
    c.click(&c.item(ONLINE_DOC)?);
    press_and_move(c, a, &[(a.0 + 12.0, a.1 + 12.0), (-20.0, a.1)]);
    release(c, (-20.0, a.1));
    c.pump();
    let dragged = c.dragged_out.borrow().clone();
    let drag_msg = msg(c);
    r.step(
        c,
        "C7 拖出窗口交给系统拖放：拖出时说明仅在线文件会被下载",
        vec![
            (
                dragged.len() == 1 && dragged[0] == vec![work.join(ONLINE_DOC)],
                "记录了一次拖出",
            ),
            (
                drag_msg.contains("仅在线") && drag_msg.contains("下载"),
                "拖出时有下载说明",
            ),
        ],
    );

    // 8. 删除确认：说明云盘里删除与“释放空间”的区别；不在云盘里的不加这段话
    c.click(&c.item(LOCAL_SHEET)?);
    c.click(&c.button("删除")?);
    let recycle_body = c.win.get_dialog_body().to_string();
    let recycle_title = c.win.get_dialog_title().to_string();
    c.click(&crate::delete::dlg_btn(c, "取消")?);
    c.special(Key::Delete, &[Key::Shift]);
    let perm_body = c.win.get_dialog_body().to_string();
    c.click(&crate::delete::dlg_btn(c, "取消")?);
    r.step(
        c,
        "C8 云盘里删除：确认框说明会同步到云端，只想腾空间请用“释放空间”",
        vec![
            (recycle_title.contains("回收站"), "仍是移到回收站的确认"),
            (
                recycle_body.contains("OneDrive")
                    && recycle_body.contains("云端")
                    && recycle_body.contains("释放空间"),
                "回收站确认框点名提供方、说明云端影响并指出“释放空间”",
            ),
            (
                recycle_body.contains("不会删除云端文件"),
                "明确“释放空间”不会删除云端文件",
            ),
            (
                perm_body.contains("永久删除会同步到云端") && perm_body.contains("释放空间"),
                "永久删除确认框同样说明",
            ),
            (work.join(LOCAL_SHEET).exists(), "取消后文件仍在"),
        ],
    );
    go(c, &fixture::root())?;
    c.click(&c.item("说明.md")?);
    c.click(&c.button("删除")?);
    let plain_body = c.win.get_dialog_body().to_string();
    c.click(&crate::delete::dlg_btn(c, "取消")?);
    r.step(
        c,
        "C9 不在云盘里的删除确认没有云盘说明",
        vec![(
            !plain_body.contains("同步文件夹") && !plain_body.contains("释放空间"),
            "普通目录的确认框保持原样",
        )],
    );
    Ok(())
}

fn provider_down(size: (u32, u32), scale: f32, r: &mut Rec) -> Res<()> {
    let work = build("cloud-down")?;
    let c = &open_window(size, scale, &work)?;
    let before = full(c, ONLINE_DOC);
    let m0 = msg(c);
    r.step(
        c,
        "D1 提供方正常：没有警告",
        vec![
            (before.contains(ONLINE_TEXT), "仅在线"),
            (m0.is_empty(), "状态栏没有提示"),
        ],
    );

    // 同步客户端退出（未运行）
    set_status(c, &work, ProviderStatus::Disconnected);
    probe(c);
    let m1 = msg(c);
    r.step(
        c,
        "D2 同步客户端退出：无需用户操作，列表与状态栏自动反映",
        vec![
            (
                full(c, ONLINE_DOC).contains(DOWN_TEXT),
                "仅在线文件标为暂时无法下载",
            ),
            (full(c, ONLINE_DIR).contains(DOWN_TEXT), "仅在线文件夹同样"),
            (
                full(c, LOCAL_SHEET).contains(LOCAL_TEXT),
                "已下载的文件不受影响",
            ),
            (
                full(c, PINNED_DOC).contains(PINNED_TEXT),
                "始终保留的文件不受影响",
            ),
            (
                m1.contains("OneDrive")
                    && m1.contains("未运行")
                    && m1.contains("已下载的文件照常可用"),
                "状态栏说明原因与影响",
            ),
            (desc(c, LOCAL_PHOTO) == READY, "已下载图片的缩略图照常"),
        ],
    );

    // 断网
    set_status(c, &work, ProviderStatus::ConnectivityLost);
    probe(c);
    r.step(
        c,
        "D4 断网：状态栏改为网络说明，标记保持“暂时无法下载”",
        vec![
            (
                full(c, ONLINE_DOC).contains(DOWN_TEXT),
                "仅在线文件仍为暂时无法下载",
            ),
            (
                full(c, LOCAL_SHEET).contains(LOCAL_TEXT),
                "已下载的文件不受影响",
            ),
            (msg(c).contains("已断开网络连接"), "状态栏改为网络说明"),
        ],
    );

    // 此时打开仅在线文件：如实说明会失败（警示色），不是假装在下载
    c.double_click(&c.item(ONLINE_DOC)?);
    let m2 = msg(c);
    r.step(
        c,
        "D3 客户端不可用时打开仅在线文件：警示色说明下载不了",
        vec![
            (
                m2.contains("只在云端")
                    && m2.contains("OneDrive 已断开网络连接")
                    && m2.contains("可能失败"),
                "如实说明",
            ),
            (!c.win.get_status_message_ok(), "用警示色"),
        ],
    );

    // 恢复
    set_status(c, &work, ProviderStatus::Connected);
    probe(c);
    r.step(
        c,
        "D5 恢复：标记回到“仅在线”，警告消失",
        vec![
            (full(c, ONLINE_DOC).contains(ONLINE_TEXT), "恢复为仅在线"),
            (!full(c, ONLINE_DOC).contains("不可用"), "不再说不可用"),
        ],
    );

    go(c, &work)?;
    r.step(
        c,
        "D5b 重新进入目录：没有残留警告，状态仍正确",
        vec![
            (msg(c).is_empty(), "状态栏没有提示"),
            (full(c, ONLINE_DOC).contains(ONLINE_TEXT), "仍为仅在线"),
        ],
    );

    // 文件状态变化：外部把本地文件“释放空间”（变为仅在线）后，目录监视触发的静默重读应反映
    attrib("+O", &work.join(LOCAL_PHOTO))?;
    c.special(Key::F5, &[]);
    c.pump();
    r.step(
        c,
        "D6 文件被外部释放空间：重读后变为仅在线，缩略图保留策略不读内容",
        vec![(
            full(c, LOCAL_PHOTO).contains(ONLINE_TEXT),
            "状态跟随文件变化",
        )],
    );
    Ok(())
}

fn sidebar(size: (u32, u32), scale: f32, r: &mut Rec) -> Res<()> {
    let od = build("cloud-od")?;
    let ic = fixture::fresh_dir(&fixture::scratch_dir("cloud-icloud"))?;
    let ph = fixture::fresh_dir(&fixture::scratch_dir("cloud-icloud-photos"))?;
    let env = LiveEnv {
        drives: DriveSource::Fixed(Rc::new(RefCell::new(Vec::new()))),
        favorites: None,
        onedrive: Some(od),
        icloud: Some(ic.clone()),
        icloud_photos: Some(ph.clone()),
        watch: false,
    };
    let c = &Ctx::open(
        size,
        scale,
        Opts {
            live: Some(env),
            ..Opts::default()
        },
    )?;
    let ids: Vec<String> = c.ctl.sidebar_rows().into_iter().map(|(i, _)| i).collect();
    c.click(&c.button("iCloud 照片")?);
    c.pump();
    let at_photos = c.ctl.location() == Some(ph.clone());
    let cur = c.win.get_sidebar_current().to_string();
    c.click(&c.button("iCloud")?);
    c.pump();
    let at_icloud = c.ctl.location() == Some(ic);
    r.step(
        c,
        "S1 侧栏云盘分组含 OneDrive、iCloud、iCloud 照片（来自同步根发现），点击进入各自目录",
        vec![
            (
                ["onedrive", "icloud", "icloud-photos"]
                    .iter()
                    .all(|k| ids.iter().any(|i| i == k)),
                "三个云盘入口都在",
            ),
            (
                at_photos && cur == "icloud-photos",
                "iCloud 照片进入其目录并高亮",
            ),
            (at_icloud, "iCloud 进入其目录"),
        ],
    );
    Ok(())
}
