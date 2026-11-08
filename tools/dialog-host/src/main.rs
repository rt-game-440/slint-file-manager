//! 文件选择对话框测试宿主（Issue #23）。
//!
//! 用法见 docs/dialog-host.md：
//!
//! ```text
//! dialog-host run <ifd|gofn|sbf> <open-single|open-multi|save|pick-folder> [--选项=值 ...]
//! dialog-host gui [--fixture=目录]        带按钮的窗口（无参数时默认）
//! dialog-host fixture [--root=目录]       重建夹具（只允许 %TEMP%\...\sfm-dialog-host\...）
//! dialog-host list                        输出方式 × 场景目录
//! ```
//!
//! 退出码：0 弹框并正常结束（含取消）；1 系统接口报错；2 参数错误。
//! 每次调用输出一行紧凑 JSON 到标准输出（UTF-8），并写日志；`--out=文件` 另存一份。

mod catalog;
mod cli;
mod fixture;
mod gofn;
mod gui;
mod ifd;
mod log;
mod outcome;
mod runner;
mod sbf;
mod util;

use std::io::Write;
use std::path::PathBuf;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Console::FreeConsole;

fn usage_error(msg: &str) -> ! {
    eprintln!(
        "参数错误：{msg}\n用法：dialog-host run <ifd|gofn|sbf> <open-single|open-multi|save|pick-folder> [--选项=值]；dialog-host gui|fixture|list；详见 docs/dialog-host.md"
    );
    std::process::exit(2);
}

fn emit(v: &serde_json::Value, out: Option<&PathBuf>) {
    let text = serde_json::to_string(v).unwrap_or_default();
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(text.as_bytes());
    let _ = o.write_all(b"\n");
    let _ = o.flush();
    if let Some(p) = out {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(p, text.as_bytes()) {
            eprintln!("警告：写入 {} 失败：{e}", p.display());
        }
    }
}

fn opt<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter().find_map(|a| a.strip_prefix(key))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("gui");
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    match cmd {
        "list" => {
            emit(&catalog::describe(), None);
        }
        "fixture" => {
            let root = opt(&args, "--root=")
                .map(PathBuf::from)
                .unwrap_or_else(fixture::default_root);
            match fixture::rebuild(&root) {
                Ok(p) => {
                    emit(
                        &serde_json::json!({"fixture": p.display().to_string(), "files": fixture::FILES}),
                        None,
                    );
                }
                Err(e) => usage_error(&e),
            }
        }
        "gui" => {
            let fx = opt(&args, "--fixture=")
                .map(PathBuf::from)
                .unwrap_or_else(fixture::default_root);
            let fx = match fixture::rebuild(&fx) {
                Ok(p) => p,
                Err(e) => usage_error(&e),
            };
            let log_file = opt(&args, "--log-file=").map(PathBuf::from);
            let log_dir = opt(&args, "--log-dir=").map(PathBuf::from);
            log::init(log_file, log_dir);
            log::line(&format!(
                "gui start bitness={} fixture={}",
                util::bitness(),
                fx.display()
            ));
            // 从资源管理器启动时不要留控制台窗口；从终端启动则只是脱离控制台
            unsafe {
                let _ = FreeConsole();
            }
            gui::run_gui(fx);
        }
        "run" => {
            if args.len() < 3 {
                usage_error("run 需要 方式 与 场景");
            }
            let (req, common) = match cli::Request::parse(&args[1], &args[2], &args[3..]) {
                Ok(x) => x,
                Err(e) => usage_error(&e),
            };
            log::init(common.log_file.clone(), common.log_dir.clone());
            log::line(&format!(
                "run start bitness={} args={:?}",
                util::bitness(),
                &args[1..]
            ));
            let owner = (req.owner == "window").then(gui::create_owner);
            let (record, code) = runner::execute(&req, owner);
            if let Some(h) = owner {
                gui::destroy(h);
            }
            emit(&record, common.out.as_ref());
            std::process::exit(code);
        }
        other => usage_error(&format!("未知命令 {other:?}")),
    }
}
