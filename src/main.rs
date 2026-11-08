//! 文件管理器主程序。
//!
//! 参数：
//! - `--open=<路径>`：启动时打开的位置（默认主目录）。
//! - `--smoke`：运行约 1 秒后自动退出，退出码 0，用于自动化启动验证。`--smoke-ms=N` 指定时长（隐含 `--smoke`）。
//! - `--data-dir=<目录>`：会话（标签、位置、排序、视图）的保存目录，默认 `%LOCALAPPDATA%\slint-file-manager`
//!   （也可用环境变量 `SFM_DATA_DIR`）；`--no-session` 不恢复也不保存会话。
//!   `--smoke`／`--bench-enter` 默认不读写会话，除非同时给出 `--data-dir=`。
//! - `--renderer=gl|software`（或环境变量 `SFM_RENDERER`）：手动指定渲染器，默认 femtovg（OpenGL），初始化失败时
//!   回退软件渲染；Slint 自带的 `SLINT_BACKEND` 仍然有效但优先级更低。见 `render` 模块与 ADR 0010（Issue #39）。
//! - `--debug-perf`（或环境变量 `SFM_DEBUG_PERF=1`）：记录帧耗时与界面事件（含进入目录耗时）到 `logs/`，默认关闭。
//! - `--bench-enter=<路径>`：进入目录计时，在该目录与其上级之间往返 `--bench-runs=N`（默认 10）次后退出；
//!   隐含 `--debug-perf`。`--bench-interval-ms=N` 为每一步间隔（默认 400，缩略图计时用 2500）。
//!   计时模式下左右面板都从目标的上级目录开始，不读取主目录；`--bench-single` 以单栏开始（默认双栏）。用法见 scripts/perf-browse.ps1、scripts/perf-thumbs.ps1。
//! - 缩略图持久缓存在数据目录的 `thumbs` 子目录（环境变量 `SFM_THUMB_CACHE_DIR` 可另行指定）；
//!   没有数据目录（`--no-session`、烟雾／计时模式未给 `--data-dir=`）且没有该环境变量时只用内存缓存。
//! - `--entry [目标]`（必须是最后一个参数）：系统入口接管注册的命令（`"<exe>" --entry "%1"`、Win+E 为 `"<exe>" --entry`），
//!   目标按原始命令行解析（见 `sysentry::args`）。没有任何 `--` 开关时，参数按 explorer 风格解析（裸路径、`/select,<项目>`）。
//!   此电脑、控制面板等虚拟位置用系统 `open` 动词交回资源管理器。见 docs/system-entry.md（Issue #17）。
//! - 单实例：有数据目录时每个数据目录一个实例（计时模式除外）。第二个进程把请求（打开位置或激活窗口）转发给主实例后退出；
//!   主实例无响应（超时默认 5 秒，测试可用环境变量 `SFM_INSTANCE_TIMEOUT_MS` 缩短）时独立运行且不保存会话。
//!   `--print-instance-name` 输出该数据目录的管道名后退出（测试用）。

#![windows_subsystem = "windows"]

use slint::ComponentHandle;
use slint_file_manager::browse::session::SessionStore;
use slint_file_manager::sysentry::instance::{self, Outcome};
use slint_file_manager::sysentry::request::{self, Request, Route};
use slint_file_manager::sysentry::{self, args};
use slint_file_manager::{MainWindow, Theme, app, perf, render, shell};
use std::path::PathBuf;
use std::time::Duration;

fn arg_value<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter().find_map(|a| a.strip_prefix(key))
}

fn main() -> Result<(), slint::PlatformError> {
    attach_console();
    // `--entry` 总在最后（本应用注册的命令），它之后的部分只按原始命令行解析（见 sysentry::args）
    let mut args: Vec<String> = std::env::args()
        .take_while(|a| !a.eq_ignore_ascii_case(args::ENTRY_FLAG))
        .collect();
    let raw = sysentry::raw_command_line();
    let tail = args::strip_program(&raw);
    let cwd = std::env::current_dir().ok();
    let entry = match args::split_entry(tail) {
        Some(part) => Some(request::route_args(part, cwd.as_deref())),
        // 没有任何 -- 开关的参数：explorer 风格（裸路径、/select,…）
        None if args.len() > 1 && args[1..].iter().all(|a| !a.starts_with("--")) => {
            Some(request::route_args(tail, cwd.as_deref()))
        }
        None => None,
    };
    let entry_req = match entry {
        Some(Route::HandBack { target, reason }) => match request::hand_back(&target) {
            // 虚拟位置交回资源管理器（系统 open 动词，不会回到本应用）
            Ok(()) => {
                println!("entry: 交回 Windows（{reason}）：{target}");
                return Ok(());
            }
            Err(e) => {
                eprintln!("entry: 交回 Windows 失败（{target}）：{e}");
                Some(Request::Activate)
            }
        },
        Some(Route::App(r)) => Some(r),
        None => None,
    };
    let bench_target = arg_value(&args, "--bench-enter=").map(PathBuf::from);
    if bench_target.is_some() && !args.iter().any(|a| a == "--debug-perf") {
        args.push("--debug-perf".into());
    }
    let bench_runs: u32 = arg_value(&args, "--bench-runs=")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let bench_interval = Duration::from_millis(
        arg_value(&args, "--bench-interval-ms=")
            .and_then(|v| v.parse().ok())
            .unwrap_or(400),
    );
    let smoke_ms: Option<u64> = args.iter().find_map(|a| {
        if a == "--smoke" {
            Some(1000)
        } else {
            a.strip_prefix("--smoke-ms=").and_then(|v| v.parse().ok())
        }
    });
    let start = arg_value(&args, "--open=").map(PathBuf::from);
    let benching_requested = bench_target.is_some();

    // 会话位置：--data-dir= 显式指定 > SFM_DATA_DIR > %LOCALAPPDATA%\slint-file-manager。
    // --no-session 不恢复也不保存；烟雾／计时模式只在显式指定数据目录时才读写会话，避免污染用户真实会话。
    let explicit_dir = arg_value(&args, "--data-dir=").map(PathBuf::from);
    let mut store = if args.iter().any(|a| a == "--no-session") {
        None
    } else if let Some(d) = explicit_dir {
        Some(SessionStore::at(d))
    } else if smoke_ms.is_some() || benching_requested {
        None
    } else {
        SessionStore::default_location()
    };
    if args.iter().any(|a| a == "--print-instance-name") {
        // 测试用：输出该数据目录对应的单实例管道名
        match &store {
            Some(s) => println!("{}", instance::pipe_name(s.dir())),
            None => println!("（没有数据目录，不启用单实例）"),
        }
        return Ok(());
    }

    // 单实例（每个数据目录一个）：已有主实例时把请求转发过去并退出；主实例无响应时独立运行（只读会话）
    let mut instance_note = None;
    let listener = match &store {
        Some(s) if !benching_requested => {
            let req = entry_req.clone().unwrap_or_else(|| match &start {
                Some(p) => Request::Open {
                    folder: p.clone(),
                    select: None,
                },
                None => Request::Activate,
            });
            match become_primary(&instance::pipe_name(s.dir()), &req) {
                Primary::Listener(l) => Some(l),
                Primary::Forwarded => return Ok(()),
                Primary::Standalone(note) => {
                    if note.is_some() {
                        store = store.map(SessionStore::read_only);
                    }
                    instance_note = note;
                    None
                }
            }
        }
        _ => None,
    };

    // 必须在创建任何窗口之前：选定渲染器并关闭原生窗口透明（Issue #39）
    let renderer = render::select(&args)?;
    let window = MainWindow::new()?;
    window
        .global::<Theme>()
        .set_text_scale(shell::text_scale_factor());
    let perf_log = perf::setup_if_requested(&window, &args);
    perf::event(
        match renderer.renderer {
            Some(render::Renderer::Gl) => "renderer-forced-gl",
            Some(render::Renderer::Software) => "renderer-forced-software",
            None if std::env::var_os("SLINT_BACKEND").is_some() => "renderer-slint-backend-env",
            None => "renderer-default",
        },
        None,
    );
    // 外部请求打开的是已存在的目录：没有可恢复的会话时作为左面板起点（与 --open= 相同）
    let start = match &entry_req {
        Some(Request::Open {
            folder,
            select: None,
        }) if folder.is_dir() => Some(folder.clone()),
        _ => start,
    };
    let mut env = app::Env::production(start, store);
    if let Some(parent) = bench_target.as_deref().and_then(|t| t.parent()) {
        // 计时：两侧面板都从目标的上级开始（不读取主目录，另一面板也不产生缩略图请求）
        env.home = Some(parent.to_path_buf());
        if env.start.is_none() {
            env.start = Some(parent.to_path_buf());
        }
        if args.iter().any(|a| a == "--bench-single") {
            env.fresh_split = false;
        }
    }
    let controller = app::wire(&window, env);
    if let Some(l) = listener {
        l.serve(app::external_handler(&controller));
    }
    if let Some(n) = &instance_note {
        controller.add_notice(n);
    }
    // 外部请求：复用或新建标签、选中目标、不可用路径的说明（已打开的标签会被激活而不是重复打开）
    if let Some(r) = entry_req.filter(|r| *r != Request::Activate) {
        let reply = controller.handle_request(r);
        println!("entry: {}", reply.message);
    }
    controller.entry_self_check();

    let benching = bench_target.is_some();
    let _bench = bench_target.map(|t| {
        app::start_bench(controller.clone(), t, bench_runs, bench_interval, || {
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
    // 接管资源管理器拖进来的文件。系统窗口在事件循环开始后才真正创建、才有 HWND，所以在循环里
    // 每 50ms 试一次（成功即停，最多 40 次）；失败时只是不能从资源管理器拖入，其他功能不受影响。
    let attach_timer = std::rc::Rc::new(slint::Timer::default());
    {
        let (c, w, t) = (controller.clone(), window.as_weak(), attach_timer.clone());
        let tries = std::cell::Cell::new(0u32);
        attach_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(50),
            move || {
                tries.set(tries.get() + 1);
                let done = w.upgrade().is_some_and(|w| app::attach_drop(&c, &w));
                if done {
                    // 最大化让出自动隐藏任务栏的 1 像素（Issue #45）
                    if let Some(h) = w
                        .upgrade()
                        .and_then(|w| slint_file_manager::dragdrop::hwnd_of(w.window()))
                    {
                        render::keep_taskbar_reachable(h);
                    }
                    t.stop();
                } else if tries.get() >= 40 {
                    t.stop();
                    eprintln!("未能登记拖放目标：不能从资源管理器拖入文件");
                }
            },
        );
    }
    controller.focus_active();
    window.run()?;
    controller.reveal_shutdown();
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

enum Primary {
    Listener(instance::Listener),
    /// 请求已转交给正在运行的实例，本进程退出。
    Forwarded,
    /// 独立运行；`Some` 为给用户的说明（主实例无响应）。
    Standalone(Option<String>),
}

/// 占用单实例管道成为主实例；已有主实例时转发请求。输出一行 `instance: …` 供真实程序测试检查。
fn become_primary(name: &str, req: &Request) -> Primary {
    for attempt in 0..2 {
        match instance::claim(name) {
            Ok(Some(l)) => {
                println!("instance: 主实例");
                return Primary::Listener(l);
            }
            Ok(None) => match instance::forward(name, req, instance::timeout()) {
                Outcome::Delivered(r) => {
                    let ok = if r.ok { "ok" } else { "fail" };
                    println!("instance: 已转交（{ok}）：{}", r.message);
                    return Primary::Forwarded;
                }
                // 主实例恰好退出：再试一次成为主实例
                Outcome::NoPrimary if attempt == 0 => continue,
                Outcome::NoPrimary => return Primary::Standalone(None),
                Outcome::Unresponsive(why) => {
                    println!("instance: 主实例无响应，独立启动：{why}");
                    return Primary::Standalone(Some(format!(
                        "已在运行的窗口没有响应（{why}），本窗口独立运行，不保存会话"
                    )));
                }
            },
            Err(e) => {
                eprintln!("instance: 无法建立单实例管道，独立运行：{e}");
                return Primary::Standalone(None);
            }
        }
    }
    Primary::Standalone(None)
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
