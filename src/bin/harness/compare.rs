//! 截图环境指纹、基线比较与差异图。

use crate::ctx::{Image, RENDERER, Res};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// 对 FNV-1a 的简单封装，用于对字体清单取指纹（不需要密码学强度）。
fn fnv(data: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 系统字体清单（文件名 + 大小）的指纹。字体变化会改变文字渲染，所以计入环境指纹。
fn fonts_fingerprint() -> String {
    let mut dirs =
        vec![PathBuf::from(std::env::var("WINDIR").unwrap_or("C:\\Windows".into())).join("Fonts")];
    if let Ok(l) = std::env::var("LOCALAPPDATA") {
        dirs.push(PathBuf::from(l).join("Microsoft/Windows/Fonts"));
    }
    let mut entries = Vec::new();
    for d in dirs {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                entries.push(format!("{}:{len}", e.file_name().to_string_lossy()));
            }
        }
    }
    entries.sort();
    format!("{:016x}", fnv(&entries.join("|")))
}

/// 环境指纹：渲染器、Slint 版本、系统、字体清单。任一项不同，截图差异就不能直接当作回归信号。
pub fn fingerprint() -> Value {
    json!({
        "renderer": RENDERER,
        "slint": "1.18.1",
        "os": std::env::consts::OS,
        "fonts": fonts_fingerprint(),
        // 样例目录的绝对路径（含用户名）出现在地址栏，修改日期按本地时区显示：两者不同则截图必然不同
        "fixture_root": crate::fixture::root().display().to_string(),
        "utc_offset_s": chrono::Local::now().offset().local_minus_utc(),
    })
}

pub fn sidecar(png: &Path) -> PathBuf {
    png.with_extension("json")
}

pub struct Opts {
    /// 单通道差值不超过该值视为相同像素（抗抗锯齿噪声）
    pub channel_threshold: u8,
    /// 允许不同像素占比
    pub max_ratio: f64,
}

pub struct Outcome {
    pub status: &'static str, // match | within-tolerance | regression | env-mismatch | no-baseline | unstable-baseline
    pub detail: Value,
}

pub fn compare(baseline: &Path, current: &Path, diff_out: &Path, o: &Opts) -> Res<Outcome> {
    if !baseline.exists() {
        return Ok(Outcome {
            status: "no-baseline",
            detail: json!({"baseline": baseline.display().to_string()}),
        });
    }
    let (a, b) = (Image::read_png(baseline)?, Image::read_png(current)?);
    let fp = |p: &Path| -> Option<Value> {
        let s = std::fs::read_to_string(sidecar(p)).ok()?;
        serde_json::from_str::<Value>(&s)
            .ok()?
            .get("fingerprint")
            .cloned()
    };
    let (fa, fb) = (fp(baseline), fp(current));
    let env_same = fa.is_some() && fa == fb;
    if a.w != b.w || a.h != b.h {
        return Ok(Outcome {
            status: if env_same {
                "regression"
            } else {
                "env-mismatch"
            },
            detail: json!({"reason": "尺寸不同", "baseline": [a.w, a.h], "current": [b.w, b.h]}),
        });
    }
    let total = (a.w * a.h) as usize;
    let mut diff = vec![0u8; total * 4];
    let (mut n, mut maxd) = (0usize, 0u8);
    for i in 0..total {
        let (pa, pb) = (&a.rgba[i * 4..i * 4 + 4], &b.rgba[i * 4..i * 4 + 4]);
        let d = (0..4).map(|c| pa[c].abs_diff(pb[c])).max().unwrap();
        maxd = maxd.max(d);
        if d > o.channel_threshold {
            n += 1;
            diff[i * 4..i * 4 + 4].copy_from_slice(&[255, 0, 0, 255]);
        } else {
            // 未变化处：基线压淡，便于看清红色差异
            for c in 0..3 {
                diff[i * 4 + c] = (pa[c] as u16 / 3 + 170) as u8;
            }
            diff[i * 4 + 3] = 255;
        }
    }
    let ratio = n as f64 / total as f64;
    let mut detail = json!({
        "differing_pixels": n, "total_pixels": total, "ratio": ratio, "max_channel_diff": maxd,
        "channel_threshold": o.channel_threshold, "max_ratio": o.max_ratio,
        "env_same": env_same, "baseline_fingerprint": fa, "current_fingerprint": fb,
    });
    let status = if maxd == 0 {
        "match"
    } else if ratio <= o.max_ratio {
        "within-tolerance"
    } else if !env_same {
        "env-mismatch"
    } else {
        "regression"
    };
    if n > 0 {
        Image {
            w: a.w,
            h: a.h,
            rgba: diff,
        }
        .write_png(diff_out)?;
        detail["diff_image"] = json!(diff_out.display().to_string());
    }
    Ok(Outcome { status, detail })
}

/// 退出码：0 全部一致；1 有回归；2 无法判断（缺基线或环境不同）；
pub fn exit_code(statuses: &[&str]) -> i32 {
    if statuses.contains(&"regression") {
        1
    } else if statuses
        .iter()
        .any(|s| matches!(*s, "env-mismatch" | "no-baseline"))
    {
        2
    } else {
        0
    }
}
