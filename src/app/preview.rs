//! 大图预览的界面接线（Issue #13）：Space 打开、←/→ 切换、Esc／Space／按钮关闭。设计与取舍见 docs/preview.md。
//!
//! - **从哪里开始**：活动面板里“光标所在项”（没有光标时取第一个选中项）。预览的图片列表是此刻该标签里全部可预览图片
//!   的快照（当前排序顺序），之后目录变化、另一面板切换标签都不影响预览。
//! - **不碰选择**：预览期间不改变任何标签的选择、光标与排序；关闭后回到打开前的选择，键盘焦点回到原面板。
//! - **不阻塞**：解码在后台线程（[`crate::preview::loader::Loader`]，只处理最新请求），界面线程只包装结果；
//!   切换时旧图保持显示、右下角提示“正在加载”；过期的结果（代号不是最新）直接丢弃。
//! - **资源**：同一时刻只持有当前一张图（动图则是它的全部帧）；关闭、切换都会释放上一张；关闭时代号加一，
//!   晚到的结果被丢弃，快速开关不残留图像、计时器或线程中的积压请求。
//! - **GIF**：默认值与取舍见 docs/preview.md；设置 `preview_gif_autoplay`（`settings.json`，缺省为播放），预览里可随时暂停／播放。
//! - **harness**：[`Loader::Manual`] 下解码请求排队，由 [`Controller::pump_preview`] 同步执行，从而能确定地截到“加载中”。

use super::*;
use crate::PreviewState;
use crate::preview::loader::{self, Outcome, Prepared};
use crate::preview::{self, Failure, Limits};
use std::path::Path;

/// 预览会话：打开后到关闭前的全部状态（关闭即丢弃）。
struct Session {
    pane: usize,
    files: Vec<PathBuf>,
    index: usize,
    /// 当前请求代号：只有代号匹配的结果才会显示
    gen_: u64,
    one_to_one: bool,
    state: UiState,
    animated: bool,
    playing: bool,
    /// 当前代号的解码结果还没回来
    loading: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum UiState {
    Loading,
    Ready,
    Failed(String),
}

/// 动图帧（界面线程持有；非动图只有一帧）。
#[derive(Default)]
struct Anim {
    frames: Vec<slint::Image>,
    delays_ms: Vec<u32>,
    idx: usize,
}

pub struct PreviewUi {
    session: RefCell<Option<Session>>,
    gen_: Cell<u64>,
    /// 线程模式的后台加载器（首次预览时创建）
    loader: RefCell<Option<loader::Loader>>,
    /// 手动模式（harness）下等待执行的请求
    pending: RefCell<Option<(u64, PathBuf)>>,
    timer: slint::Timer,
    anim: RefCell<Anim>,
    autoplay: bool,
    limits: Limits,
    /// 打开过多少次、解码结果被丢弃（过期）多少次（harness 与测试读取）
    opened: Cell<u64>,
    discarded: Cell<u64>,
}

impl PreviewUi {
    pub fn new(settings_file: Option<&Path>) -> PreviewUi {
        PreviewUi {
            session: RefCell::new(None),
            gen_: Cell::new(0),
            loader: RefCell::new(None),
            pending: RefCell::new(None),
            timer: slint::Timer::default(),
            anim: RefCell::new(Anim::default()),
            autoplay: load_autoplay(settings_file),
            limits: Limits::default(),
            opened: Cell::new(0),
            discarded: Cell::new(0),
        }
    }
}

/// 读取 `settings.json` 的 `preview_gif_autoplay`：缺失或无效时为 true（播放）。
pub fn load_autoplay(file: Option<&Path>) -> bool {
    file.and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("preview_gif_autoplay")?.as_bool())
        .unwrap_or(true)
}

/// harness 与测试读取的预览快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewInfo {
    pub open: bool,
    pub pane: usize,
    pub index: usize,
    pub count: usize,
    pub path: Option<PathBuf>,
    pub state: &'static str,
    pub failure: Option<String>,
    pub one_to_one: bool,
    pub animated: bool,
    pub playing: bool,
    /// 解码请求在排队或进行中
    pub busy: bool,
    /// 当前显示的动图帧序号
    pub frame: usize,
    pub frames: usize,
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

impl Controller {
    // ---------- 打开 ----------

    /// Space：预览活动面板光标所在的图片。不是图片时只在状态栏说明原因，不打开任何界面。
    pub fn preview_open(&self, p: usize) {
        self.activate_pane(p);
        if self.preview.session.borrow().is_some() || self.dialog.borrow().is_some() {
            return;
        }
        let picked = {
            let ws = self.ws.borrow();
            let t = ws.active_tab(p);
            if t.is_recycle_bin() {
                Err("回收站里的项目不能预览：先还原再预览".to_string())
            } else {
                let rows = t.rows();
                let target = t
                    .cursor()
                    .filter(|&i| i < rows.len())
                    .or_else(|| (0..rows.len()).find(|&i| t.is_selected(i)));
                match target {
                    None => Err("请先选择要预览的图片".to_string()),
                    Some(i) => {
                        let e = &rows[i].entry;
                        if e.is_dir {
                            Err("文件夹没有预览，按 Enter 进入".to_string())
                        } else if preview::is_video(&e.ext) {
                            Err("视频不在应用内播放：双击或按 Enter 用默认应用打开".to_string())
                        } else if !preview::is_previewable(&e.ext) {
                            Err(
                                "这种类型不支持预览（只预览 WebP、JPEG、PNG、GIF、BMP 图片）"
                                    .to_string(),
                            )
                        } else {
                            let files: Vec<PathBuf> = (0..rows.len())
                                .filter(|&k| {
                                    !rows[k].entry.is_dir
                                        && preview::is_previewable(&rows[k].entry.ext)
                                })
                                .filter_map(|k| t.path_of(k))
                                .collect();
                            match t.path_of(i) {
                                Some(path) => match files.iter().position(|f| *f == path) {
                                    Some(index) => Ok((files, index)),
                                    None => Err("无法预览这个文件".to_string()),
                                },
                                None => Err("无法预览这个文件".to_string()),
                            }
                        }
                    }
                }
            }
        };
        let (files, index) = match picked {
            Ok(v) => v,
            Err(msg) => {
                self.set_info(&msg);
                return;
            }
        };
        let Some(w) = self.window.upgrade() else {
            return;
        };
        self.preview.opened.set(self.preview.opened.get() + 1);
        let gen_ = self.next_preview_gen();
        let path = files[index].clone();
        *self.preview.session.borrow_mut() = Some(Session {
            pane: p,
            files,
            index,
            gen_,
            one_to_one: false,
            state: UiState::Loading,
            animated: false,
            playing: false,
            loading: true,
        });
        w.set_preview_image(slint::Image::default());
        w.set_preview_info("".into());
        w.set_preview_note("".into());
        w.set_preview_message("".into());
        w.set_preview_one_to_one(false);
        w.set_preview_token(w.get_preview_token() + 1);
        w.set_preview_visible(true);
        self.sync_preview();
        self.request_preview(gen_, path);
    }

    fn next_preview_gen(&self) -> u64 {
        let g = self.preview.gen_.get() + 1;
        self.preview.gen_.set(g);
        g
    }

    /// 提交解码请求：线程模式交给后台加载器，手动模式（harness）排队等 [`Controller::pump_preview`]。
    fn request_preview(&self, gen_: u64, path: PathBuf) {
        match &self.loader {
            Loader::Manual(_) => {
                *self.preview.pending.borrow_mut() = Some((gen_, path));
            }
            Loader::Threaded => {
                let id = self.id;
                let mut slot = self.preview.loader.borrow_mut();
                let l = slot.get_or_insert_with(|| {
                    loader::Loader::new(
                        Arc::new(move |gen_, path, outcome| {
                            post(
                                id,
                                Msg::Preview {
                                    gen_,
                                    path,
                                    outcome,
                                },
                            )
                        }),
                        self.preview.limits,
                    )
                });
                l.request(gen_, path);
            }
        }
    }

    // ---------- 切换与显示模式 ----------

    /// 切换图片：`delta` 为相对步数，越界取首尾（不循环）。到头时什么也不做。
    pub fn preview_step(&self, delta: i32) {
        let (gen_, path) = {
            let mut s = self.preview.session.borrow_mut();
            let Some(s) = s.as_mut() else { return };
            let last = s.files.len() as i64 - 1;
            let target = (s.index as i64 + delta as i64).clamp(0, last) as usize;
            if target == s.index {
                return;
            }
            s.index = target;
            let g = self.preview.gen_.get() + 1;
            self.preview.gen_.set(g);
            s.gen_ = g;
            s.loading = true;
            // 上一张是失败说明时没有图可留着，回到“加载中”；否则旧图保持显示直到新图就绪
            if matches!(s.state, UiState::Failed(_)) {
                s.state = UiState::Loading;
            }
            (g, s.files[target].clone())
        };
        self.sync_preview();
        self.request_preview(gen_, path);
    }

    pub fn preview_set_one_to_one(&self, on: bool) {
        {
            let mut s = self.preview.session.borrow_mut();
            let Some(s) = s.as_mut() else { return };
            if s.state != UiState::Ready {
                return;
            }
            s.one_to_one = on;
        }
        self.sync_preview();
    }

    /// 动图播放／暂停。
    pub fn preview_toggle_play(&self) {
        {
            let mut s = self.preview.session.borrow_mut();
            let Some(s) = s.as_mut() else { return };
            if !s.animated {
                return;
            }
            s.playing = !s.playing;
        }
        self.restart_animation();
        self.sync_preview();
    }

    // ---------- 关闭 ----------

    /// Esc／Space／按钮关闭：释放图像与动画，丢弃排队与迟到的请求，焦点回到原面板。选择不变。
    pub fn preview_close(&self) {
        let Some(s) = self.preview.session.borrow_mut().take() else {
            return;
        };
        self.preview.timer.stop();
        *self.preview.anim.borrow_mut() = Anim::default();
        *self.preview.pending.borrow_mut() = None;
        if let Some(l) = self.preview.loader.borrow().as_ref() {
            l.cancel();
        }
        // 代号加一：已在解码的请求完成后会被当作过期结果丢弃
        self.next_preview_gen();
        if let Some(w) = self.window.upgrade() {
            w.set_preview_visible(false);
            w.set_preview_image(slint::Image::default());
            w.set_preview_busy(false);
            w.set_preview_animated(false);
            w.set_preview_playing(false);
        }
        self.focus_pane(s.pane);
    }

    // ---------- 结果 ----------

    /// 后台解码完成（线程模式经消息，手动模式由 [`Controller::pump_preview`] 调用）。
    pub(super) fn on_preview(&self, gen_: u64, path: PathBuf, outcome: Outcome) {
        let current = self
            .preview
            .session
            .borrow()
            .as_ref()
            .is_some_and(|s| s.gen_ == gen_);
        if !current {
            self.preview.discarded.set(self.preview.discarded.get() + 1);
            return;
        }
        match outcome {
            Ok(prepared) => self.show_prepared(path, prepared),
            Err(f) => self.show_failure(f),
        }
    }

    fn show_prepared(&self, path: PathBuf, p: Prepared) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let animated = p.frames.len() > 1;
        let info = format!(
            "{} × {} 像素 · {}{}",
            p.w,
            p.h,
            thumbs::bytes_text(size),
            if animated {
                format!(" · 动图 {} 帧", p.frames.len())
            } else {
                String::new()
            }
        );
        let frames: Vec<slint::Image> =
            p.frames.into_iter().map(slint::Image::from_rgba8).collect();
        let first = frames[0].clone();
        {
            let mut a = self.preview.anim.borrow_mut();
            *a = Anim {
                frames,
                delays_ms: p.delays_ms,
                idx: 0,
            };
        }
        {
            let mut s = self.preview.session.borrow_mut();
            let Some(s) = s.as_mut() else { return };
            s.state = UiState::Ready;
            s.loading = false;
            s.animated = animated;
            s.playing = animated && self.preview.autoplay;
        }
        w.set_preview_image(first);
        w.set_preview_info(info.into());
        w.set_preview_note(p.note.unwrap_or_default().into());
        self.restart_animation();
        self.sync_preview();
    }

    fn show_failure(&self, f: Failure) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        self.preview.timer.stop();
        *self.preview.anim.borrow_mut() = Anim::default();
        {
            let mut s = self.preview.session.borrow_mut();
            let Some(s) = s.as_mut() else { return };
            s.state = UiState::Failed(f.code().to_string());
            s.loading = false;
            s.animated = false;
            s.playing = false;
        }
        w.set_preview_image(slint::Image::default());
        w.set_preview_info("".into());
        w.set_preview_note("".into());
        w.set_preview_message(f.message().into());
        self.sync_preview();
    }

    // ---------- 动图 ----------

    /// 按当前播放状态（重新）安排下一帧。
    fn restart_animation(&self) {
        self.preview.timer.stop();
        let playing = self
            .preview
            .session
            .borrow()
            .as_ref()
            .is_some_and(|s| s.playing && s.animated);
        if !playing {
            return;
        }
        let delay = {
            let a = self.preview.anim.borrow();
            a.delays_ms.get(a.idx).copied().unwrap_or(100)
        };
        self.arm_frame(delay);
    }

    fn arm_frame(&self, delay_ms: u32) {
        let id = self.id;
        self.preview.timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(delay_ms as u64),
            move || {
                let c = REGISTRY.with(|r| r.borrow().get(&id).and_then(Weak::upgrade));
                if let Some(c) = c {
                    c.next_frame();
                }
            },
        );
    }

    fn next_frame(&self) {
        let playing = self
            .preview
            .session
            .borrow()
            .as_ref()
            .is_some_and(|s| s.playing && s.animated);
        if !playing {
            return;
        }
        let (img, delay) = {
            let mut a = self.preview.anim.borrow_mut();
            if a.frames.len() < 2 {
                return;
            }
            a.idx = (a.idx + 1) % a.frames.len();
            (
                a.frames[a.idx].clone(),
                a.delays_ms.get(a.idx).copied().unwrap_or(100),
            )
        };
        if let Some(w) = self.window.upgrade() {
            w.set_preview_image(img);
        }
        self.arm_frame(delay);
    }

    // ---------- harness 与测试 ----------

    /// 手动模式：同步执行排队的解码请求并写回结果。返回是否执行了请求。
    pub fn pump_preview(&self) -> bool {
        match self.take_pending_preview() {
            Some((gen_, path)) => {
                self.deliver_preview(gen_, path);
                true
            }
            None => false,
        }
    }

    /// 手动模式：取走排队的解码请求但不执行（模拟“解码还没回来”，之后可用 [`Controller::deliver_preview`] 迟到送达）。
    pub fn take_pending_preview(&self) -> Option<(u64, PathBuf)> {
        self.preview.pending.borrow_mut().take()
    }

    /// 手动模式：同步解码并送达指定代号的结果。代号已过期则被丢弃（计入 [`Controller::preview_counters`]）。
    pub fn deliver_preview(&self, gen_: u64, path: PathBuf) {
        let out = loader::run(&path, &self.preview.limits);
        self.on_preview(gen_, path, out);
    }

    /// 预览是否打开。
    pub fn preview_open_now(&self) -> bool {
        self.preview.session.borrow().is_some()
    }

    /// 预览的当前状态快照。
    pub fn preview_info(&self) -> PreviewInfo {
        let s = self.preview.session.borrow();
        let a = self.preview.anim.borrow();
        match s.as_ref() {
            None => PreviewInfo {
                open: false,
                pane: 0,
                index: 0,
                count: 0,
                path: None,
                state: "closed",
                failure: None,
                one_to_one: false,
                animated: false,
                playing: false,
                busy: false,
                frame: 0,
                frames: 0,
            },
            Some(s) => PreviewInfo {
                open: true,
                pane: s.pane,
                index: s.index,
                count: s.files.len(),
                path: s.files.get(s.index).cloned(),
                state: match s.state {
                    UiState::Loading => "loading",
                    UiState::Ready => "ready",
                    UiState::Failed(_) => "failed",
                },
                failure: match &s.state {
                    UiState::Failed(c) => Some(c.clone()),
                    _ => None,
                },
                one_to_one: s.one_to_one,
                animated: s.animated,
                playing: s.playing,
                busy: s.loading,
                frame: a.idx,
                frames: a.frames.len(),
            },
        }
    }

    /// `(打开次数, 过期而被丢弃的结果数)`。
    pub fn preview_counters(&self) -> (u64, u64) {
        (self.preview.opened.get(), self.preview.discarded.get())
    }

    /// 预览持有的资源：`(动画帧数, 排队请求是否存在, 计时器是否在运行)`——关闭后都应为空。
    pub fn preview_resources(&self) -> (usize, bool, bool) {
        (
            self.preview.anim.borrow().frames.len(),
            self.preview.pending.borrow().is_some(),
            self.preview.timer.running(),
        )
    }

    // ---------- 写回界面 ----------

    fn sync_preview(&self) {
        let Some(w) = self.window.upgrade() else {
            return;
        };
        let s = self.preview.session.borrow();
        let Some(s) = s.as_ref() else { return };
        w.set_preview_title(file_name(&s.files[s.index]).into());
        w.set_preview_busy(s.loading && s.state != UiState::Loading);
        w.set_preview_position(format!("{} / {}", s.index + 1, s.files.len()).into());
        w.set_preview_can_prev(s.index > 0);
        w.set_preview_can_next(s.index + 1 < s.files.len());
        w.set_preview_one_to_one(s.one_to_one);
        w.set_preview_animated(s.animated);
        w.set_preview_playing(s.playing);
        w.set_preview_state(match s.state {
            UiState::Loading => PreviewState::Loading,
            UiState::Ready => PreviewState::Ready,
            UiState::Failed(_) => PreviewState::Failed,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::load_autoplay;

    #[test]
    fn autoplay_setting_defaults_to_on_and_is_robust() {
        let dir = std::env::temp_dir().join(format!("sfm-autoplay-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("settings.json");
        // 没有文件、没有路径：默认播放
        assert!(load_autoplay(None));
        assert!(load_autoplay(Some(&f)));
        // 无效 JSON、缺少该项、类型不对：默认播放
        for bad in ["not json", "{}", "{\"preview_gif_autoplay\": \"no\"}", "[]"] {
            std::fs::write(&f, bad).unwrap();
            assert!(load_autoplay(Some(&f)), "{bad}");
        }
        std::fs::write(
            &f,
            "{\"preview_gif_autoplay\": false, \"thumb_cache_mb\": 256}",
        )
        .unwrap();
        assert!(!load_autoplay(Some(&f)));
        std::fs::write(&f, "{\"preview_gif_autoplay\": true}").unwrap();
        assert!(load_autoplay(Some(&f)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
