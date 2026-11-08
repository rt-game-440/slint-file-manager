//! 现代 `IFileOpenDialog`／`IFileSaveDialog`（含 `FOS_PICKFOLDERS` 选文件夹）。

use crate::cli::{Request, Scenario};
use crate::outcome::{Outcome, hresult_text};
use crate::util::{take_pwstr, wide};
use serde_json::json;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FOS_PICKFOLDERS,
    FileOpenDialog, FileSaveDialog, IFileDialog, IFileOpenDialog, IFileSaveDialog, IShellItem,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::{Interface, PCWSTR};

/// HRESULT_FROM_WIN32(ERROR_CANCELLED)
const HR_CANCELLED: i32 = 0x800704C7u32 as i32;

pub fn run(req: &Request, owner: Option<HWND>) -> Outcome {
    match run_inner(req, owner) {
        Ok(o) => o,
        Err(e) => Outcome::failed("HRESULT", hresult_text(e.code().0), e.message()),
    }
}

fn run_inner(req: &Request, owner: Option<HWND>) -> windows::core::Result<Outcome> {
    unsafe {
        let save = req.scenario == Scenario::Save;
        let dlg: IFileDialog = if save {
            CoCreateInstance::<_, IFileSaveDialog>(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)?
                .cast()?
        } else {
            CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?
                .cast()?
        };

        // 在系统默认选项上叠加：始终要求文件系统路径；多选、选文件夹、覆盖确认按请求设置
        let mut opts = dlg.GetOptions()? | FOS_FORCEFILESYSTEM;
        match req.scenario {
            Scenario::OpenMulti => opts |= FOS_ALLOWMULTISELECT,
            Scenario::PickFolder => opts |= FOS_PICKFOLDERS,
            _ => {}
        }
        if save {
            if req.overwrite_prompt {
                opts |= FOS_OVERWRITEPROMPT;
            } else {
                opts &= !FOS_OVERWRITEPROMPT;
            }
        }
        dlg.SetOptions(opts)?;

        if let Some(t) = &req.title {
            let w = wide(t);
            dlg.SetTitle(PCWSTR(w.as_ptr()))?;
        }

        // 过滤器：名称与通配符的宽字符串必须活到 Show 返回
        let names: Vec<Vec<u16>> = req.filters.iter().map(|f| wide(&f.name)).collect();
        let specs: Vec<Vec<u16>> = req.filters.iter().map(|f| wide(&f.patterns)).collect();
        if !req.filters.is_empty() {
            let arr: Vec<COMDLG_FILTERSPEC> = names
                .iter()
                .zip(&specs)
                .map(|(n, s)| COMDLG_FILTERSPEC {
                    pszName: PCWSTR(n.as_ptr()),
                    pszSpec: PCWSTR(s.as_ptr()),
                })
                .collect();
            dlg.SetFileTypes(&arr)?;
            if let Some(i) = req.filter_index {
                dlg.SetFileTypeIndex(i)?;
            }
        }
        if let Some(e) = &req.default_ext {
            let w = wide(e);
            dlg.SetDefaultExtension(PCWSTR(w.as_ptr()))?;
        }
        if let Some(n) = &req.default_name {
            let w = wide(n);
            dlg.SetFileName(PCWSTR(w.as_ptr()))?;
        }
        if let Some(d) = &req.init_dir {
            let w = wide(d);
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None)?;
            dlg.SetFolder(&item)?;
        }

        let filter_index_of = |dlg: &IFileDialog| -> Option<u32> {
            if req.filters.is_empty() {
                None
            } else {
                dlg.GetFileTypeIndex().ok()
            }
        };

        match dlg.Show(owner) {
            Ok(()) => {}
            Err(e) if e.code().0 == HR_CANCELLED => {
                return Ok(Outcome::cancelled(
                    "HRESULT",
                    hresult_text(HR_CANCELLED),
                    None,
                    json!({}),
                ));
            }
            Err(e) => return Err(e),
        }

        let mut paths = Vec::new();
        if req.scenario == Scenario::OpenMulti {
            let items = dlg.cast::<IFileOpenDialog>()?.GetResults()?;
            for i in 0..items.GetCount()? {
                paths.push(path_of(&items.GetItemAt(i)?)?);
            }
        } else {
            paths.push(path_of(&dlg.GetResult()?)?);
        }
        Ok(Outcome {
            return_kind: "HRESULT",
            return_code: hresult_text(0),
            cancelled: false,
            error: None,
            paths,
            filter_index: filter_index_of(&dlg),
            extras: json!({}),
        })
    }
}

unsafe fn path_of(item: &IShellItem) -> windows::core::Result<String> {
    Ok(take_pwstr(unsafe {
        item.GetDisplayName(SIGDN_FILESYSPATH)?
    }))
}
