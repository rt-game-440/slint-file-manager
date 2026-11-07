//! 系统打开入口（第二阶段，Issue #16 研究与原型；#17 文件夹打开、#18 文件定位复用）。
//!
//! - [`args`]：外部打开请求的参数解析（本应用接管动词、explorer 风格 `/select,` 等），纯函数。
//! - [`takeover`]：接管注册计划、写入前备份、应用、恢复与复核；与注册表根无关，附内存实现。
//! - [`winreg`]：Windows 注册表实现与测试专用根。
//!
//! 入口清单、适配方式、风险与未覆盖项见 `docs/research/0016-system-entry-takeover.md`。
//! 本模块目前**没有接到主程序**：不会在任何真实的文件关联位置写入。

pub mod args;
pub mod takeover;
pub mod winreg;

/// 当前进程的原始命令行（`GetCommandLineW`），交给 [`args::strip_program`] 与 [`args::parse`]。
pub fn raw_command_line() -> String {
    use windows_sys::Win32::System::Environment::GetCommandLineW;
    unsafe {
        let p = GetCommandLineW();
        if p.is_null() {
            return String::new();
        }
        let mut n = 0usize;
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }
}
