//! 基线程序：打开最小窗口。
//! `--smoke` 参数：窗口显示并运行事件循环约 1 秒后自动退出，退出码 0，用于自动化启动验证。

use std::time::Duration;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let smoke = std::env::args().any(|a| a == "--smoke");
    let window = MainWindow::new()?;

    if smoke {
        slint::Timer::single_shot(Duration::from_millis(1000), || {
            let _ = slint::quit_event_loop();
        });
        // 打印便于验证记录的渲染后端信息
        println!("smoke: 启动成功，事件循环运行中");
    }

    window.run()?;
    if smoke {
        println!("smoke: 正常退出");
    }
    Ok(())
}
