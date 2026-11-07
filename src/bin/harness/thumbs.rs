//! 缩略图（Issue #10）：截图场景的准备步骤与交互脚本。
//!
//! 交互脚本在缩略图样例的草稿副本上运行（`%TEMP%\sfm-harness\scratch\thumbs-*`，每次重建），
//! 持久缓存也在草稿目录，不接触用户真实目录与真实缓存。覆盖：加载中可选择与打开、换目录作废旧请求且
//! 不贴错结果、各格式内容（方向、透明、GIF 第一帧、按内容识别）、异常文件回退图标、慢文件超时后补图、
//! 磁盘缓存写入与重启复用、文件修改／改名／删除后不显示旧缩略图、“更多”菜单的占用、容量与清空。

use crate::ctx::{Ctx, Opts, Res};
use crate::dual::Rec;
use crate::fixture::{self, THUMB_FILES};
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use serde_json::Value;
use slint::Model;
use slint_file_manager::thumbs::decode::samples;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

pub const LOADING: &str = "缩略图加载中";
pub const READY: &str = "缩略图已显示";
pub const FAILED: &str = "无法生成缩略图";
pub const TIMEOUT: &str = "缩略图超时";

/// 样例里可生成缩略图的文件（01–07 成功，08、09 失败，10 慢）。
const IMAGES: usize = 10;

/// “假视频”魔数：内容以它开头的文件，替身 Shell 返回一张 256×144 的测试图案；其余一律“不支持”。
pub const FAKE_VIDEO: &[u8] = b"FAKEVID";
/// 仅缓存查询时，路径含此标记视为“系统缓存里已有缩略图”。
pub const CACHE_HIT: &str = "缓存命中";

/// 系统缩略图替身：确定性、不依赖本机解码器，并记录每次调用的 `(路径, cache_only)`，
/// 供断言“云占位只走仅缓存、不隐式下载”。真实 Shell 的兼容性见 `video-compat`。
#[derive(Default)]
pub struct FakeShell {
    pub calls: std::sync::Mutex<Vec<(PathBuf, bool)>>,
}

impl FakeShell {
    pub fn calls(&self) -> Vec<(PathBuf, bool)> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl slint_file_manager::thumbs::shell::ShellProvider for FakeShell {
    fn thumbnail(
        &self,
        path: &Path,
        cache_only: bool,
    ) -> Result<slint_file_manager::thumbs::Thumb, slint_file_manager::thumbs::Fail> {
        use slint_file_manager::thumbs::Fail;
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((path.to_path_buf(), cache_only));
        if cache_only {
            // 仅缓存（云占位）：不读文件内容；约定名字含“命中”的才算系统缓存里已有
            return if path.to_string_lossy().contains(CACHE_HIT) {
                Ok(samples::pattern(256, 144, 77))
            } else {
                Err(Fail::Unsupported)
            };
        }
        let head = fs::read(path).map_err(|_| Fail::Io)?;
        if head.starts_with(FAKE_VIDEO) {
            Ok(samples::pattern(256, 144, 33))
        } else {
            Err(Fail::Unsupported)
        }
    }
}

/// 文件项的无障碍描述（缩略图状态）。
pub fn desc(c: &Ctx, name: &str) -> String {
    let full = c
        .item(name)
        .ok()
        .and_then(|e| e.accessible_description())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "<找不到>".into());
    // 描述末尾可能带“云盘：…”（Issue #14），缩略图状态只看前半
    full.split("云盘：")
        .next()
        .unwrap_or("")
        .trim_end_matches('；')
        .to_string()
}

fn states(c: &Ctx) -> Vec<String> {
    THUMB_FILES.iter().map(|n| desc(c, n)).collect()
}

/// 进入目录（地址栏）并执行读取。
fn go(c: &Ctx, dir: &Path) -> Res<()> {
    c.enter_address(&dir.to_string_lossy())?;
    c.pump();
    Ok(())
}

// ---------- 截图场景 ----------

/// 进入缩略图样例但不执行缩略图请求：图片显示加载中的占位。
pub fn scene_loading(c: &Ctx) -> Res<()> {
    c.hold_thumbs.set(true);
    go(c, &fixture::thumbs_root())?;
    c.click(&c.item(THUMB_FILES[0])?);
    Ok(())
}

/// 全部完成（慢文件已补图），选中横拍竖看的 JPEG。
pub fn scene_ready(c: &Ctx) -> Res<()> {
    go(c, &fixture::thumbs_root())?;
    c.ctl.finish_slow_thumbs();
    c.advance(16);
    c.click(&c.item(THUMB_FILES[1])?);
    Ok(())
}

/// 慢文件超时（显示图标）、损坏与超大文件失败（显示图标），选中损坏文件。
pub fn scene_fallback(c: &Ctx) -> Res<()> {
    go(c, &fixture::thumbs_root())?;
    c.click(&c.item(THUMB_FILES[7])?);
    Ok(())
}

/// 生成完成（含慢文件补图、写入缓存）后打开“更多”菜单。
pub fn scene_cache_menu(c: &Ctx) -> Res<()> {
    go(c, &fixture::thumbs_root())?;
    c.ctl.finish_slow_thumbs();
    c.click(&c.button("更多")?);
    c.advance(16);
    Ok(())
}

// ---------- 交互脚本 ----------

pub fn copy_dir(from: &Path, to: &Path) -> Res<()> {
    fixture::fresh_dir(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        let dst = to.join(e.file_name());
        fs::copy(e.path(), &dst)?;
        // fs::copy 保留修改时间；显式再设一次，保证缓存键在两次运行间一致
        let m = e.metadata()?.modified()?;
        fs::OpenOptions::new()
            .write(true)
            .open(&dst)?
            .set_modified(m)?;
    }
    Ok(())
}

/// 左面板模型里某个文件的缩略图图像。
fn thumb_of(c: &Ctx, name: &str) -> Option<slint::Image> {
    let m = c.win.get_left_items();
    (0..m.row_count())
        .filter_map(|i| m.row_data(i))
        .find(|it| it.name == name)
        .map(|it| it.thumb)
}

fn pixel(img: &slint::Image, x: u32, y: u32) -> Option<[u8; 4]> {
    let b = img.to_rgba8()?;
    if x >= b.width() || y >= b.height() {
        return None;
    }
    let p = b.as_slice()[(y * b.width() + x) as usize];
    Some([p.r, p.g, p.b, p.a])
}

fn size_of(img: &Option<slint::Image>) -> (u32, u32) {
    img.as_ref().map_or((0, 0), |i| {
        let s = i.size();
        (s.width, s.height)
    })
}

fn reddish(p: Option<[u8; 4]>) -> bool {
    p.is_some_and(|p| p[0] > 180 && p[1] < 90 && p[2] < 90)
}

fn more_menu_row(c: &Ctx, label: &str) -> Res<ElementHandle> {
    c.find(AccessibleRole::Button, label)
}

fn open_thumbs(size: (u32, u32), scale: f32, cache: &Path) -> Res<Ctx> {
    Ctx::open(
        size,
        scale,
        Opts {
            thumb_cache: Some(cache.to_path_buf()),
            ..Opts::default()
        },
    )
}

fn disk_files(c: &Ctx) -> u64 {
    c.ctl.thumbs().disk().map_or(0, |d| d.usage().files)
}

/// 缩略图交互脚本。
pub fn thumbs(size: (u32, u32), scale: f32) -> Res<(Vec<Value>, Vec<String>)> {
    let work = fixture::scratch_dir("thumbs-work");
    let cache = fixture::scratch_dir("thumbs-cache");
    copy_dir(&fixture::thumbs_root(), &work)?;
    let _ = fs::remove_dir_all(&cache);
    let mut r = Rec::new();

    {
        let c = &open_thumbs(size, scale, &cache)?;
        // 1. 进入目录，缩略图请求尚未执行
        c.hold_thumbs.set(true);
        go(c, &work)?;
        let s = states(c);
        r.step(
            c,
            "进入缩略图样例（请求未执行）",
            vec![
                (
                    s[..IMAGES].iter().all(|d| d == LOADING),
                    "10 个图片文件都应为加载中",
                ),
                (s[10].is_empty(), "文本没有缩略图状态"),
                (
                    s[11] == LOADING,
                    "TIFF 交给系统缩略图，请求前同样显示加载中",
                ),
                (
                    c.ctl.thumbs().pending_manual() > IMAGES,
                    "可见图片与 TIFF 应已排队",
                ),
            ],
        );

        // 2. 加载中照常选择与打开
        c.click(&c.item(THUMB_FILES[0])?);
        c.double_click(&c.item(THUMB_FILES[2])?);
        r.step(
            c,
            "加载中可选择与打开",
            vec![
                (c.ctl.selected_names() == [THUMB_FILES[2]], "双击项被选中"),
                (
                    c.opened
                        .borrow()
                        .last()
                        .is_some_and(|p| p.ends_with(THUMB_FILES[2])),
                    "双击图片交给默认应用打开",
                ),
            ],
        );

        // 3. 结果出来之前换目录：旧请求作废，新目录的结果不会贴到旧文件上
        let before = c.ctl.thumbs().counters();
        go(c, &fixture::root())?;
        let after = c.ctl.thumbs().counters();
        c.hold_thumbs.set(false);
        c.pump();
        let placeholder = desc(c, "图片001.webp");
        r.step(
            c,
            "换目录作废旧请求",
            vec![
                (
                    after.cancelled_queued >= before.cancelled_queued + IMAGES as u64,
                    "旧目录排队中的请求应全部作废",
                ),
                (after.started == before.started, "作废的请求不应开始"),
                (
                    placeholder == FAILED,
                    "样例目录的占位 WebP（全零内容）应回退图标",
                ),
            ],
        );

        // 4. 回到缩略图样例并执行
        c.hold_thumbs.set(true);
        c.click(&c.button("后退")?);
        c.pump();
        c.click(&c.item(THUMB_FILES[2])?);
        c.hold_thumbs.set(false);
        c.pump();
        let s = states(c);
        let jpg = thumb_of(c, THUMB_FILES[1]);
        let png = thumb_of(c, THUMB_FILES[2]);
        let gif = thumb_of(c, THUMB_FILES[3]);
        let sniff = thumb_of(c, THUMB_FILES[6]);
        let (jw, jh) = size_of(&jpg);
        r.step(
            c,
            "生成结果与回退",
            vec![
                (s[..7].iter().all(|d| d == READY), "01–07 应显示缩略图"),
                (
                    s[7] == FAILED && s[8] == FAILED,
                    "损坏与超大尺寸文件回退图标",
                ),
                (s[9] == TIMEOUT, "慢文件超时后显示图标"),
                (s[10].is_empty(), "文本保持无状态"),
                (
                    s[11] == FAILED,
                    "系统也取不到缩略图的 TIFF 回退图标（替身 Shell 判不支持）",
                ),
                (
                    size_of(&thumb_of(c, THUMB_FILES[0])) == (256, 171),
                    "WebP 1200×800 缩到 256×171",
                ),
                ((jw, jh) == (171, 256), "EXIF 方向 6 的 JPEG 显示为竖图"),
                (
                    jpg.as_ref()
                        .is_some_and(|i| reddish(pixel(i, jw - 8, 8)) && !reddish(pixel(i, 8, 8))),
                    "方向校正后红块在右上角",
                ),
                (
                    png.as_ref()
                        .and_then(|i| pixel(i, 128, 128))
                        .is_some_and(|p| p[3] == 0),
                    "PNG 透明区域保持透明",
                ),
                (
                    gif.as_ref().is_some_and(|i| reddish(pixel(i, 4, 4))),
                    "GIF 取第一帧（第二帧为纯蓝）",
                ),
                (
                    size_of(&sniff) == (256, 171),
                    "扩展名为 .jpg 的 PNG 按内容识别",
                ),
                (
                    c.ctl.selected_names() == [THUMB_FILES[2]],
                    "生成结果不改变选择",
                ),
            ],
        );

        // 5. 慢文件迟到补图
        let n = c.ctl.finish_slow_thumbs();
        c.advance(16);
        r.step(
            c,
            "慢文件迟到补图",
            vec![
                (n == 1, "应补上 1 张"),
                (desc(c, THUMB_FILES[9]) == READY, "慢文件最终显示缩略图"),
                (
                    c.ctl.thumbs().counters().late_ready == 1,
                    "计数：迟到补图 1",
                ),
            ],
        );

        // 6. 持久缓存
        r.step(
            c,
            "成功的缩略图写入磁盘缓存",
            vec![(
                disk_files(c) == 8,
                "01–07 与慢文件共 8 个缓存条目，失败的不写",
            )],
        );
    }

    // 7. 重启：新窗口使用同一缓存目录
    {
        let c = &open_thumbs(size, scale, &cache)?;
        go(c, &work)?;
        let k = c.ctl.thumbs().counters();
        r.step(
            c,
            "重启后从磁盘缓存读取",
            vec![
                (k.from_disk == 7, "01–07 从磁盘缓存读取"),
                (k.decoded == 0, "没有重新解码成功的图片"),
                (
                    states(c)[..7].iter().all(|d| d == READY),
                    "01–07 显示缩略图",
                ),
            ],
        );

        // 8. 修改、改名、删除
        let t_new = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let edited = work.join(THUMB_FILES[0]);
        fs::write(
            &edited,
            samples::webp_lossy(&samples::pattern(600, 600, 10)),
        )?;
        fs::OpenOptions::new()
            .write(true)
            .open(&edited)?
            .set_modified(t_new)?;
        let renamed = "05 改名后.bmp";
        fs::rename(work.join(THUMB_FILES[4]), work.join(renamed))?;
        fs::remove_file(work.join(THUMB_FILES[5]))?;
        let before = c.ctl.thumbs().counters();
        c.click(&c.button("刷新")?);
        c.pump();
        let after = c.ctl.thumbs().counters();
        r.step(
            c,
            "修改、改名、删除后不显示旧缩略图",
            vec![
                (
                    size_of(&thumb_of(c, THUMB_FILES[0])) == (256, 256),
                    "修改后的文件重新生成（新尺寸 600×600 → 256×256）",
                ),
                (desc(c, renamed) == READY, "改名后的文件显示缩略图"),
                (
                    after.decoded == before.decoded + 2,
                    "修改与改名的文件各重新解码一次，其余来自缓存",
                ),
                (c.item(THUMB_FILES[5]).is_err(), "删除的文件不再出现"),
            ],
        );

        // 9. “更多”菜单：占用与清空
        c.click(&c.button("更多")?);
        c.advance(16);
        let usage = c.win.get_thumb_cache_usage().to_string();
        let files_before = disk_files(c);
        r.step(
            c,
            "更多菜单显示缓存占用",
            vec![
                (usage.starts_with("已用 "), "占用说明以“已用”开头"),
                (
                    usage.contains(&format!("（{files_before} 张）")),
                    "占用说明含条目数",
                ),
                (usage.ends_with("上限 512 MB"), "默认上限 512 MB"),
            ],
        );
        c.click(&more_menu_row(c, "清空缩略图缓存")?);
        c.hold_thumbs.set(true);
        c.pump();
        let s = states(c);
        let msg = c.win.get_status_message().to_string();
        r.step(
            c,
            "清空缓存后重新生成",
            vec![
                (disk_files(c) == 0, "磁盘缓存条目清零"),
                (msg.starts_with("已清空缩略图缓存"), "状态栏提示已清空"),
                (s[0] == LOADING, "可见图片回到加载中"),
                (
                    PathBuf::from(&work).exists() && work.join(THUMB_FILES[1]).exists(),
                    "原图不受影响",
                ),
            ],
        );
        c.hold_thumbs.set(false);
        let before = c.ctl.thumbs().counters();
        c.pump();
        let after = c.ctl.thumbs().counters();
        r.step(
            c,
            "清空后再次生成并写回缓存",
            vec![
                (states(c)[0] == READY, "重新显示缩略图"),
                (after.from_disk == before.from_disk, "清空后没有磁盘命中"),
                (disk_files(c) > 0, "重新写入缓存"),
            ],
        );

        // 10. 容量上限
        c.click(&c.button("更多")?);
        c.advance(16);
        c.click(&more_menu_row(c, "256 MB")?);
        c.advance(16);
        r.step(
            c,
            "设置缓存容量上限",
            vec![
                (
                    c.win.get_thumb_cache_capacity_mb() == 256,
                    "界面显示 256 MB",
                ),
                (
                    c.ctl
                        .thumbs()
                        .disk()
                        .is_some_and(|d| d.capacity() == 256 << 20),
                    "磁盘缓存容量改为 256 MiB",
                ),
            ],
        );
    }
    Ok((r.steps, r.fails))
}
