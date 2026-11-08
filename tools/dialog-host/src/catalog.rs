//! 方式 × 场景目录，以及带按钮窗口里使用的预设（与命令行同一套参数）。

use serde_json::{Value, json};
use std::path::Path;

pub struct Preset {
    pub label: &'static str,
    pub api: &'static str,
    pub scenario: &'static str,
    pub opts: Vec<String>,
}

fn filters() -> Vec<String> {
    vec![
        "--filter=文本文件|*.txt;*.md".into(),
        "--filter=图片|*.png;*.jpg".into(),
        "--filter=所有文件|*.*".into(),
    ]
}

pub fn presets(fixture: &Path) -> Vec<Preset> {
    let docs = format!("--init-dir={}", fixture.join("docs").display());
    let with = |extra: &[&str], f: bool| -> Vec<String> {
        let mut v = vec![docs.clone()];
        if f {
            v.extend(filters());
        }
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    };
    vec![
        Preset {
            label: "现代 打开单个",
            api: "ifd",
            scenario: "open-single",
            opts: with(&[], true),
        },
        Preset {
            label: "现代 多选",
            api: "ifd",
            scenario: "open-multi",
            opts: with(&[], true),
        },
        Preset {
            label: "现代 保存（默认名 draft、扩展名 txt）",
            api: "ifd",
            scenario: "save",
            opts: with(&["--default-name=draft", "--default-ext=txt"], true),
        },
        Preset {
            label: "现代 保存 existing.txt（覆盖确认）",
            api: "ifd",
            scenario: "save",
            opts: with(&["--default-name=existing.txt"], true),
        },
        Preset {
            label: "现代 选择文件夹",
            api: "ifd",
            scenario: "pick-folder",
            opts: with(&[], false),
        },
        Preset {
            label: "旧式 打开单个",
            api: "gofn",
            scenario: "open-single",
            opts: with(&[], true),
        },
        Preset {
            label: "旧式 多选",
            api: "gofn",
            scenario: "open-multi",
            opts: with(&[], true),
        },
        Preset {
            label: "旧式 保存（默认名 draft、扩展名 txt）",
            api: "gofn",
            scenario: "save",
            opts: with(&["--default-name=draft", "--default-ext=txt"], true),
        },
        Preset {
            label: "旧式 保存 existing.txt（覆盖确认）",
            api: "gofn",
            scenario: "save",
            opts: with(&["--default-name=existing.txt"], true),
        },
        Preset {
            label: "SHBrowse 选择文件夹（新样式）",
            api: "sbf",
            scenario: "pick-folder",
            opts: with(&["--title=选择夹具里的文件夹"], false),
        },
        Preset {
            label: "SHBrowse 选择文件夹（旧样式）",
            api: "sbf",
            scenario: "pick-folder",
            opts: with(&["--sbf-style=legacy", "--title=选择夹具里的文件夹"], false),
        },
    ]
}

/// `list` 子命令的输出：支持的方式与场景，以及全部选项。
pub fn describe() -> Value {
    json!({
        "apis": {
            "ifd": { "desc": "IFileOpenDialog / IFileSaveDialog", "scenarios": ["open-single", "open-multi", "save", "pick-folder"] },
            "gofn": { "desc": "GetOpenFileNameW / GetSaveFileNameW", "scenarios": ["open-single", "open-multi", "save"] },
            "sbf": { "desc": "SHBrowseForFolderW", "scenarios": ["pick-folder"] },
        },
        "options": {
            "--init-dir=目录": "初始目录",
            "--title=文本": "对话框标题（sbf 为说明文字）",
            "--filter=名称|模式;模式": "文件类型过滤器，可重复",
            "--filter-index=N": "初始选中的过滤器，从 1 起",
            "--default-name=文本": "默认文件名",
            "--default-ext=扩展名": "默认扩展名（不含点）",
            "--overwrite-prompt=on|off": "保存时覆盖确认，默认 on",
            "--sbf-style=new|legacy": "sbf 样式，默认 new",
            "--owner=none|window": "所有者窗口，默认 none",
            "--out=文件": "JSON 同时写入该文件（UTF-8）",
            "--log-file=文件 / --log-dir=目录": "日志位置，默认 logs\\yyyy-MM-dd HH-mm-ss.log（或 SFM_LOG_DIR）",
        },
        "note": "取消不是对话框配置，而是操作者的动作；基准里的 *-cancel 用例由驱动点击取消按钮产生",
    })
}
