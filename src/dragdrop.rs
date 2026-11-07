//! 与资源管理器之间的拖放（OLE）。
//!
//! - **放进来**：winit 自带的拖放目标对任何拖入一律答复“复制”，既没法移动也没法给出正确的提示，所以主窗口建好后
//!   [`attach`] 撤销它并登记自己的 `IDropTarget`。拖放期间由 [`DropHost`]（主程序的控制器）按指针位置与键盘修饰键
//!   决定效果与提示；释放时由本程序自己执行复制或移动，**移动时回答“没有任何效果”**——告诉来源“我已经自己搬完了”，
//!   来源就不会再去删它自己的文件。
//! - **拖出去**：[`start_drag`] 用外壳的数据对象（与资源管理器拖出文件同一种）和 `SHDoDragDrop` 发起拖放，
//!   阻塞到用户释放或取消为止。这期间 winit 不会重绘（OLE 的模态循环），属于已知限制。
//! - 窗口句柄来自 Slint 的 `raw-window-handle-06`；只在界面线程使用。

use std::path::PathBuf;
use std::rc::Rc;
use windows::Win32::Foundation::{HWND, POINTL};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl, OleInitialize, RegisterDragDrop,
    ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, IShellItemArray, SHCreateShellItemArrayFromIDLists, SHDoDragDrop,
    SHParseDisplayName,
};
use windows::core::{PCWSTR, implement};

/// 拖放期间的主机接口：由控制器实现。坐标是窗口客户区的物理像素。
pub trait DropHost {
    /// 外部拖放进入窗口（`paths` 为拖来的文件）。
    fn enter(&self, paths: &[PathBuf]);
    /// 指针移动：返回要显示的效果（`DROPEFFECT_*` 位值，0 为不可放置）。
    fn over(&self, client: (i32, i32), keys: u32, allowed: u32) -> u32;
    /// 离开窗口或取消。
    fn leave(&self);
    /// 释放：执行放置并返回回答给来源的效果。
    fn dropped(&self, client: (i32, i32), keys: u32, allowed: u32) -> u32;
}

pub const MK_CONTROL: u32 = 0x8;
pub const MK_SHIFT: u32 = 0x4;
pub const EFFECT_COPY: u32 = 1;
pub const EFFECT_MOVE: u32 = 2;

fn wide(p: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 从拖放数据里取出文件路径（没有 `CF_HDROP` 时为空）。
fn paths_of(data: &IDataObject) -> Vec<PathBuf> {
    let fmt = FORMATETC {
        cfFormat: 15, // CF_HDROP
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    // SAFETY: GetData 返回的存储介质在读取后用 ReleaseStgMedium 释放；全局内存按其大小只读复制。
    unsafe {
        let mut medium = match data.GetData(&fmt) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("GetData 失败：{e}");
                return Vec::new();
            }
        };
        let h = medium.u.hGlobal;
        let mut out = Vec::new();
        if !h.is_invalid() {
            use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
            let size = GlobalSize(h);
            let p = GlobalLock(h) as *const u8;
            if !p.is_null() {
                out = crate::clipboard::parse_hdrop(std::slice::from_raw_parts(p, size));
                let _ = GlobalUnlock(h);
            }
        }
        ReleaseStgMedium(&mut medium);
        out
    }
}

#[implement(IDropTarget)]
struct Target {
    hwnd: isize,
    host: Rc<dyn DropHost>,
}

impl Target {
    fn client(&self, pt: &POINTL) -> (i32, i32) {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
        let mut p = POINT { x: pt.x, y: pt.y };
        // SAFETY: hwnd 是主窗口句柄；p 是有效的局部变量。
        unsafe { ScreenToClient(self.hwnd as _, &mut p) };
        (p.x, p.y)
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(
        &self,
        pdataobj: windows::core::Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let paths = pdataobj.as_ref().map(paths_of).unwrap_or_default();
        self.host.enter(&paths);
        self.DragOver(grfkeystate, pt, pdweffect)
    }

    fn DragOver(
        &self,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // SAFETY: pdweffect 由 OLE 提供，指向可读写的 DROPEFFECT。
        unsafe {
            let allowed = (*pdweffect).0;
            let e = self.host.over(self.client(pt), grfkeystate.0, allowed);
            *pdweffect = DROPEFFECT(e & allowed);
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        self.host.leave();
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: windows::core::Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // 释放时再读一次路径，防止进入与释放之间数据变化
        let paths = pdataobj.as_ref().map(paths_of).unwrap_or_default();
        // SAFETY: 同 DragOver。
        unsafe {
            let allowed = (*pdweffect).0;
            self.host.enter(&paths);
            let e = self.host.dropped(self.client(pt), grfkeystate.0, allowed);
            *pdweffect = DROPEFFECT(e & allowed);
        }
        Ok(())
    }
}

/// 取窗口的 HWND（窗口必须已显示）。
pub fn hwnd_of(window: &slint::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle();
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// 接管窗口的拖放目标：撤销 winit 的，登记 `host` 的。成功返回 `true`。
pub fn attach(hwnd: isize, host: Rc<dyn DropHost>) -> bool {
    // SAFETY: hwnd 是主窗口句柄；OleInitialize 重复调用无害；注册的对象在窗口生命周期内保持（由 OLE 持有引用）。
    unsafe {
        let _ = OleInitialize(None);
        let h = HWND(hwnd as _);
        let _ = RevokeDragDrop(h);
        let target: IDropTarget = Target { hwnd, host }.into();
        RegisterDragDrop(h, &target).is_ok()
    }
}

/// 外壳数据对象：由路径的 PIDL 建立（与资源管理器拖出文件同一种，含 `CF_HDROP` 与外壳 ID 列表）。
unsafe fn shell_data_object(ptrs: &[*const ITEMIDLIST]) -> windows::core::Result<IDataObject> {
    // SAFETY: ptrs 都是有效的绝对 PIDL，调用期间不释放。
    unsafe {
        let arr: IShellItemArray = SHCreateShellItemArrayFromIDLists(ptrs)?;
        arr.BindToHandler(None, &BHID_DataObject)
    }
}

/// 拖出的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragOut {
    /// 用户取消或没有放到任何地方。
    Cancelled,
    /// 目标已经复制或移动（移动由目标自己搬完，来源不需要再做什么）。
    Dropped,
}

/// 把文件拖出窗口：发起系统拖放并阻塞到结束。数据对象与资源管理器拖出文件时相同。
pub fn start_drag(hwnd: isize, paths: &[PathBuf]) -> Result<DragOut, String> {
    // SAFETY: 所有 PIDL 在调用结束后用 CoTaskMemFree 释放；接口在本函数内使用并释放。
    unsafe {
        let _ = OleInitialize(None);
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        let free = |v: &Vec<*mut ITEMIDLIST>| {
            for p in v {
                CoTaskMemFree(Some(*p as *const _));
            }
        };
        for p in paths {
            let w = wide(p);
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None).is_err()
                || pidl.is_null()
            {
                free(&pidls);
                return Err(format!("无法为“{}”建立拖放数据", p.display()));
            }
            pidls.push(pidl);
        }
        let ptrs: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| *p as *const ITEMIDLIST).collect();
        let data = shell_data_object(&ptrs);
        let r = match data {
            Ok(d) => {
                let hw = if hwnd == 0 {
                    None
                } else {
                    Some(HWND(hwnd as _))
                };
                SHDoDragDrop(hw, &d, None, DROPEFFECT(EFFECT_COPY | EFFECT_MOVE))
                    .map(|e| {
                        if e == DROPEFFECT_NONE {
                            DragOut::Cancelled
                        } else {
                            DragOut::Dropped
                        }
                    })
                    // DRAGDROP_S_DROP / DRAGDROP_S_CANCEL 是成功类 HRESULT，windows 把它们当 Ok；这里的错误是真错误
                    .map_err(|e| format!("拖放失败：{e}"))
            }
            Err(e) => Err(format!("无法建立拖放数据：{e}")),
        };
        free(&pidls);
        r
    }
}

/// 拖出的“数据对象”能否读回与 `paths` 相同的 `CF_HDROP`：自测用，不发起真正的拖放。
pub fn data_object_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    // SAFETY: 同 start_drag。
    unsafe {
        let _ = OleInitialize(None);
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        for p in paths {
            let w = wide(p);
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None)
                .map_err(|e| e.to_string())?;
            pidls.push(pidl);
        }
        let ptrs: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| *p as *const ITEMIDLIST).collect();
        let data = shell_data_object(&ptrs);
        for p in &pidls {
            CoTaskMemFree(Some(*p as *const _));
        }
        Ok(paths_of(&data.map_err(|e| e.to_string())?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_data_object_carries_hdrop() {
        let dir = std::env::temp_dir()
            .join("sfm-unit")
            .join(format!("dd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("报告 一.txt"), dir.join("文件夹"));
        std::fs::write(&a, "x").unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let got = data_object_paths(&[a.clone(), b.clone()]).unwrap();
        let norm = |v: Vec<PathBuf>| -> Vec<String> {
            v.iter()
                .map(|p| p.to_string_lossy().to_lowercase())
                .collect()
        };
        assert_eq!(norm(got), norm(vec![a, b]));
    }
}
