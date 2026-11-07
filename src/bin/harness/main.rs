//! 调试 harness：无头驱动界面、导出元素树、确定性截图与基线比较、动画逐帧采集、生成审阅材料。
//! 用法见 docs/debug-harness.md；命令封装在 scripts/harness.ps1。
//! 必须以 `--features harness` 构建，发布产物不含本程序。

mod compare;
mod ctx;
mod delete;
mod dual;
mod dump;
mod fixture;
mod frames;
mod live;
mod menu;
mod scenarios;
mod search;
mod thumbs;
mod xfer;

use ctx::{Ctx, Res};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const BASELINES: &str = "tests/baselines";
const ARTIFACTS: &str = "artifacts";
/// 默认逻辑窗口尺寸，与产品初始窗口一致
const DEFAULT_SIZE: &str = "1100x720";

struct Args {
    cmd: String,
    opts: HashMap<String, String>,
}

impl Args {
    fn parse() -> Res<Args> {
        let mut it = std::env::args().skip(1);
        let cmd = it.next().ok_or("缺少子命令，见 docs/debug-harness.md")?;
        let mut opts = HashMap::new();
        while let Some(a) = it.next() {
            let key = a.strip_prefix("--").ok_or(format!("无法识别的参数 {a}"))?;
            if let Some((k, v)) = key.split_once('=') {
                opts.insert(k.to_string(), v.to_string());
            } else {
                opts.insert(key.to_string(), it.next().ok_or(format!("--{key} 缺少值"))?);
            }
        }
        Ok(Args { cmd, opts })
    }
    fn get(&self, k: &str, default: &str) -> String {
        self.opts
            .get(k)
            .cloned()
            .unwrap_or_else(|| default.to_string())
    }
    fn size(&self) -> Res<(u32, u32)> {
        let s = self.get("size", DEFAULT_SIZE);
        let (w, h) = s.split_once('x').ok_or("--size 形如 1100x720")?;
        Ok((w.parse()?, h.parse()?))
    }
    fn scales(&self) -> Res<Vec<f32>> {
        self.get("scales", "1,1.5")
            .split(',')
            .map(|s| Ok(s.trim().parse()?))
            .collect()
    }
    fn dir(&self, k: &str, default: &str) -> PathBuf {
        PathBuf::from(self.get(k, default))
    }
}

fn pct(scale: f32) -> u32 {
    (scale * 100.0).round() as u32
}

/// 文件名包含场景、逻辑尺寸、缩放百分比。
fn file_stem(scenario: &str, size: (u32, u32), scale: f32) -> String {
    format!("{scenario}_{}x{}_{}", size.0, size.1, pct(scale))
}

/// 样例目录信息：只记录名称与可见条目数，不接触用户真实目录。
fn fixture_meta() -> Value {
    json!({"name": fixture::ROOT_NAME, "root_entries": fixture::root_visible_count()})
}

fn meta(args: &Args, scale: f32) -> Res<Value> {
    let size = args.size()?;
    Ok(json!({
        "window": {"width": size.0, "height": size.1, "scale": scale},
        "renderer": ctx::RENDERER,
        "fixture": fixture_meta(),
    }))
}

fn write_json(path: &Path, v: &Value) -> Res<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(v)? + "\n")?;
    Ok(())
}

/// tree：按场景准备界面后导出元素树 JSON。
fn cmd_tree(args: &Args) -> Res<i32> {
    let scale: f32 = args.get("scale", "1").parse()?;
    let name = args.get("scenario", "grid-selected");
    let c = scenarios::open(&name, args.size()?, scale)?;
    let mut v = dump::tree(&c);
    v["meta"] = meta(args, scale)?;
    v["meta"]["scenario"] = json!(name);
    let text = serde_json::to_string_pretty(&v)? + "\n";
    match args.opts.get("out") {
        Some(p) => std::fs::write(p, text)?,
        None => print!("{text}"),
    }
    Ok(0)
}

/// run：交互脚本（查找、点击、按键、滚动、断言），重复运行并核对结果一致。
fn cmd_run(args: &Args) -> Res<i32> {
    let scale: f32 = args.get("scale", "1").parse()?;
    let repeat: usize = args.get("repeat", "2").parse()?;
    let size = args.size()?;
    let mut runs: Vec<Value> = Vec::new();
    let mut fails: Vec<String> = Vec::new();
    for _ in 0..repeat {
        // 单栏浏览流程、双栏标签流程、重启恢复流程各自独立，结果合并后核对重复运行是否一致
        let c = Ctx::new(size, scale)?;
        let (single, f1) = scenarios::interaction(&c).map_err(|e| format!("[单栏] {e}"))?;
        drop(c);
        let (dual, f2) = dual::dual(size, scale).map_err(|e| format!("[双栏] {e}"))?;
        let (restore, f3) = dual::restore(size, scale).map_err(|e| format!("[恢复] {e}"))?;
        let (del, f4) = delete::delete(size, scale).map_err(|e| format!("[删除] {e}"))?;
        let (context, f5) = menu::context(size, scale).map_err(|e| format!("[右键菜单] {e}"))?;
        let (thumbs, f6) = thumbs::thumbs(size, scale).map_err(|e| format!("[缩略图] {e}"))?;
        let (xf, f7) = xfer::xfer(size, scale).map_err(|e| format!("[新建传输] {e}"))?;
        fails.extend(f1.into_iter().map(|f| format!("[单栏] {f}")));
        fails.extend(f2.into_iter().map(|f| format!("[双栏] {f}")));
        fails.extend(f3.into_iter().map(|f| format!("[恢复] {f}")));
        fails.extend(f4.into_iter().map(|f| format!("[删除] {f}")));
        fails.extend(f5.into_iter().map(|f| format!("[右键菜单] {f}")));
        fails.extend(f6.into_iter().map(|f| format!("[缩略图] {f}")));
        fails.extend(f7.into_iter().map(|f| format!("[新建传输] {f}")));
        runs.push(json!({"single": single, "dual": dual, "restore": restore, "delete": del, "context": context, "thumbs": thumbs, "xfer": xf}));
    }
    let consistent = runs.windows(2).all(|w| w[0] == w[1]);
    if !consistent {
        fails.push("重复运行的状态记录不一致".into());
        // 指出第一处不同的步骤与字段，便于定位不确定性来源
        'outer: for sec in [
            "single", "dual", "restore", "delete", "context", "thumbs", "xfer",
        ] {
            let (a, b) = (&runs[0][sec], &runs[1][sec]);
            for (x, y) in a
                .as_array()
                .into_iter()
                .flatten()
                .zip(b.as_array().into_iter().flatten())
            {
                if x != y {
                    let mut keys = Vec::new();
                    collect_diff("", x, y, &mut keys);
                    fails.push(format!(
                        "首个不一致：[{sec}] {}：{}",
                        x["step"],
                        keys.join("；")
                    ));
                    break 'outer;
                }
            }
        }
    }
    let out = args.dir("out", &format!("{ARTIFACTS}/state/interaction.json"));
    write_json(
        &out,
        &json!({"meta": meta(args, scale)?, "repeat": repeat, "consistent": consistent,
                "failures": fails, "steps": runs[0]}),
    )?;
    println!(
        "交互脚本：单栏 {} 步、双栏 {} 步、重启恢复 {} 步、删除与回收站 {} 步、右键菜单 {} 步、缩略图 {} 步、新建重命名传输 {} 步，重复 {repeat} 次，{}；输出 {}",
        runs[0]["single"].as_array().map_or(0, |a| a.len()),
        runs[0]["dual"].as_array().map_or(0, |a| a.len()),
        runs[0]["restore"].as_array().map_or(0, |a| a.len()),
        runs[0]["delete"].as_array().map_or(0, |a| a.len()),
        runs[0]["context"].as_array().map_or(0, |a| a.len()),
        runs[0]["thumbs"].as_array().map_or(0, |a| a.len()),
        runs[0]["xfer"].as_array().map_or(0, |a| a.len()),
        if fails.is_empty() {
            "全部断言通过且结果一致"
        } else {
            "存在失败"
        },
        out.display()
    );
    for f in &fails {
        println!("  失败：{f}");
    }
    Ok(if fails.is_empty() { 0 } else { 1 })
}

fn collect_diff(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in x {
                collect_diff(
                    &format!("{path}.{k}"),
                    v,
                    y.get(k).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (v, w)) in x.iter().zip(y).enumerate() {
                collect_diff(&format!("{path}[{i}]"), v, w, out);
            }
        }
        _ if a != b => out.push(format!("{path}: {a} != {b}")),
        _ => {}
    }
}

/// 渲染一个场景的截图，同一场景用两个全新窗口渲染两次，不一致则标记不稳定。
fn render_stable(c_args: &Args, name: &str, scale: f32) -> Res<(ctx::Image, bool)> {
    let mut imgs = Vec::new();
    for _ in 0..2 {
        let c = scenarios::open(name, c_args.size()?, scale)?;
        imgs.push(c.snapshot()?);
    }
    let stable = imgs[0].rgba == imgs[1].rgba;
    Ok((imgs.remove(0), stable))
}

fn scenario_list(args: &Args) -> Vec<String> {
    match args.get("scenarios", "all").as_str() {
        "all" => scenarios::STATIC
            .iter()
            .map(|s| s.name.to_string())
            .collect(),
        s => s.split(',').map(|x| x.trim().to_string()).collect(),
    }
}

/// snap：输出截图与指纹到目录（默认 artifacts/snapshots/current）。
fn snap_to(args: &Args, out: &Path) -> Res<(Vec<(String, PathBuf)>, bool)> {
    let size = args.size()?;
    let mut files = Vec::new();
    let mut all_stable = true;
    for name in scenario_list(args) {
        for scale in args.scales()? {
            let (img, stable) = render_stable(args, &name, scale)?;
            let stem = file_stem(&name, size, scale);
            let png = out.join(format!("{stem}.png"));
            img.write_png(&png)?;
            write_json(
                &compare::sidecar(&png),
                &json!({"scenario": name, "logical_size": [size.0, size.1], "scale": scale,
                        "physical_size": [img.w, img.h], "stable": stable,
                        "fingerprint": compare::fingerprint()}),
            )?;
            println!(
                "{} {}",
                if stable {
                    "截图"
                } else {
                    "截图（不稳定！）"
                },
                png.display()
            );
            all_stable &= stable;
            files.push((name.clone(), png));
        }
    }
    Ok((files, all_stable))
}

fn cmd_snap(args: &Args) -> Res<i32> {
    let (_, stable) = snap_to(
        args,
        &args.dir("out", &format!("{ARTIFACTS}/snapshots/current")),
    )?;
    if !stable {
        println!("存在连续渲染不一致的场景，不能作为回归信号（退出码 3）");
    }
    Ok(if stable { 0 } else { 3 })
}

fn cmd_bless(args: &Args) -> Res<i32> {
    let base = args.dir("baseline", BASELINES);
    let (_, stable) = snap_to(args, &base)?;
    if !stable {
        println!("拒绝：存在不稳定场景，已写出的基线不可信，请先排查（退出码 3）");
        return Ok(3);
    }
    println!("基线已更新：{}。请人工确认差异后再提交。", base.display());
    Ok(0)
}

fn cmd_compare(args: &Args) -> Res<i32> {
    let base = args.dir("baseline", BASELINES);
    let cur = args.dir("out", &format!("{ARTIFACTS}/snapshots/current"));
    let diff_dir = args.dir("diff", &format!("{ARTIFACTS}/snapshots/diff"));
    let opts = compare::Opts {
        channel_threshold: args.get("channel-threshold", "2").parse()?,
        max_ratio: args.get("max-ratio", "0.0005").parse()?,
    };
    let (files, stable) = snap_to(args, &cur)?;
    let size = args.size()?;
    let mut report = Vec::new();
    let mut statuses = Vec::new();
    for (name, png) in &files {
        let fname = png.file_name().unwrap();
        let diff_png = diff_dir.join(format!(
            "{}.diff.png",
            fname.to_string_lossy().trim_end_matches(".png")
        ));
        let _ = std::fs::remove_file(&diff_png);
        let o = compare::compare(&base.join(fname), png, &diff_png, &opts)?;
        let diff_note = o.detail["diff_image"]
            .as_str()
            .map(|d| format!("  差异图：{d}"))
            .unwrap_or_default();
        println!("{:<18} {}{diff_note}", o.status, fname.to_string_lossy());
        statuses.push(o.status);
        report.push(json!({"scenario": name, "file": fname.to_string_lossy(), "status": o.status, "detail": o.detail}));
    }
    let _ = size;
    write_json(
        &diff_dir.join("compare-report.json"),
        &json!({"results": report, "stable": stable}),
    )?;
    let code = if !stable {
        3
    } else {
        compare::exit_code(&statuses)
    };
    println!(
        "比较完成，退出码 {code}（0 一致；1 回归；2 缺基线或环境不同无法判断；3 截图不稳定）。报告：{}",
        diff_dir.join("compare-report.json").display()
    );
    Ok(code)
}

/// live：磁盘、收藏与目录变化同步脚本（真实文件系统通知与 subst 虚拟盘，结果取决于真实时钟，只断言不比对记录）。
fn cmd_live(args: &Args) -> Res<i32> {
    let review = args.opts.get("review-dir").map(PathBuf::from);
    let (items, fails) = live::run(review.clone())?;
    let out = args.dir("out", &format!("{ARTIFACTS}/state/live.json"));
    write_json(
        &out,
        &json!({"meta": meta(args, 1.0)?, "failures": fails, "steps": items}),
    )?;
    println!(
        "磁盘／收藏／目录变化同步：{} 步，{}；输出 {}",
        items.len(),
        if fails.is_empty() {
            "全部通过"
        } else {
            "存在失败"
        },
        out.display()
    );
    for f in &fails {
        println!("  失败：{f}");
    }
    Ok(if fails.is_empty() { 0 } else { 1 })
}

/// search：搜索脚本（目录／全盘搜索、取消、过期结果、增强后端故障注入与回退），搜索任务手动驱动，结果确定。
fn cmd_search(args: &Args) -> Res<i32> {
    let (items, fails) = search::run()?;
    let out = args.dir("out", &format!("{ARTIFACTS}/state/search.json"));
    write_json(
        &out,
        &json!({"meta": meta(args, 1.0)?, "failures": fails, "steps": items}),
    )?;
    println!(
        "搜索脚本：{} 步，{}；输出 {}",
        items.len(),
        if fails.is_empty() {
            "全部通过"
        } else {
            "存在失败"
        },
        out.display()
    );
    for f in &fails {
        println!("  失败：{f}");
    }
    Ok(if fails.is_empty() { 0 } else { 1 })
}

/// search-bench：基本搜索的真实规模与耗时。`--dir=目录` 目录范围，`--all` 全部本机磁盘，`--query=` 查询（默认 `.zzzq`，几乎无命中，
/// 测的是纯遍历），`--repeat=` 次数。只输出统计，不记录路径或文件名；不碰样例目录。
fn cmd_search_bench(args: &Args) -> Res<i32> {
    let dir = args.opts.get("dir").map(PathBuf::from);
    let all = args.opts.contains_key("all");
    if dir.is_none() && !all {
        return Err("search-bench 需要 --dir=目录 或 --all true".into());
    }
    let query = args.get("query", ".zzzq");
    let repeat: usize = args.get("repeat", "2").parse()?;
    let rows = search::bench(dir, all, &query, repeat)?;
    let out = args.dir("out", &format!("{ARTIFACTS}/state/search-bench.json"));
    write_json(
        &out,
        &json!({"query_len": query.chars().count(), "runs": rows}),
    )?;
    println!("统计输出 {}", out.display());
    Ok(0)
}

fn cmd_frames(args: &Args) -> Res<i32> {
    let scale: f32 = args.get("scale", "1").parse()?;
    let size = args.size()?;
    let name = args.get("scenario", "loading-appear");
    let sc = scenarios::find_anim(&name)?;
    let p = frames::Params {
        interval_ms: args.get("interval", "16").parse()?,
        duration_ms: args.get("duration", "600").parse()?,
    };
    let out = args.dir(
        "out",
        &format!("{ARTIFACTS}/frames/{}", file_stem(&name, size, scale)),
    );
    let c = Ctx::new(size, scale)?;
    let summary = frames::capture(&c, sc, &p, &out)?;
    let op = &summary["loading"]["opacity"];
    println!(
        "帧序列：{}（间隔 {}ms，时长 {}ms）",
        out.display(),
        p.interval_ms,
        p.duration_ms
    );
    println!(
        "loading.opacity：起点 {} 终点 {} 过冲 {} 稳定于 {}ms",
        op["start"], op["end"], op["overshoot"], op["settle_ms"]
    );
    Ok(0)
}

/// review：把已有产物整理成可推送的 3 级审阅清单（编号、内容、预期效果）。
fn cmd_review(args: &Args) -> Res<i32> {
    let root = args.dir("dir", ARTIFACTS);
    let size = args.size()?;
    let mut md = String::from(
        "# 界面审阅材料\n\n对照每条的“预期效果”查看对应图片，并给出通过或意见。截图对比只证明与基线一致，视觉好坏需人工判断。\n\n",
    );
    let mut n = 0;
    for s in scenarios::STATIC {
        for scale in args.scales()? {
            let rel = format!("snapshots/current/{}.png", file_stem(s.name, size, scale));
            if root.join(&rel).exists() {
                n += 1;
                md += &format!(
                    "{n}. 静态场景 `{}`，缩放 {}%\n   - 内容：{}\n   - 预期效果：{}\n   - 文件：{rel}\n",
                    s.name,
                    pct(scale),
                    s.content,
                    s.expected
                );
            }
        }
    }
    for s in scenarios::ANIM {
        for scale in args.scales()? {
            let dir = format!("frames/{}", file_stem(s.name, size, scale));
            if root.join(&dir).join("contact-sheet.png").exists() {
                n += 1;
                md += &format!(
                    "{n}. 动画场景 `{}`，缩放 {}%\n   - 内容：{}\n   - 预期效果：{}\n   - 文件：{dir}/contact-sheet.png（联系表，每格上方左为帧号、右为毫秒）、{dir}/anim.gif\n",
                    s.name,
                    pct(scale),
                    s.content,
                    s.expected
                );
            }
        }
    }
    if n == 0 {
        return Err("未发现任何产物，请先运行 snap 与 frames".into());
    }
    let out = root.join("review/REVIEW.md");
    std::fs::create_dir_all(out.parent().unwrap())?;
    std::fs::write(&out, md)?;
    println!("审阅清单：{}（共 {n} 项）", out.display());
    Ok(0)
}

fn main() {
    let code = (|| -> Res<i32> {
        let args = Args::parse()?;
        ctx::init_platform();
        if args.cmd != "review" && args.cmd != "search-bench" {
            fixture::ensure()?;
        }
        match args.cmd.as_str() {
            "tree" => cmd_tree(&args),
            "run" => cmd_run(&args),
            "snap" => cmd_snap(&args),
            "bless" => cmd_bless(&args),
            "compare" => cmd_compare(&args),
            "frames" => cmd_frames(&args),
            "live" => cmd_live(&args),
            "search" => cmd_search(&args),
            "search-bench" => cmd_search_bench(&args),
            "review" => cmd_review(&args),
            c => Err(format!(
                "未知子命令 {c}；可用：tree run snap bless compare frames live search search-bench review"
            )
            .into()),
        }
    })()
    .unwrap_or_else(|e| {
        eprintln!("错误：{e}");
        10
    });
    std::process::exit(code);
}
