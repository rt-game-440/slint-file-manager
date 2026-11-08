//! 旧式 `GetOpenFileNameW`／`GetSaveFileNameW`。始终带 `OFN_EXPLORER`（Windows 10 上该调用本来就走资源管理器风格对话框）。

use crate::cli::{Request, Scenario};
use crate::outcome::Outcome;
use crate::util::{until_nul, wide};
use serde_json::json;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetOpenFileNameW, GetSaveFileNameW, OFN_ALLOWMULTISELECT, OFN_EXPLORER,
    OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST,
    OPENFILENAMEW,
};
use windows::core::{PCWSTR, PWSTR};

pub fn run(req: &Request, owner: Option<HWND>) -> Outcome {
    let save = req.scenario == Scenario::Save;
    let multi = req.scenario == Scenario::OpenMulti;

    // 过滤器串：显示名\0通配符\0 ... \0\0
    let mut filter: Vec<u16> = Vec::new();
    for f in &req.filters {
        filter.extend(f.name.encode_utf16());
        filter.push(0);
        filter.extend(f.patterns.encode_utf16());
        filter.push(0);
    }
    filter.push(0);

    // 多选需要大缓冲区；文件名缓冲预填默认名
    let mut buf: Vec<u16> = vec![0; if multi { 65536 } else { 4096 }];
    if let Some(n) = &req.default_name {
        for (i, c) in n.encode_utf16().enumerate().take(buf.len() - 1) {
            buf[i] = c;
        }
    }
    let init_dir = req.init_dir.as_deref().map(wide);
    let title = req.title.as_deref().map(wide);
    let def_ext = req.default_ext.as_deref().map(wide);

    let mut flags = OFN_EXPLORER | OFN_NOCHANGEDIR | OFN_HIDEREADONLY | OFN_PATHMUSTEXIST;
    if save {
        if req.overwrite_prompt {
            flags |= OFN_OVERWRITEPROMPT;
        }
    } else {
        flags |= OFN_FILEMUSTEXIST;
    }
    if multi {
        flags |= OFN_ALLOWMULTISELECT;
    }

    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner.unwrap_or_default(),
        lpstrFilter: if req.filters.is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR(filter.as_ptr())
        },
        nFilterIndex: if req.filters.is_empty() {
            0
        } else {
            req.filter_index.unwrap_or(1)
        },
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrInitialDir: init_dir
            .as_ref()
            .map(|w| PCWSTR(w.as_ptr()))
            .unwrap_or(PCWSTR::null()),
        lpstrTitle: title
            .as_ref()
            .map(|w| PCWSTR(w.as_ptr()))
            .unwrap_or(PCWSTR::null()),
        lpstrDefExt: def_ext
            .as_ref()
            .map(|w| PCWSTR(w.as_ptr()))
            .unwrap_or(PCWSTR::null()),
        Flags: flags,
        ..Default::default()
    };

    let ok = unsafe {
        if save {
            GetSaveFileNameW(&mut ofn)
        } else {
            GetOpenFileNameW(&mut ofn)
        }
    }
    .as_bool();

    if !ok {
        let ext_err = unsafe { CommDlgExtendedError() };
        return if ext_err.0 == 0 {
            Outcome::cancelled("BOOL", "FALSE".into(), None, json!({"comdlg_error": 0}))
        } else {
            Outcome::failed(
                "BOOL",
                "FALSE".into(),
                format!("CommDlgExtendedError=0x{:04X}", ext_err.0),
            )
        };
    }

    // 解析缓冲：多选时为“目录\0名1\0名2\0\0”，其余为完整路径
    // OPENFILENAMEW 在 x86 上是 packed 结构，字段必须先拷到局部变量
    let (n_filter_index, n_file_offset, n_file_ext, flags_out) = (
        ofn.nFilterIndex,
        ofn.nFileOffset,
        ofn.nFileExtension,
        ofn.Flags.0,
    );
    let off = n_file_offset as usize;
    let mut segments: Vec<String> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    let multi_layout = multi && off > 0 && off <= buf.len() && buf[off - 1] == 0;
    if multi_layout {
        let dir = until_nul(&buf[..off]);
        segments.push(dir.clone());
        let mut pos = off;
        while pos < buf.len() && buf[pos] != 0 {
            let name = until_nul(&buf[pos..]);
            pos += name.encode_utf16().count() + 1;
            segments.push(name.clone());
            paths.push(format!("{}\\{}", dir.trim_end_matches('\\'), name));
        }
    } else {
        let p = until_nul(&buf);
        segments.push(p.clone());
        paths.push(p);
    }
    // 偏移量本身随路径长度变化（含用户名），不能比对；改记“偏移处的文本”，并记 0／非 0
    let at = |o: usize| (o > 0 && o < buf.len()).then(|| until_nul(&buf[o..]));
    let name_at_file_offset = at(off);
    let name_at_ext_offset = at(n_file_ext as usize);

    Outcome {
        return_kind: "BOOL",
        return_code: "TRUE".into(),
        cancelled: false,
        error: None,
        paths,
        filter_index: if req.filters.is_empty() {
            None
        } else {
            Some(n_filter_index)
        },
        extras: json!({
            "buffer_layout": if multi_layout { "dir+names" } else { "single-path" },
            "buffer_segments": segments,
            "file_offset_nonzero": n_file_offset != 0,
            "text_at_file_offset": name_at_file_offset,
            "extension_offset_nonzero": n_file_ext != 0,
            "text_at_extension_offset": name_at_ext_offset,
            "flags_out": format!("0x{flags_out:08X}"),
        }),
    }
}
