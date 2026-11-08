//! 系统入口接管的启用、停用、修复与启动自检（Issue #17）。
//!
//! 在 [`takeover`] 的备份／应用／恢复之上加三件事：
//!
//! - **状态判断**（[`inspect`]）：已接管、未接管、冲突（部分值被别的程序改了）、关联失效
//!   （注册的命令指向另一个 exe：程序被移动、改名，或原路径已不存在）。
//! - **备份持久化**：备份写到数据目录的 [`BACKUP_FILE`]（先写临时文件再改名），写盘失败则不动注册表。
//! - **修复／重新接管**沿用旧备份里的原值：再次接管时注册表里的现值是本应用上次写入的，
//!   不能把它当成“原值”，否则恢复时会恢复成本应用自己的值。
//!
//! 注册表根由 [`Root::from_env`] 决定：默认是真实的 `HKCU\Software\Classes`；只用于测试的环境变量
//! [`REG_ROOT_ENV`] 可以把根换成 `HKCU\Software\slint-file-manager-test\…` 下的测试根，指到别处则直接拒绝
//! （接管功能不可用），**从不**退回真实根。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::args::ENTRY_FLAG;
use super::takeover::{
    self, Backup, CLSID_WIN_E, RegStore, RegValue, RestoreReport, SavedValue, Scope, Status, VERB,
    VERB_LABEL, ValueSpec,
};
use super::winreg::{TEST_PREFIX, WinReg};

/// 备份文件名（位于数据目录）。
pub const BACKUP_FILE: &str = "entry-takeover-backup.json";
/// 产品使用的注册表根（相对 HKCU）。
pub const PRODUCT_BASE: &str = r"Software\Classes";
/// 只用于测试：把注册表根换成 `HKCU\<值>`，值必须位于 [`TEST_PREFIX`] 之下。
pub const REG_ROOT_ENV: &str = "SFM_TEST_REG_ROOT";
/// 接管范围：文件系统文件夹与驱动器、Win+E。
pub const SCOPES: [Scope; 2] = [Scope::FileSystemFolders, Scope::WinE];

/// 当前接管状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    NotTaken,
    Taken,
    /// 部分值被别的程序改动（列出 `键\值名`）。
    Conflict(Vec<String>),
    /// 注册的命令指向别的程序路径。`missing` 为该路径已不存在。
    Stale {
        old_exe: PathBuf,
        missing: bool,
    },
}

/// 注册表根。
pub struct Root {
    pub store: WinReg,
    /// 是否真实的 `HKCU\Software\Classes`（只有真实根才通知外壳刷新关联）。
    pub real: bool,
}

impl Root {
    /// 按环境变量选择根；测试根变量指向测试前缀之外时返回错误说明（接管功能不可用）。
    pub fn from_env() -> Result<Root, String> {
        match std::env::var(REG_ROOT_ENV) {
            Ok(v) if !v.trim().is_empty() => test_root(&v).map(|store| Root { store, real: false }),
            _ => Ok(Root {
                store: WinReg::hkcu(PRODUCT_BASE),
                real: true,
            }),
        }
    }
}

/// 校验测试根：必须是 `Software\slint-file-manager-test\<名称>`（可带 `HKCU\` 前缀），不含 `..`。
pub fn test_root(v: &str) -> Result<WinReg, String> {
    let mut b = v.trim().trim_matches('\\');
    for p in ["HKEY_CURRENT_USER\\", "HKCU\\"] {
        if b.len() >= p.len() && b[..p.len()].eq_ignore_ascii_case(p) {
            b = &b[p.len()..];
        }
    }
    let prefix = format!("{TEST_PREFIX}\\");
    let ok = b.len() > prefix.len()
        && b[..prefix.len()].eq_ignore_ascii_case(&prefix)
        && !b.split('\\').any(|p| p.is_empty() || p == "." || p == "..");
    if ok {
        Ok(WinReg::hkcu(b))
    } else {
        Err(format!(
            "环境变量 {REG_ROOT_ENV} 必须位于 HKCU\\{TEST_PREFIX}\\ 之下，已拒绝：{v}"
        ))
    }
}

/// 本应用写入的命令行（`"<exe>" --entry…`）里的 exe；不是本应用格式时返回 `None`。
pub fn own_command_exe(cmd: &str) -> Option<PathBuf> {
    let rest = cmd.trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    let exe = &rest[..end];
    let tail = rest[end + 1..].trim_start();
    let flag_ok = tail.len() >= ENTRY_FLAG.len()
        && tail[..ENTRY_FLAG.len()].eq_ignore_ascii_case(ENTRY_FLAG)
        && tail[ENTRY_FLAG.len()..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace);
    (flag_ok && !exe.is_empty()).then(|| PathBuf::from(exe))
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

/// 本应用命令所在的键（相对根）。
fn command_keys() -> [String; 3] {
    [
        format!("Directory\\shell\\{VERB}\\command"),
        format!("Drive\\shell\\{VERB}\\command"),
        win_e_command_key(),
    ]
}

fn win_e_command_key() -> String {
    format!("CLSID\\{CLSID_WIN_E}\\shell\\opennewwindow\\command")
}

/// 注册表里本应用命令指向的 exe（去重，按出现顺序）。
fn registered_exes(store: &(impl RegStore + ?Sized)) -> io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    for k in command_keys() {
        if let Some(e) = store
            .get(&k, "")?
            .and_then(|v| v.text())
            .and_then(|t| own_command_exe(&t))
            && !out.iter().any(|x| same_path(x, &e))
        {
            out.push(e);
        }
    }
    Ok(out)
}

/// 只读：判断接管状态。`exe` 为当前程序路径。
pub fn inspect(store: &(impl RegStore + ?Sized), exe: &Path) -> io::Result<State> {
    if let Some(old) = registered_exes(store)?
        .into_iter()
        .find(|e| !same_path(e, exe))
    {
        let missing = !old.is_file();
        return Ok(State::Stale {
            old_exe: old,
            missing,
        });
    }
    Ok(
        match takeover::status(store, &takeover::plan(exe, &SCOPES))? {
            Status::Applied => State::Taken,
            Status::NotApplied => State::NotTaken,
            Status::Partial(v) => State::Conflict(v),
        },
    )
}

/// 值是不是本应用写入的（当前计划的值，或任意路径的本应用命令）。
fn is_own(spec: &ValueSpec, v: &RegValue) -> bool {
    *v == spec.value
        || (spec.name.is_empty()
            && spec.key.to_ascii_lowercase().ends_with("\\command")
            && v.text().and_then(|t| own_command_exe(&t)).is_some())
}

/// 启用前检查：将被覆盖的“别人的值”（本应用自己的旧值不算），用于确认对话框。
pub fn preflight(
    store: &(impl RegStore + ?Sized),
    exe: &Path,
) -> io::Result<Vec<(String, String)>> {
    let plan = takeover::plan(exe, &SCOPES);
    let mut out = Vec::new();
    for s in &plan {
        if let Some(cur) = store.get(&s.key, &s.name)?
            && !is_own(s, &cur)
        {
            let label = if s.name.is_empty() {
                format!("{}\\(默认)", s.key)
            } else {
                format!("{}\\{}", s.key, s.name)
            };
            out.push((label, cur.text().unwrap_or_else(|| "（非文本值）".into())));
        }
    }
    Ok(out)
}

/// 读取持久化的备份：没有文件为 `Ok(None)`；无法解析为 `Err`。
pub fn load_backup(path: &Path) -> Result<Option<Backup>, String> {
    match fs::read_to_string(path) {
        Ok(s) => Backup::from_json(&s)
            .map(Some)
            .ok_or_else(|| format!("备份文件无法解析：{}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("读取备份文件失败（{}）：{e}", path.display())),
    }
}

/// 原子写入：临时文件 + 改名。
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

/// 把无法解析的备份移到一旁（保留原内容供人工排查），返回新位置。
fn set_aside(path: &Path) -> Option<PathBuf> {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let to = path.with_file_name(format!("entry-takeover-backup.corrupt-{stamp}.json"));
    fs::rename(path, &to).ok().map(|_| to)
}

/// 启用、修复或重新接管的结果。
#[derive(Debug, Default)]
pub struct TakeReport {
    /// 原备份无法解析、已移到一旁的位置。
    pub set_aside: Option<PathBuf>,
}

/// 启用／修复／重新接管：备份（沿用旧备份里的原值）→ 写盘 → 写注册表；失败时回滚。
pub fn take(
    store: &mut (impl RegStore + ?Sized),
    exe: &Path,
    backup_path: &Path,
) -> Result<TakeReport, String> {
    let mut report = TakeReport::default();
    let old = match load_backup(backup_path) {
        Ok(b) => b,
        Err(_) => {
            report.set_aside = set_aside(backup_path);
            if report.set_aside.is_none() {
                return Err(format!(
                    "备份文件无法解析，也无法移走：{}",
                    backup_path.display()
                ));
            }
            None
        }
    };
    let plan = takeover::plan(exe, &SCOPES);
    let fresh = takeover::capture(store, &plan).map_err(|e| format!("读取注册表失败：{e}"))?;
    let mut any_own = false;
    let values: Vec<SavedValue> = plan
        .iter()
        .zip(fresh.values)
        .map(|(spec, mut v)| {
            if v.original.as_ref().is_some_and(|o| is_own(spec, o)) {
                any_own = true;
                v.original = old.as_ref().and_then(|b| {
                    b.values
                        .iter()
                        .find(|x| {
                            x.key.eq_ignore_ascii_case(&spec.key)
                                && x.name.eq_ignore_ascii_case(&spec.name)
                        })
                        .and_then(|x| x.original.clone())
                });
            }
            v
        })
        .collect();
    let mut missing: Vec<String> = Vec::new();
    let mut add = |k: &str| {
        if !missing.iter().any(|m| m.eq_ignore_ascii_case(k)) {
            missing.push(k.to_string());
        }
    };
    if let Some(b) = &old {
        // 旧备份里“原来不存在”的键，如果期间被别人写进了内容，就当作别人的键（恢复时本来也不会删）
        for k in &b.missing_keys {
            if only_own_content(store, k, &plan).map_err(|e| format!("读取注册表失败：{e}"))?
            {
                add(k);
            }
        }
    }
    fresh.missing_keys.iter().for_each(|k| add(k));
    if old.is_none() && any_own {
        // 没有旧备份但注册表里已有本应用的值：本应用自己的键视为原来不存在（恢复时只删空键）
        own_keys().iter().for_each(|k| add(k));
    }
    let backup = Backup {
        values,
        missing_keys: missing,
    };
    takeover::apply_prepared(store, &plan, backup, |b| {
        write_atomic(backup_path, &b.to_json())
    })
    .map_err(|e| {
        let rb = match e.rollback {
            Ok(r) if r.is_clean() => "已回滚".to_string(),
            Ok(r) => format!("回滚不完整：{}", describe(&r)),
            Err(x) => format!("回滚失败：{x}"),
        };
        format!("写入失败：{}（{rb}）", e.error)
    })?;
    Ok(report)
}

/// 键不存在，或其中（含子键）只有本应用写入的值。
fn only_own_content(
    store: &(impl RegStore + ?Sized),
    key: &str,
    plan: &[ValueSpec],
) -> io::Result<bool> {
    if !store.key_exists(key)? {
        return Ok(true);
    }
    let base = key.trim_matches('\\').to_ascii_lowercase();
    for ((rel, name), v) in store.snapshot(key)?.values {
        let full = if rel.is_empty() {
            base.clone()
        } else {
            format!("{base}\\{rel}")
        };
        let own = plan.iter().any(|s| {
            s.key.eq_ignore_ascii_case(&full) && s.name.eq_ignore_ascii_case(&name) && is_own(s, &v)
        });
        if !own {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 本应用自己创建的键（由深到浅），从不包含 `CLSID`、`Directory\shell` 这类共享键。
fn own_keys() -> Vec<String> {
    let mut v = Vec::new();
    for class in ["Directory", "Drive"] {
        v.push(format!("{class}\\shell\\{VERB}\\command"));
        v.push(format!("{class}\\shell\\{VERB}"));
    }
    let base = format!("CLSID\\{CLSID_WIN_E}");
    v.push(format!("{base}\\shell\\opennewwindow\\command"));
    v.push(format!("{base}\\shell\\opennewwindow"));
    v.push(format!("{base}\\shell"));
    v.push(base);
    v
}

/// 停用的结果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DisableReport {
    pub restore: RestoreReport,
    /// 没有可用备份，只删除了本应用写入的值（原来的默认动词无从得知，回到系统默认）。
    pub without_backup: bool,
    /// 备份无法解析、已移到一旁的位置。
    pub set_aside: Option<PathBuf>,
}

impl DisableReport {
    pub fn is_clean(&self) -> bool {
        self.restore.is_clean() && !self.without_backup
    }
}

/// 停用（恢复原设置）：按备份恢复；备份缺失或损坏时只删除本应用写入的值与键。
/// 恢复后注册表里不再有本应用的值时删除备份文件。
pub fn disable(
    store: &mut (impl RegStore + ?Sized),
    backup_path: &Path,
) -> Result<DisableReport, String> {
    let mut report = DisableReport::default();
    let backup = match load_backup(backup_path) {
        Ok(b) => b,
        Err(_) => {
            report.set_aside = set_aside(backup_path);
            None
        }
    };
    let e = |x: io::Error| format!("恢复注册表失败：{x}");
    match &backup {
        Some(b) => report.restore = takeover::restore(store, b).map_err(e)?,
        None => {
            report.without_backup = true;
            remove_own(store).map_err(e)?;
        }
    }
    if !has_own_values(store).map_err(e)? {
        match fs::remove_file(backup_path) {
            Ok(()) => {}
            Err(x) if x.kind() == io::ErrorKind::NotFound => {}
            Err(x) => return Err(format!("删除备份文件失败：{x}")),
        }
    }
    Ok(report)
}

/// 注册表里是否还有本应用写入的值。
pub fn has_own_values(store: &(impl RegStore + ?Sized)) -> io::Result<bool> {
    if !registered_exes(store)?.is_empty() {
        return Ok(true);
    }
    for class in ["Directory", "Drive"] {
        let k = format!("{class}\\shell");
        if store.get(&k, "")?.and_then(|v| v.text()).as_deref() == Some(VERB) {
            return Ok(true);
        }
        if store
            .get(&format!("{k}\\{VERB}"), "")?
            .and_then(|v| v.text())
            .as_deref()
            == Some(VERB_LABEL)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 没有备份时的兜底：只删除本应用写入的值和本应用自己的空键。
fn remove_own(store: &mut (impl RegStore + ?Sized)) -> io::Result<()> {
    for class in ["Directory", "Drive"] {
        let shell = format!("{class}\\shell");
        if store.get(&shell, "")?.and_then(|v| v.text()).as_deref() == Some(VERB) {
            store.delete_value(&shell, "")?;
        }
        let verb = format!("{shell}\\{VERB}");
        let cmd = format!("{verb}\\command");
        if store
            .get(&cmd, "")?
            .and_then(|v| v.text())
            .and_then(|t| own_command_exe(&t))
            .is_some()
        {
            store.delete_value(&cmd, "")?;
        }
        if store.get(&verb, "")?.and_then(|v| v.text()).as_deref() == Some(VERB_LABEL) {
            store.delete_value(&verb, "")?;
        }
    }
    let wk = win_e_command_key();
    if store
        .get(&wk, "")?
        .and_then(|v| v.text())
        .and_then(|t| own_command_exe(&t))
        .is_some()
    {
        store.delete_value(&wk, "")?;
        if store
            .get(&wk, "DelegateExecute")?
            .and_then(|v| v.text())
            .as_deref()
            == Some("")
        {
            store.delete_value(&wk, "DelegateExecute")?;
        }
    }
    for k in own_keys() {
        if store.key_exists(&k)? {
            store.delete_key_if_empty(&k)?;
        }
    }
    Ok(())
}

/// 恢复报告的简短说明（给用户看）。
pub fn describe(r: &RestoreReport) -> String {
    let mut parts = Vec::new();
    if !r.conflicts.is_empty() {
        parts.push(format!(
            "被别的程序改过、未回滚：{}",
            r.conflicts.join("、")
        ));
    }
    if !r.leftover_keys.is_empty() {
        parts.push(format!(
            "有别人内容而保留的键：{}",
            r.leftover_keys.join("、")
        ));
    }
    if !r.mismatches.is_empty() {
        parts.push(format!("复核不一致：{}", r.mismatches.join("、")));
    }
    parts.join("；")
}

/// 通知外壳文件关联已变化（只对真实根调用）。
pub fn notify_shell() {
    use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
    // SAFETY: SHCNE_ASSOCCHANGED 不带项目参数。
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST,
            std::ptr::null(),
            std::ptr::null(),
        );
    }
}

/// 备份文件位置。
pub fn backup_path(data_dir: &Path) -> PathBuf {
    data_dir.join(BACKUP_FILE)
}

#[cfg(test)]
mod tests {
    use super::super::takeover::{MemStore, seed_rehearsal};
    use super::super::winreg::TestRoot;
    use super::*;

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\Program Files\Slint 文件管理器\slint-file-manager.exe")
    }

    fn moved() -> PathBuf {
        PathBuf::from(r"D:\新位置\slint-file-manager.exe")
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sfm-manage-test-{}-{name}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parses_own_commands() {
        assert_eq!(
            own_command_exe(r#""C:\a b\x.exe" --entry "%1""#),
            Some(PathBuf::from(r"C:\a b\x.exe"))
        );
        assert_eq!(
            own_command_exe(r#""C:\x.exe" --entry"#),
            Some(PathBuf::from(r"C:\x.exe"))
        );
        assert_eq!(own_command_exe(r#""C:\x.exe" --entryx"#), None);
        assert_eq!(own_command_exe(r#""C:\Zed\Zed.exe" "%V""#), None);
        assert_eq!(own_command_exe(r#"C:\x.exe --entry"#), None);
    }

    #[test]
    fn test_root_override_is_confined() {
        assert!(test_root(r"Software\slint-file-manager-test\abc").is_ok());
        assert!(test_root(r"HKCU\Software\slint-file-manager-test\abc\d").is_ok());
        for bad in [
            r"Software\Classes",
            r"Software\slint-file-manager-test",
            r"Software\slint-file-manager-test\",
            r"Software\slint-file-manager-test\..\Classes",
            r"Software\slint-file-manager-testX\a",
            r"HKLM\Software\slint-file-manager-test\a",
        ] {
            assert!(test_root(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn enable_disable_roundtrip_restores_seed() {
        let dir = tmp("roundtrip");
        let bp = backup_path(&dir);
        let mut m = MemStore::new();
        seed_rehearsal(&mut m).unwrap();
        let before = m.snapshot("").unwrap();
        assert_eq!(inspect(&m, &exe()).unwrap(), State::NotTaken);
        let pre = preflight(&m, &exe()).unwrap();
        assert_eq!(pre, vec![(r"Drive\shell\(默认)".into(), "none".into())]);
        take(&mut m, &exe(), &bp).unwrap();
        assert!(bp.is_file());
        assert_eq!(inspect(&m, &exe()).unwrap(), State::Taken);
        // 再次接管（幂等）不改变备份里的原值
        take(&mut m, &exe(), &bp).unwrap();
        assert!(preflight(&m, &exe()).unwrap().is_empty());
        let r = disable(&mut m, &bp).unwrap();
        assert!(r.is_clean(), "{r:?}");
        assert_eq!(m.snapshot("").unwrap(), before);
        assert!(!bp.exists());
        assert_eq!(inspect(&m, &exe()).unwrap(), State::NotTaken);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_after_move_then_repair_keeps_originals() {
        let dir = tmp("stale");
        let bp = backup_path(&dir);
        let mut m = MemStore::new();
        seed_rehearsal(&mut m).unwrap();
        let before = m.snapshot("").unwrap();
        take(&mut m, &moved(), &bp).unwrap();
        // 当前程序在另一个位置：关联失效（旧路径不存在）
        match inspect(&m, &exe()).unwrap() {
            State::Stale { old_exe, missing } => {
                assert_eq!(old_exe, moved());
                assert!(missing);
            }
            s => panic!("{s:?}"),
        }
        // 修复：沿用旧备份的原值
        take(&mut m, &exe(), &bp).unwrap();
        assert_eq!(inspect(&m, &exe()).unwrap(), State::Taken);
        let b = load_backup(&bp).unwrap().unwrap();
        let drive = b
            .values
            .iter()
            .find(|v| v.key == r"Drive\shell" && v.name.is_empty())
            .unwrap();
        assert_eq!(drive.original, Some(RegValue::sz("none")));
        assert!(disable(&mut m, &bp).unwrap().is_clean());
        assert_eq!(m.snapshot("").unwrap(), before);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn conflict_detected_and_disable_reports_it() {
        let dir = tmp("conflict");
        let bp = backup_path(&dir);
        let mut m = MemStore::new();
        take(&mut m, &exe(), &bp).unwrap();
        m.set(r"Directory\shell", "", &RegValue::sz("other"))
            .unwrap();
        match inspect(&m, &exe()).unwrap() {
            State::Conflict(v) => assert_eq!(v, vec![r"Directory\shell\(默认)".to_string()]),
            s => panic!("{s:?}"),
        }
        // 重新接管：别人的值成为新的“原值”，停用后回到它
        take(&mut m, &exe(), &bp).unwrap();
        assert_eq!(inspect(&m, &exe()).unwrap(), State::Taken);
        assert!(disable(&mut m, &bp).unwrap().is_clean());
        assert_eq!(
            m.get(r"Directory\shell", "").unwrap(),
            Some(RegValue::sz("other"))
        );
        assert!(!has_own_values(&m).unwrap());
        // 再接管后又被改：停用如实报告冲突，不覆盖别人的值
        take(&mut m, &exe(), &bp).unwrap();
        m.set(r"Drive\shell", "", &RegValue::sz("x")).unwrap();
        let r = disable(&mut m, &bp).unwrap();
        assert_eq!(r.restore.conflicts, vec![r"Drive\shell\(默认)".to_string()]);
        assert_eq!(m.get(r"Drive\shell", "").unwrap(), Some(RegValue::sz("x")));
        assert!(!bp.exists(), "不再有本应用的值时删除备份");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn disable_without_backup_removes_only_own() {
        let dir = tmp("nobackup");
        let bp = backup_path(&dir);
        let mut m = MemStore::new();
        seed_rehearsal(&mut m).unwrap();
        take(&mut m, &exe(), &bp).unwrap();
        fs::write(&bp, "损坏").unwrap();
        let r = disable(&mut m, &bp).unwrap();
        assert!(r.without_backup);
        assert!(r.set_aside.as_ref().is_some_and(|p| p.is_file()));
        assert!(!has_own_values(&m).unwrap());
        // 别人的内容都在；本应用的键已删除；共享键保留
        assert!(m.key_exists(r"Directory\shell\Zed\command").unwrap());
        assert!(!m.key_exists(&format!(r"Directory\shell\{VERB}")).unwrap());
        assert!(!m.key_exists(&format!("CLSID\\{CLSID_WIN_E}")).unwrap());
        assert!(m.key_exists(r"Drive\shell").unwrap());
        assert_eq!(inspect(&m, &exe()).unwrap(), State::NotTaken);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persist_failure_leaves_registry_untouched() {
        let dir = tmp("persist");
        // 备份文件的上级是一个普通文件：无法写盘
        let blocker = dir.join("blocked");
        fs::write(&blocker, "x").unwrap();
        let bp = blocker.join(BACKUP_FILE);
        let mut m = MemStore::new();
        seed_rehearsal(&mut m).unwrap();
        let before = m.snapshot("").unwrap();
        assert!(take(&mut m, &exe(), &bp).is_err());
        assert_eq!(m.snapshot("").unwrap(), before);
        fs::remove_dir_all(dir).unwrap();
    }

    /// 真实注册表测试根：启用 → 失效 → 修复 → 停用，最后与布置逐值一致。
    #[test]
    fn real_test_root_lifecycle() {
        let dir = tmp("real");
        let bp = backup_path(&dir);
        let root = TestRoot::new().unwrap();
        let mut reg = WinReg::hkcu(root.reg.base());
        seed_rehearsal(&mut reg).unwrap();
        let before = reg.snapshot("").unwrap();
        take(&mut reg, &moved(), &bp).unwrap();
        assert!(matches!(
            inspect(&reg, &exe()).unwrap(),
            State::Stale { .. }
        ));
        take(&mut reg, &exe(), &bp).unwrap();
        assert_eq!(inspect(&reg, &exe()).unwrap(), State::Taken);
        let r = disable(&mut reg, &bp).unwrap();
        assert!(r.is_clean(), "{r:?}");
        assert_eq!(reg.snapshot("").unwrap(), before);
        root.cleanup().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}
