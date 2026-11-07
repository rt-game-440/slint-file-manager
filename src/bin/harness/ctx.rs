//! 无头上下文：测试后端（模拟时钟 + 软件渲染器离屏）、窗口配置、元素查找与输入模拟、截图与 PNG 读写。

use i_slint_backend_testing::{
    ElementHandle, ElementRoot, TestingBackend, TestingBackendOptions, get_mocked_time,
    mock_elapsed_time,
};
use slint::ComponentHandle;
use slint::platform::{PointerEventButton, WindowEvent};
use slint_file_manager::{MainWindow, app};
use std::error::Error;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;
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

pub struct Ctx {
    pub win: MainWindow,
    t0: u64,
}

impl Ctx {
    /// 以逻辑尺寸和缩放比例创建主窗口。顺序固定：先显示，再设缩放，再设尺寸。
    pub fn new(size: (u32, u32), scale: f32) -> Res<Ctx> {
        let win = MainWindow::new()?;
        app::wire(&win);
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
        Ok(Ctx {
            win,
            t0: get_mocked_time(),
        })
    }

    /// 推进模拟时钟（驱动动画、定时器、变更回调）。
    pub fn advance(&self, ms: u64) {
        mock_elapsed_time(Duration::from_millis(ms));
    }

    /// 自窗口创建起的模拟时间（毫秒）。
    pub fn elapsed(&self) -> u64 {
        get_mocked_time() - self.t0
    }

    pub fn by_label(&self, label: &str) -> Res<ElementHandle> {
        unique(
            ElementHandle::find_by_accessible_label(&self.win, label).collect(),
            label,
        )
    }

    /// `id` 形如 `MainWindow::demo-panel`。
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

    /// 发送一次按键（按下并释放）。特殊键用 `slint::platform::Key` 转成字符串。
    pub fn key(&self, text: &str) {
        let t: slint::SharedString = text.into();
        self.win
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: t.clone() });
        self.win
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text: t });
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

/// 元素根（供 dump 使用）。
pub fn root(ctx: &Ctx) -> ElementHandle {
    ctx.win.root_element()
}

/// 把 f32 取整到 3 位小数，避免浮点噪声进入断言与基线。
pub fn r3(x: f32) -> f64 {
    ((x as f64) * 1000.0).round() / 1000.0
}
