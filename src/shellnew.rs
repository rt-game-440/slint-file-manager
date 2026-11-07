//! “新建”菜单里的文件模板：来自系统的 `ShellNew` 注册（与资源管理器“新建”菜单同源）。
//!
//! - 只检查一组常见扩展名（见 [`CANDIDATES`]），每个扩展名在 `HKEY_CLASSES_ROOT\.ext\ShellNew` 或
//!   `HKEY_CLASSES_ROOT\.ext\<ProgID>\ShellNew` 下找 `NullFile`（空文件）、`FileName`（模板文件）、`Data`（内置内容）。
//!   没有注册（例如没装 Office）的类型不出现，不会提供一个新建出来打不开的文件。
//! - 菜单标签用内置中文名（系统的显示名依赖语言包与间接字符串，不稳定），文件名按资源管理器习惯：`新建 文本文档.txt`。
//! - 创建模板内容在“新建”时才读取（[`Template::content`]），模板文件缺失时如实报错。

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW,
};

/// 模板内容的来源。
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// 空文件。
    Empty,
    /// 注册表里内置的字节。
    Data(Vec<u8>),
    /// 模板文件（复制其内容）。
    File(std::path::PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    /// 菜单上的名称，也是新文件名的一部分（`新建 {label}{ext}`）。
    pub label: String,
    /// 带点的扩展名。
    pub ext: String,
    pub source: Source,
}

impl Template {
    pub fn empty(label: &str, ext: &str) -> Template {
        Template {
            label: label.into(),
            ext: ext.into(),
            source: Source::Empty,
        }
    }

    /// 新文件的内容。模板文件读不出来时返回原因。
    pub fn content(&self) -> Result<Vec<u8>, String> {
        match &self.source {
            Source::Empty => Ok(Vec::new()),
            Source::Data(d) => Ok(d.clone()),
            Source::File(p) => {
                std::fs::read(p).map_err(|e| format!("读取模板“{}”失败：{e}", p.display()))
            }
        }
    }
}

/// 检查的扩展名与菜单名称（顺序即菜单顺序）。
const CANDIDATES: [(&str, &str); 6] = [
    (".txt", "文本文档"),
    (".docx", "Microsoft Word 文档"),
    (".xlsx", "Microsoft Excel 工作表"),
    (".pptx", "Microsoft PowerPoint 演示文稿"),
    (".rtf", "RTF 文档"),
    (".bmp", "BMP 图片"),
];

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Key(HKEY);

impl Key {
    fn open(path: &str) -> Option<Key> {
        let mut h: HKEY = std::ptr::null_mut();
        // SAFETY: 路径以 NUL 结尾；成功后由 Drop 关闭。
        let r =
            unsafe { RegOpenKeyExW(HKEY_CLASSES_ROOT, wide(path).as_ptr(), 0, KEY_READ, &mut h) };
        (r == ERROR_SUCCESS).then_some(Key(h))
    }

    /// 取值：（类型, 字节）；不存在时为 `None`。
    fn value(&self, name: &str) -> Option<(u32, Vec<u8>)> {
        let n = wide(name);
        let (mut ty, mut len) = (0u32, 0u32);
        // SAFETY: 先查询长度，再按长度读取。
        unsafe {
            let r = RegQueryValueExW(
                self.0,
                n.as_ptr(),
                std::ptr::null(),
                &mut ty,
                std::ptr::null_mut(),
                &mut len,
            );
            if r != ERROR_SUCCESS {
                return None;
            }
            let mut buf = vec![0u8; len as usize];
            let r = RegQueryValueExW(
                self.0,
                n.as_ptr(),
                std::ptr::null(),
                &mut ty,
                buf.as_mut_ptr(),
                &mut len,
            );
            if r != ERROR_SUCCESS {
                return None;
            }
            buf.truncate(len as usize);
            Some((ty, buf))
        }
    }

    fn subkeys(&self) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0.. {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            // SAFETY: 缓冲区与长度一致。
            let r = unsafe {
                RegEnumKeyExW(
                    self.0,
                    i,
                    name.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if r != ERROR_SUCCESS {
                break;
            }
            out.push(String::from_utf16_lossy(&name[..len as usize]));
        }
        out
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: 与成功的 RegOpenKeyExW 配对。
        unsafe { RegCloseKey(self.0) };
    }
}

fn reg_string(ty: u32, bytes: &[u8]) -> Option<String> {
    // REG_SZ = 1，REG_EXPAND_SZ = 2
    if ty != 1 && ty != 2 {
        return None;
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&u| u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

fn expand(s: &str) -> String {
    let mut out = s.to_string();
    for var in ["SystemRoot", "APPDATA", "USERPROFILE"] {
        if let Ok(v) = std::env::var(var) {
            out = out
                .replace(&format!("%{var}%"), &v)
                .replace(&format!("%{}%", var.to_uppercase()), &v);
        }
    }
    out
}

/// 在 `ShellNew` 键里解析出内容来源。
fn source_in(k: &Key) -> Option<Source> {
    if k.value("NullFile").is_some() {
        return Some(Source::Empty);
    }
    if let Some((ty, b)) = k.value("FileName")
        && let Some(name) = reg_string(ty, &b)
    {
        let name = expand(&name);
        let p = std::path::PathBuf::from(&name);
        let mut candidates = vec![p.clone()];
        if p.is_relative() {
            if let Ok(root) = std::env::var("SystemRoot") {
                candidates.push(std::path::Path::new(&root).join("ShellNew").join(&name));
            }
            if let Ok(app) = std::env::var("APPDATA") {
                candidates.push(
                    std::path::Path::new(&app)
                        .join("Microsoft")
                        .join("Windows")
                        .join("Templates")
                        .join(&name),
                );
            }
        }
        return candidates
            .into_iter()
            .find(|c| c.is_file())
            .map(Source::File);
    }
    if let Some((3, d)) = k.value("Data") {
        // REG_BINARY = 3
        return Some(Source::Data(d));
    }
    None
}

/// 读取系统注册的新建模板。
pub fn system_templates() -> Vec<Template> {
    let mut out = Vec::new();
    for (ext, label) in CANDIDATES {
        let mut found = Key::open(&format!("{ext}\\ShellNew")).and_then(|k| source_in(&k));
        if found.is_none()
            && let Some(root) = Key::open(ext)
        {
            for sub in root.subkeys() {
                if let Some(k) = Key::open(&format!("{ext}\\{sub}\\ShellNew"))
                    && let Some(s) = source_in(&k)
                {
                    found = Some(s);
                    break;
                }
            }
        }
        if let Some(source) = found {
            out.push(Template {
                label: label.into(),
                ext: ext.into(),
                source,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_has_text_template() {
        let t = system_templates();
        let txt = t.iter().find(|t| t.ext == ".txt");
        assert!(txt.is_some(), "系统应注册了 .txt 新建模板：{t:?}");
        assert_eq!(
            txt.unwrap().content().unwrap().len(),
            0,
            "文本文档模板是空文件"
        );
    }

    #[test]
    fn expand_env() {
        let root = std::env::var("SystemRoot").unwrap();
        assert_eq!(expand("%SystemRoot%\\x"), format!("{root}\\x"));
    }

    #[test]
    fn missing_template_file_is_reported() {
        let t = Template {
            label: "x".into(),
            ext: ".x".into(),
            source: Source::File("Z:\\不存在\\模板.x".into()),
        };
        assert!(t.content().unwrap_err().contains("读取模板"));
    }
}
