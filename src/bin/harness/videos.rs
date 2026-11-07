//! 视频缩略图编码兼容测量（Issue #13）：对 `scripts/make-video-fixtures.ps1` 生成的夹具，
//! 用真实的 Windows Shell（`IShellItemImageFactory`）逐个取缩略图，记录是否出图、耗时、尺寸与内容是否有效。
//!
//! 只读夹具目录，不读取用户文件；结果依赖本机安装的解码器，所以每条都带本机环境说明，结论不能外推到别的机器。
//! 输出：`artifacts/state/video-compat.json`、`artifacts/video-compat/*.png`（每个出图样本的缩略图，供 3 级审阅）。

use crate::ctx::{Image, Res};
use serde_json::{Value, json};
use slint_file_manager::thumbs::shell::{self, thumbnail_diag};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// 缩略图像素的亮度标准差：测试图案（testsrc2）色块丰富，空白／纯色图的标准差接近 0。
fn luma_stats(rgba: &[u8]) -> (f64, f64) {
    let n = (rgba.len() / 4).max(1) as f64;
    let l: Vec<f64> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64)
        .collect();
    let mean = l.iter().sum::<f64>() / n;
    let var = l.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    (mean, var.sqrt())
}

pub fn cmd(dir: &Path, out_png: &Path, out_json: &Path) -> Res<i32> {
    let raw = std::fs::read(dir.join("manifest.json")).map_err(|e| {
        format!(
            "读不到 {}\\manifest.json（{e}）；先运行 scripts/make-video-fixtures.ps1",
            dir.display()
        )
    })?;
    // PowerShell 5 写出的 UTF-8 带 BOM
    let manifest: Vec<Value> =
        serde_json::from_slice(raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&raw))?;
    std::fs::create_dir_all(out_png)?;
    let mut rows = Vec::new();
    let mut fails = Vec::new();
    for m in &manifest {
        let file = m["file"].as_str().unwrap_or_default();
        let kind = m["kind"].as_str().unwrap_or_default();
        let path: PathBuf = dir.join(file);
        // 首次（可能含系统组件初始化）与第二次（可能命中系统缓存）都记，第二次只作参考
        let t0 = Instant::now();
        let r1 = thumbnail_diag(&path, false);
        let first_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = Instant::now();
        let _ = thumbnail_diag(&path, false);
        let second_ms = t1.elapsed().as_secs_f64() * 1000.0;
        // 仅缓存查询（云占位路径用的标志）：对普通文件应能命中刚生成的系统缓存，或返回“未命中”，但绝不报崩溃
        let cache_only = thumbnail_diag(&path, true).is_ok();
        let mut row = json!({
            "file": file, "kind": kind, "desc": m["desc"], "bytes": std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            "first_ms": (first_ms * 10.0).round() / 10.0, "second_ms": (second_ms * 10.0).round() / 10.0,
            "cache_only_hit": cache_only,
        });
        match r1 {
            Ok(t) => {
                let (mean, sd) = luma_stats(&t.rgba);
                let has_content = sd > 12.0;
                row["result"] = json!("thumbnail");
                row["size"] = json!([t.w, t.h]);
                row["luma_mean"] = json!((mean * 10.0).round() / 10.0);
                row["luma_sd"] = json!((sd * 10.0).round() / 10.0);
                row["has_content"] = json!(has_content);
                let png = out_png.join(format!("{}.png", file.replace('.', "-")));
                Image {
                    w: t.w,
                    h: t.h,
                    rgba: t.rgba,
                }
                .write_png(&png)?;
                if kind == "corrupt" {
                    fails.push(format!("{file}：损坏文件竟然出图（应回退图标）"));
                } else if !has_content {
                    fails.push(format!("{file}：出图但内容近似纯色（标准差 {sd:.1}）"));
                }
            }
            Err(e) => {
                row["result"] = json!("fallback");
                row["fail"] = json!(e.fail.as_str());
                row["hresult"] = json!(format!("0x{:08X}", e.hresult as u32));
            }
        }
        rows.push(row);
    }
    let ok = rows.iter().filter(|r| r["result"] == "thumbnail").count();
    let v = json!({
        "note": "结果取决于本机安装的解码器；只说明本机，不能外推到其他机器",
        "shell_exts_video": shell::SHELL_VIDEO_EXTS,
        "total": rows.len(), "thumbnail": ok, "fallback": rows.len() - ok,
        "rows": rows, "failures": fails,
    });
    if let Some(d) = out_json.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(out_json, serde_json::to_string_pretty(&v)? + "\n")?;
    println!(
        "{:<28} {:<10} {:<12} {:>9} {:>9}  说明",
        "文件", "结果", "尺寸", "首次ms", "二次ms"
    );
    for r in &rows {
        let size = r["size"]
            .as_array()
            .map(|a| format!("{}x{}", a[0], a[1]))
            .unwrap_or_default();
        let res = match r["result"].as_str() {
            Some("thumbnail") => "缩略图".to_string(),
            _ => format!("回退({})", r["hresult"].as_str().unwrap_or("")),
        };
        println!(
            "{:<28} {:<10} {:<12} {:>9} {:>9}  {}",
            r["file"].as_str().unwrap_or(""),
            res,
            size,
            r["first_ms"],
            r["second_ms"],
            r["desc"].as_str().unwrap_or("")
        );
    }
    println!(
        "共 {} 个：缩略图 {ok}，回退 {}；输出 {}",
        rows.len(),
        rows.len() - ok,
        out_json.display()
    );
    for f in &fails {
        println!("  异常：{f}");
    }
    Ok(i32::from(!fails.is_empty()))
}
