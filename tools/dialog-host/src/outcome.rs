//! 一次调用的结果，以及稳定结构的 JSON 组装。

use serde_json::{Value, json};
use std::path::Path;

pub struct Outcome {
    /// 返回值的类型：`HRESULT`（IFileDialog::Show）、`BOOL`（GetOpenFileName 系）、`PIDL`（SHBrowseForFolder）
    pub return_kind: &'static str,
    /// 返回值的可读形式：`0x00000000`、`0x800704C7`、`TRUE`／`FALSE`、`pidl`／`null`
    pub return_code: String,
    pub cancelled: bool,
    /// 非取消的失败原因（如 CommDlgExtendedError 非零）；没有则为 null
    pub error: Option<String>,
    pub paths: Vec<String>,
    /// 返回时选中的过滤器（从 1 起）；没有设置过滤器时为 null
    pub filter_index: Option<u32>,
    /// 各方式特有的返回数据（如 gofn 的 nFileOffset、sbf 的显示名）
    pub extras: Value,
}

impl Outcome {
    pub fn cancelled(
        kind: &'static str,
        code: String,
        filter_index: Option<u32>,
        extras: Value,
    ) -> Self {
        Outcome {
            return_kind: kind,
            return_code: code,
            cancelled: true,
            error: None,
            paths: Vec::new(),
            filter_index,
            extras,
        }
    }

    pub fn failed(kind: &'static str, code: String, error: String) -> Self {
        Outcome {
            return_kind: kind,
            return_code: code,
            cancelled: false,
            error: Some(error),
            paths: Vec::new(),
            filter_index: None,
            extras: json!({}),
        }
    }

    pub fn to_json(&self) -> Value {
        let files: Vec<Value> = self.paths.iter().map(|p| file_json(p)).collect();
        json!({
            "return_kind": self.return_kind,
            "return_code": self.return_code,
            "cancelled": self.cancelled,
            "error": self.error,
            "paths": self.paths,
            "files": files,
            "filter_index": self.filter_index,
            "extras": self.extras,
        })
    }
}

/// 最终文件名与扩展名（不含点；没有扩展名为空串）、是否已存在、是否目录。
fn file_json(p: &str) -> Value {
    let path = Path::new(p);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_string())
        .unwrap_or_default();
    json!({
        "path": p,
        "name": name,
        "extension": ext,
        "exists": path.exists(),
        "is_dir": path.is_dir(),
    })
}

pub fn hresult_text(hr: i32) -> String {
    format!("0x{:08X}", hr as u32)
}
