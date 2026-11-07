//! 文件管理器主程序。
//!
//! 参数：
//! - `--open=<路径>`：启动时打开的位置（默认主目录）。
//! - `--smoke`：运行约 1 秒后自动退出，退出码 0，用于自动化启动验证。`--smoke-ms=N` 指定时长（隐含 `--smoke`）。
//! - `--data-dir=<目录>`：会话（标签、位置、排序、视图）的保存目录，默认 `%LOCALAPPDATA%\slint-file-manager`
//!   （也可用环境变量 `SFM_DATA_DIR`）；`--no-session` 不恢复也不保存会话。
//!   `--smoke`／`--bench-enter` 默认不读写会话，除非同时给出 `--data-dir=`。
//! - `--debug-perf`（或环境变量 `SFM_DEBUG_PERF=1`）：记录帧耗时与界面事件（含进入目录耗时）到 `logs/`，默认关闭。
//! - `--bench-enter=<路径>`：进入目录计时，在该目录与其上级之间往返 `--bench-runs=N`（默认 10）次后退出；
//!   隐含 `--debug-perf`。用法见 scripts/perf-browse.ps1。

#![windows_subsystem = "windows"]

use slint::ComponentHandle;
use slint_file_manager::browse::session::SessionStore;
use slint_file_manager::{MainWindow, Theme, app, perf, shell};
use std::path::PathBuf;
use std::time::Duration;

fn arg_value<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter().find_map(|a| a.strip_prefix(key))
}

fn main() -> Result<(), slint::PlatformError> {
    attach_console();
    let mut args: Vec<String> = std::env::args().collect();
    let bench_target = arg_value(&args, "--bench-enter=").map(PathBuf::from);
    if bench_target.is_some() && !args.iter().any(|a| a == "--debug-perf") {
        args.push("--debug-perf".into());
    }
    let bench_runs: u32 = arg_value(&args, "--bench-runs=")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let smoke_ms: Option<u64> = args.iter().find_map(|a| {
        if a == "--smoke" {
            Some(1000)
        } else {
            a.strip_prefix("--smoke-ms=").and_then(|v| v.parse().ok())
        }
    });
    let start = arg_value(&args, "--open=").map(PathBuf::from);
    let benching_requested = bench_target.is_some();

    let window = MainWindow::new()?;
    window
        .global::<Theme>()
        .set_text_scale(shell::text_scale_factor());
    let perf_log = perf::setup_if_requested(&window, &args);
    // 会话位置：--data-dir= 显式指定 > SFM_DATA_DIR > %LOCALAPPDATA%\slint-file-manager。
    // --no-session 不恢复也不保存；烟雾／计时模式只在显式指定数据目录时才读写会话，避免污染用户真实会话。
    let explicit_dir = arg_value(&args, "--data-dir=").map(PathBuf::from);
    let store = if args.iter().any(|a| a == "--no-session") {
        None
    } else if let Some(d) = explicit_dir {
        Some(SessionStore::at(d))
    } else if smoke_ms.is_some() || benching_requested {
        None
    } else {
        SessionStore::default_location()
    };
    let controller = app::wire(&window, app::Env::production(start, store));

    let benching = bench_target.is_some();
    let _bench = bench_target.map(|t| {
        app::start_bench(controller.clone(), t, bench_runs, || {
            let _ = slint::quit_event_loop();
        })
    });

    if let Some(ms) = smoke_ms {
        slint::Timer::single_shot(Duration::from_millis(ms), || {
            let _ = slint::quit_event_loop();
        });
        println!("smoke: 启动成功，事件循环运行中");
    }

    window.show()?;
    controller.focus_active();
    window.run()?;
    // 正常退出时再保存一次（定时保存之外的最后变化）
    if let Err(e) = controller.save_now() {
        eprintln!("{e}");
    }
    if let Some(p) = &perf_log {
        perf::finish();
        if benching {
            println!("bench: 日志 {}", p.display());
        }
    }
    if smoke_ms.is_some() {
        println!("smoke: 正常退出");
    }
    Ok(())
}

/// 以 GUI 子系统构建时，从终端启动也能看到 smoke／bench 输出：附着到父进程控制台（没有则忽略）。
fn attach_console() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(pid: u32) -> i32;
    }
    // SAFETY: ATTACH_PARENT_PROCESS = u32::MAX；失败时无副作用。
    unsafe {
        AttachConsole(u32::MAX);
    }
}
