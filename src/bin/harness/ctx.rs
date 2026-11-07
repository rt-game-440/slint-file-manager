//! 无头上下文：测试后端（模拟时钟 + 软件渲染器离屏）、窗口配置、元素查找与输入模拟、截图与 PNG 读写。

use i_slint_backend_testing::{
    ElementHandle, ElementRoot, TestingBackend, TestingBackendOptions, get_mocked_time,
    mock_elapsed_time,
};
use slint::ComponentHandle;
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint_file_manager::app::{
    self, Controller, DriveSource, Env, LiveEnv, Loader, MenuCall, MenuMode, Opener, ThumbEnv,
    ThumbMode,
};
use slint_file_manager::browse::places::Drive;
use slint_file_manager::browse::session::SessionStore;
use slint_file_manager::fileops::StashBackend;
use slint_file_manager::{MainWindow, Theme};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::error::Error;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

pub type Res<T> = Result<T, Box<dyn Error>>;

/// 渲染器说明，写入截图指纹。离屏截图固定使用软件渲染器，与产品默认渲染器（femtovg/OpenGL）不同。
pub const RENDERER: &str = "software（i-slint-renderer-software 1.18.1，离屏）";

/// 每个进程只能初始化一次平台：模拟时钟、无事件循环、软件渲染器。
pub fn init_platform() {
    let backend = TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    });
    slint::platform::set_platform(Box::new(backend)).expect("平台已被初始化");
}

/// RGBA8 图像。
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn write_png(&self, path: &Path) -> Res<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut enc = png::Encoder::new(BufWriter::new(File::create(path)?), self.w, self.h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(&self.rgba)?;
        Ok(())
    }

    pub fn read_png(path: &Path) -> Res<Image> {
        let mut reader = png::Decoder::new(BufReader::new(File::open(path)?)).read_info()?;
        let mut buf = vec![0; reader.output_buffer_size().ok_or("PNG 尺寸异常")?];
        let info = reader.next_frame(&mut buf)?;
        if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
            return Err(format!("{} 不是 RGBA8 PNG", path.display()).into());
        }
        buf.truncate(info.buffer_size());
        Ok(Image {
            w: info.width,
            h: info.height,
            rgba: buf,
        })
    }

    /// 整数倍盒式缩小（取平均）。
    pub fn downscale(&self, f: u32) -> Image {
        if f <= 1 {
            return Image {
                w: self.w,
                h: self.h,
                rgba: self.rgba.clone(),
            };
        }
        let (w, h) = (self.w / f, self.h / f);
        let mut out = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0u32; 4];
                for dy in 0..f {
                    for dx in 0..f {
                        let i = (((y * f + dy) * self.w + x * f + dx) * 4) as usize;
                        for (c, a) in acc.iter_mut().enumerate() {
                            *a += self.rgba[i + c] as u32;
                        }
                    }
                }
                let o = ((y * w + x) * 4) as usize;
                for (c, a) in acc.iter().enumerate() {
                    out[o + c] = (a / (f * f)) as u8;
                }
            }
        }
        Image { w, h, rgba: out }
    }
}

/// 模拟时钟对齐周期（毫秒），需为界面中循环动画周期的整数倍
const CLOCK_ALIGN: u64 = 3600;

pub struct Ctx {
    pub win: MainWindow,
    /// 浏览控制器：手动加载模式，调用 [`Ctx::pump`] 才执行目录读取
    pub ctl: Rc<Controller>,
    /// “用默认应用打开”的记录（harness 不启动任何程序）
    pub opened: Rc<RefCell<Vec<PathBuf>>>,
    /// 注入的磁盘列表（`LiveEnv` 使用 `DriveSource::Fixed` 时），修改它即模拟 U 盘插拔
    pub drives: Option<Rc<RefCell<Vec<Drive>>>>,
    /// 临时目录“回收站”：删除、回收站浏览与还原都走它，绝不触碰系统真实回收站
    pub stash: Arc<StashBackend>,
    /// 右键菜单请求的记录（harness 不弹出系统菜单）
    pub menus: Rc<RefCell<Vec<MenuCall>>>,
    /// 为真时 [`Ctx::pump`] 不执行缩略图请求（截“加载中”状态）
    pub hold_thumbs: Cell<bool>,
    t0: u64,
}

/// 每个窗口一个全新的暂存目录（`%TEMP%\sfm-harness\stash\<序号>`）。
static STASH_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 窗口选项：起始目录、文字缩放、是否立即完成首次加载、双栏与会话目录。
pub struct Opts {
    pub start: PathBuf,
    pub text_scale: f32,
    pub load: bool,
    /// 没有可恢复会话时是否双栏（默认单栏，保持旧场景与基线的构图；双栏场景显式打开）
    pub split: bool,
    /// 会话保存目录；`None` 表示不恢复也不保存（绝不使用用户真实目录，见 fixture::session_dir）
    pub session: Option<PathBuf>,
    /// 侧栏磁盘／收藏与目录监视的环境；`None` 为惰性环境（无磁盘、无收藏、不监视）
    pub live: Option<LiveEnv>,
    /// 搜索环境（后端、全盘根目录）；`None` 为惰性环境（手动模式、无增强、全盘根目录为空）
    pub search: Option<app::SearchEnv>,
    /// 缩略图持久缓存目录；`None` 只用内存缓存（默认）
    pub thumb_cache: Option<PathBuf>,
}

/// harness 里文件名含这个字的图片被当作“慢文件”：先超时，之后由 [`Controller::finish_slow_thumbs`] 补图。
pub const SLOW_MARK: &str = "慢";

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            start: crate::fixture::root(),
            text_scale: 1.0,
            load: true,
            split: false,
            session: None,
            live: None,
            search: None,
            thumb_cache: None,
        }
    }
}

impl Ctx {
    /// 以样例目录为起点打开主窗口并完成首次加载。
    pub fn new(size: (u32, u32), scale: f32) -> Res<Ctx> {
        Ctx::open(size, scale, Opts::default())
    }

    /// 以逻辑尺寸和缩放比例创建主窗口。顺序固定：先显示，再设缩放，再设尺寸。
    pub fn open(size: (u32, u32), scale: f32, o: Opts) -> Res<Ctx> {
        // 把模拟时钟对齐到固定周期：animation-tick() 驱动的循环动画（加载进度条）在同一进程内
        // 先后创建的窗口里相位一致，截图才稳定
        let t = get_mocked_time();
        mock_elapsed_time(Duration::from_millis(CLOCK_ALIGN - t % CLOCK_ALIGN));
        let win = MainWindow::new()?;
        win.global::<Theme>().set_text_scale(o.text_scale);
        let opened = Rc::new(RefCell::new(Vec::new()));
        let drives = match &o.live {
            Some(LiveEnv {
                drives: DriveSource::Fixed(h),
                ..
            }) => Some(h.clone()),
            _ => None,
        };
        let n = STASH_SEQ.fetch_add(1, Ordering::SeqCst);
        let stash = StashBackend::new(crate::fixture::fresh_dir(
            &crate::fixture::parent().join("stash").join(n.to_string()),
        )?);
        let menus = Rc::new(RefCell::new(Vec::new()));
        let ctl = app::wire(
            &win,
            Env {
                // 带会话时不传起始目录（等同没有 --open 参数），否则会被当作“打开目录请求”改变恢复出的标签
                start: if o.session.is_some() {
                    None
                } else {
                    Some(o.start)
                },
                home: Some(crate::fixture::root()),
                loader: Loader::Manual(RefCell::new(VecDeque::new())),
                opener: Opener::Record(opened.clone()),
                menu: MenuMode::Record(menus.clone()),
                store: o.session.map(SessionStore::at),
                fresh_split: o.split,
                live: o.live.unwrap_or_else(LiveEnv::inert),
                backend: stash.clone(),
                search: o.search.unwrap_or_else(app::SearchEnv::inert),
                thumbs: ThumbEnv {
                    mode: ThumbMode::Manual(Some(Box::new(|p: &Path| {
                        p.file_name()
                            .is_some_and(|n| n.to_string_lossy().contains(SLOW_MARK))
                    }))),
                    cache_dir: o.thumb_cache,
                    settings_file: None,
                },
            },
        );
        win.show()?;
        win.window()
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: scale,
            });
        // 测试后端的 set_size 按缩放 1 换算：物理尺寸取其原值，逻辑尺寸需另发 Resized 事件，
        // 两者都设对，快照缓冲（物理）与布局（逻辑）才一致。
        win.window().set_size(slint::PhysicalSize::new(
            (size.0 as f32 * scale).round() as u32,
            (size.1 as f32 * scale).round() as u32,
        ));
        win.window().dispatch_event(WindowEvent::Resized {
            size: slint::LogicalSize::new(size.0 as f32, size.1 as f32),
        });
        mock_elapsed_time(Duration::from_millis(1));
        ctl.focus_active();
        let c = Ctx {
            win,
            ctl,
            opened,
            drives,
            stash,
            menus,
            hold_thumbs: Cell::new(false),
            t0: get_mocked_time(),
        };
        if o.load {
            c.pump();
        }
        Ok(c)
    }

    /// 执行排队的目录读取（以及缩略图请求，除非 `hold_thumbs`）并推进一点时间让界面更新；返回执行的目录读取数。
    pub fn pump(&self) -> usize {
        let n = self.ctl.pump();
        if !self.hold_thumbs.get() {
            self.ctl.pump_thumbs();
        }
        self.advance(16);
        if !self.hold_thumbs.get() {
            // 可见范围变化回调（推进时间后触发）可能追加请求；结果在下一次截图前写回
            self.ctl.pump_thumbs();
        }
        n
    }

    /// 推进模拟时钟（驱动动画、定时器、变更回调）。
    pub fn advance(&self, ms: u64) {
        mock_elapsed_time(Duration::from_millis(ms));
    }

    /// 自窗口创建起的模拟时间（毫秒）。
    pub fn elapsed(&self) -> u64 {
        get_mocked_time() - self.t0
    }

    /// `id` 形如 `BrowserPane::loading-indicator`。
    pub fn by_id(&self, id: &str) -> Res<ElementHandle> {
        unique(
            ElementHandle::find_by_element_id(&self.win, id).collect(),
            id,
        )
    }

    /// 在元素中心点击（按下后模拟 50ms 再释放）。
    pub fn click(&self, e: &ElementHandle) {
        e.mock_single_click(PointerEventButton::Left);
    }

    /// 在窗口逻辑坐标处单击（用于点击元素中心以外的位置，例如地址栏右侧空白）。
    pub fn click_at(&self, x: f32, y: f32) {
        let position = slint::LogicalPosition::new(x, y);
        let w = self.win.window();
        w.dispatch_event(WindowEvent::PointerMoved { position });
        w.dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        self.advance(50);
        w.dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
    }

    /// 在元素内距右边缘 `inset` 处单击。
    pub fn click_right_edge(&self, e: &ElementHandle, inset: f32) {
        let (p, s) = (e.absolute_position(), e.size());
        self.click_at(p.x + s.width - inset, p.y + s.height / 2.0);
    }

    /// 按住修饰键执行 `f`（Slint 从窗口的修饰键状态得到指针事件的 modifiers）。
    pub fn with_mods(&self, mods: &[Key], f: impl FnOnce()) {
        for m in mods {
            self.press(&key_text(*m));
        }
        f();
        for m in mods.iter().rev() {
            self.release(&key_text(*m));
        }
    }

    pub fn ctrl_click(&self, e: &ElementHandle) {
        self.with_mods(&[Key::Control], || self.click(e));
    }

    pub fn shift_click(&self, e: &ElementHandle) {
        self.with_mods(&[Key::Shift], || self.click(e));
    }

    /// 双击：两次单击（各含 50ms 按住），间隔在系统双击时间内。
    pub fn double_click(&self, e: &ElementHandle) {
        self.click(e);
        self.click(e);
    }

    fn press(&self, t: &str) {
        self.win
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: t.into() });
    }

    fn release(&self, t: &str) {
        self.win
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text: t.into() });
    }

    /// 发送一次按键（按下并释放）。
    pub fn key(&self, text: &str) {
        self.press(text);
        self.release(text);
    }

    /// 发送特殊键，可带修饰键。
    pub fn special(&self, k: Key, mods: &[Key]) {
        self.with_mods(mods, || self.key(&key_text(k)));
    }

    /// 在已聚焦的文本框中输入（逐字符发送按键事件）。
    pub fn type_text(&self, text: &str) {
        for ch in text.chars() {
            self.key(&ch.to_string());
        }
    }

    pub fn snapshot(&self) -> Res<Image> {
        let buf = self.win.window().take_snapshot()?;
        Ok(Image {
            w: buf.width(),
            h: buf.height(),
            rgba: buf.as_bytes().to_vec(),
        })
    }
}

fn unique(mut v: Vec<ElementHandle>, key: &str) -> Res<ElementHandle> {
    match v.len() {
        1 => Ok(v.remove(0)),
        0 => Err(format!("找不到元素：{key}").into()),
        n => Err(format!("元素标识不唯一（{n} 个）：{key}").into()),
    }
}

pub fn key_text(k: Key) -> String {
    let t: slint::SharedString = k.into();
    t.to_string()
}

/// 元素根（供 dump 使用）。
pub fn root(ctx: &Ctx) -> ElementHandle {
    ctx.win.root_element()
}

/// 把 f32 取整到 3 位小数，避免浮点噪声进入断言与基线。
pub fn r3(x: f32) -> f64 {
    ((x as f64) * 1000.0).round() / 1000.0
}
