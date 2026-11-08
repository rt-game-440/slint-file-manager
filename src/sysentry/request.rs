//! 外部打开请求 → 本应用要做的事（Issue #17）。
//!
//! [`route`] 是纯函数：把 [`args::EntryRequest`] 变成“交给应用的请求”（激活窗口，或打开文件夹并可选选中项目），
//! 或“交回 Windows”（此电脑、控制面板等应用没有对应位置的虚拟位置）。目标是否存在由接收请求的实例在打开时检查。
//!
//! 交回 Windows 时只用系统的 `open` 动词打开 `::{CLSID}`／`shell:` 这类虚拟位置：这些属于 `Folder` 类，
//! 本应用从不接管它们，因此不会循环回到本应用。

use std::path::PathBuf;

use serde_json::{Value, json};

use super::args::{self, CLSID_THIS_PC, Disposition, EntryRequest, SystemReason};

/// 交给应用实例的请求（可跨进程转发）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// 激活现有窗口（Win+E、没有目标的启动）。没有窗口时按会话恢复正常启动。
    Activate,
    /// 打开文件夹；`select` 为要选中的项目。`folder` 若在打开时发现是文件，则打开其所在文件夹并选中它。
    Open {
        folder: PathBuf,
        select: Option<PathBuf>,
    },
}

/// 请求的去向。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    App(Request),
    /// 交回 Windows：用系统 `open` 动词打开 `target`。
    HandBack {
        target: String,
        reason: String,
    },
}

/// 决定去向。`resolve_shell` 把 `shell:` 已知文件夹名解析为文件系统路径（解析不到返回 `None`）。
pub fn route(req: &EntryRequest, resolve_shell: impl Fn(&str) -> Option<PathBuf>) -> Route {
    match req.disposition() {
        Disposition::OpenFolder(folder) => Route::App(Request::Open {
            folder,
            select: None,
        }),
        Disposition::OpenAndSelect { folder, item } => Route::App(Request::Open {
            folder,
            select: Some(item),
        }),
        Disposition::OpenDefault => Route::App(Request::Activate),
        Disposition::ResolveShell { name, select } => match resolve_shell(&name) {
            Some(p) if select => match p.parent() {
                Some(parent) => Route::App(Request::Open {
                    folder: parent.to_path_buf(),
                    select: Some(p.clone()),
                }),
                None => Route::App(Request::Open {
                    folder: p,
                    select: None,
                }),
            },
            Some(p) => Route::App(Request::Open {
                folder: p,
                select: None,
            }),
            None => Route::HandBack {
                target: format!("shell:{name}"),
                reason: "不是文件系统位置".into(),
            },
        },
        Disposition::System(SystemReason::ThisPc) => Route::HandBack {
            target: format!("::{{{CLSID_THIS_PC}}}"),
            reason: "此电脑".into(),
        },
        Disposition::System(SystemReason::Virtual(target)) => Route::HandBack {
            target,
            reason: "虚拟位置".into(),
        },
        // 拿不到路径（/idlist、COM 激活）或目标无效：只激活窗口
        Disposition::System(SystemReason::Opaque(_) | SystemReason::Invalid(_)) => {
            Route::App(Request::Activate)
        }
    }
}

/// 解析原始参数（`--entry` 起的部分，或 explorer 风格参数）并决定去向。
pub fn route_args(tail: &str, cwd: Option<&std::path::Path>) -> Route {
    route(&args::parse(tail, cwd), resolve_shell_folder)
}

impl Request {
    pub fn to_json(&self) -> String {
        let v = match self {
            Request::Activate => json!({ "type": "activate" }),
            Request::Open { folder, select } => json!({
                "type": "open",
                "folder": folder.to_string_lossy(),
                "select": select.as_ref().map(|s| s.to_string_lossy().into_owned()),
            }),
        };
        v.to_string()
    }

    pub fn from_json(s: &str) -> Option<Request> {
        let v: Value = serde_json::from_str(s).ok()?;
        match v.get("type")?.as_str()? {
            "activate" => Some(Request::Activate),
            "open" => {
                let folder = v.get("folder")?.as_str()?;
                if folder.is_empty() {
                    return None;
                }
                let select = match v.get("select") {
                    None | Some(Value::Null) => None,
                    Some(x) => Some(PathBuf::from(x.as_str()?)),
                };
                Some(Request::Open {
                    folder: PathBuf::from(folder),
                    select,
                })
            }
            _ => None,
        }
    }
}

/// 应用对请求的答复。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub ok: bool,
    pub message: String,
}

impl Reply {
    pub fn ok(message: impl Into<String>) -> Reply {
        Reply {
            ok: true,
            message: message.into(),
        }
    }

    pub fn fail(message: impl Into<String>) -> Reply {
        Reply {
            ok: false,
            message: message.into(),
        }
    }

    pub fn to_json(&self) -> String {
        json!({ "ok": self.ok, "message": self.message }).to_string()
    }

    pub fn from_json(s: &str) -> Option<Reply> {
        let v: Value = serde_json::from_str(s).ok()?;
        Some(Reply {
            ok: v.get("ok")?.as_bool()?,
            message: v.get("message")?.as_str()?.to_string(),
        })
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `shell:<名称>` → 文件系统路径（如 `shell:Downloads`）；虚拟位置或解析失败返回 `None`。
/// 在独立线程里初始化 COM 后解析，不影响调用线程的 COM 状态。
pub fn resolve_shell_folder(name: &str) -> Option<PathBuf> {
    let full = format!("shell:{name}");
    std::thread::spawn(move || {
        use windows::Win32::System::Com::{
            COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
        };
        use windows::Win32::UI::Shell::Common::ITEMIDLIST;
        use windows::Win32::UI::Shell::{
            GPFIDL_DEFAULT, SHGetPathFromIDListEx, SHParseDisplayName,
        };
        use windows::core::PCWSTR;
        let w = wide(&full);
        // SAFETY: 按文档调用；PIDL 用 CoTaskMemFree 释放。
        unsafe {
            let inited = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            let mut out = None;
            if SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None).is_ok()
                && !pidl.is_null()
            {
                let mut buf = vec![0u16; 32768];
                if SHGetPathFromIDListEx(pidl, &mut buf, GPFIDL_DEFAULT).as_bool() {
                    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
                    if n > 0 {
                        out = Some(PathBuf::from(String::from_utf16_lossy(&buf[..n])));
                    }
                }
                CoTaskMemFree(Some(pidl as *const _));
            }
            if inited {
                CoUninitialize();
            }
            out
        }
    })
    .join()
    .ok()
    .flatten()
}

/// 交回 Windows：用系统 `open` 动词打开虚拟位置。
pub fn hand_back(target: &str) -> Result<(), String> {
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
    };
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let file = wide(target);
    let verb = wide("open");
    // SAFETY: 按 ShellExecuteExW 文档初始化结构体；字符串缓冲区在调用期间有效。
    unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        );
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOASYNC;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut info) != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(tail: &str) -> Route {
        route(&args::parse(tail, None), |n| {
            n.eq_ignore_ascii_case("Downloads")
                .then(|| PathBuf::from(r"C:\Users\me\Downloads"))
        })
    }

    fn open(f: &str, s: Option<&str>) -> Route {
        Route::App(Request::Open {
            folder: PathBuf::from(f),
            select: s.map(PathBuf::from),
        })
    }

    #[test]
    fn routes_takeover_verbs() {
        assert_eq!(r(r#"--entry "C:\""#), open(r"C:\", None));
        assert_eq!(r(r#"--entry "E:\文档\a b""#), open(r"E:\文档\a b", None));
        assert_eq!(r("--entry"), Route::App(Request::Activate));
        assert_eq!(r(""), Route::App(Request::Activate));
    }

    #[test]
    fn routes_select_and_shell_folders() {
        assert_eq!(r(r"/select,C:\a\b.txt"), open(r"C:\a", Some(r"C:\a\b.txt")));
        assert_eq!(r("shell:Downloads"), open(r"C:\Users\me\Downloads", None));
        assert_eq!(
            r("/select,shell:Downloads"),
            open(r"C:\Users\me", Some(r"C:\Users\me\Downloads"))
        );
        // 回收站是应用里的虚拟位置
        assert_eq!(
            r("shell:RecycleBinFolder"),
            Route::App(Request::Open {
                folder: crate::browse::location::recycle_bin_path(),
                select: None
            })
        );
    }

    #[test]
    fn virtual_locations_go_back_to_windows() {
        assert!(matches!(
            r("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"),
            Route::HandBack { target, .. } if target == "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"
        ));
        assert!(matches!(
            r("shell:ControlPanelFolder"),
            Route::HandBack { target, .. } if target == "shell:ControlPanelFolder"
        ));
        assert!(matches!(
            r("::{26EE0668-A00A-44D7-9371-BEB064C98683}"),
            Route::HandBack { .. }
        ));
        // 拿不到路径：只激活
        assert_eq!(r("/idlist,:123:456"), Route::App(Request::Activate));
    }

    #[test]
    fn json_roundtrip_and_rejects_garbage() {
        for req in [
            Request::Activate,
            Request::Open {
                folder: PathBuf::from(r"C:\文档\a b"),
                select: None,
            },
            Request::Open {
                folder: PathBuf::from(r"\\srv\share"),
                select: Some(PathBuf::from(r"\\srv\share\x.txt")),
            },
        ] {
            assert_eq!(Request::from_json(&req.to_json()), Some(req));
        }
        assert_eq!(Request::from_json("{}"), None);
        assert_eq!(Request::from_json(r#"{"type":"run","cmd":"calc"}"#), None);
        assert_eq!(Request::from_json(r#"{"type":"open","folder":""}"#), None);
        assert_eq!(Request::from_json("not json"), None);
        let rep = Reply::fail("找不到“甲”");
        assert_eq!(Reply::from_json(&rep.to_json()), Some(rep));
    }
}
