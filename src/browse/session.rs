//! 浏览会话的持久化：数据模型、JSON 格式与磁盘存取。
//!
//! 会话 = 两个浏览面板各自的标签组（位置、排序、视图方式、前进后退历史）、各面板活动标签、
//! 活动面板与是否双栏。不保存选择、滚动位置，也不记录文件内容；恢复时不会自动打开任何文件。
//!
//! 文件：`<数据目录>\session.json`，UTF-8 JSON，顶层必有整数 `version`（当前为 [`VERSION`]）。
//! 数据目录默认 `%LOCALAPPDATA%\slint-file-manager`，环境变量 `SFM_DATA_DIR` 或启动参数 `--data-dir=` 可覆盖
//! （测试一律用临时目录，不碰用户真实配置）。
//!
//! 安全降级：文件无法解析、结构不符、版本不是当前版本、过大时，**不使用也不覆盖**它，而是把原文件移动为
//! `session.backup-<时间>-<原因>.json`，本次使用默认会话并向调用方返回说明文字；备份失败时本次运行不再写会话，
//! 原文件保持不动。写入使用“临时文件 + 重命名”，写到一半崩溃不会留下半个文件。

use super::sort::{SortKey, SortSpec};
use super::tab::{TabSeed, ViewMode};
use super::workspace::MAX_TABS;
use serde_json::{Map, Value, json};
use std::cell::Cell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 当前会话文件格式版本。
pub const VERSION: i64 = 1;
/// 会话文件名。
pub const FILE: &str = "session.json";
/// 超过该大小的会话文件视为损坏（正常内容远小于此）。
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// 保留的备份数量上限（按时间保留最新的）。
const KEEP_BACKUPS: usize = 5;

/// 一个浏览面板的持久状态。`tabs` 至少一项。
#[derive(Clone, Debug, PartialEq)]
pub struct PaneSeed {
    pub tabs: Vec<TabSeed>,
    pub active: usize,
}

/// 整个浏览会话。
#[derive(Clone, Debug, PartialEq)]
pub struct SessionState {
    pub split: bool,
    pub active_pane: usize,
    pub panes: [PaneSeed; 2],
}

impl SessionState {
    /// 首次运行的默认会话：双栏等宽，每侧一个标签。
    pub fn fresh(left: PathBuf, right: PathBuf, split: bool) -> SessionState {
        let seed = |p: PathBuf| PaneSeed {
            tabs: vec![TabSeed {
                location: p,
                sort: SortSpec::default(),
                view: ViewMode::default(),
                back: Vec::new(),
                forward: Vec::new(),
            }],
            active: 0,
        };
        SessionState {
            split,
            active_pane: 0,
            panes: [seed(left), seed(right)],
        }
    }
}

// ---------- JSON ----------

/// 会话文件无法使用的原因。
#[derive(Debug, PartialEq)]
pub enum ParseError {
    /// 不是合法 JSON（或不是 UTF-8）。
    Syntax(String),
    /// 版本号不是当前支持的版本。
    Version(i64),
    /// JSON 合法但结构、类型或取值不符合格式。
    Invalid(String),
}

fn sort_key_name(k: SortKey) -> &'static str {
    match k {
        SortKey::Name => "name",
        SortKey::Modified => "modified",
        SortKey::Size => "size",
        SortKey::Kind => "kind",
    }
}

fn view_name(v: ViewMode) -> &'static str {
    match v {
        ViewMode::Grid => "grid",
        ViewMode::List => "list",
    }
}

fn path_json(p: &Path) -> Value {
    json!(p.to_string_lossy())
}

/// 序列化为带缩进的 JSON 文本（含 `saved_at` 本地时间，仅供人查看）。
pub fn to_json(state: &SessionState) -> String {
    let pane = |p: &PaneSeed| {
        json!({
            "active": p.active,
            "tabs": p.tabs.iter().map(|t| json!({
                "location": path_json(&t.location),
                "sort": {"key": sort_key_name(t.sort.key), "ascending": t.sort.ascending},
                "view": view_name(t.view),
                "back": t.back.iter().map(|p| path_json(p)).collect::<Vec<_>>(),
                "forward": t.forward.iter().map(|p| path_json(p)).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })
    };
    let v = json!({
        "version": VERSION,
        "saved_at": chrono::Local::now().to_rfc3339(),
        "split": state.split,
        "active_pane": state.active_pane,
        "panes": [pane(&state.panes[0]), pane(&state.panes[1])],
    });
    serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError::Invalid(msg.into()))
}

fn field<'a>(o: &'a Map<String, Value>, k: &str, ctx: &str) -> Result<&'a Value, ParseError> {
    o.get(k)
        .ok_or_else(|| ParseError::Invalid(format!("{ctx}缺少字段 {k}")))
}

fn as_object<'a>(v: &'a Value, ctx: &str) -> Result<&'a Map<String, Value>, ParseError> {
    v.as_object()
        .ok_or_else(|| ParseError::Invalid(format!("{ctx}应为对象")))
}

fn as_bool(v: &Value, ctx: &str) -> Result<bool, ParseError> {
    v.as_bool()
        .ok_or_else(|| ParseError::Invalid(format!("{ctx}应为布尔值")))
}

fn as_index(v: &Value, ctx: &str) -> Result<usize, ParseError> {
    v.as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| ParseError::Invalid(format!("{ctx}应为非负整数")))
}

fn as_path(v: &Value, ctx: &str) -> Result<PathBuf, ParseError> {
    let s = v
        .as_str()
        .ok_or_else(|| ParseError::Invalid(format!("{ctx}应为字符串")))?;
    let p = PathBuf::from(s);
    // 回收站是虚拟位置（不是磁盘路径），允许进入会话
    if !p.is_absolute() && !super::location::is_recycle_bin(&p) {
        return invalid(format!("{ctx}不是绝对路径"));
    }
    Ok(p)
}

fn path_list(v: &Value, ctx: &str) -> Result<Vec<PathBuf>, ParseError> {
    let a = v
        .as_array()
        .ok_or_else(|| ParseError::Invalid(format!("{ctx}应为数组")))?;
    a.iter().map(|p| as_path(p, ctx)).collect()
}

fn parse_tab(v: &Value) -> Result<TabSeed, ParseError> {
    let o = as_object(v, "标签")?;
    let location = as_path(field(o, "location", "标签")?, "标签位置")?;
    let sort_o = as_object(field(o, "sort", "标签")?, "排序")?;
    let key = match field(sort_o, "key", "排序")?.as_str() {
        Some("name") => SortKey::Name,
        Some("modified") => SortKey::Modified,
        Some("size") => SortKey::Size,
        Some("kind") => SortKey::Kind,
        _ => return invalid("排序键无法识别"),
    };
    let ascending = as_bool(field(sort_o, "ascending", "排序")?, "排序方向")?;
    let view = match field(o, "view", "标签")?.as_str() {
        Some("grid") => ViewMode::Grid,
        Some("list") => ViewMode::List,
        _ => return invalid("视图方式无法识别"),
    };
    // 历史是附带信息：缺失视为空，类型不符才算损坏
    let hist = |k: &str| match o.get(k) {
        Some(v) => path_list(v, "历史"),
        None => Ok(Vec::new()),
    };
    Ok(TabSeed {
        location,
        sort: SortSpec { key, ascending },
        view,
        back: hist("back")?,
        forward: hist("forward")?,
    })
}

fn parse_pane(v: &Value) -> Result<PaneSeed, ParseError> {
    let o = as_object(v, "面板")?;
    let tabs = field(o, "tabs", "面板")?
        .as_array()
        .ok_or_else(|| ParseError::Invalid("面板标签列表应为数组".into()))?;
    if tabs.is_empty() {
        return invalid("面板没有任何标签");
    }
    if tabs.len() > MAX_TABS {
        return invalid(format!("面板标签数 {} 超过上限 {MAX_TABS}", tabs.len()));
    }
    let tabs = tabs.iter().map(parse_tab).collect::<Result<Vec<_>, _>>()?;
    // 活动索引越界属于无害的不一致：收敛到最后一个标签，不判为损坏
    let active = as_index(field(o, "active", "面板")?, "活动标签")?.min(tabs.len() - 1);
    Ok(PaneSeed { tabs, active })
}

/// 解析会话文件内容。版本不符优先于结构检查，使“较新版本写的文件”不被误报为损坏。
pub fn from_json(bytes: &[u8]) -> Result<SessionState, ParseError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| ParseError::Syntax(e.to_string()))?;
    let o = v
        .as_object()
        .ok_or_else(|| ParseError::Invalid("顶层应为对象".into()))?;
    let version = o
        .get("version")
        .and_then(Value::as_i64)
        .ok_or_else(|| ParseError::Invalid("缺少整数版本号 version".into()))?;
    if version != VERSION {
        return Err(ParseError::Version(version));
    }
    let panes = field(o, "panes", "会话")?
        .as_array()
        .ok_or_else(|| ParseError::Invalid("panes 应为数组".into()))?;
    if panes.len() != 2 {
        return invalid(format!("panes 应有 2 项，实际 {}", panes.len()));
    }
    let active_pane = as_index(field(o, "active_pane", "会话")?, "活动面板")?;
    if active_pane > 1 {
        return invalid("活动面板应为 0 或 1");
    }
    Ok(SessionState {
        split: as_bool(field(o, "split", "会话")?, "split")?,
        active_pane,
        panes: [parse_pane(&panes[0])?, parse_pane(&panes[1])?],
    })
}

// ---------- 存取 ----------

/// 加载结果。
#[derive(Debug)]
pub enum LoadOutcome {
    /// 没有会话文件（首次运行或已被移走）。
    Missing,
    Restored(SessionState),
    /// 文件不可用，已降级：调用方使用默认会话并向用户展示 `message`。
    Degraded(Degraded),
}

#[derive(Debug)]
pub struct Degraded {
    /// 给用户看的说明：原因、备份位置、本次使用默认会话。
    pub message: String,
    /// 备份文件位置；备份失败为 `None`（此时本次运行不写会话）。
    pub backup: Option<PathBuf>,
}

pub struct SessionStore {
    dir: PathBuf,
    /// 为 false 时 [`SessionStore::save`] 拒绝写入（原文件无法移走，不覆盖它）
    writable: Cell<bool>,
}

fn is_backup_name(n: &str) -> bool {
    n.starts_with("session.backup-") && n.ends_with(".json")
}

impl SessionStore {
    pub fn at(dir: impl Into<PathBuf>) -> SessionStore {
        SessionStore {
            dir: dir.into(),
            writable: Cell::new(true),
        }
    }

    /// 默认位置：`SFM_DATA_DIR`，否则 `%LOCALAPPDATA%\slint-file-manager`。两者都没有时返回 `None`（不保存会话）。
    pub fn default_location() -> Option<SessionStore> {
        if let Some(d) = std::env::var_os("SFM_DATA_DIR").filter(|d| !d.is_empty()) {
            return Some(SessionStore::at(d));
        }
        let base = std::env::var_os("LOCALAPPDATA").filter(|d| !d.is_empty())?;
        Some(SessionStore::at(
            PathBuf::from(base).join("slint-file-manager"),
        ))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE)
    }

    /// 读取会话。损坏或版本不兼容时把原文件移成备份（见模块说明）。
    pub fn load(&self) -> LoadOutcome {
        let path = self.path();
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return LoadOutcome::Missing,
            Err(e) => {
                // 文件在但读不了（权限等）：不覆盖它
                self.writable.set(false);
                return LoadOutcome::Degraded(Degraded {
                    message: format!("无法读取上次的会话（{e}），本次使用默认会话且不会保存会话。"),
                    backup: None,
                });
            }
        };
        let parsed = if meta.len() > MAX_BYTES {
            Err(ParseError::Invalid(format!(
                "文件大小 {} 字节超过上限",
                meta.len()
            )))
        } else {
            match fs::read(&path) {
                Ok(b) => from_json(&b),
                Err(e) => {
                    self.writable.set(false);
                    return LoadOutcome::Degraded(Degraded {
                        message: format!(
                            "无法读取上次的会话（{e}），本次使用默认会话且不会保存会话。"
                        ),
                        backup: None,
                    });
                }
            }
        };
        match parsed {
            Ok(state) => LoadOutcome::Restored(state),
            Err(err) => {
                let (tag, why) = match &err {
                    ParseError::Version(v) => (
                        format!("v{v}"),
                        format!("会话文件版本为 {v}，当前只支持版本 {VERSION}"),
                    ),
                    ParseError::Syntax(m) => {
                        ("corrupt".to_string(), format!("会话文件已损坏（{m}）"))
                    }
                    ParseError::Invalid(m) => {
                        ("corrupt".to_string(), format!("会话文件格式不正确（{m}）"))
                    }
                };
                LoadOutcome::Degraded(self.set_aside(&tag, &why))
            }
        }
    }

    /// 把当前会话文件移成备份。
    fn set_aside(&self, tag: &str, why: &str) -> Degraded {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let mut target = self.dir.join(format!("session.backup-{ts}-{tag}.json"));
        let mut n = 1;
        while target.exists() {
            target = self.dir.join(format!("session.backup-{ts}-{tag}-{n}.json"));
            n += 1;
        }
        match fs::rename(self.path(), &target) {
            Ok(()) => {
                self.prune_backups();
                Degraded {
                    message: format!(
                        "{why}，原文件已备份为 {}，本次使用默认会话。",
                        target
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    ),
                    backup: Some(target),
                }
            }
            Err(e) => {
                self.writable.set(false);
                Degraded {
                    message: format!(
                        "{why}，且无法备份（{e}）；本次使用默认会话，不会保存会话以免覆盖原文件。"
                    ),
                    backup: None,
                }
            }
        }
    }

    fn prune_backups(&self) {
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| is_backup_name(n))
            .collect();
        names.sort();
        if names.len() > KEEP_BACKUPS {
            for n in &names[..names.len() - KEEP_BACKUPS] {
                let _ = fs::remove_file(self.dir.join(n));
            }
        }
    }

    /// 原子写入会话文件。目录不存在时创建。
    pub fn save(&self, state: &SessionState) -> io::Result<()> {
        if !self.writable.get() {
            return Err(io::Error::other("会话文件不可覆盖，已停止保存"));
        }
        fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(format!("{FILE}.tmp"));
        {
            use std::io::Write;
            let mut f = fs::File::create(&tmp)?;
            f.write_all(to_json(state).as_bytes())?;
            f.sync_data()?;
        }
        fs::rename(&tmp, self.path())
    }

    pub fn is_writable(&self) -> bool {
        self.writable.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(p: &str, key: SortKey, asc: bool, view: ViewMode) -> TabSeed {
        TabSeed {
            location: p.into(),
            sort: SortSpec {
                key,
                ascending: asc,
            },
            view,
            back: vec![r"C:\前".into()],
            forward: vec![r"C:\后".into()],
        }
    }

    fn sample() -> SessionState {
        SessionState {
            split: false,
            active_pane: 1,
            panes: [
                PaneSeed {
                    tabs: vec![
                        tab(r"C:\左一", SortKey::Name, true, ViewMode::Grid),
                        tab(r"D:\左二（含中文）", SortKey::Size, false, ViewMode::List),
                    ],
                    active: 1,
                },
                PaneSeed {
                    tabs: vec![tab(
                        r"\\server\share\右",
                        SortKey::Kind,
                        true,
                        ViewMode::List,
                    )],
                    active: 0,
                },
            ],
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("sfm-session-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn json_roundtrip_keeps_everything() {
        let s = sample();
        assert_eq!(from_json(to_json(&s).as_bytes()).unwrap(), s);
    }

    #[test]
    fn rejects_garbage_and_wrong_versions() {
        assert!(matches!(
            from_json(b"{ not json"),
            Err(ParseError::Syntax(_))
        ));
        assert!(matches!(
            from_json(&[0xff, 0xfe, 0x00]),
            Err(ParseError::Syntax(_))
        ));
        assert!(matches!(from_json(b"[]"), Err(ParseError::Invalid(_))));
        assert!(matches!(from_json(b"{}"), Err(ParseError::Invalid(_))));
        assert_eq!(from_json(br#"{"version":2}"#), Err(ParseError::Version(2)));
        assert_eq!(from_json(br#"{"version":0}"#), Err(ParseError::Version(0)));
        // 版本正确但结构不符
        assert!(matches!(
            from_json(br#"{"version":1,"split":true,"active_pane":0,"panes":[]}"#),
            Err(ParseError::Invalid(_))
        ));
    }

    #[test]
    fn rejects_bad_fields_but_clamps_active() {
        let mut v: Value = serde_json::from_str(&to_json(&sample())).unwrap();
        v["panes"][0]["active"] = json!(99);
        let s = from_json(v.to_string().as_bytes()).unwrap();
        assert_eq!(s.panes[0].active, 1, "越界的活动索引收敛到最后一个标签");

        let mut v: Value = serde_json::from_str(&to_json(&sample())).unwrap();
        v["panes"][1]["tabs"][0]["location"] = json!("相对路径");
        assert!(matches!(
            from_json(v.to_string().as_bytes()),
            Err(ParseError::Invalid(_))
        ));

        let mut v: Value = serde_json::from_str(&to_json(&sample())).unwrap();
        v["panes"][1]["tabs"][0]["view"] = json!("cover-flow");
        assert!(matches!(
            from_json(v.to_string().as_bytes()),
            Err(ParseError::Invalid(_))
        ));

        let mut v: Value = serde_json::from_str(&to_json(&sample())).unwrap();
        v["panes"][0]["tabs"] = json!([]);
        assert!(matches!(
            from_json(v.to_string().as_bytes()),
            Err(ParseError::Invalid(_))
        ));
    }

    #[test]
    fn store_roundtrip_and_missing() {
        let d = tmp("roundtrip");
        let st = SessionStore::at(&d);
        assert!(matches!(st.load(), LoadOutcome::Missing));
        st.save(&sample()).unwrap();
        assert!(!d.join("session.json.tmp").exists(), "临时文件已改名");
        match st.load() {
            LoadOutcome::Restored(s) => assert_eq!(s, sample()),
            o => panic!("应恢复：{o:?}"),
        }
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn corrupt_file_is_backed_up_not_overwritten() {
        let d = tmp("corrupt");
        let st = SessionStore::at(&d);
        let bad = "{\"version\":1, 垃圾".as_bytes();
        fs::write(st.path(), bad).unwrap();
        let LoadOutcome::Degraded(g) = st.load() else {
            panic!("应降级")
        };
        let backup = g.backup.expect("应有备份");
        assert_eq!(fs::read(&backup).unwrap(), bad, "备份保留原始字节");
        assert!(!st.path().exists(), "原位置不留假会话");
        assert!(g.message.contains("损坏") && g.message.contains("备份"));
        // 之后可以正常保存并恢复，备份不受影响
        st.save(&sample()).unwrap();
        assert!(matches!(st.load(), LoadOutcome::Restored(_)));
        assert!(backup.exists());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn newer_version_is_preserved() {
        let d = tmp("newer");
        let st = SessionStore::at(&d);
        fs::write(st.path(), br#"{"version":7,"future":true}"#).unwrap();
        let LoadOutcome::Degraded(g) = st.load() else {
            panic!("应降级")
        };
        assert!(g.message.contains('7') && g.message.contains("版本"));
        let b = g.backup.unwrap();
        assert!(b.to_string_lossy().contains("-v7"));
        assert_eq!(fs::read(b).unwrap(), br#"{"version":7,"future":true}"#);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn failed_backup_blocks_saving() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = tmp("blocked");
        let st = SessionStore::at(&d);
        fs::write(st.path(), b"garbage").unwrap();
        // 以“只允许他人读取”的方式持有文件：读取可以，但重命名（备份）会因共享冲突失败
        let _hold = fs::OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ
            .open(st.path())
            .unwrap();
        let LoadOutcome::Degraded(g) = st.load() else {
            panic!("应降级")
        };
        assert!(g.backup.is_none());
        assert!(g.message.contains("无法备份") && g.message.contains("不会保存"));
        assert!(!st.is_writable());
        assert!(st.save(&sample()).is_err());
        drop(_hold);
        assert_eq!(fs::read(st.path()).unwrap(), b"garbage", "原文件未被覆盖");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn backups_are_pruned() {
        let d = tmp("prune");
        let st = SessionStore::at(&d);
        for i in 0..8 {
            fs::write(
                d.join(format!("session.backup-2026010{i}-000000-corrupt.json")),
                b"x",
            )
            .unwrap();
        }
        fs::write(st.path(), b"garbage").unwrap();
        let _ = st.load();
        let n = fs::read_dir(&d)
            .unwrap()
            .filter(|e| is_backup_name(&e.as_ref().unwrap().file_name().to_string_lossy()))
            .count();
        assert_eq!(n, KEEP_BACKUPS);
        let _ = fs::remove_dir_all(d);
    }
}
