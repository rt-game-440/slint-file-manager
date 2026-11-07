//! 真实云盘受控实验（Issue #14）：`harness cloud-lab --root=<云盘同步目录>`。
//!
//! 只在**本命令自己新建的临时测试文件夹**（`sfm-issue14-test-*`）里做会改变云盘状态的事，用户原有文件一个都不碰：
//! 1. 新建 3 个小文件，等提供方接管（占位文件、与云端一致）→ 状态应为“本地可用”。
//! 2. 对文件 A 设“始终保留”（`attrib +P -U`，与右键菜单“始终保留在此设备上”同一机制）→ 应为“始终保留”。
//! 3. 对文件 B、C 设“释放空间”（`attrib -P +U`）→ 应为“仅在线”（本地内容被清掉，云端保留）。
//! 4. 对仅在线的 B、C 走产品里所有不应下载的入口（目录枚举、状态判定、原生缩略图守卫、预览守卫、系统缩略图仅缓存、搜索遍历），
//!    再核对 B、C 仍是“仅在线”（没有被下载）。
//! 5. 对照：显式读取 C 的内容（相当于用户明确打开）→ 应变为“本地可用”，证明水合检测有效。
//! 6. 恢复：A 取消固定、B 显式读取（恢复为本地可用），逐个核对三个文件都回到“本地可用、固定状态未指定”。
//! 7. 删除测试文件夹并核对已删除（云盘里的删除会同步到云端，进入提供方的回收站，由提供方规则保留一段时间）。
//!
//! 只输出本命令自己的测试文件的状态（名称是固定的 a/b/c，不含用户文件名），写入 `artifacts/state/cloud-lab-<标签>.json`。
//! 无论中途成败，最后都会尝试删除测试文件夹。

use crate::ctx::Res;
use serde_json::{Value, json};
use slint_file_manager::browse::entry::list_dir_with;
use slint_file_manager::cloud::{self, CloudProbe, Guard, SYSTEM};
use slint_file_manager::preview::{self, Failure};
use slint_file_manager::search::{
    BackendPref, Cancel, Event, Query, Request, Scope, engine::run as search_run,
};
use slint_file_manager::thumbs::decode::{Limits, generate, samples};
use slint_file_manager::thumbs::shell::{ShellProvider, SystemShell};
use slint_file_manager::thumbs::{Fail, Source};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PREFIX: &str = "sfm-issue14-test-";

struct Lab {
    dir: PathBuf,
    steps: Vec<Value>,
    problems: Vec<String>,
}

fn attrib(flags: &[&str], p: &Path) -> Res<()> {
    let ok = std::process::Command::new("attrib")
        .args(flags)
        .arg(p)
        .status()?
        .success();
    if ok {
        Ok(())
    } else {
        Err(format!("attrib {flags:?} 失败").into())
    }
}

impl Lab {
    /// 产品路径给出的状态代号（目录枚举 + 云文件 API）。
    fn state(&self, name: &str) -> String {
        match list_dir_with(&self.dir, &SYSTEM) {
            Ok((v, _)) => v
                .into_iter()
                .find(|e| e.name == name)
                .map_or("缺失".into(), |e| e.cloud.code().to_string()),
            Err(_) => "读取失败".into(),
        }
    }

    fn detail(&self, name: &str) -> Value {
        let p = self.dir.join(name);
        let attrs = std::fs::metadata(&p).map(|m| m.file_attributes()).ok();
        let ph = SYSTEM.placeholder(&p);
        json!({
            "file": name,
            "state": self.state(name),
            "attrs": attrs.map(|a| format!("{a:#x}")),
            "recall_attr": attrs.map(cloud::is_recall),
            "pinned_attr": attrs.map(|a| a & cloud::ATTR_PINNED != 0),
            "unpinned_attr": attrs.map(|a| a & cloud::ATTR_UNPINNED != 0),
            "placeholder_on_disk_bytes": ph.map(|x| x.on_disk),
            "placeholder_pin": ph.map(|x| format!("{:?}", x.pin)),
            "placeholder_in_sync": ph.map(|x| x.in_sync),
        })
    }

    /// 轮询直到状态达到 `want`（最长 `secs` 秒）；返回耗时毫秒，未达到为 `None`。
    fn wait(&self, name: &str, want: &str, secs: u64) -> Option<u64> {
        let t0 = Instant::now();
        loop {
            if self.state(name) == want {
                return Some(t0.elapsed().as_millis() as u64);
            }
            if t0.elapsed() > Duration::from_secs(secs) {
                return None;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn record(&mut self, step: &str, ok: bool, detail: Value) {
        println!("  [{}] {step}", if ok { "通过" } else { "未通过" });
        if !ok {
            self.problems.push(step.to_string());
        }
        self.steps
            .push(json!({"step": step, "ok": ok, "detail": detail}));
    }
}

fn run_inner(lab: &mut Lab, wait_secs: u64) -> Res<()> {
    std::fs::create_dir(&lab.dir)?;
    std::fs::write(lab.dir.join("a.txt"), vec![b'a'; 2048])?;
    std::fs::write(
        lab.dir.join("b.png"),
        samples::png(&samples::pattern(64, 48, 7)),
    )?;
    std::fs::write(lab.dir.join("c.txt"), vec![b'c'; 2048])?;

    // 1. 等提供方接管：本地可用
    let w = [
        lab.wait("a.txt", "local", wait_secs),
        lab.wait("b.png", "local", wait_secs),
        lab.wait("c.txt", "local", wait_secs),
    ];
    let d = json!([
        lab.detail("a.txt"),
        lab.detail("b.png"),
        lab.detail("c.txt")
    ]);
    lab.record(
        "1 新建后提供方接管，三个文件均为“本地可用”",
        w.iter().all(|x| x.is_some()),
        json!({"seconds_to_local_ms": w, "files": d}),
    );
    if !w.iter().all(|x| x.is_some()) {
        return Ok(());
    }

    // 2. 始终保留
    attrib(&["+P", "-U"], &lab.dir.join("a.txt"))?;
    let t = lab.wait("a.txt", "pinned", 30);
    lab.record(
        "2 设“始终保留”后显示为“始终保留”",
        t.is_some(),
        json!({"ms": t, "file": lab.detail("a.txt")}),
    );

    // 3. 释放空间（提供方异步执行：b 必须在限时内变为仅在线；c 只记录，释放可能更晚或不发生）
    attrib(&["-P", "+U"], &lab.dir.join("b.png"))?;
    attrib(&["-P", "+U"], &lab.dir.join("c.txt"))?;
    let tb = lab.wait("b.png", "online", wait_secs);
    let tc = lab.wait("c.txt", "online", 20);
    lab.record(
        "3 释放空间后 b.png 显示为“仅在线”（云端内容保留）",
        tb.is_some(),
        json!({"b_ms": tb, "c_ms_or_null_if_not_freed_in_20s": tc,
               "b": lab.detail("b.png"), "c": lab.detail("c.txt")}),
    );
    if tb.is_none() {
        return Ok(());
    }

    // 4. 不应下载的入口
    let b = lab.dir.join("b.png");
    let src = {
        let md = std::fs::metadata(&b)?;
        Source::new(b.clone(), md.len(), md.modified().ok())
    };
    let native = generate(&src, &Limits::default()).err();
    let prev = preview::decode_file(&b, &preview::Limits::default()).err();
    let guard = cloud::open_local_only(&b).err();
    let shell = SystemShell.thumbnail(&b, true).map(|_| "有缓存");
    let hits = Mutex::new(0usize);
    let req = Request {
        query: Query::parse("png"),
        scope: Scope::Dir(lab.dir.clone()),
        pref: BackendPref::BasicOnly,
    };
    search_run(&req, None, &Cancel::new(), &|e| {
        if let Event::Hits(h) = e {
            *hits.lock().unwrap() += h.len();
        }
    });
    let hits = hits.into_inner().unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let sb = lab.state("b.png");
    let sc_before = tc.is_some();
    let sc = lab.state("c.txt");
    lab.record(
        "4 浏览类入口都没有下载仅在线文件",
        native == Some(Fail::Cloud)
            && prev == Some(Failure::Cloud)
            && guard == Some(Guard::Cloud)
            && hits >= 1
            && sb == "online"
            && (!sc_before || sc == "online"),
        json!({
            "native_thumbnail": native.map(|f| f.as_str()),
            "preview_guard": prev.map(|f| format!("{f:?}")),
            "open_local_only": guard.map(|g| format!("{g:?}")),
            "shell_cache_only": match shell { Ok(s) => s.to_string(), Err(f) => f.as_str().to_string() },
            "search_hits_for_online_png": hits,
            "b_state_after": sb, "c_state_after": sc,
        }),
    );

    // 5. 对照：显式读取 = 用户明确打开 → 下载
    let bytes = std::fs::read(lab.dir.join("b.png"))?;
    let tl = lab.wait("b.png", "local", 60);
    lab.record(
        "5 对照：显式读取 b.png 后变为“本地可用”（水合检测有效）",
        tl.is_some() && !bytes.is_empty(),
        json!({"ms": tl, "bytes": bytes.len(), "file": lab.detail("b.png")}),
    );

    // 6. 恢复并逐个核对
    attrib(&["-P"], &lab.dir.join("a.txt"))?;
    attrib(&["-U"], &lab.dir.join("b.png"))?;
    attrib(&["-U"], &lab.dir.join("c.txt"))?;
    // c.txt 若仍处于仅在线（提供方延迟释放），显式读取一次让它回到本地可用
    if lab.state("c.txt") == "online" {
        let _ = std::fs::read(lab.dir.join("c.txt"))?;
    }
    let ta = lab.wait("a.txt", "local", 30);
    let tb2 = lab.wait("b.png", "local", 30);
    let tc2 = lab.wait("c.txt", "local", 60);
    let unspecified = |n: &str| {
        SYSTEM
            .placeholder(&lab.dir.join(n))
            .is_some_and(|p| p.pin == cloud::Pin::Unspecified)
    };
    lab.record(
        "6 恢复原状：三个文件逐个核对均为“本地可用、固定状态未指定”",
        ta.is_some()
            && tb2.is_some()
            && tc2.is_some()
            && unspecified("a.txt")
            && unspecified("b.png")
            && unspecified("c.txt"),
        json!({"a": lab.detail("a.txt"), "b": lab.detail("b.png"), "c": lab.detail("c.txt")}),
    );
    Ok(())
}

pub fn run(root: &Path, label: &str, wait_secs: u64, out: &Path) -> Res<i32> {
    if !root.is_dir() {
        return Err(format!("--root 不是目录：{label}").into());
    }
    if cloud::SYSTEM.sync_root(root).is_none() {
        return Err("--root 不在任何云盘同步根内，拒绝运行".into());
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let dir = root.join(format!("{PREFIX}{stamp}"));
    println!("云盘受控实验（{label}）：临时测试文件夹 {PREFIX}{stamp}");
    let mut lab = Lab {
        dir: dir.clone(),
        steps: Vec::new(),
        problems: Vec::new(),
    };
    let r = run_inner(&mut lab, wait_secs);
    if let Err(e) = &r {
        lab.problems.push(format!("实验中断：{e}"));
        println!("  实验中断：{e}");
    }
    // 7. 无论成败都删除自己的测试文件夹（只删名字带前缀、且直接位于 --root 下的文件夹）
    let safe = dir
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with(PREFIX))
        && dir.parent() == Some(root);
    let removed = if safe && dir.exists() {
        // 先取消只读／固定标记，避免个别提供方对固定文件的删除限制
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let _ = attrib(&["-P", "-U"], &e.path());
            }
        }
        std::fs::remove_dir_all(&dir).is_ok()
    } else {
        !dir.exists()
    };
    std::thread::sleep(Duration::from_millis(500));
    let gone = !dir.exists();
    lab.record(
        "7 测试文件夹已删除",
        removed && gone,
        json!({"removed_call_ok": removed, "exists_after": !gone}),
    );
    let ok = lab.problems.is_empty();
    let v = json!({
        "label": label,
        "note": "只在本命令新建的临时测试文件夹内操作；删除会同步到云端并进入提供方回收站",
        "ok": ok,
        "problems": lab.problems,
        "steps": lab.steps,
    });
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(out, serde_json::to_string_pretty(&v)? + "\n")?;
    println!(
        "输出 {}；{}",
        out.display(),
        if ok {
            "全部通过"
        } else {
            "存在未通过项"
        }
    );
    Ok(if ok { 0 } else { 1 })
}
