fn main() {
    let mut config = slint_build::CompilerConfiguration::new();
    // 元素查找与元素树导出依赖调试信息；仅在 harness 特性下生成，发布产物不带。
    if std::env::var_os("CARGO_FEATURE_HARNESS").is_some() {
        config = config.with_debug_info(true);
    }
    slint_build::compile_with_config("ui/main.slint", config).expect("编译 Slint 界面失败");
}
