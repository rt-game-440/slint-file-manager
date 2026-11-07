//! 外部打开请求的参数解析（纯函数，不访问磁盘、注册表或外壳）。
//!
//! 输入是**原始命令行字符串**（`GetCommandLineW` 去掉程序名之后的部分），不是 Rust 的 `std::env::args()`：
//! MSVC 规则会把 `"C:\"` 里的 `\"` 当作转义引号，得到 `C:"`；而 explorer 风格参数用逗号分隔。
//! 两种来源共用一个解析器：
//!
//! - 本应用注册的接管动词：`"<exe>" --entry "%1"`（文件夹、驱动器）、`"<exe>" --entry`（Win+E）。
//! - explorer 风格参数：`/select,<项目>`、`/e,`、`/n,`、`/root,<对象>`、`/separate`、裸路径，
//!   用于 Everything 等允许自定义“打开所在文件夹”命令的程序（见 docs/research/0016-system-entry-takeover.md）。
//!
//! 解析结果只描述“请求了什么”；目标是否存在、是文件还是目录、`shell:` 已知文件夹解析到哪里，
//! 由 #17/#18 在运行时判断。外部参数从不被当作命令执行。

use std::path::{Path, PathBuf};

use crate::browse::location::{self, RECYCLE_BIN};

/// 本应用接管动词在命令行里使用的标记。
pub const ENTRY_FLAG: &str = "--entry";

/// 回收站的 CLSID（`::{645FF040-...}`）。
pub const CLSID_RECYCLE_BIN: &str = "645FF040-5081-101B-9F08-00AA002F954E";
/// “此电脑”的 CLSID（`::{20D04FE0-...}`）。
pub const CLSID_THIS_PC: &str = "20D04FE0-3AEA-1069-A2D8-08002B30309D";

/// 请求中的一个目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// 文件系统路径（驱动器、目录、文件、UNC），已规范化，未检查是否存在。
    Path(PathBuf),
    /// `shell:<名称>` 虚拟路径，保存的是冒号后的名称（原样大小写）。
    Shell(String),
    /// `::{GUID}` 外壳命名空间对象。`clsid` 为大写、不带花括号的 GUID；`rest` 为其后的剩余部分（可能为空）。
    Clsid { clsid: String, rest: String },
}

/// 解析出的外部打开请求。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntryRequest {
    /// 命令行带有 [`ENTRY_FLAG`]，即来自本应用注册的接管动词。
    pub from_takeover: bool,
    /// 要打开的位置（裸路径参数）。
    pub open: Option<Target>,
    /// `/select,` 要选中的项目。
    pub select: Option<Target>,
    /// `/root,` 指定的根。
    pub root: Option<Target>,
    /// `/n`：要求新窗口。按 ADR 0002 本应用总是复用现有窗口，此标志只作记录。
    pub new_window: bool,
    /// `/e`：要求带树形导航的视图。只作记录。
    pub explore: bool,
    /// `/separate`：要求独立进程。只作记录。
    pub separate: bool,
    /// 无法处理的部分：`/idlist`（共享内存 PIDL）、`/factory`、`-Embedding`、未知开关、多余的位置参数、无法解析的目标。
    pub unsupported: Vec<String>,
}

/// 无法由本应用处理、应交回 Windows 的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemReason {
    /// “此电脑”：应用没有对应位置（侧栏只有磁盘列表）。
    ThisPc,
    /// 其他外壳虚拟位置（控制面板、网络、库等）。
    Virtual(String),
    /// 以 PIDL 句柄（`/idlist`）或 COM 激活（`/factory`、`-Embedding`）传入，拿不到路径。
    Opaque(String),
    /// 目标无法解析（相对路径但没有工作目录、空目标等）。
    Invalid(String),
}

/// 请求的处理方式（运行时还要检查目标是否存在）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// 打开一个位置（目录、驱动器根，或回收站虚拟位置）。目标若在运行时被发现是文件，由调用方决定交给默认应用。
    OpenFolder(PathBuf),
    /// 打开 `folder` 并选中 `item`。
    OpenAndSelect { folder: PathBuf, item: PathBuf },
    /// 没有目标（Win+E、裸 `explorer.exe`）：按应用默认行为（激活现有窗口或打开起始位置）。
    OpenDefault,
    /// `shell:` 已知文件夹（如 `shell:Downloads`），需在运行时解析为文件系统路径；解析不到文件系统路径时交回 Windows。
    ResolveShell { name: String, select: bool },
    /// 交回 Windows 处理。
    System(SystemReason),
}

/// 从完整命令行中去掉程序名（argv[0]），返回其余部分（已去掉前导空白）。
///
/// 规则与 Windows 一致：以引号开头时程序名到下一个引号为止，否则到第一个空白为止。
pub fn strip_program(cmdline: &str) -> &str {
    let s = cmdline.trim_start();
    let rest = if let Some(after) = s.strip_prefix('"') {
        match after.find('"') {
            Some(i) => &after[i + 1..],
            None => "",
        }
    } else {
        match s.find([' ', '\t']) {
            Some(i) => &s[i..],
            None => "",
        }
    };
    rest.trim_start()
}

/// 按 explorer 规则切分参数：引号外的逗号分隔；每段去掉首尾空白与全部引号（路径里不可能有引号）；
/// 段首以 `/` 或 `-` 开头、后跟空白的开关被拆出（如 `/n /select`、`--entry C:\a b`）。空段被丢弃。
pub fn tokenize(tail: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in tail.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            ',' if !quoted => pieces.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    pieces.push(cur);

    let mut out = Vec::new();
    for piece in pieces {
        let mut rest = piece.trim();
        // 拆出段首的开关；开关本身不含引号
        loop {
            let starts_switch = rest.starts_with('/') || rest.starts_with('-');
            match rest.find(char::is_whitespace) {
                Some(i) if starts_switch && !rest[..i].contains('"') => {
                    out.push(rest[..i].to_string());
                    rest = rest[i..].trim_start();
                }
                _ => break,
            }
        }
        let v: String = rest.chars().filter(|&c| c != '"').collect();
        let v = v.trim();
        if !v.is_empty() {
            out.push(v.to_string());
        }
    }
    out
}

/// 解析原始参数（[`strip_program`] 之后的部分）。`cwd` 用于解析相对路径（explorer 相对其工作目录解析）。
pub fn parse(tail: &str, cwd: Option<&Path>) -> EntryRequest {
    let mut req = EntryRequest::default();
    let tokens = tokenize(tail);
    let mut it = tokens.into_iter().peekable();
    while let Some(tok) = it.next() {
        let lower = tok.to_ascii_lowercase();
        match lower.as_str() {
            ENTRY_FLAG => req.from_takeover = true,
            "/n" => req.new_window = true,
            "/e" => req.explore = true,
            "/separate" => req.separate = true,
            "/select" | "/root" => {
                let Some(v) = it.next_if(|t| !is_switch(t)) else {
                    req.unsupported.push(format!("{tok}（缺少目标）"));
                    continue;
                };
                match classify(&v, cwd) {
                    Some(t) if lower == "/select" => req.select = Some(t),
                    Some(t) => req.root = Some(t),
                    None => req.unsupported.push(format!("{tok},{v}")),
                }
            }
            "/idlist" | "/factory" => {
                let v = it.next_if(|t| !is_switch(t)).unwrap_or_default();
                req.unsupported.push(format!("{tok},{v}"));
            }
            "-embedding" | "/embedding" => req.unsupported.push(tok),
            _ if is_switch(&tok) => req.unsupported.push(tok),
            _ => match classify(&tok, cwd) {
                Some(t) if req.open.is_none() => req.open = Some(t),
                Some(_) => req.unsupported.push(tok),
                None => req.unsupported.push(tok),
            },
        }
    }
    req
}

/// 开关：以 `/` 开头（Windows 路径不会以 `/` 开头），或以 `-` 开头且是已知开关。
fn is_switch(t: &str) -> bool {
    t.starts_with('/') || t.eq_ignore_ascii_case(ENTRY_FLAG) || t.eq_ignore_ascii_case("-embedding")
}

/// 把一个参数归类为目标；无法解析时返回 `None`。
pub fn classify(raw: &str, cwd: Option<&Path>) -> Option<Target> {
    let s = raw.trim().trim_matches('"').trim();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = strip_prefix_ci(s, "::{") {
        let end = rest.find('}')?;
        let clsid = rest[..end].to_ascii_uppercase();
        let tail = rest[end + 1..].trim_start_matches('\\').to_string();
        return Some(Target::Clsid { clsid, rest: tail });
    }
    if let Some(rest) = strip_prefix_ci(s, "shell:") {
        let name = rest.trim_start_matches(':').trim();
        if name.starts_with('{') {
            // shell:::{GUID} 与 ::{GUID} 等价
            return classify(&format!("::{name}"), cwd);
        }
        return (!name.is_empty()).then(|| Target::Shell(name.to_string()));
    }
    if let Some(rest) = strip_prefix_ci(s, "file:") {
        return file_url_to_path(rest).map(Target::Path);
    }
    let s = s.replace('/', "\\");
    // \\?\C:\x → C:\x；\\?\UNC\srv\share → \\srv\share
    let s = if let Some(r) = strip_prefix_ci(&s, "\\\\?\\UNC\\") {
        format!("\\\\{r}")
    } else if let Some(r) = s.strip_prefix("\\\\?\\") {
        r.to_string()
    } else {
        s
    };
    let p = if is_unc(&s) || has_drive_root(&s) {
        PathBuf::from(&s)
    } else if is_bare_drive(&s) {
        PathBuf::from(format!("{s}\\"))
    } else if s.starts_with('\\') {
        // 当前盘的根相对路径（\Users）：需要当前盘
        let cwd = cwd?;
        let cwd = location::normalize(cwd);
        let root = cwd.components().next()?;
        let mut p = PathBuf::from(root.as_os_str());
        p.push(&s);
        p
    } else {
        cwd?.join(&s)
    };
    Some(Target::Path(location::normalize(&p)))
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let n = prefix.len();
    (s.len() >= n && s.is_char_boundary(n) && s[..n].eq_ignore_ascii_case(prefix)).then(|| &s[n..])
}

fn is_unc(s: &str) -> bool {
    let Some(r) = s.strip_prefix("\\\\") else {
        return false;
    };
    // 至少 \\server\share
    let mut parts = r.split('\\').filter(|p| !p.is_empty());
    parts.next().is_some() && parts.next().is_some()
}

fn is_bare_drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn has_drive_root(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\'
}

/// `file:` URL（冒号之后的部分）→ 路径。支持 `///C:/a%20b`、`//server/share/x`；百分号转义按 UTF-8 解码。
fn file_url_to_path(rest: &str) -> Option<PathBuf> {
    let decoded = percent_decode(rest)?;
    let d = decoded.replace('/', "\\");
    let path = if let Some(r) = d.strip_prefix("\\\\\\") {
        r.to_string() // file:///C:/x
    } else if let Some(r) = d.strip_prefix("\\\\localhost\\") {
        r.to_string()
    } else if d.starts_with("\\\\") {
        d // file://server/share
    } else {
        return None;
    };
    if is_unc(&path) || has_drive_root(&path) {
        Some(location::normalize(Path::new(&path)))
    } else if is_bare_drive(&path) {
        Some(PathBuf::from(format!("{path}\\")))
    } else {
        None
    }
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

impl EntryRequest {
    /// 决定处理方式。优先级：`/select` ＞ 裸路径 ＞ `/root` ＞ 无目标。
    /// 含 `/idlist`、`/factory`、`-Embedding` 且没有任何可用目标时交回 Windows。
    pub fn disposition(&self) -> Disposition {
        if let Some(sel) = &self.select {
            return match sel {
                Target::Path(item) => match item.parent() {
                    Some(folder) => Disposition::OpenAndSelect {
                        folder: folder.to_path_buf(),
                        item: item.clone(),
                    },
                    // 选中驱动器根（/select,C:\）：explorer 会打开“此电脑”并选中该盘；应用里直接打开该盘
                    None => Disposition::OpenFolder(item.clone()),
                },
                other => virtual_disposition(other, true),
            };
        }
        if let Some(t) = self.open.as_ref().or(self.root.as_ref()) {
            return match t {
                Target::Path(p) => Disposition::OpenFolder(p.clone()),
                other => virtual_disposition(other, false),
            };
        }
        if let Some(opaque) = self
            .unsupported
            .iter()
            .find(|u| is_opaque(u))
            .filter(|_| !self.from_takeover)
        {
            return Disposition::System(SystemReason::Opaque(opaque.clone()));
        }
        Disposition::OpenDefault
    }
}

fn is_opaque(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    l.starts_with("/idlist") || l.starts_with("/factory") || l.ends_with("embedding")
}

fn virtual_disposition(t: &Target, select: bool) -> Disposition {
    match t {
        Target::Path(p) => Disposition::OpenFolder(p.clone()),
        Target::Shell(name) => {
            let full = format!("shell:{name}");
            if full.eq_ignore_ascii_case(RECYCLE_BIN) {
                Disposition::OpenFolder(location::recycle_bin_path())
            } else if name.eq_ignore_ascii_case("MyComputerFolder") {
                Disposition::System(SystemReason::ThisPc)
            } else {
                Disposition::ResolveShell {
                    name: name.clone(),
                    select,
                }
            }
        }
        Target::Clsid { clsid, rest } => {
            if clsid == CLSID_RECYCLE_BIN && rest.is_empty() {
                Disposition::OpenFolder(location::recycle_bin_path())
            } else if clsid == CLSID_THIS_PC && rest.is_empty() {
                Disposition::System(SystemReason::ThisPc)
            } else if clsid == CLSID_THIS_PC && (has_drive_root(rest) || is_bare_drive(rest)) {
                // ::{此电脑}\C:\ 这类写法：落到真实路径
                let p = classify(rest, None);
                match p {
                    Some(Target::Path(p)) => Disposition::OpenFolder(p),
                    _ => Disposition::System(SystemReason::ThisPc),
                }
            } else {
                Disposition::System(SystemReason::Virtual(format!("::{{{clsid}}}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn path(s: &str) -> Option<Target> {
        Some(Target::Path(p(s)))
    }

    #[test]
    fn strips_program_name() {
        assert_eq!(
            strip_program(r#""C:\Program Files\sfm\sfm.exe" --entry "C:\a b""#),
            r#"--entry "C:\a b""#
        );
        assert_eq!(strip_program(r"explorer.exe /select,C:\x"), r"/select,C:\x");
        assert_eq!(strip_program("sfm.exe"), "");
        assert_eq!(strip_program(r#""C:\sfm.exe""#), "");
        assert_eq!(strip_program(r#"  "C:\sfm.exe"   /e,C:\"#), r"/e,C:\");
    }

    #[test]
    fn tokenizes_explorer_style() {
        assert_eq!(
            tokenize(r"/select,C:\a\b.txt"),
            vec!["/select", r"C:\a\b.txt"]
        );
        assert_eq!(
            tokenize(r#"/select,"C:\a,b\c d.txt""#),
            vec!["/select", r"C:\a,b\c d.txt"]
        );
        assert_eq!(
            tokenize(r"/e,/root,C:\TestDir"),
            vec!["/e", "/root", r"C:\TestDir"]
        );
        assert_eq!(tokenize(r"/n /e, C:\x"), vec!["/n", "/e", r"C:\x"]);
        assert_eq!(
            tokenize(r#"--entry "C:\Program Files""#),
            vec!["--entry", r"C:\Program Files"]
        );
        assert_eq!(tokenize(",, ,"), Vec::<String>::new());
        // 不带引号的空格路径保持为一个参数（explorer 不按空格切分）
        assert_eq!(
            tokenize(r"/select,C:\Program Files\a.txt"),
            vec!["/select", r"C:\Program Files\a.txt"]
        );
    }

    #[test]
    fn takeover_verb_with_drive_root_keeps_backslash() {
        // 动词命令 "<exe>" --entry "%1"，%1 = C:\ 时 MSVC 规则会得到 C:"；原始命令行解析不受影响
        let r = parse(r#"--entry "C:\""#, None);
        assert!(r.from_takeover);
        assert_eq!(r.open, path(r"C:\"));
        assert_eq!(r.disposition(), Disposition::OpenFolder(p(r"C:\")));
        let r = parse(r#"--entry "D:""#, None);
        assert_eq!(r.open, path(r"D:\"));
    }

    #[test]
    fn takeover_verb_with_chinese_and_spaces() {
        let r = parse(r#"--entry "E:\文档\我的 照片""#, None);
        assert_eq!(r.open, path(r"E:\文档\我的 照片"));
        assert!(r.unsupported.is_empty());
    }

    #[test]
    fn win_e_without_target_opens_default() {
        let r = parse("--entry", None);
        assert!(r.from_takeover);
        assert_eq!(r.disposition(), Disposition::OpenDefault);
        assert_eq!(parse("", None).disposition(), Disposition::OpenDefault);
    }

    #[test]
    fn select_opens_parent_and_selects() {
        let r = parse(r#"/select,"C:\Users\me\下载\a b.zip""#, None);
        assert_eq!(
            r.disposition(),
            Disposition::OpenAndSelect {
                folder: p(r"C:\Users\me\下载"),
                item: p(r"C:\Users\me\下载\a b.zip"),
            }
        );
        // 开关大小写不敏感，/ 视为 \，. 与 .. 按字面消去
        let r = parse(r"/SELECT,C:/a/./b/../c.txt", None);
        assert_eq!(r.select, path(r"C:\a\c.txt"));
        // /select 选中驱动器根
        assert_eq!(
            parse(r"/select,C:\", None).disposition(),
            Disposition::OpenFolder(p(r"C:\"))
        );
    }

    #[test]
    fn select_takes_priority_and_flags_are_recorded() {
        let r = parse(r"/n,/e,/root,D:\r,/select,D:\r\x.txt", None);
        assert!(r.new_window && r.explore);
        assert_eq!(r.root, path(r"D:\r"));
        assert!(matches!(r.disposition(), Disposition::OpenAndSelect { .. }));
        let r = parse(r"/e,/root,D:\r", None);
        assert_eq!(r.disposition(), Disposition::OpenFolder(p(r"D:\r")));
        let r = parse(r"/separate,C:\x", None);
        assert!(r.separate);
        assert_eq!(r.disposition(), Disposition::OpenFolder(p(r"C:\x")));
    }

    #[test]
    fn unc_and_verbatim_paths() {
        assert_eq!(
            parse(r"\\nas\share\照片", None).open,
            path(r"\\nas\share\照片")
        );
        assert_eq!(
            parse(r"/select,\\nas\share\a.jpg", None).disposition(),
            Disposition::OpenAndSelect {
                folder: p(r"\\nas\share\"),
                item: p(r"\\nas\share\a.jpg"),
            }
        );
        assert_eq!(parse(r"\\?\C:\very\long", None).open, path(r"C:\very\long"));
        assert_eq!(
            parse(r"\\?\UNC\nas\share\x", None).open,
            path(r"\\nas\share\x")
        );
        // 只有 \\server 不是合法 UNC 目标，也没有工作目录可解析
        assert_eq!(parse(r"\\nas", None).open, None);
    }

    #[test]
    fn relative_paths_need_cwd() {
        let cwd = p(r"C:\work\proj");
        assert_eq!(
            parse(r"src\..\docs", Some(&cwd)).open,
            path(r"C:\work\proj\docs")
        );
        assert_eq!(parse(r"\Windows", Some(&cwd)).open, path(r"C:\Windows"));
        let r = parse("docs", None);
        assert_eq!(r.open, None);
        assert_eq!(r.unsupported, vec!["docs"]);
    }

    #[test]
    fn file_urls() {
        assert_eq!(
            parse("file:///C:/a%20b/%E4%B8%AD.txt", None).open,
            path(r"C:\a b\中.txt")
        );
        assert_eq!(
            parse("file://nas/share/x", None).open,
            path(r"\\nas\share\x")
        );
        assert_eq!(parse("file:///relative", None).open, None);
        assert_eq!(parse("file:///C:/bad%zz", None).open, None);
    }

    #[test]
    fn shell_and_clsid_virtual_locations() {
        let rb = location::recycle_bin_path();
        assert_eq!(
            parse("shell:RecycleBinFolder", None).disposition(),
            Disposition::OpenFolder(rb.clone())
        );
        assert_eq!(
            parse("::{645ff040-5081-101b-9f08-00aa002f954e}", None).disposition(),
            Disposition::OpenFolder(rb)
        );
        assert_eq!(
            parse("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}", None).disposition(),
            Disposition::System(SystemReason::ThisPc)
        );
        assert_eq!(
            parse("shell:MyComputerFolder", None).disposition(),
            Disposition::System(SystemReason::ThisPc)
        );
        assert_eq!(
            parse(r"::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\D:\", None).disposition(),
            Disposition::OpenFolder(p(r"D:\"))
        );
        // 控制面板
        assert_eq!(
            parse("::{26EE0668-A00A-44D7-9371-BEB064C98683}", None).disposition(),
            Disposition::System(SystemReason::Virtual(
                "::{26EE0668-A00A-44D7-9371-BEB064C98683}".into()
            ))
        );
        assert_eq!(
            parse("shell:Downloads", None).disposition(),
            Disposition::ResolveShell {
                name: "Downloads".into(),
                select: false
            }
        );
        assert_eq!(
            parse(r"/select,shell:Desktop", None).disposition(),
            Disposition::ResolveShell {
                name: "Desktop".into(),
                select: true
            }
        );
        assert_eq!(
            parse("shell:::{645FF040-5081-101B-9F08-00AA002F954E}", None).disposition(),
            Disposition::OpenFolder(location::recycle_bin_path())
        );
        assert_eq!(classify("shell:", None), None);
        assert_eq!(classify("::{broken", None), None);
    }

    #[test]
    fn opaque_and_unknown_switches() {
        let r = parse(r"/idlist,:2416:1234", None);
        assert_eq!(r.unsupported, vec!["/idlist,:2416:1234"]);
        assert!(matches!(
            r.disposition(),
            Disposition::System(SystemReason::Opaque(_))
        ));
        let r = parse(
            r"/factory,{75dff2b7-6936-4c06-a8bb-676a7b00b24b} -Embedding",
            None,
        );
        assert_eq!(r.unsupported.len(), 1);
        assert!(matches!(r.disposition(), Disposition::System(_)));
        // 未知开关被记录但不影响路径
        let r = parse(r"/weird,C:\x", None);
        assert_eq!(r.unsupported, vec!["/weird"]);
        assert_eq!(r.disposition(), Disposition::OpenFolder(p(r"C:\x")));
        // /select 缺少目标
        let r = parse("/select", None);
        assert_eq!(r.unsupported.len(), 1);
        assert_eq!(r.disposition(), Disposition::OpenDefault);
        // 多余的位置参数只保留第一个
        let r = parse(r"C:\a,D:\b", None);
        assert_eq!(r.open, path(r"C:\a"));
        assert_eq!(r.unsupported, vec![r"D:\b"]);
    }

    #[test]
    fn arguments_are_never_commands() {
        // 形似命令的参数只会被当作（不存在的）相对路径或无法解析的目标，不会被执行
        let r = parse(r#""C:\x" & calc.exe"#, None);
        assert_eq!(r.open, path(r"C:\x & calc.exe"));
        let r = parse("calc.exe", None);
        assert_eq!(r.open, None);
    }
}
