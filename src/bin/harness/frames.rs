//! 动画逐帧采集：模拟时钟按固定间隔推进，导出帧 PNG、属性曲线（CSV/JSON）、联系表与 GIF。
//! 模拟时钟只验证动画定义本身（时长、缓动、过冲、终态），不能证明真实运行时不卡顿。

use crate::ctx::{Ctx, Image, Res, r3};
use crate::scenarios::AnimScenario;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::fs::File;
use std::path::Path;

pub struct Sample {
    pub frame: usize,
    pub t_ms: u64,
    pub mock_ms: u64,
    /// 每个被跟踪元素：x, y, w, h, opacity
    pub vals: Vec<[f64; 5]>,
}

fn sample(ctx: &Ctx, frame: usize, t_ms: u64, tracked: Tracked) -> Res<Sample> {
    let mut vals = Vec::new();
    for (_, id) in tracked {
        let e = ctx.by_id(id)?;
        let (p, s) = (e.absolute_position(), e.size());
        vals.push([
            r3(p.x),
            r3(p.y),
            r3(s.width),
            r3(s.height),
            r3(e.computed_opacity()),
        ]);
    }
    Ok(Sample {
        frame,
        t_ms,
        mock_ms: ctx.elapsed(),
        vals,
    })
}

/// 被跟踪的元素：（显示名，元素标识）。
type Tracked = &'static [(&'static str, &'static str)];

const PROPS: [&str; 5] = ["x", "y", "w", "h", "opacity"];

/// 每个属性的起点、终点、极值、过冲与稳定时间。
fn summary(samples: &[Sample], tracked: Tracked) -> Value {
    let mut out = serde_json::Map::new();
    for (ti, (name, _)) in tracked.iter().enumerate() {
        let mut props = serde_json::Map::new();
        for (pi, p) in PROPS.iter().enumerate() {
            let series: Vec<f64> = samples.iter().map(|s| s.vals[ti][pi]).collect();
            let (start, end) = (series[0], *series.last().unwrap());
            let (min, max) = series
                .iter()
                .fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(*v), b.max(*v)));
            if (max - min).abs() < 1e-9 {
                props.insert(
                    p.to_string(),
                    json!({"start": start, "end": end, "changed": false}),
                );
                continue;
            }
            // 沿变化方向的过冲：超出终点的幅度
            let overshoot = if end >= start { max - end } else { end - min };
            let eps = (max - min) * 0.005;
            let settle = samples
                .iter()
                .rposition(|s| (s.vals[ti][pi] - end).abs() > eps)
                .map(|i| samples.get(i + 1).map_or(samples[i].t_ms, |n| n.t_ms));
            props.insert(
                p.to_string(),
                json!({"start": start, "end": end, "min": min, "max": max,
                       "overshoot": r3(overshoot as f32), "settle_ms": settle, "changed": true}),
            );
        }
        out.insert(name.to_string(), Value::Object(props));
    }
    Value::Object(out)
}

fn csv(samples: &[Sample], tracked: Tracked) -> String {
    let mut s = String::from("frame,t_ms,mock_time_ms");
    for (name, _) in tracked {
        for p in PROPS {
            let _ = write!(s, ",{name}_{p}");
        }
    }
    s.push('\n');
    for sm in samples {
        let _ = write!(s, "{},{},{}", sm.frame, sm.t_ms, sm.mock_ms);
        for v in &sm.vals {
            for x in v {
                let _ = write!(s, ",{x}");
            }
        }
        s.push('\n');
    }
    s
}

pub struct Params {
    pub interval_ms: u64,
    pub duration_ms: u64,
}

/// 采集并写入 `out` 目录。返回曲线摘要。
pub fn capture(ctx: &Ctx, sc: &AnimScenario, p: &Params, out: &Path) -> Res<Value> {
    std::fs::create_dir_all(out)?;
    // 先渲染一帧：属性必须在变化前被读取过，动画才会从旧值开始（真实窗口的首帧起同理）
    ctx.snapshot()?;
    (sc.start)(ctx)?; // 触发动画；t=0 为触发瞬间
    let n = (p.duration_ms / p.interval_ms) as usize;
    let mut samples = Vec::new();
    let mut sheet_cells: Vec<(usize, u64, Image)> = Vec::new();
    let mut gif_frames: Vec<Image> = Vec::new();
    let mut factor = (0u32, 0u32);
    for i in 0..=n {
        let t = i as u64 * p.interval_ms;
        let s = sample(ctx, i, t, sc.tracked)?;
        let img = ctx.snapshot()?;
        img.write_png(&out.join(format!("frame_{i:04}_t{t:04}ms.png")))?;
        if factor.0 == 0 {
            factor = ((img.w / 240).max(1), (img.w / 480).max(1));
        }
        sheet_cells.push((i, t, img.downscale(factor.0)));
        gif_frames.push(img.downscale(factor.1));
        samples.push(s);
        if i < n {
            ctx.advance(p.interval_ms);
        }
    }
    std::fs::write(out.join("curve.csv"), csv(&samples, sc.tracked))?;
    let sum = summary(&samples, sc.tracked);
    let curve = json!({
        "scenario": sc.name, "interval_ms": p.interval_ms, "duration_ms": p.duration_ms,
        "tracked": sc.tracked.iter().map(|(n, id)| json!({"name": n, "id": id})).collect::<Vec<_>>(),
        "properties": PROPS,
        "samples": samples.iter().map(|s| json!({"frame": s.frame, "t_ms": s.t_ms, "mock_time_ms": s.mock_ms, "values": s.vals})).collect::<Vec<_>>(),
        "summary": sum,
    });
    std::fs::write(
        out.join("curve.json"),
        serde_json::to_string_pretty(&curve)?,
    )?;
    contact_sheet(&sheet_cells)?.write_png(&out.join("contact-sheet.png"))?;
    write_gif(&gif_frames, p.interval_ms, &out.join("anim.gif"))?;
    Ok(sum)
}

fn write_gif(frames: &[Image], interval_ms: u64, path: &Path) -> Res<()> {
    let (w, h) = (frames[0].w as u16, frames[0].h as u16);
    let mut enc = gif::Encoder::new(File::create(path)?, w, h, &[])?;
    enc.set_repeat(gif::Repeat::Infinite)?;
    let delay = ((interval_ms / 10).max(2)) as u16;
    for f in frames {
        let mut rgba = f.rgba.clone();
        let mut fr = gif::Frame::from_rgba_speed(w, h, &mut rgba, 10);
        fr.delay = delay;
        enc.write_frame(&fr)?;
    }
    Ok(())
}

// 3x5 数字点阵，用于联系表标注（左：帧号，右：毫秒）
const DIGITS: [[u8; 5]; 10] = [
    [7, 5, 5, 5, 7],
    [2, 6, 2, 2, 7],
    [7, 1, 7, 4, 7],
    [7, 1, 7, 1, 7],
    [5, 5, 7, 1, 1],
    [7, 4, 7, 1, 7],
    [7, 4, 7, 5, 7],
    [7, 1, 1, 1, 1],
    [7, 5, 7, 5, 7],
    [7, 5, 7, 1, 7],
];

fn draw_number(img: &mut Image, mut x: u32, y: u32, n: u64, scale: u32) {
    for ch in n.to_string().bytes() {
        let g = DIGITS[(ch - b'0') as usize];
        for (row, bits) in g.iter().enumerate() {
            for col in 0..3 {
                if bits >> (2 - col) & 1 == 1 {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let (px, py) = (x + col * scale + dx, y + row as u32 * scale + dy);
                            if px < img.w && py < img.h {
                                let i = ((py * img.w + px) * 4) as usize;
                                img.rgba[i..i + 4].copy_from_slice(&[0, 0, 0, 255]);
                            }
                        }
                    }
                }
            }
        }
        x += 4 * scale;
    }
}

/// 联系表：行优先排列，每格上方一条白色标注栏，左为帧号、右为毫秒。
fn contact_sheet(cells: &[(usize, u64, Image)]) -> Res<Image> {
    let (cw, ch) = (cells[0].2.w, cells[0].2.h);
    let bar = 14u32;
    let cols = 6u32.min(cells.len() as u32);
    let rows = (cells.len() as u32).div_ceil(cols);
    let (w, h) = (cols * cw, rows * (ch + bar));
    let mut sheet = Image {
        w,
        h,
        rgba: vec![255; (w * h * 4) as usize],
    };
    for (k, (frame, t, img)) in cells.iter().enumerate() {
        let (cx, cy) = ((k as u32 % cols) * cw, (k as u32 / cols) * (ch + bar));
        for y in 0..ch {
            let src = (y * cw * 4) as usize;
            let dst = (((cy + bar + y) * w + cx) * 4) as usize;
            sheet.rgba[dst..dst + (cw * 4) as usize]
                .copy_from_slice(&img.rgba[src..src + (cw * 4) as usize]);
        }
        draw_number(&mut sheet, cx + 3, cy + 3, *frame as u64, 2);
        draw_number(&mut sheet, cx + cw / 2, cy + 3, *t, 2);
    }
    Ok(sheet)
}
