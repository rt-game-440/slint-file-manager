//! 界面接线：把 Slint 回调连到业务与观测事件。主程序与 harness 共用。

use crate::{MainWindow, perf};
use slint::ComponentHandle;
use std::time::Duration;

/// 示例动画时长（毫秒），需与 ui/main.slint 中 `animate` 的 duration 保持一致，
/// 仅用于在调试日志中标记“动画结束”。
pub const DEMO_ANIMATION_MS: u64 = 400;

/// 连接示例交互的回调。调试日志关闭时 `perf::event` 直接返回。
pub fn wire(window: &MainWindow) {
    window.on_demo_animation_toggled(|_open| {
        perf::event("animation-start", None);
        perf::set_animating(true);
        slint::Timer::single_shot(Duration::from_millis(DEMO_ANIMATION_MS + 50), || {
            perf::set_animating(false);
            perf::event("animation-end", None);
        });
    });
    window.on_demo_scrolled(|| perf::event("scroll", None));
}

/// 示例动画自动播放：用于录屏或帧耗时日志复现，周期性切换面板。
pub fn start_autoplay(window: &MainWindow, interval: Duration) -> slint::Timer {
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    timer.start(slint::TimerMode::Repeated, interval, move || {
        if let Some(w) = weak.upgrade() {
            w.invoke_demo_toggle();
        }
    });
    timer
}
