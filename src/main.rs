//! 基线程序：打开最小窗口（含调试观测框架的示例交互与动画）。
//!
//! 参数：
//! - `--smoke`：运行约 1 秒后自动退出，退出码 0，用于自动化启动验证。`--smoke-ms=N` 指定时长（隐含 `--smoke`）。
//! - `--debug-perf`（或环境变量 `SFM_DEBUG_PERF=1`）：记录帧耗时与界面事件到 `logs/`，默认关闭。
//! - `--demo-autoplay`：每 500ms 自动切换示例面板，用于录屏与帧耗时复现。

use slint::ComponentHandle;
use slint_file_manager::{MainWindow, app, perf};
use std::time::Duration;

fn main() -> Result<(), slint::PlatformError> {
    let args: Vec<String> = std::env::args().collect();
    let smoke_ms: Option<u64> = args.iter().find_map(|a| {
        if a == "--smoke" {
            Some(1000)
        } else {
            a.strip_prefix("--smoke-ms=").and_then(|v| v.parse().ok())
        }
    });
    let window = MainWindow::new()?;
    app::wire(&window);
    let perf_log = perf::setup_if_requested(&window, &args);
    let _autoplay = args
        .iter()
        .any(|a| a == "--demo-autoplay")
        .then(|| app::start_autoplay(&window, Duration::from_millis(500)));

    if let Some(ms) = smoke_ms {
        slint::Timer::single_shot(Duration::from_millis(ms), || {
            let _ = slint::quit_event_loop();
        });
        // 打印便于验证记录的渲染后端信息
        println!("smoke: 启动成功，事件循环运行中");
    }

    window.run()?;
    if perf_log.is_some() {
        perf::finish();
    }
    if smoke_ms.is_some() {
        println!("smoke: 正常退出");
    }
    Ok(())
}
