//! Issue #5 缩略图路线原型。子命令：
//!
//! - `gen`：生成夹具（%TEMP%\sfm-thumbs）
//! - `bench --dir=目录 --route=auto|libwebp-scaled|libwebp-fast|libwebp-full|rust-image|shell [--workers=N] [--size=256]`
//! - `quality --dir=目录 [--dump=目录]`：各路线与“整幅解码+缩放”的画质差
//! - `alpha --file=带透明的 webp`
//! - `scenario --dir=目录 --kind=enter|scroll|dirswitch --condition=cold|warm|restart ...`
//!
//! 输出为单行 JSON（`--out=文件` 另存）。只含统计，不含路径、文件名、图像。

mod bench;
mod cache;
mod common;
mod decode;
mod fixture;
mod pipeline;
mod scenario;
mod shell;
mod webp;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("gen") => fixture::cmd(&args[1..]),
        Some("bench") => bench::cmd(&args[1..]),
        Some("quality") => bench::quality(&args[1..]),
        Some("alpha") => bench::alpha_check(&args[1..]),
        Some("scenario") => scenario::cmd(&args[1..]),
        _ => {
            eprintln!("用法见 src/main.rs 顶部注释");
            2
        }
    };
    std::process::exit(code);
}
