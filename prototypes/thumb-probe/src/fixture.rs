//! 夹具生成：程序合成的高分辨率图片，写在 %TEMP%\sfm-thumbs 下（可丢弃，不入库）。
//!
//! - `webp-photo-60`：60 张 1000–1200 万像素 WebP，质量调到平均约 200 KB（对应 Issue #1 的匿名案例体积）。
//! - `webp-dense-12`：12 张同分辨率但细节密集的 WebP（体积大一个数量级），用来证明耗时不能由体积推断。
//! - `formats`：JPEG／PNG／BMP／GIF／带透明 WebP 各一张。
//! - `mixed`：`webp-photo-60` 的副本，固定位置替换为损坏、空文件、错误扩展名、超大尺寸等异常文件。
//! - `many-600`：`webp-photo-60` 的 10 份硬链接（600 个不同路径），用于滚动取消场景。

use crate::common::{arg, arg_parse, temp_root};
use crate::webp;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const DIMS: [(u32, u32); 6] = [
    (4000, 3000),
    (3000, 4000),
    (3840, 2880),
    (4032, 3024),
    (3968, 2976),
    (4096, 2800),
];

fn hash(x: u32, y: u32, seed: u32) -> u32 {
    let mut h =
        x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA6B) ^ seed.wrapping_mul(0xC2B2_AE35);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^ (h >> 15)
}

/// 合成“照片感”图像：平滑渐变 + 几个色块 + 可调白噪声。
fn render(w: u32, h: u32, seed: u32, noise: f32) -> Vec<u8> {
    let mut buf = vec![0u8; w as usize * h as usize * 4];
    let s = seed as f32;
    let blobs: Vec<(f32, f32, f32, [f32; 3])> = (0..5)
        .map(|k| {
            let r = |n: u32| (hash(k, n, seed) & 0xFFFF) as f32 / 65535.0;
            (
                r(1),
                r(2),
                0.05 + r(3) * 0.2,
                [r(4) - 0.5, r(5) - 0.5, r(6) - 0.5],
            )
        })
        .collect();
    let threads = 6usize;
    let rows_per = (h as usize).div_ceil(threads);
    std::thread::scope(|sc| {
        for (ti, chunk) in buf.chunks_mut(rows_per * w as usize * 4).enumerate() {
            let blobs = &blobs;
            sc.spawn(move || {
                let y0 = ti * rows_per;
                for (ry, row) in chunk.chunks_mut(w as usize * 4).enumerate() {
                    let y = (y0 + ry) as u32;
                    let fy = y as f32 / h as f32;
                    for x in 0..w {
                        let fx = x as f32 / w as f32;
                        let mut c = [
                            0.5 + 0.35 * (3.1 * fx + s * 0.7).sin() * (2.3 * fy + s).cos(),
                            0.5 + 0.3 * (2.0 * fx + 4.0 * fy + s * 1.3).sin(),
                            0.55 + 0.3 * fy,
                        ];
                        for (cx, cy, rad, col) in blobs {
                            let d2 = ((fx - cx) * (fx - cx) + (fy - cy) * (fy - cy)) / (rad * rad);
                            if d2 < 9.0 {
                                let k = (-d2).exp();
                                for i in 0..3 {
                                    c[i] += col[i] * k;
                                }
                            }
                        }
                        let n = ((hash(x, y, seed) & 255) as f32 / 255.0 - 0.5) * noise;
                        let p = &mut row[x as usize * 4..x as usize * 4 + 4];
                        for i in 0..3 {
                            p[i] = ((c[i] * 255.0 + n).clamp(0.0, 255.0)) as u8;
                        }
                        p[3] = 255;
                    }
                }
            });
        }
    });
    buf
}

fn name(i: usize) -> String {
    format!("IMG_{:04}.webp", i + 1)
}

fn gen_set(dir: &Path, count: usize, noise: f32, quality: f32, tag: &str) {
    let marker = dir.join(".ok");
    let want = format!("{count} {noise} {quality}");
    if fs::read_to_string(&marker).is_ok_and(|s| s == want) {
        println!("夹具 {tag} 已存在，跳过");
        return;
    }
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).unwrap();
    let next = AtomicUsize::new(0);
    let total = std::sync::atomic::AtomicU64::new(0);
    let t = std::time::Instant::now();
    std::thread::scope(|sc| {
        for _ in 0..3 {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= count {
                        break;
                    }
                    let (w, h) = DIMS[i % DIMS.len()];
                    let rgba = render(w, h, i as u32 + 1, noise);
                    let b = webp::encode(&rgba, w, h, quality);
                    total.fetch_add(b.len() as u64, Ordering::SeqCst);
                    fs::write(dir.join(name(i)), b).unwrap();
                }
            });
        }
    });
    let tot = total.load(Ordering::SeqCst);
    println!(
        "夹具 {tag}：{count} 张，合计 {:.2} MiB（平均 {:.0} KiB），生成耗时 {:.1}s",
        tot as f64 / 1048576.0,
        tot as f64 / count as f64 / 1024.0,
        t.elapsed().as_secs_f64()
    );
    fs::write(marker, want).unwrap();
}

/// 异常夹具的位置（0 起）与类型，写入 manifest.json。
pub const FAULTS: [(usize, &str); 6] = [
    (6, "truncated"),
    (12, "zero_length"),
    (20, "png_named_webp"),
    (28, "oversize_144mp"),
    (35, "junk_with_riff_header"),
    (44, "bitflip_body"),
];

fn gen_mixed(root: &Path) {
    let src = root.join("webp-photo-60");
    let dir = root.join("mixed");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for i in 0..60 {
        fs::copy(src.join(name(i)), dir.join(name(i))).unwrap();
    }
    for (i, kind) in FAULTS {
        let p = dir.join(name(i));
        let good = fs::read(src.join(name(i))).unwrap();
        match kind {
            "truncated" => fs::write(&p, &good[..good.len() * 4 / 10]).unwrap(),
            "zero_length" => fs::write(&p, []).unwrap(),
            "png_named_webp" => {
                let img = image::RgbaImage::from_fn(640, 480, |x, y| {
                    image::Rgba([(x / 3) as u8, (y / 2) as u8, 128, 255])
                });
                let mut v = Vec::new();
                img.write_to(&mut std::io::Cursor::new(&mut v), image::ImageFormat::Png)
                    .unwrap();
                fs::write(&p, v).unwrap();
            }
            "oversize_144mp" => {
                let (w, h) = (12000u32, 12000u32);
                let rgba = render(w, h, 99, 0.0);
                fs::write(&p, webp::encode(&rgba, w, h, 40.0)).unwrap();
            }
            "junk_with_riff_header" => {
                let mut v = b"RIFF\x00\x10\x00\x00WEBPVP8 ".to_vec();
                v.extend((0..50_000u32).map(|k| (hash(k, 7, 7) & 255) as u8));
                fs::write(&p, v).unwrap();
            }
            "bitflip_body" => {
                let mut v = good.clone();
                let (a, b) = (v.len() * 4 / 10, v.len() * 6 / 10);
                for (k, byte) in v[a..b].iter_mut().enumerate() {
                    *byte ^= (hash(k as u32, 3, 3) & 255) as u8;
                }
                fs::write(&p, v).unwrap();
            }
            _ => unreachable!(),
        }
    }
    let m: Vec<_> = FAULTS
        .iter()
        .map(|(i, k)| serde_json::json!({"index": i, "kind": k}))
        .collect();
    fs::write(
        dir.join("..").join("mixed-manifest.json"),
        serde_json::to_string_pretty(&m).unwrap(),
    )
    .unwrap();
    println!(
        "夹具 mixed：60 个文件，其中 {} 个异常（见 mixed-manifest.json）",
        FAULTS.len()
    );
}

fn gen_many(root: &Path) {
    let src = root.join("webp-photo-60");
    let dir = root.join("many-600");
    if dir.exists() && fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0) == 600 {
        return;
    }
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for i in 0..600 {
        let s = src.join(name(i % 60));
        let d = dir.join(name(i));
        if fs::hard_link(&s, &d).is_err() {
            fs::copy(&s, &d).unwrap();
        }
    }
    println!("夹具 many-600：600 个路径（硬链接，同盘时不占额外空间）");
}

/// `webp-variants`：同一幅 1200 万像素图的无损、有损带透明两个变体，验证“解码时缩放”不只对有损无透明成立。
fn gen_variants(root: &Path) {
    let dir = root.join("webp-variants");
    if dir.join("lossless.webp").exists() && dir.join("lossy-alpha.webp").exists() {
        return;
    }
    fs::create_dir_all(&dir).unwrap();
    let (w, h) = (4000u32, 3000u32);
    let mut rgba = render(w, h, 11, 6.0);
    fs::write(
        dir.join("lossless.webp"),
        webp::encode_lossless(&rgba, w, h),
    )
    .unwrap();
    // 径向渐变透明：中心不透明，边缘半透明
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 / w as f32 - 0.5;
            let dy = y as f32 / h as f32 - 0.5;
            let a = (255.0 - (dx * dx + dy * dy).sqrt() * 300.0).clamp(40.0, 255.0);
            rgba[(y * w + x) as usize * 4 + 3] = a as u8;
        }
    }
    fs::write(
        dir.join("lossy-alpha.webp"),
        webp::encode(&rgba, w, h, 75.0),
    )
    .unwrap();
    println!("夹具 webp-variants：无损与有损带透明各一张（1200 万像素）");
}

fn gen_formats(root: &Path) {
    let dir = root.join("formats");
    if dir.join("sample.bmp").exists() {
        return;
    }
    fs::create_dir_all(&dir).unwrap();
    let (w, h) = (4000u32, 3000u32);
    let rgba = render(w, h, 7, 6.0);
    let img = image::RgbaImage::from_raw(w, h, rgba).unwrap();
    let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
    let t = std::time::Instant::now();
    rgb.save_with_format(dir.join("sample.jpg"), image::ImageFormat::Jpeg)
        .unwrap();
    rgb.save_with_format(dir.join("sample.png"), image::ImageFormat::Png)
        .unwrap();
    rgb.save_with_format(dir.join("sample.bmp"), image::ImageFormat::Bmp)
        .unwrap();
    // GIF 编码很慢，用较小尺寸
    let small = image::imageops::resize(&img, 2000, 1500, image::imageops::FilterType::Triangle);
    small
        .save_with_format(dir.join("sample.gif"), image::ImageFormat::Gif)
        .unwrap();
    // 带透明的 WebP：中间有透明圆
    let (aw, ah) = (2000u32, 1500u32);
    let mut a = render(aw, ah, 3, 4.0);
    for y in 0..ah {
        for x in 0..aw {
            let (dx, dy) = (x as f32 - 1000.0, y as f32 - 750.0);
            if dx * dx + dy * dy < 400.0 * 400.0 {
                a[(y * aw + x) as usize * 4 + 3] = 0;
            }
        }
    }
    fs::write(dir.join("alpha.webp"), webp::encode(&a, aw, ah, 70.0)).unwrap();
    println!(
        "夹具 formats：jpg/png/bmp/gif/alpha.webp（{:.1}s）",
        t.elapsed().as_secs_f64()
    );
}

pub fn cmd(args: &[String]) -> i32 {
    let root = arg(args, "root")
        .map(PathBuf::from)
        .unwrap_or_else(temp_root);
    fs::create_dir_all(&root).unwrap();
    let q: f32 = arg_parse(args, "quality", 50.0);
    let noise: f32 = arg_parse(args, "noise", 17.5);
    gen_set(&root.join("webp-photo-60"), 60, noise, q, "webp-photo-60");
    gen_set(&root.join("webp-dense-12"), 12, 40.0, 85.0, "webp-dense-12");
    gen_formats(&root);
    gen_mixed(&root);
    gen_many(&root);
    gen_variants(&root);
    println!("根目录：{}", root.display());
    0
}
