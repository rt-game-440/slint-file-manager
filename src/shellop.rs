//! 产品的复制与移动：交给系统的 `IFileOperation`。
//!
//! - 与资源管理器同一套引擎：同卷移动是改名，跨卷移动是复制后删除源（由系统在确认目标完整后才删除源），
//!   长时间操作有系统的进度窗口与取消按钮，权限不足时系统自己弹出提升或拒绝提示。
//! - 目标路径（含最终名称）由调用方在计划阶段确定且保证不存在，本模块只做“把 src 放成 dest”。
//!   系统的“替换或跳过”询问只会在计划与执行之间有人抢先创建了同名项目时出现，由用户在系统界面中亲自选择，
//!   本应用不会代替用户选择覆盖。
//! - 结果以现实为准：返回成功当且仅当目标已出现（移动时源已消失）；用户取消映射为 [`FailKind::Aborted`]。
//! - 所有 COM 调用都在调用线程内初始化（单线程套间）并在返回前释放，可从任意工作线程调用。

use crate::fileops::{FailKind, OpError, OpResult};
use crate::recycle::{Com, map_hresult};
use crate::xfer::Mode;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::UI::Shell::{
    FOF_NOERRORUI, FOFX_NOCOPYHOOKS, FileOperation, IFileOperation, IShellItem,
    SHCreateItemFromParsingName,
};
use windows::core::PCWSTR;

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 把 `src` 复制或移动成完整路径 `dest`。
pub fn transfer(src: &Path, dest: &Path, mode: Mode) -> OpResult {
    if dest.symlink_metadata().is_ok() {
        return Err(OpError::new(FailKind::Exists, ""));
    }
    let src_md = std::fs::symlink_metadata(src).map_err(|e| OpError::from_io(&e))?;
    let (Some(dest_dir), Some(dest_name)) = (dest.parent(), dest.file_name()) else {
        return Err(OpError::new(FailKind::Other, "目标路径无效"));
    };
    if src_md.is_dir() && crate::xfer::is_inside(dest_dir, src) {
        return Err(OpError::new(FailKind::Nested, ""));
    }
    let _com = Com::init();
    let (mut aborted, mut hr_err) = (false, None);
    // SAFETY: 所有路径缓冲区都以 NUL 结尾并在调用期间有效；接口在本块内使用并释放。
    let r = unsafe {
        (|| -> windows::core::Result<()> {
            let (ws, wd) = (wide(src), wide(dest_dir));
            let src_item: IShellItem = SHCreateItemFromParsingName(PCWSTR(ws.as_ptr()), None)?;
            let dir_item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wd.as_ptr()), None)?;
            let name: Vec<u16> = dest_name.encode_wide().chain(std::iter::once(0)).collect();
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            // 保留系统进度窗口与取消；错误由本程序汇报，不再弹系统错误框
            op.SetOperationFlags(FOF_NOERRORUI | FOFX_NOCOPYHOOKS)?;
            match mode {
                Mode::Copy => op.CopyItem(&src_item, &dir_item, PCWSTR(name.as_ptr()), None)?,
                Mode::Move => op.MoveItem(&src_item, &dir_item, PCWSTR(name.as_ptr()), None)?,
            }
            let r = op.PerformOperations();
            aborted = op
                .GetAnyOperationsAborted()
                .map(|b| b.as_bool())
                .unwrap_or(false);
            r
        })()
    };
    if let Err(e) = r {
        hr_err = Some(e);
    }
    let dest_ok = dest.symlink_metadata().is_ok();
    let src_gone = src.symlink_metadata().is_err();
    if aborted && !(dest_ok && (mode == Mode::Copy || src_gone)) {
        return Err(OpError::new(FailKind::Aborted, ""));
    }
    match (hr_err, dest_ok) {
        (None, true) if mode == Mode::Copy || src_gone => Ok(()),
        (None, true) => Err(OpError::new(
            FailKind::Other,
            "已复制到目标位置，但原项目仍在；两处现在都有这个项目",
        )),
        (None, false) => Err(OpError::new(
            FailKind::Other,
            "系统报告成功，但目标位置没有出现该项目",
        )),
        (Some(e), _) => Err(map_hresult(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join("sfm-unit")
            .join(format!("shellop-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn real_copy_and_move_via_ifileoperation() {
        let root = temp("real");
        fs::create_dir_all(root.join("s").join("d").join("sub")).unwrap();
        fs::write(root.join("s").join("f.txt"), "内容").unwrap();
        fs::write(root.join("s").join("d").join("sub").join("g.txt"), "g").unwrap();
        fs::create_dir_all(root.join("t")).unwrap();
        // 复制文件，源保留
        transfer(
            &root.join("s").join("f.txt"),
            &root.join("t").join("f2.txt"),
            Mode::Copy,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("t").join("f2.txt")).unwrap(),
            "内容"
        );
        assert!(root.join("s").join("f.txt").exists());
        // 移动文件夹，源消失
        transfer(
            &root.join("s").join("d"),
            &root.join("t").join("d"),
            Mode::Move,
        )
        .unwrap();
        assert!(!root.join("s").join("d").exists());
        assert_eq!(
            fs::read_to_string(root.join("t").join("d").join("sub").join("g.txt")).unwrap(),
            "g"
        );
        // 目标已存在：直接失败，内容不变
        let e = transfer(
            &root.join("s").join("f.txt"),
            &root.join("t").join("f2.txt"),
            Mode::Copy,
        )
        .unwrap_err();
        assert_eq!(e.kind, FailKind::Exists);
        // 源不存在
        let e = transfer(
            &root.join("没有.txt"),
            &root.join("t").join("x.txt"),
            Mode::Copy,
        )
        .unwrap_err();
        assert_eq!(e.kind, FailKind::NotFound);
        // 文件夹放进自己里面
        let e = transfer(
            &root.join("t"),
            &root.join("t").join("d").join("t"),
            Mode::Copy,
        )
        .unwrap_err();
        assert_eq!(e.kind, FailKind::Nested);
    }
}
