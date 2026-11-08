//! 系统打开入口（Issue #16 研究与原型；#17 文件夹打开与单实例；#18 文件定位复用）。
//!
//! - [`args`]：外部打开请求的参数解析（本应用接管动词、explorer 风格 `/select,` 等），纯函数。
//! - [`request`]：请求 → 交给应用（激活／打开文件夹）或交回 Windows（虚拟位置），以及跨进程请求的格式。
//! - [`instance`]：每个数据目录一个实例；第二个进程把请求经命名管道转发给主实例，有超时与回退。
//! - [`takeover`]：接管注册计划、写入前备份、应用、恢复与复核；与注册表根无关，附内存实现。
//! - [`manage`]：启用、停用、修复与启动自检（状态、备份文件、测试用注册表根覆盖 `SFM_TEST_REG_ROOT`）。
//! - [`winreg`]：Windows 注册表实现与测试专用根。
//!
//! 入口清单与研究见 `docs/research/0016-system-entry-takeover.md`；用户可见行为、注册表键与手工恢复见
//! `docs/system-entry.md`。只有用户在设置里启用接管时才写真实的 `HKCU\Software\Classes`。

pub mod args;
pub mod instance;
pub mod manage;
pub mod request;
pub mod shellwin;
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
