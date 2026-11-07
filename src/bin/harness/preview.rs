//! 大图预览与系统缩略图（Issue #13）：截图场景的准备步骤与交互脚本。
//!
//! - [`preview`]：预览的完整生命周期——Space 打开（从哪个面板、哪个文件）、后台解码的加载中／就绪、←/→/Home/End 切换与不越界、
//!   适应窗口与 100%、Esc／Space／按钮关闭、关闭后选择／光标／焦点不变、预览期间键盘与鼠标不落到下面的界面、
//!   快速开关不残留、过期结果被丢弃、损坏／超大／云占位／缺失的失败状态、GIF 播放与暂停、另一面板与其标签不受影响、
//!   不能预览的项目只给出状态栏说明、双击仍用默认应用打开。
//! - [`shell`]：系统缩略图（视频、云占位只查缓存、磁盘缓存复用与失效）。系统部分用确定性替身（见 `thumbs::FakeShell`），
//!   真实 Shell 的兼容性另见 `video-compat`。
//!
//! 样例都在草稿副本上（`%TEMP%\sfm-harness\scratch\preview-*`，每次重建），不接触用户真实目录。

use crate::ctx::{Ctx, Opts, Res};
use crate::dual::Rec;
use crate::dump::extras;
use crate::fixture::{self, THUMB_FILES};
use crate::thumbs::{FAILED, FAKE_VIDEO, LOADING, READY, copy_dir, desc};
use i_slint_backend_testing::AccessibleRole;
use serde_json::Value;
use slint::platform::Key;
use slint_file_manager::thumbs::decode::samples;
use std::fs;
use std::path::{Path, PathBuf};

// ---------- 小工具 ----------

fn go(c: &Ctx, dir: &Path) -> Res<()> {
    c.enter_address(&dir.to_string_lossy())?;
    c.pump();
    Ok(())
}

fn space(c: &Ctx) {
    c.key(" ");
    c.advance(16);
}

fn tap(c: &Ctx, k: Key) {
    c.special(k, &[]);
    c.advance(16);
}

fn letter(c: &Ctx, s: &str) {
    c.key(s);
    c.advance(16);
}

fn overlay_open(c: &Ctx) -> bool {
    !c.find_all(AccessibleRole::Groupbox, "图片预览").is_empty()
}

fn overlay_desc(c: &Ctx) -> String {
    c.find_all(AccessibleRole::Groupbox, "图片预览")
        .first()
        .and_then(|e| e.accessible_description())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// 预览层里带标签的文字（`accessible-value`）。
fn text(c: &Ctx, label: &str) -> String {
    c.find_all(AccessibleRole::Text, label)
        .first()
        .and_then(|e| e.accessible_value())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn has_button(c: &Ctx, label: &str) -> bool {
    !c.find_all(AccessibleRole::Button, label).is_empty()
}

fn button_checked(c: &Ctx, label: &str) -> Option<bool> {
    c.find_all(AccessibleRole::Button, label)
        .first()
        .and_then(|e| e.accessible_checked())
}

/// 预览里当前图片的像素尺寸（Slint 图像模型）。
fn shown(c: &Ctx) -> (u32, u32) {
    let s = c.win.get_preview_image().size();
    (s.width, s.height)
}

fn shown_is_blank(c: &Ctx) -> bool {
    shown(c) == (0, 0)
}

fn shown_pixel(c: &Ctx, x: u32, y: u32) -> Option<[u8; 4]> {
    let b = c.win.get_preview_image().to_rgba8()?;
    if x >= b.width() || y >= b.height() {
        return None;
    }
    let p = b.as_slice()[(y * b.width() + x) as usize];
    Some([p.r, p.g, p.b, p.a])
}

fn reddish(p: Option<[u8; 4]>) -> bool {
    p.is_some_and(|p| p[0] > 180 && p[1] < 90 && p[2] < 90)
}

fn bluish(p: Option<[u8; 4]>) -> bool {
    p.is_some_and(|p| p[2] > 180 && p[0] < 80)
}

/// 图片本体在窗口里的逻辑尺寸。
fn pic_size(c: &Ctx) -> (f32, f32) {
    c.by_id("PreviewOverlay::pic")
        .map(|e| {
            let s = e.size();
            (s.width, s.height)
        })
        .unwrap_or((0.0, 0.0))
}

fn stage_size(c: &Ctx) -> (f32, f32) {
    c.by_id("PreviewOverlay::stage")
        .map(|e| {
            let s = e.size();
            (s.width, s.height)
        })
        .unwrap_or((0.0, 0.0))
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.6
}

fn msg(c: &Ctx) -> String {
    c.win.get_status_message().to_string()
}

fn panes(c: &Ctx) -> Value {
    extras(c)["panes"].clone()
}

/// 动图直到显示第 `f` 帧（最多等 1 秒模拟时间）。
fn until_frame(c: &Ctx, f: usize) -> bool {
    for _ in 0..100 {
        if c.ctl.preview_info().frame == f {
            return true;
        }
        c.advance(10);
    }
    c.ctl.preview_info().frame == f
}

fn offline(path: &Path) -> Res<()> {
    let ok = std::process::Command::new("attrib")
        .arg("+O")
        .arg(path)
        .status()?
        .success();
    if ok {
        Ok(())
    } else {
        Err("attrib +O 失败".into())
    }
}

// ---------- 截图场景 ----------

/// 进入缩略图样例，选中 `name` 并按空格；`pump` 为真时让解码完成，最后推进到淡入结束。
fn open_at(c: &Ctx, name: &str, pump: bool) -> Res<()> {
    go(c, &fixture::thumbs_root())?;
    c.click(&c.item(name)?);
    space(c);
    if pump {
        c.ctl.pump_preview();
    }
    c.advance(240);
    Ok(())
}

pub fn scene_fit(c: &Ctx) -> Res<()> {
    open_at(c, THUMB_FILES[1], true)
}

pub fn scene_one_to_one(c: &Ctx) -> Res<()> {
    open_at(c, THUMB_FILES[0], true)?;
    letter(c, "1");
    Ok(())
}

pub fn scene_loading(c: &Ctx) -> Res<()> {
    open_at(c, THUMB_FILES[0], false)
}

pub fn scene_failed(c: &Ctx) -> Res<()> {
    open_at(c, THUMB_FILES[7], true)
}

pub fn scene_too_large(c: &Ctx) -> Res<()> {
    open_at(c, THUMB_FILES[8], true)
}

/// 自动播放，停在第二帧（先暂停等淡入结束再继续，截图时仍是第二帧）。
pub fn scene_gif_playing(c: &Ctx) -> Res<()> {
    open_at(c, THUMB_FILES[3], false)?;
    c.ctl.pump_preview();
    until_frame(c, 1);
    letter(c, "p");
    c.advance(240);
    letter(c, "p");
    Ok(())
}

/// 打开后立刻暂停：停在第一帧。
pub fn scene_gif_first_frame(c: &Ctx) -> Res<()> {
    go(c, &fixture::thumbs_root())?;
    c.click(&c.item(THUMB_FILES[3])?);
    space(c);
    c.ctl.pump_preview();
    letter(c, "p");
    c.advance(240);
    Ok(())
}

/// 动画场景：预览层打开（图片已解码）；t=0 取打开瞬间。
pub fn start_appear(c: &Ctx) -> Res<()> {
    go(c, &fixture::thumbs_root())?;
    c.click(&c.item(THUMB_FILES[1])?);
    c.key(" ");
    c.ctl.pump_preview();
    Ok(())
}

/// 打开动画的曲线断言：整体不透明度从 0 起 ≤200ms 内单调淡入到 1、无过冲；图片本体位置与大小全程不变。
pub fn check_appear(sum: &Value) -> Result<(), String> {
    let op = &sum["overlay"]["opacity"];
    let num = |v: &Value| v.as_f64().unwrap_or(f64::NAN);
    if num(&op["start"]) != 0.0 || num(&op["end"]) != 1.0 {
        return Err(format!("不透明度应从 0 到 1：{op}"));
    }
    // 单调且无过冲：最小值就是起点，最大值就是终点
    if num(&op["min"]) != 0.0 || num(&op["max"]) != 1.0 || num(&op["overshoot"]) != 0.0 {
        return Err(format!("不透明度有回落或过冲：{op}"));
    }
    match op["settle_ms"].as_u64() {
        Some(ms) if (100..=200).contains(&ms) => {}
        other => {
            return Err(format!(
                "淡入应在 100–200ms 内完成（动画 160ms）：{other:?}"
            ));
        }
    }
    for p in ["x", "y", "w", "h"] {
        if sum["picture"][p]["changed"] != false {
            return Err(format!(
                "图片本体的 {p} 在淡入过程中不应变化：{}",
                sum["picture"][p]
            ));
        }
    }
    Ok(())
}

// ---------- 交互脚本：预览 ----------

fn png_bytes(w: u32, h: u32, seed: u8) -> Vec<u8> {
    samples::png(&samples::pattern(w, h, seed))
}

/// 预览交互脚本。
pub fn preview(size: (u32, u32), scale: f32) -> Res<(Vec<Value>, Vec<String>)> {
    let work = fixture::scratch_dir("preview-work");
    let other = fixture::scratch_dir("preview-other");
    copy_dir(&fixture::thumbs_root(), &work)?;
    fixture::fresh_dir(&other)?;
    fs::create_dir(work.join("子文件夹"))?;
    fs::write(work.join("13 视频.mp4"), FAKE_VIDEO)?;
    fs::write(work.join("14 云端.png"), png_bytes(64, 48, 5))?;
    offline(&work.join("14 云端.png"))?;
    fs::write(other.join("a红.png"), png_bytes(300, 200, 40))?;
    fs::write(
        other.join("b动图.gif"),
        samples::gif2(&samples::pattern(200, 150, 60)),
    )?;
    fs::write(other.join("c说明.txt"), "不是图片".as_bytes())?;
    let mut r = Rec::new();

    let c = &Ctx::open(
        size,
        scale,
        Opts {
            start: work.clone(),
            split: true,
            ..Opts::default()
        },
    )?;
    // 右面板去“另一个目录”，再回到左面板
    c.special(Key::F6, &[]);
    go(c, &other)?;
    c.special(Key::F6, &[]);
    let kinds = [
        "01", "02", "03", "04", "05", "06", "07", "08", "09", "10", "14",
    ];
    // 列表视图：16 个条目都在可见范围内（网格视图下靠后的条目在窗口外，不在元素树里）
    c.click(&c.button("列表视图")?);
    c.pump();
    let shell_calls0 = c.shell.calls().len();

    // 1. Space 打开：左面板光标所在的图片，解码未完成时显示加载中
    c.click(&c.item_in(0, "03 透明.png")?);
    let cursor_before = c.win.get_cursor_index();
    let panes_before = panes(c);
    space(c);
    let i = c.ctl.preview_info();
    r.step(
        c,
        "空格打开光标所在图片（解码未完成）",
        vec![
            (i.open && c.win.get_preview_visible(), "预览层应打开"),
            (i.pane == 0, "来自左面板"),
            (
                i.index == 2 && i.count == kinds.len(),
                "同目录 11 张图片里的第 3 张",
            ),
            (i.state == "loading" && i.busy, "解码未完成时为加载中"),
            (overlay_open(c), "元素树里有“图片预览”"),
            (overlay_desc(c) == "加载中", "无障碍描述为加载中"),
            (text(c, "预览文件名") == "03 透明.png", "顶栏显示文件名"),
            (text(c, "预览位置") == "3 / 11", "顶栏显示位置"),
            (shown(c) == (0, 0), "还没有图像"),
            (c.ctl.selected_in(0) == ["03 透明.png"], "打开不改变选择"),
            (!c.win.get_dialog_visible(), "不弹对话框"),
            (c.ctl.ws().active_pane() == 0, "活动面板仍是左面板"),
        ],
    );

    // 2. 后台解码完成
    let ran = c.ctl.pump_preview();
    c.advance(16);
    let i = c.ctl.preview_info();
    let (pw, ph) = pic_size(c);
    r.step(
        c,
        "解码完成显示图片（适应窗口，不放大）",
        vec![
            (ran, "有排队的解码请求"),
            (i.state == "ready" && !i.busy, "状态为就绪"),
            (shown(c) == (512, 512), "按原尺寸解码（不是缩略图尺寸）"),
            (
                text(c, "预览图片信息").starts_with("512 × 512 像素"),
                "信息栏写出像素尺寸",
            ),
            (overlay_desc(c) == "已显示", "无障碍描述为已显示"),
            (
                near(pw, 512.0 / scale) && near(ph, 512.0 / scale),
                "小于画面区的图按原大小居中，不放大",
            ),
            (
                button_checked(c, "适应窗口") == Some(true)
                    && button_checked(c, "100%") == Some(false),
                "默认适应窗口",
            ),
            (
                has_button(c, "上一张") && has_button(c, "下一张"),
                "两侧都有切换按钮",
            ),
            (
                !has_button(c, "播放动画") && !has_button(c, "暂停动画"),
                "静态图没有播放按钮",
            ),
        ],
    );

    // 3. → 切到下一张：旧图保持显示，解码完成后换图
    tap(c, Key::RightArrow);
    let i = c.ctl.preview_info();
    let busy_text = text(c, "预览切换状态");
    let still_old = shown(c) == (512, 512);
    c.ctl.pump_preview();
    c.advance(16);
    let j = c.ctl.preview_info();
    r.step(
        c,
        "→ 切换下一张（旧图保持，完成后换图）",
        vec![
            (
                i.index == 3 && i.state == "ready" && i.busy,
                "已指向下一张、新图解码中",
            ),
            (still_old, "新图就绪前旧图保持显示"),
            (busy_text == "正在加载", "提示正在加载"),
            (text(c, "预览位置") == "4 / 11", "位置更新"),
            (text(c, "预览文件名") == "04 动图.gif", "文件名更新"),
            (
                j.state == "ready" && !j.busy && j.animated && j.frames == 2,
                "GIF 解码为两帧并标为动图",
            ),
            (shown(c) == (400, 300), "显示 GIF 第一帧的原尺寸"),
            (
                text(c, "预览图片信息").contains("动图 2 帧"),
                "信息栏标注动图帧数",
            ),
            (j.playing, "默认自动播放（待用户确认，见 docs/preview.md）"),
            (has_button(c, "暂停动画"), "动图有“暂停动画”按钮"),
            (
                c.ctl.selected_in(0) == ["03 透明.png"],
                "切换预览不改变面板选择",
            ),
        ],
    );

    // 4. GIF 播放：模拟时间推进，帧会前进
    let first = shown_pixel(c, 2, 2);
    let advanced = until_frame(c, 1);
    let second = shown_pixel(c, 2, 2);
    let (_, _, running) = c.ctl.preview_resources();
    r.step(
        c,
        "GIF 自动播放（模拟时钟）",
        vec![
            (reddish(first), "第一帧是红色图案"),
            (advanced && bluish(second), "到时间后换成第二帧（纯蓝）"),
            (running, "播放中计时器在运行"),
        ],
    );

    // 5. 暂停／继续
    letter(c, "p");
    let paused = c.ctl.preview_info();
    let f_at_pause = paused.frame;
    c.advance(1000);
    let after = c.ctl.preview_info();
    let (_, _, running) = c.ctl.preview_resources();
    let label_play = has_button(c, "播放动画");
    c.click(&c.button("播放动画")?);
    c.advance(16);
    let resumed = c.ctl.preview_info();
    let moved = until_frame(c, 1 - f_at_pause);
    r.step(
        c,
        "P 暂停、按钮继续动图",
        vec![
            (!paused.playing && label_play, "P 暂停，按钮变为“播放动画”"),
            (after.frame == f_at_pause, "暂停期间帧不变"),
            (!running, "暂停时计时器停止"),
            (
                resumed.playing && has_button(c, "暂停动画"),
                "点按钮继续播放",
            ),
            (moved, "继续后帧再次前进"),
        ],
    );

    // 6. 点击左侧箭头回到上一张；静态图释放动画帧与计时器
    c.click(&c.button("上一张")?);
    c.advance(16);
    c.ctl.pump_preview();
    c.advance(16);
    let i = c.ctl.preview_info();
    let (frames, queued, running) = c.ctl.preview_resources();
    r.step(
        c,
        "点“上一张”回到静态图，释放动画资源",
        vec![
            (i.index == 2 && i.state == "ready", "回到第 3 张"),
            (!i.animated && !i.playing, "不再是动图"),
            (
                frames == 1 && !queued && !running,
                "只持有当前一张图，无计时器、无排队请求",
            ),
        ],
    );

    // 7. Home／End 与首尾不越界；末尾是云占位文件
    tap(c, Key::Home);
    c.ctl.pump_preview();
    c.advance(16);
    let at_first = c.ctl.preview_info();
    let (opened0, _) = c.ctl.preview_counters();
    let before_noop = (c.ctl.preview_info().index, c.ctl.preview_resources());
    tap(c, Key::LeftArrow);
    let noop = (c.ctl.preview_info().index, c.ctl.preview_resources());
    r.step(
        c,
        "Home 到首张，←不越界",
        vec![
            (
                at_first.index == 0 && text(c, "预览位置") == "1 / 11",
                "Home 回到第一张",
            ),
            (
                !has_button(c, "上一张") && has_button(c, "下一张"),
                "首张没有“上一张”",
            ),
            (
                before_noop == noop && !c.ctl.preview_info().busy,
                "到头再按 ← 什么也不发生",
            ),
            (c.ctl.preview_counters().0 == opened0, "没有重新打开"),
        ],
    );
    tap(c, Key::End);
    c.ctl.pump_preview();
    c.advance(16);
    let last = c.ctl.preview_info();
    let reason = text(c, "预览失败原因");
    let status = text(c, "预览状态");
    tap(c, Key::RightArrow);
    r.step(
        c,
        "End 到末张（云占位文件：只说明，不下载）",
        vec![
            (last.index == 10 && last.count == 11, "End 到最后一张"),
            (
                last.state == "failed" && last.failure.as_deref() == Some("cloud"),
                "云端占位文件显示云端失败状态",
            ),
            (
                status == "无法预览" && reason.contains("云端"),
                "界面说明原因，不弹窗",
            ),
            (overlay_desc(c) == "无法预览", "无障碍描述为无法预览"),
            (c.ctl.preview_info().index == 10, "末张再按 → 不越界"),
            (
                has_button(c, "上一张") && !has_button(c, "下一张"),
                "末张没有“下一张”",
            ),
            (
                c.shell.calls().len() == shell_calls0,
                "预览不调用系统缩略图、不读取云文件内容",
            ),
            (
                !has_button(c, "适应窗口") || button_checked(c, "适应窗口").is_some(),
                "失败态按钮仍存在",
            ),
        ],
    );

    // 8. 从失败切回正常图；适应窗口与 100%
    tap(c, Key::LeftArrow);
    let loading = c.ctl.preview_info();
    c.ctl.pump_preview();
    c.advance(16);
    let ok = c.ctl.preview_info();
    let (sw, sh) = stage_size(c);
    let (fw, fh) = pic_size(c);
    letter(c, "1");
    let (ow, oh) = pic_size(c);
    let one = c.ctl.preview_info().one_to_one;
    let one_btn = (button_checked(c, "100%"), button_checked(c, "适应窗口"));
    letter(c, "0");
    let (bw, bh) = pic_size(c);
    letter(c, "1");
    letter(c, "f");
    r.step(
        c,
        "适应窗口与 100%（1／0／F 键）",
        vec![
            (
                loading.state == "loading" && ok.state == "ready" && ok.index == 9,
                "失败态切走时回到加载中，完成后显示 10 慢.webp",
            ),
            (shown(c) == (800, 800), "800×800 原尺寸解码"),
            (
                fw <= sw + 0.5 && fh <= sh + 0.5 && fw < 800.0 / scale - 1.0 && near(fw, fh),
                "适应窗口：缩到画面区内，保持比例",
            ),
            (
                one && near(ow, 800.0 / scale) && near(oh, 800.0 / scale),
                "按 1：实际大小（1 个图像像素 = 1 个物理像素）",
            ),
            (one_btn == (Some(true), Some(false)), "100% 按钮勾选"),
            (near(bw, fw) && near(bh, fh), "按 0 回到适应窗口"),
            (!c.ctl.preview_info().one_to_one, "按 F 同样适应窗口"),
        ],
    );

    // 9. Esc 关闭：选择、光标、焦点、面板状态都不变，资源释放
    tap(c, Key::Escape);
    let i = c.ctl.preview_info();
    r.step(
        c,
        "Esc 关闭，回到原选择与焦点",
        vec![
            (
                !i.open && !c.win.get_preview_visible() && !overlay_open(c),
                "预览层关闭",
            ),
            (c.ctl.selected_in(0) == ["03 透明.png"], "选择不变"),
            (c.win.get_cursor_index() == cursor_before, "光标不变"),
            (panes(c) == panes_before, "两个面板的标签与选择都不变"),
            (
                c.win.get_focused_pane() == 0 && c.win.get_active_pane() == 0,
                "焦点回到左面板",
            ),
            (
                c.ctl.preview_resources() == (0, false, false),
                "关闭后不持有图像、排队请求与计时器",
            ),
            (shown(c) == (0, 0), "预览图像已清空"),
        ],
    );

    // 10. 再次打开仍从原选择开始；空格与按钮也能关闭
    space(c);
    let reopened = c.ctl.preview_info();
    space(c);
    let closed_by_space = !c.ctl.preview_info().open && !overlay_open(c);
    space(c);
    c.click(&c.button("关闭预览")?);
    c.advance(16);
    r.step(
        c,
        "再次打开、空格与“关闭预览”按钮关闭",
        vec![
            (
                reopened.open && reopened.index == 2,
                "再次打开仍从选中的图片开始",
            ),
            (closed_by_space, "空格关闭"),
            (!c.ctl.preview_info().open && !overlay_open(c), "按钮关闭"),
            (c.win.get_focused_pane() == 0, "焦点回到原面板"),
            (c.ctl.selected_in(0) == ["03 透明.png"], "选择仍不变"),
        ],
    );

    // 11. 不能预览的项目：只在状态栏说明
    let mut checks: Vec<(bool, &str)> = Vec::new();
    for (name, want, label) in [
        ("11 说明.txt", "不支持预览", "文本文件"),
        ("子文件夹", "文件夹没有预览", "文件夹"),
        ("13 视频.mp4", "视频不在应用内播放", "视频"),
        ("12 不支持.tif", "不支持预览", "TIFF"),
    ] {
        c.click(&c.item_in(0, name)?);
        space(c);
        let ok = !c.ctl.preview_info().open && !overlay_open(c) && msg(c).contains(want);
        checks.push((ok, label));
    }
    r.step(
        c,
        "不能预览的项目只给出状态栏说明",
        checks
            .into_iter()
            .map(|(ok, l)| {
                (
                    ok,
                    match l {
                        "文本文件" => "文本文件：不打开预览并提示不支持",
                        "文件夹" => "文件夹：提示按 Enter 进入",
                        "视频" => "视频：提示用默认应用打开，不内置播放",
                        _ => "TIFF：只有系统缩略图，不进应用内预览",
                    },
                )
            })
            .collect(),
    );

    // 12. 双击仍用默认应用打开
    c.double_click(&c.item_in(0, "01 风景.webp")?);
    r.step(
        c,
        "双击图片仍用默认应用打开",
        vec![
            (
                c.opened
                    .borrow()
                    .last()
                    .is_some_and(|p| p.ends_with("01 风景.webp")),
                "双击交给默认应用",
            ),
            (!c.ctl.preview_info().open, "双击不打开预览"),
        ],
    );

    // 13. 预览期间键盘与鼠标不落到下面的界面
    c.click(&c.item_in(0, "02 横拍竖看.jpg")?);
    let opened_n = c.opened.borrow().len();
    let loc = c.ctl.location_in(0);
    let right_before = panes(c)[1].clone();
    space(c);
    c.ctl.pump_preview();
    c.advance(16);
    tap(c, Key::Delete);
    let del_dialog = c.win.get_dialog_visible();
    tap(c, Key::Return);
    tap(c, Key::DownArrow);
    tap(c, Key::UpArrow);
    tap(c, Key::Tab);
    tap(c, Key::F6);
    letter(c, "a");
    c.ctrl("a");
    c.advance(16);
    c.click_at(180.0, 330.0);
    c.advance(16);
    let i = c.ctl.preview_info();
    r.step(
        c,
        "预览期间键盘与鼠标不落到下面的界面",
        vec![
            (i.open && i.index == 1, "预览仍打开，仍在第 2 张"),
            (
                !del_dialog && !c.win.get_dialog_visible(),
                "Delete 不会弹出删除确认",
            ),
            (c.opened.borrow().len() == opened_n, "回车不会打开文件"),
            (
                c.ctl.selected_in(0) == ["02 横拍竖看.jpg"],
                "面板选择不变（方向键、Ctrl+A、点击都被吞掉）",
            ),
            (c.ctl.location_in(0) == loc, "位置不变"),
            (c.ctl.ws().active_pane() == 0, "F6 不切换面板"),
            (panes(c)[1] == right_before, "右面板状态不变"),
        ],
    );
    tap(c, Key::Escape);

    // 14. 快速开关不残留；过期结果被丢弃
    let (o0, d0) = c.ctl.preview_counters();
    for _ in 0..30 {
        space(c);
        tap(c, Key::Escape);
    }
    let (o1, _) = c.ctl.preview_counters();
    r.step(
        c,
        "快速开关 30 次不残留",
        vec![
            (o1 == o0 + 30, "每次空格都打开了（焦点每次都回到面板）"),
            (
                !c.ctl.preview_info().open && !overlay_open(c),
                "最终是关闭的",
            ),
            (
                c.ctl.preview_resources() == (0, false, false),
                "无图像、无排队请求、无计时器",
            ),
            (c.ctl.selected_in(0) == ["02 横拍竖看.jpg"], "选择不变"),
        ],
    );
    space(c);
    let a = c.ctl.take_pending_preview();
    tap(c, Key::RightArrow);
    if let Some((g, p)) = a.clone() {
        c.ctl.deliver_preview(g, p);
    }
    let stale = c.ctl.preview_counters().1;
    let info = c.ctl.preview_info();
    let blank = shown(c) == (0, 0);
    c.ctl.pump_preview();
    c.advance(16);
    let latest = c.ctl.preview_info();
    let latest_shown = !shown_is_blank(c);
    tap(c, Key::RightArrow);
    let b = c.ctl.take_pending_preview();
    tap(c, Key::Escape);
    if let Some((g, p)) = b.clone() {
        c.ctl.deliver_preview(g, p);
    }
    let d2 = c.ctl.preview_counters().1;
    r.step(
        c,
        "过期结果被丢弃，不贴到新图或已关闭的预览上",
        vec![
            (a.is_some() && b.is_some(), "两次都有在途请求"),
            (stale == d0 + 1, "切换后迟到的旧结果被丢弃并计数"),
            (info.state == "loading" && blank, "旧结果没有显示出来"),
            (
                latest.state == "ready" && latest.index == 2 && latest_shown,
                "最新请求的结果正常显示（第 3 张）",
            ),
            (d2 == d0 + 2, "关闭后迟到的结果也被丢弃"),
            (
                !c.ctl.preview_info().open && !overlay_open(c),
                "关闭的预览不会被迟到结果重新打开",
            ),
            (c.ctl.preview_resources() == (0, false, false), "无残留"),
        ],
    );

    // 15. 失败状态：损坏、超大
    c.click(&c.item_in(0, "08 损坏.webp")?);
    space(c);
    c.ctl.pump_preview();
    c.advance(16);
    let bad = c.ctl.preview_info();
    let bad_reason = text(c, "预览失败原因");
    tap(c, Key::RightArrow);
    c.ctl.pump_preview();
    c.advance(16);
    let huge = c.ctl.preview_info();
    let huge_reason = text(c, "预览失败原因");
    let t0 = std::time::Instant::now();
    tap(c, Key::RightArrow);
    c.ctl.pump_preview();
    c.advance(16);
    let recovered = c.ctl.preview_info();
    r.step(
        c,
        "损坏与超大图片：明确的失败状态，可继续切换",
        vec![
            (
                bad.state == "failed" && bad.failure.as_deref() == Some("corrupt"),
                "损坏的 WebP 为损坏",
            ),
            (bad_reason.contains("损坏"), "说明写明损坏"),
            (
                huge.state == "failed" && huge.failure.as_deref() == Some("too_large"),
                "30000×30000 为过大",
            ),
            (
                huge_reason.contains("30000 × 30000") && huge_reason.contains("没有加载"),
                "说明写出尺寸并说明没有加载",
            ),
            (
                recovered.state == "ready" && recovered.index == 9,
                "失败后可继续切到正常图片",
            ),
            (t0.elapsed().as_secs() < 5, "切换不被失败拖住"),
            (!c.win.get_dialog_visible(), "失败不弹对话框"),
        ],
    );
    tap(c, Key::Escape);

    // 16. 预览打开期间文件被删除：缺失状态
    c.click(&c.item_in(0, "01 风景.webp")?);
    space(c);
    c.ctl.pump_preview();
    c.advance(16);
    fs::remove_file(work.join("02 横拍竖看.jpg"))?;
    tap(c, Key::RightArrow);
    c.ctl.pump_preview();
    c.advance(16);
    let gone = c.ctl.preview_info();
    let gone_reason = text(c, "预览失败原因");
    tap(c, Key::RightArrow);
    c.ctl.pump_preview();
    c.advance(16);
    let next = c.ctl.preview_info();
    r.step(
        c,
        "预览中文件被删除：缺失状态，不卡住",
        vec![
            (
                gone.state == "failed" && gone.failure.as_deref() == Some("missing"),
                "已删除的文件显示缺失",
            ),
            (gone_reason.contains("不存在"), "说明写明文件已不存在"),
            (
                next.state == "ready" && next.index == 2,
                "继续切到下一张正常",
            ),
        ],
    );
    tap(c, Key::Escape);

    // 17. 右面板预览；另一面板及其标签不受影响
    if c.ctl.ws().active_pane() != 0 {
        c.special(Key::F6, &[]);
    }
    c.ctrl("t");
    c.pump();
    let left_tabs = c.ctl.ws().pane(0).len();
    c.special(Key::F6, &[]);
    c.click(&c.item_in(1, "a红.png")?);
    let left_before = panes(c)[0].clone();
    let right_sel = c.ctl.selected_in(1);
    space(c);
    c.ctl.pump_preview();
    c.advance(16);
    let i = c.ctl.preview_info();
    let in_other = i.path.as_deref().is_some_and(|p| p.starts_with(&other));
    tap(c, Key::RightArrow);
    c.ctl.pump_preview();
    c.advance(16);
    let g = c.ctl.preview_info();
    tap(c, Key::Escape);
    r.step(
        c,
        "右面板预览：只含右面板目录的图片，左面板与其标签不受影响",
        vec![
            (left_tabs == 2, "左面板有两个标签（测试前提）"),
            (
                i.pane == 1 && i.count == 2 && i.index == 0 && in_other,
                "从右面板的 a红.png 开始，列表只有该目录的 2 张图",
            ),
            (
                g.animated && g.frames == 2 && file_name_is(&g.path, "b动图.gif"),
                "→ 切到右面板目录的 GIF",
            ),
            (panes(c)[0] == left_before, "左面板（含其标签）状态完全不变"),
            (c.ctl.selected_in(1) == right_sel, "右面板选择不变"),
            (
                c.win.get_focused_pane() == 1 && c.win.get_active_pane() == 1,
                "焦点回到右面板",
            ),
            (
                c.ctl.preview_resources() == (0, false, false),
                "关闭后无残留",
            ),
        ],
    );

    // 18. 回收站里不能预览
    c.special(Key::F6, &[]);
    c.click(&c.button("回收站")?);
    c.pump();
    space(c);
    r.step(
        c,
        "回收站里的项目不能预览",
        vec![
            (!c.ctl.preview_info().open && !overlay_open(c), "不打开预览"),
            (msg(c).contains("回收站"), "状态栏提示先还原再预览"),
        ],
    );

    // 19. 设置 preview_gif_autoplay=false：GIF 默认停在第一帧，可手动播放
    {
        let dir = fixture::fresh_dir(&fixture::scratch_dir("preview-settings"))?;
        let settings = dir.join("settings.json");
        fs::write(&settings, "{\"preview_gif_autoplay\": false}")?;
        let c = &Ctx::open(
            size,
            scale,
            Opts {
                start: fixture::thumbs_root(),
                settings_file: Some(settings),
                ..Opts::default()
            },
        )?;
        c.click(&c.item(THUMB_FILES[3])?);
        space(c);
        c.ctl.pump_preview();
        c.advance(16);
        let i = c.ctl.preview_info();
        c.advance(1000);
        let still = c.ctl.preview_info();
        let (_, _, running) = c.ctl.preview_resources();
        let first = reddish(shown_pixel(c, 2, 2));
        letter(c, "p");
        let moved = until_frame(c, 1);
        r.step(
            c,
            "设置 preview_gif_autoplay=false：停在第一帧，P 手动播放",
            vec![
                (i.animated && !i.playing, "动图但不自动播放"),
                (
                    still.frame == 0 && first && !running,
                    "1 秒后仍是第一帧，计时器未启动",
                ),
                (has_button(c, "暂停动画"), "按 P 后按钮变为“暂停动画”"),
                (moved, "手动播放后帧前进"),
            ],
        );
    }
    Ok((r.steps, r.fails))
}

fn file_name_is(p: &Option<PathBuf>, name: &str) -> bool {
    p.as_ref()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == name)
}

// ---------- 交互脚本：系统缩略图 ----------

/// 系统缩略图（视频等）的调度与缓存：确定性替身 Shell，记录每次调用。
pub fn shell(size: (u32, u32), scale: f32) -> Res<(Vec<Value>, Vec<String>)> {
    let work = fixture::fresh_dir(&fixture::scratch_dir("shell-work"))?;
    let cache = fixture::scratch_dir("shell-cache");
    let _ = fs::remove_dir_all(&cache);
    let a = "A 视频.mp4";
    let b = "B 视频.mkv";
    let cc = "C 云端缓存命中.mp4";
    let d = "D 云端.mp4";
    let e = "E 云端.png";
    let f = "F 普通.png";
    let mut video = FAKE_VIDEO.to_vec();
    video.extend_from_slice(&[0u8; 64]);
    fs::write(work.join(a), &video)?;
    fs::write(work.join(b), b"not a video the fake can decode")?;
    fs::write(work.join(cc), &video)?;
    fs::write(work.join(d), &video)?;
    fs::write(work.join(e), png_bytes(64, 48, 9))?;
    fs::write(work.join(f), png_bytes(64, 48, 9))?;
    fs::write(work.join("G 说明.txt"), "文本".as_bytes())?;
    for n in [cc, d, e] {
        offline(&work.join(n))?;
    }
    let mut r = Rec::new();
    let open = |size, scale| {
        Ctx::open(
            size,
            scale,
            Opts {
                thumb_cache: Some(cache.clone()),
                ..Opts::default()
            },
        )
    };
    let states =
        |c: &Ctx| -> Vec<String> { [a, b, cc, d, e, f].iter().map(|n| desc(c, n)).collect() };
    let calls_of = |c: &Ctx, n: &str| -> Vec<bool> {
        c.shell
            .calls()
            .into_iter()
            .filter(|(p, _)| p.ends_with(n))
            .map(|(_, only)| only)
            .collect()
    };

    {
        let c = &open(size, scale)?;
        c.hold_thumbs.set(true);
        let calls0 = c.shell.calls().len(); // 起始目录（样例目录）里的视频已请求过
        go(c, &work)?;
        let before = states(c);
        r.step(
            c,
            "进入目录（系统缩略图请求未执行）",
            vec![
                (
                    before.iter().all(|s| s == LOADING),
                    "视频、云占位与图片都先显示加载中",
                ),
                (desc(c, "G 说明.txt").is_empty(), "文本没有缩略图状态"),
                (c.shell.calls().len() == calls0, "此时没有调用系统缩略图"),
            ],
        );
        c.hold_thumbs.set(false);
        c.pump();
        let s = states(c);
        let k = c.ctl.thumbs().counters();
        r.step(
            c,
            "视频走系统缩略图，云占位只查系统缓存",
            vec![
                (s[0] == READY, "A：系统给出缩略图"),
                (s[1] == FAILED, "B：系统取不到 → 回退图标"),
                (s[2] == READY, "C：云占位但系统缓存已有 → 显示"),
                (s[3] == FAILED, "D：云占位且系统缓存没有 → 回退图标，不下载"),
                (
                    s[4] == FAILED,
                    "E：云端 PNG 不读取内容，只查系统缓存 → 回退图标",
                ),
                (s[5] == READY, "F：普通 PNG 走原生解码"),
                (
                    calls_of(c, a) == [false] && calls_of(c, b) == [false],
                    "本地视频整体提取一次",
                ),
                (
                    calls_of(c, cc) == [true]
                        && calls_of(c, d) == [true]
                        && calls_of(c, e) == [true],
                    "云占位文件只做“仅缓存”查询，从不触发提取或下载",
                ),
                (calls_of(c, f).is_empty(), "原生图片不经过系统缩略图"),
                (k.shell_ok == 2, "两张来自系统缩略图（A、C）"),
                (k.shell_handoff >= 1, "云端 PNG 由原生判定后转交 Shell 车道"),
            ],
        );
    }
    // 重启：系统缩略图成功的写入了持久缓存，失败的不写
    {
        let c = &open(size, scale)?;
        go(c, &work)?;
        let s = states(c);
        let k = c.ctl.thumbs().counters();
        r.step(
            c,
            "重启后系统缩略图从磁盘缓存读取",
            vec![
                (
                    s[0] == READY && s[2] == READY && s[5] == READY,
                    "A、C、F 直接来自磁盘缓存",
                ),
                (
                    calls_of(c, a).is_empty() && calls_of(c, cc).is_empty(),
                    "命中缓存的不再调用系统",
                ),
                (k.from_disk == 3, "三张从磁盘读取"),
                (
                    calls_of(c, b) == [false] && calls_of(c, d) == [true],
                    "失败的没有缓存，重新查询（云占位仍只查缓存）",
                ),
            ],
        );
        // 文件被改动后重新提取
        fs::write(work.join(a), &video[..video.len() - 8])?;
        fs::OpenOptions::new()
            .write(true)
            .open(work.join(a))?
            .set_modified(fixture::time(300))?;
        c.click(&c.button("刷新")?);
        c.pump();
        r.step(
            c,
            "文件被修改后不显示旧缩略图",
            vec![
                (desc(c, a) == READY, "修改后的视频重新提取并显示"),
                (calls_of(c, a) == [false], "只多调用一次系统缩略图"),
            ],
        );
    }
    Ok((r.steps, r.fails))
}
