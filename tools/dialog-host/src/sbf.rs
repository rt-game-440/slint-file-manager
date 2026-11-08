//! 旧式文件夹选择 `SHBrowseForFolderW`。

use crate::cli::Request;
use crate::outcome::Outcome;
use crate::util::{until_nul, wide};
use serde_json::json;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    BFFM_INITIALIZED, BFFM_SETSELECTIONW, BIF_EDITBOX, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS,
    BROWSEINFOW, SHBrowseForFolderW, SHGetPathFromIDListW,
};
use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
use windows::core::{PCWSTR, PWSTR};

/// 对话框初始化时，把 lpData（初始目录的宽字符串指针）设为选中项。
unsafe extern "system" fn callback(hwnd: HWND, msg: u32, _lparam: LPARAM, data: LPARAM) -> i32 {
    if msg == BFFM_INITIALIZED && data.0 != 0 {
        unsafe { SendMessageW(hwnd, BFFM_SETSELECTIONW, Some(WPARAM(1)), Some(data)) };
    }
    0
}

pub fn run(req: &Request, owner: Option<HWND>) -> Outcome {
    let init_dir = req.init_dir.as_deref().map(wide);
    let title = wide(req.title.as_deref().unwrap_or(""));
    let mut display = vec![0u16; 260];
    let flags = if req.sbf_style == "legacy" {
        BIF_RETURNONLYFSDIRS
    } else {
        BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX
    };
    let bi = BROWSEINFOW {
        hwndOwner: owner.unwrap_or_default(),
        pidlRoot: std::ptr::null_mut(),
        pszDisplayName: PWSTR(display.as_mut_ptr()),
        lpszTitle: if req.title.is_some() {
            PCWSTR(title.as_ptr())
        } else {
            PCWSTR::null()
        },
        ulFlags: flags,
        lpfn: Some(callback),
        lParam: LPARAM(init_dir.as_ref().map(|w| w.as_ptr() as isize).unwrap_or(0)),
        iImage: 0,
    };

    let pidl = unsafe { SHBrowseForFolderW(&bi) };
    let extras_base = json!({ "flags": format!("0x{:08X}", flags) });
    if pidl.is_null() {
        let mut o = Outcome::cancelled("PIDL", "null".into(), None, extras_base);
        o.extras["display_name"] = json!(until_nul(&display));
        return o;
    }
    let mut path = [0u16; 260];
    let got = unsafe { SHGetPathFromIDListW(pidl, &mut path) };
    unsafe { CoTaskMemFree(Some(pidl as *const _)) };
    if !got.as_bool() {
        return Outcome::failed(
            "PIDL",
            "pidl".into(),
            "SHGetPathFromIDListW 失败（所选项不是文件系统目录）".into(),
        );
    }
    let mut extras = extras_base;
    extras["display_name"] = json!(until_nul(&display));
    Outcome {
        return_kind: "PIDL",
        return_code: "pidl".into(),
        cancelled: false,
        error: None,
        paths: vec![until_nul(&path)],
        filter_index: None,
        extras,
    }
}
