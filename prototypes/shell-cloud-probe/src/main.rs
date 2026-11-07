//! Issue #3 兼容原型（一次性验证工具，不属于主程序）。
//!
//! 子命令：
//! - `env`                       打印系统与进程占位符兼容模式
//! - `menu ...`                  原生右键菜单枚举、直接调用、TrackPopupMenu 键盘取消/选择
//! - `cloud-info <root>`         同步根提供方信息
//! - `cloud-scan <root>`         只读枚举占位文件状态（不打开文件内容）
//! - `thumb-scan <root> ...`     对一批文件按指定方式取缩略图，并观测是否发生水合（下载）
//! - `placeholder <file>`        打印单个文件的占位信息
//! - `read-file <file>`          显式读取全部内容（模拟“打开/下载”路径）

mod cloud;
mod menu;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprintln!(
            "用法：shell-cloud-probe <env|menu|cloud-info|cloud-scan|thumb-scan|placeholder|read-file> ..."
        );
        std::process::exit(2);
    };
    let rest = &args[1..];
    let code = match cmd.as_str() {
        "env" => cloud::cmd_env(),
        "menu" => menu::cmd_menu(rest),
        "cloud-info" => cloud::cmd_cloud_info(rest),
        "cloud-scan" => cloud::cmd_cloud_scan(rest),
        "thumb-scan" => cloud::cmd_thumb_scan(rest),
        "placeholder" => cloud::cmd_placeholder(rest),
        "read-file" => cloud::cmd_read_file(rest),
        _ => {
            eprintln!("未知子命令：{cmd}");
            2
        }
    };
    std::process::exit(code);
}

/// 简易参数读取：`--key value`。
pub fn opt<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

pub fn flag(args: &[String], key: &str) -> bool {
    args.iter().any(|a| a == key)
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
