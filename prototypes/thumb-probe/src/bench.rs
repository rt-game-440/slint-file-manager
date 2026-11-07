//! 路线对比：对目录里的每个文件按指定路线生成缩略图，输出分段耗时、内存与失败类别统计。
//! 只输出统计，不输出路径、文件名或图像；`--dump` 仅用于合成夹具的人工目视检查。

use crate::common::*;
use crate::decode::{self, Route};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

pub fn list_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.file_type().is_ok_and(|t| t.is_file())
                        && !e.file_name().to_string_lossy().starts_with('.')
                })
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    v
}

struct Sample {
    timing: Timing,
    wall_us: u64,
    src: SrcInfo,
}

pub fn cmd(args: &[String]) -> i32 {
    let Some(dir) = arg(args, "dir") else {
        eprintln!("需要 --dir=目录");
        return 2;
    };
    let route = Route::parse(&arg(args, "route").unwrap_or_else(|| "auto".into()));
    let Some(route) = route else {
        eprintln!("未知路线");
        return 2;
    };
    let size: u32 = arg_parse(args, "size", 256);
    let workers: usize = arg_parse(args, "workers", 1);
    let limit: usize = arg_parse(args, "limit", usize::MAX);
    let repeat: usize = arg_parse(args, "repeat", 1);
    let mut lim = Limits::default();
    if flag(args, "no-limit") {
        lim.max_pixels = u64::MAX;
    }
    let mut files = list_files(std::path::Path::new(&dir));
    if let Some(e) = arg(args, "ext") {
        files.retain(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case(&e)));
    }
    if let Some(n) = arg(args, "only") {
        files.retain(|p| p.file_name().is_some_and(|x| x.to_string_lossy() == n));
    }
    files.truncate(limit);
    if files.is_empty() {
        eprintln!("目录里没有文件");
        return 2;
    }

    let base = mem();
    let samples: Mutex<Vec<Sample>> = Mutex::new(Vec::new());
    let fails: Mutex<BTreeMap<&'static str, u64>> = Mutex::new(BTreeMap::new());
    let hresults: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
    let next = AtomicUsize::new(0);
    let total_jobs = files.len() * repeat;
    let t_all = Instant::now();
    std::thread::scope(|sc| {
        for _ in 0..workers {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= total_jobs {
                        break;
                    }
                    let p = &files[i % files.len()];
                    let t = Instant::now();
                    match decode::generate(route, p, size, &lim) {
                        Ok((_, tm, info)) => samples.lock().unwrap().push(Sample {
                            timing: tm,
                            wall_us: t.elapsed().as_micros() as u64,
                            src: info,
                        }),
                        Err(f) => {
                            *fails.lock().unwrap().entry(f.as_str()).or_insert(0) += 1;
                            if route == Route::Shell {
                                *hresults
                                    .lock()
                                    .unwrap()
                                    .entry(format!("0x{:08X}", crate::shell::last_hresult() as u32))
                                    .or_insert(0) += 1;
                            }
                        }
                    }
                }
            });
        }
    });
    let wall = t_all.elapsed();
    let after = mem();
    let s = samples.into_inner().unwrap();
    let col = |f: fn(&Sample) -> u64| s.iter().map(f).collect::<Vec<_>>();
    let px: Vec<f64> = s
        .iter()
        .filter(|x| x.src.w > 0)
        .map(|x| x.src.w as f64 * x.src.h as f64 / 1e6)
        .collect();
    let bytes: Vec<f64> = s
        .iter()
        .filter(|x| x.src.bytes > 0)
        .map(|x| x.src.bytes as f64)
        .collect();
    let mean = |v: &[f64]| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    let r1 = |x: f64| (x * 10.0).round() / 10.0;
    let out = serde_json::json!({
        "kind": "route-bench",
        "route": route.name(),
        "size": size,
        "workers": workers,
        "files": files.len(),
        "repeat": repeat,
        "ok": s.len(),
        "failed": fails.into_inner().unwrap(),
        "shell_hresults": hresults.into_inner().unwrap(),
        "wall_ms": r1(ms(wall)),
        "throughput_per_s": r1(s.len() as f64 / wall.as_secs_f64()),
        "per_file_ms": {
            "wall": stats_ms(&col(|x| x.wall_us)),
            "read": if route == Route::Shell { serde_json::Value::Null } else { stats_ms(&col(|x| x.timing.read_us)) },
            "decode": stats_ms(&col(|x| x.timing.decode_us)),
            "scale": if matches!(route, Route::LibwebpScaled | Route::LibwebpFast | Route::Shell) { serde_json::Value::Null } else { stats_ms(&col(|x| x.timing.scale_us)) },
        },
        "source_megapixels_mean": mean(&px).map(r1),
        "source_bytes_mean": mean(&bytes).map(|x| x.round()),
        "memory": {
            "baseline": mem_json(&base),
            "after": mem_json(&after),
            "peak_commit_over_baseline_mib": r1(after.peak_commit - base.commit),
        },
        "note": "scale 为 null 表示该路线在解码中完成缩放（或 Shell 不可分段），其耗时已含在 decode 里；read 为 null 表示不可单独测量",
    });
    println!("{out}");
    if let Some(p) = arg(args, "out") {
        let _ = std::fs::write(p, serde_json::to_string_pretty(&out).unwrap());
    }
    0
}

/// 画质对照：各路线缩略图相对“libwebp 整幅解码 + 双线性缩放”的平均绝对误差与 PSNR。
/// `--dump=目录` 把结果存成 PNG（仅限合成夹具）。
pub fn quality(args: &[String]) -> i32 {
    let Some(dir) = arg(args, "dir") else {
        eprintln!("需要 --dir=目录");
        return 2;
    };
    let size: u32 = arg_parse(args, "size", 256);
    let limit: usize = arg_parse(args, "limit", 6);
    let lim = Limits::default();
    let mut files = list_files(std::path::Path::new(&dir));
    files.retain(|p| p.extension().is_some_and(|e| e == "webp"));
    files.truncate(limit);
    let dump = arg(args, "dump").map(PathBuf::from);
    if let Some(d) = &dump {
        let _ = std::fs::create_dir_all(d);
    }
    let routes = [
        Route::LibwebpScaled,
        Route::LibwebpFast,
        Route::RustImage,
        Route::Shell,
    ];
    let mut rows = Vec::new();
    for r in routes {
        let (mut mae_sum, mut psnr_sum, mut n, mut mismatch, mut failed) = (0.0, 0.0, 0, 0, 0);
        for (fi, p) in files.iter().enumerate() {
            let Ok((refimg, _, _)) = decode::generate(Route::LibwebpFull, p, size, &lim) else {
                continue;
            };
            let Ok((img, _, _)) = decode::generate(r, p, size, &lim) else {
                failed += 1;
                continue;
            };
            if let Some(d) = &dump {
                dump_png(&d.join(format!("{}-{fi}.png", r.name())), &img);
                if r == Route::LibwebpScaled {
                    dump_png(&d.join(format!("libwebp-full-{fi}.png")), &refimg);
                }
            }
            if (img.w, img.h) != (refimg.w, refimg.h) {
                mismatch += 1;
                continue;
            }
            let (mut abs, mut sq, mut cnt) = (0f64, 0f64, 0f64);
            for (a, b) in img.rgba.chunks_exact(4).zip(refimg.rgba.chunks_exact(4)) {
                for k in 0..3 {
                    let d = a[k] as f64 - b[k] as f64;
                    abs += d.abs();
                    sq += d * d;
                    cnt += 1.0;
                }
            }
            let mse = sq / cnt;
            mae_sum += abs / cnt;
            psnr_sum += if mse == 0.0 {
                99.0
            } else {
                10.0 * (255.0 * 255.0 / mse).log10()
            };
            n += 1;
        }
        rows.push(serde_json::json!({
            "route": r.name(),
            "compared": n,
            "size_mismatch": mismatch,
            "failed": failed,
            "mae": (n > 0).then(|| (mae_sum / n as f64 * 100.0).round() / 100.0),
            "psnr_db": (n > 0).then(|| (psnr_sum / n as f64 * 10.0).round() / 10.0),
        }));
    }
    let out = serde_json::json!({"kind": "quality", "reference": "libwebp-full", "size": size, "files": files.len(), "routes": rows});
    println!("{out}");
    if let Some(p) = arg(args, "out") {
        let _ = std::fs::write(p, serde_json::to_string_pretty(&out).unwrap());
    }
    0
}

fn dump_png(p: &std::path::Path, t: &Thumb) {
    if let Some(img) = image::RgbaImage::from_raw(t.w, t.h, t.rgba.clone()) {
        let _ = img.save(p);
    }
}

/// 透明区域正确性：带透明圆的 WebP 经各路线缩放后，圆心 alpha 应接近 0、边角 alpha 应为 255。
pub fn alpha_check(args: &[String]) -> i32 {
    let Some(f) = arg(args, "file") else {
        eprintln!("需要 --file=带透明的 webp");
        return 2;
    };
    let p = PathBuf::from(f);
    let lim = Limits::default();
    let mut rows = Vec::new();
    for r in [
        Route::LibwebpScaled,
        Route::LibwebpFast,
        Route::LibwebpFull,
        Route::RustImage,
        Route::Shell,
    ] {
        match decode::generate(r, &p, 256, &lim) {
            Ok((t, _, _)) => {
                let at = |x: u32, y: u32| t.rgba[((y * t.w + x) * 4 + 3) as usize];
                rows.push(serde_json::json!({
                    "route": r.name(), "w": t.w, "h": t.h,
                    "alpha_center": at(t.w / 2, t.h / 2), "alpha_corner": at(2, 2),
                }));
            }
            Err(e) => rows.push(serde_json::json!({"route": r.name(), "failed": e.as_str()})),
        }
    }
    let out = serde_json::json!({"kind": "alpha-check", "routes": rows});
    println!("{out}");
    if let Some(p) = arg(args, "out") {
        let _ = std::fs::write(p, serde_json::to_string_pretty(&out).unwrap());
    }
    0
}
