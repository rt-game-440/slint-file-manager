//! 查询语法：名称、扩展名、路径三类条件，空白分隔的多个条件全部满足才算匹配（AND）。
//!
//! 规则（大小写一律不敏感，按 Unicode 小写比较）：
//!
//! | 写法 | 含义 | 例子 |
//! | --- | --- | --- |
//! | `报告` | 文件名包含该文字 | `报告` 匹配 `年度报告2.txt`、`说明.md` 不匹配 |
//! | `.pdf` 或 `ext:pdf` | 扩展名等于（文件夹不匹配）；`ext:` 可用逗号或分号列出多个 | `ext:jpg,png` |
//! | `path:项目` | 完整路径包含该文字 | `path:照片\海边` |
//! | 含 `\` 或 `/` 的词 | 同 `path:`，完整路径包含该文字 | `项目\结论` |
//! | `*` `?` | 通配符，仍按“名称中任意位置出现”匹配 | `海边0*.webp` |
//! | `"含 空格 的词"` | 双引号内为一个整体，且一律按名称文字匹配（不解释前缀与通配符） | `".git"` |
//!
//! 例：`报告 .txt` = 名称含“报告”且扩展名为 txt；`path:照片 .webp` = 路径含“照片”的 webp 文件。

/// 一个条件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    /// 名称包含（已小写）
    Name(String),
    /// 名称通配（已小写，`*` `?`），在名称任意位置匹配
    Wild(Vec<char>),
    /// 扩展名等于其中之一（已小写，不含点）
    Ext(Vec<String>),
    /// 完整路径包含（已小写，统一为 `\`）
    Path(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub terms: Vec<Term>,
    raw: String,
}

struct Token {
    text: String,
    quoted: bool,
}

fn tokenize(s: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut in_q = false;
    let mut has = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                if in_q {
                    in_q = false;
                } else {
                    in_q = true;
                    quoted = true;
                    has = true;
                }
            }
            c if c.is_whitespace() && !in_q => {
                if has {
                    out.push(Token {
                        text: std::mem::take(&mut cur),
                        quoted,
                    });
                    quoted = false;
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(Token { text: cur, quoted });
    }
    out
}

fn norm_path(s: &str) -> String {
    s.replace('/', "\\").to_lowercase()
}

impl Query {
    pub fn parse(input: &str) -> Query {
        let mut terms = Vec::new();
        for t in tokenize(input) {
            if t.text.is_empty() {
                continue;
            }
            let lower = t.text.to_lowercase();
            let term = if t.quoted {
                Term::Name(lower)
            } else if let Some(rest) = lower.strip_prefix("ext:") {
                let exts: Vec<String> = rest
                    .split([',', ';'])
                    .map(|e| e.trim().trim_start_matches('.').to_string())
                    .filter(|e| !e.is_empty())
                    .collect();
                if exts.is_empty() {
                    continue;
                }
                Term::Ext(exts)
            } else if let Some(rest) = lower.strip_prefix("path:") {
                if rest.is_empty() {
                    continue;
                }
                Term::Path(norm_path(rest))
            } else if lower.contains('\\') || lower.contains('/') {
                Term::Path(norm_path(&lower))
            } else if lower.chars().count() > 1
                && lower.starts_with('.')
                && !lower.contains(['*', '?'])
                && !lower[1..].contains('.')
            {
                Term::Ext(vec![lower[1..].to_string()])
            } else if lower.contains(['*', '?']) {
                Term::Wild(lower.chars().collect())
            } else {
                Term::Name(lower)
            };
            terms.push(term);
        }
        Query {
            terms,
            raw: input.trim().to_string(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// 是否有需要完整路径的条件。
    pub fn needs_path(&self) -> bool {
        self.terms.iter().any(|t| matches!(t, Term::Path(_)))
    }

    /// 名称（含扩展名）与是否文件夹；`path` 只在有路径条件时才被调用（至多一次）。
    pub fn matches(&self, name: &str, is_dir: bool, path: impl FnOnce() -> String) -> bool {
        if self.terms.is_empty() {
            return false;
        }
        let lname = name.to_lowercase();
        let mut chars: Option<Vec<char>> = None;
        let mut path_lower: Option<String> = None;
        let mut path = Some(path);
        for t in &self.terms {
            let ok = match t {
                Term::Name(s) => lname.contains(s.as_str()),
                Term::Wild(p) => {
                    let c = chars.get_or_insert_with(|| lname.chars().collect());
                    wild_contains(p, c)
                }
                Term::Ext(list) => {
                    !is_dir && ext_of(&lname).is_some_and(|x| list.iter().any(|e| e == x))
                }
                Term::Path(s) => {
                    if path_lower.is_none()
                        && let Some(f) = path.take()
                    {
                        path_lower = Some(norm_path(&f()));
                    }
                    path_lower
                        .as_deref()
                        .is_some_and(|p| p.contains(s.as_str()))
                }
            };
            if !ok {
                return false;
            }
        }
        true
    }
}

fn ext_of(lname: &str) -> Option<&str> {
    match lname.rfind('.') {
        Some(i) if i > 0 && i + 1 < lname.len() => Some(&lname[i + 1..]),
        _ => None,
    }
}

/// `pat`（含 `*` `?`）在 `text` 的任意位置出现，等价于整串匹配 `*pat*`。
fn wild_contains(pat: &[char], text: &[char]) -> bool {
    let mut full: Vec<char> = Vec::with_capacity(pat.len() + 2);
    full.push('*');
    full.extend_from_slice(pat);
    full.push('*');
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < full.len() && full[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if p < full.len() && (full[p] == '?' || full[p] == text[t]) {
            p += 1;
            t += 1;
        } else if let Some((sp, st)) = star {
            p = sp + 1;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while p < full.len() && full[p] == '*' {
        p += 1;
    }
    p == full.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(q: &str, name: &str, is_dir: bool, path: &str) -> bool {
        Query::parse(q).matches(name, is_dir, || path.to_string())
    }

    #[test]
    fn name_substring_is_case_insensitive() {
        assert!(m("报告", "年度报告2.txt", false, ""));
        assert!(m("report", "Annual REPORT.txt", false, ""));
        assert!(!m("报告", "说明.md", false, ""));
        assert!(m("ÄÖ", "äöü.txt", false, ""), "非 ASCII 大小写同样不敏感");
    }

    #[test]
    fn all_terms_must_match() {
        assert!(m("报告 年度", "年度报告.txt", false, ""));
        assert!(!m("报告 季度", "年度报告.txt", false, ""));
    }

    #[test]
    fn extension_terms() {
        assert!(m(".pdf", "A.PDF", false, ""));
        assert!(m("ext:jpg;png", "x.png", false, ""));
        assert!(m("ext:.JPG,png", "x.jpg", false, ""));
        assert!(!m(".pdf", "a.pdf.bak", false, ""));
        assert!(!m(".pdf", "文件夹.pdf", true, ""), "文件夹没有扩展名");
        assert!(!m(".git", "noext", false, ""));
        assert!(m("报告 .txt", "报告2.txt", false, ""));
        assert!(!m("报告 .txt", "报告2.md", false, ""));
    }

    #[test]
    fn dotfile_is_not_extension_of_itself() {
        // 名称就是 ".gitignore" 时没有扩展名；引号可强制按名称匹配
        assert!(!m(".gitignore", ".gitignore", false, ""));
        assert!(m("\".gitignore\"", ".gitignore", false, ""));
    }

    #[test]
    fn path_terms() {
        let p = "C:\\Users\\张三\\照片\\海边001.webp";
        assert!(m("path:照片", "海边001.webp", false, p));
        assert!(m("照片\\海边", "海边001.webp", false, p));
        assert!(m("照片/海边", "海边001.webp", false, p));
        assert!(!m("path:文档", "海边001.webp", false, p));
        assert!(m("path:USERS .webp", "海边001.webp", false, p));
    }

    #[test]
    fn path_is_lazy() {
        let q = Query::parse("报告");
        assert!(!q.needs_path());
        assert!(q.matches("报告.txt", false, || panic!("不应读取路径")));
        assert!(Query::parse("path:a").needs_path());
    }

    #[test]
    fn wildcards_match_anywhere() {
        assert!(m("海边0*.webp", "海边001.webp", false, ""));
        assert!(m("海?001", "x海边001.webp", false, ""));
        assert!(!m("海边0*.png", "海边001.webp", false, ""));
        assert!(m("a*b", "xxaYYbzz", false, ""));
        assert!(!m("a*b", "xxbYYazz", false, ""));
        assert!(m("*", "任意", false, ""));
    }

    #[test]
    fn quotes_group_words() {
        assert!(m("\"a b\"", "x a b y", false, ""));
        assert!(!m("\"a b\"", "a_b", false, ""));
        assert!(
            m("\"ext:pdf\"", "ext:pdf.txt", false, ""),
            "引号内不解释前缀"
        );
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert!(Query::parse("   ").is_empty());
        assert!(!m("", "a", false, ""));
        assert!(Query::parse("ext:").is_empty());
    }
}
