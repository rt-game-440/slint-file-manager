//! 系统入口接管的注册计划、写入前备份、应用与恢复（与具体注册表根无关）。
//!
//! 所有键路径都是**相对路径**，由 [`RegStore`] 的实现决定挂在哪个根下：
//!
//! - 产品（#17）使用 `HKCU\Software\Classes`（只写当前用户、不需要管理员，HKCR 合并视图里覆盖 HKLM）。
//! - 测试与演练使用 `HKCU\Software\slint-file-manager-test\<随机>`，结束后整棵删除（见 `winreg`）。
//!   本模块和测试从不写真实的文件关联。
//!
//! 安全规则：
//!
//! 1. 写入前逐值备份原状，包括“原来没有这个值”“原来没有这个键（以及哪些祖先键）”；备份先交给调用方持久化，再动注册表。
//! 2. 只新增自己的动词 `slintfm.open` 并切换默认动词，**不改写** `open`／`explore` 等系统动词，
//!    这样回退时显式调用 `open` 动词仍然到达资源管理器，不会循环回到本应用。
//! 3. 恢复时只回滚“仍是本应用写入的值”的项；被别的程序改过的值视为冲突，不覆盖、不删除，如实报告。
//! 4. 只删除备份时不存在、且恢复后已经空了的键；里面有别人新写的内容就保留并报告。
//! 5. 恢复后逐值复核与备份一致，报告残留与不一致，不把“尽力而为”当作成功。

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use serde_json::{Value, json};

/// 本应用新增的外壳动词名。
pub const VERB: &str = "slintfm.open";
/// 动词显示名称。
pub const VERB_LABEL: &str = "用 Slint 文件管理器打开";
/// Win+E 调用的外壳对象（“文件资源管理器”启动入口）的 CLSID。
pub const CLSID_WIN_E: &str = "{52205fd8-5dfb-447d-801a-d0b52f2e83e1}";

pub const REG_SZ: u32 = 1;
pub const REG_EXPAND_SZ: u32 = 2;
pub const REG_DWORD: u32 = 4;

/// 接管范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// 文件系统文件夹与驱动器：`Directory\shell`、`Drive\shell` 的默认动词。不碰 `Folder`（虚拟位置）。
    FileSystemFolders,
    /// Win+E：覆盖 `CLSID\{52205fd8-...}\shell\opennewwindow\command`。
    WinE,
}

/// 注册表值：类型与原始字节，按字节比较（保证恢复后类型、内容完全一致）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegValue {
    pub kind: u32,
    pub bytes: Vec<u8>,
}

impl RegValue {
    /// REG_SZ（UTF-16LE，带结尾 NUL）。
    pub fn sz(s: &str) -> RegValue {
        RegValue {
            kind: REG_SZ,
            bytes: utf16_bytes(s),
        }
    }

    pub fn expand_sz(s: &str) -> RegValue {
        RegValue {
            kind: REG_EXPAND_SZ,
            bytes: utf16_bytes(s),
        }
    }

    pub fn dword(v: u32) -> RegValue {
        RegValue {
            kind: REG_DWORD,
            bytes: v.to_le_bytes().to_vec(),
        }
    }

    /// 字符串类型时返回文本（去掉结尾 NUL）。
    pub fn text(&self) -> Option<String> {
        if self.kind != REG_SZ && self.kind != REG_EXPAND_SZ {
            return None;
        }
        let w: Vec<u16> = self
            .bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
        String::from_utf16(&w[..end]).ok()
    }
}

fn utf16_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// 计划中的一个值。`name` 为空串表示键的默认值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueSpec {
    pub key: String,
    pub name: String,
    pub value: RegValue,
}

fn spec(key: impl Into<String>, name: &str, value: RegValue) -> ValueSpec {
    ValueSpec {
        key: key.into(),
        name: name.to_string(),
        value,
    }
}

/// 接管动词的命令行：`"<exe>" --entry "%1"`。由本应用自己用原始命令行解析（见 `args`），
/// 所以 `%1 = C:\` 时的 `"C:\"` 不会被 MSVC 规则吃掉反斜杠。
pub fn folder_command(exe: &Path) -> String {
    format!("\"{}\" {} \"%1\"", exe.display(), super::args::ENTRY_FLAG)
}

/// Win+E 的命令行：`"<exe>" --entry`。
pub fn win_e_command(exe: &Path) -> String {
    format!("\"{}\" {}", exe.display(), super::args::ENTRY_FLAG)
}

/// 生成接管计划（相对键路径，按写入顺序：先建动词，最后切换默认动词；恢复时倒序）。
pub fn plan(exe: &Path, scopes: &[Scope]) -> Vec<ValueSpec> {
    let mut out = Vec::new();
    if scopes.contains(&Scope::FileSystemFolders) {
        for class in ["Directory", "Drive"] {
            let verb = format!("{class}\\shell\\{VERB}");
            out.push(spec(verb.clone(), "", RegValue::sz(VERB_LABEL)));
            out.push(spec(
                format!("{verb}\\command"),
                "",
                RegValue::sz(&folder_command(exe)),
            ));
            out.push(spec(format!("{class}\\shell"), "", RegValue::sz(VERB)));
        }
    }
    if scopes.contains(&Scope::WinE) {
        let key = format!("CLSID\\{CLSID_WIN_E}\\shell\\opennewwindow\\command");
        // 与 Files 的公开做法一致：HKCU 中的同名键在合并视图里整体覆盖 HKLM 的键，
        // 仍显式写空的 DelegateExecute，防止合并到系统的委托执行对象。
        out.push(spec(key.clone(), "DelegateExecute", RegValue::sz("")));
        out.push(spec(key, "", RegValue::sz(&win_e_command(exe))));
    }
    out
}

/// 某个根下的注册表读写。键路径相对该根，大小写不敏感，`\` 分隔。
pub trait RegStore {
    fn key_exists(&self, key: &str) -> io::Result<bool>;
    /// 读取值；键或值不存在时返回 `None`。
    fn get(&self, key: &str, name: &str) -> io::Result<Option<RegValue>>;
    /// 写入值，必要时创建键（及祖先键）。
    fn set(&mut self, key: &str, name: &str, value: &RegValue) -> io::Result<()>;
    /// 删除值；值或键不存在时视为成功。
    fn delete_value(&mut self, key: &str, name: &str) -> io::Result<()>;
    /// 键存在且没有任何值和子键时删除它，返回是否删除。
    fn delete_key_if_empty(&mut self, key: &str) -> io::Result<bool>;
    /// 递归列出 `key`（空串为根）下的全部键和值，键路径与值名统一小写，相对 `key`。
    fn snapshot(&self, key: &str) -> io::Result<Snapshot>;
}

/// 一棵键树的完整内容，用于“恢复后与原状逐值一致”的比对。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub keys: BTreeSet<String>,
    pub values: BTreeMap<(String, String), RegValue>,
}

/// 一个值的备份。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedValue {
    pub key: String,
    pub name: String,
    /// 写入前的值；`None` 表示原来没有这个值。
    pub original: Option<RegValue>,
    /// 本应用写入的值。
    pub applied: RegValue,
}

/// 写入前备份。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Backup {
    pub values: Vec<SavedValue>,
    /// 写入前不存在的键（含祖先），恢复时由深到浅删除空键。
    pub missing_keys: Vec<String>,
}

/// 当前接管状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// 计划中的值全部是本应用写入的值。
    Applied,
    /// 计划中的值没有一个是本应用写入的值。
    NotApplied,
    /// 部分生效（被别的程序改动或写入中断），列出不一致的 `键\值名`。
    Partial(Vec<String>),
}

/// 恢复结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// 被别的程序改过、因此没有回滚的值（`键\值名`）。
    pub conflicts: Vec<String>,
    /// 备份时不存在、恢复后仍有别人内容而保留的键。
    pub leftover_keys: Vec<String>,
    /// 复核时与备份不一致的值（不含冲突项）。
    pub mismatches: Vec<String>,
}

impl RestoreReport {
    /// 完全恢复原状。
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty() && self.leftover_keys.is_empty() && self.mismatches.is_empty()
    }
}

/// 应用失败：写入时出错，已尝试回滚。
#[derive(Debug)]
pub struct ApplyError {
    pub error: io::Error,
    /// 回滚结果；回滚本身失败时为 `Err`。
    pub rollback: io::Result<RestoreReport>,
}

fn label(key: &str, name: &str) -> String {
    if name.is_empty() {
        format!("{key}\\(默认)")
    } else {
        format!("{key}\\{name}")
    }
}

/// `a\b\c` → [`a`, `a\b`, `a\b\c`]。
fn ancestors(key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for part in key.split('\\').filter(|p| !p.is_empty()) {
        if !cur.is_empty() {
            cur.push('\\');
        }
        cur.push_str(part);
        out.push(cur.clone());
    }
    out
}

/// 只读：记录计划涉及的每个值和键的原状。
pub fn capture(store: &(impl RegStore + ?Sized), plan: &[ValueSpec]) -> io::Result<Backup> {
    let mut backup = Backup::default();
    let mut seen = BTreeSet::new();
    for s in plan {
        for k in ancestors(&s.key) {
            if seen.insert(k.to_ascii_lowercase()) && !store.key_exists(&k)? {
                backup.missing_keys.push(k);
            }
        }
        backup.values.push(SavedValue {
            key: s.key.clone(),
            name: s.name.clone(),
            original: store.get(&s.key, &s.name)?,
            applied: s.value.clone(),
        });
    }
    Ok(backup)
}

/// 应用计划：先备份并交给 `persist` 持久化（失败则不写注册表），再按顺序写入；
/// 任一写入失败时立即用备份回滚并返回 [`ApplyError`]。
pub fn apply(
    store: &mut (impl RegStore + ?Sized),
    plan: &[ValueSpec],
    persist: impl FnOnce(&Backup) -> io::Result<()>,
) -> Result<Backup, ApplyError> {
    let backup = capture(store, plan).map_err(|error| ApplyError {
        error,
        rollback: Ok(RestoreReport::default()),
    })?;
    apply_prepared(store, plan, backup, persist)
}

/// 用调用方准备好的备份应用计划（修复、重新接管时沿用旧备份里的原值，见 `manage`）：
/// 先交给 `persist` 持久化（失败则不写注册表），再按顺序写入；任一写入失败时按该备份回滚。
pub fn apply_prepared(
    store: &mut (impl RegStore + ?Sized),
    plan: &[ValueSpec],
    backup: Backup,
    persist: impl FnOnce(&Backup) -> io::Result<()>,
) -> Result<Backup, ApplyError> {
    if let Err(error) = persist(&backup) {
        return Err(ApplyError {
            error,
            rollback: Ok(RestoreReport::default()),
        });
    }
    for s in plan {
        if let Err(error) = store.set(&s.key, &s.name, &s.value) {
            let rollback = restore(store, &backup);
            return Err(ApplyError { error, rollback });
        }
    }
    Ok(backup)
}

/// 按备份恢复并复核。
pub fn restore(store: &mut (impl RegStore + ?Sized), backup: &Backup) -> io::Result<RestoreReport> {
    let mut report = RestoreReport::default();
    let mut conflicted = BTreeSet::new();
    for v in backup.values.iter().rev() {
        let current = store.get(&v.key, &v.name)?;
        if current.as_ref() == Some(&v.applied) {
            match &v.original {
                Some(o) => store.set(&v.key, &v.name, o)?,
                None => store.delete_value(&v.key, &v.name)?,
            }
        } else if current != v.original {
            // 既不是本应用写入的值，也不是原值：被别人改过，不动它
            let l = label(&v.key, &v.name);
            conflicted.insert(l.clone());
            report.conflicts.push(l);
        }
    }
    let mut missing = backup.missing_keys.clone();
    missing.sort_by_key(|k| std::cmp::Reverse(k.matches('\\').count()));
    for k in &missing {
        if store.key_exists(k)? && !store.delete_key_if_empty(k)? {
            report.leftover_keys.push(k.clone());
        }
    }
    for v in &backup.values {
        let l = label(&v.key, &v.name);
        if !conflicted.contains(&l) && store.get(&v.key, &v.name)? != v.original {
            report.mismatches.push(l);
        }
    }
    Ok(report)
}

/// 只读：计划当前是否生效。
pub fn status(store: &(impl RegStore + ?Sized), plan: &[ValueSpec]) -> io::Result<Status> {
    let mut differ = Vec::new();
    for s in plan {
        if store.get(&s.key, &s.name)?.as_ref() != Some(&s.value) {
            differ.push(label(&s.key, &s.name));
        }
    }
    Ok(if differ.is_empty() {
        Status::Applied
    } else if differ.len() == plan.len() {
        Status::NotApplied
    } else {
        Status::Partial(differ)
    })
}

/// 应用前检查：计划会覆盖的“已有且不同”的值（例如别的文件管理器已经设置了默认动词）。
/// 调用方应把这些项展示给用户确认，而不是静默覆盖。
pub fn preflight(
    store: &(impl RegStore + ?Sized),
    plan: &[ValueSpec],
) -> io::Result<Vec<(String, RegValue)>> {
    let mut out = Vec::new();
    for s in plan {
        if let Some(cur) = store.get(&s.key, &s.name)?
            && cur != s.value
        {
            out.push((label(&s.key, &s.name), cur));
        }
    }
    Ok(out)
}

// ---------- 备份的持久化格式（JSON） ----------

/// 备份格式版本。
pub const BACKUP_VERSION: i64 = 1;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn value_json(v: &RegValue) -> Value {
    json!({ "type": v.kind, "data": hex(&v.bytes) })
}

fn value_from(v: &Value) -> Option<RegValue> {
    Some(RegValue {
        kind: u32::try_from(v.get("type")?.as_u64()?).ok()?,
        bytes: unhex(v.get("data")?.as_str()?)?,
    })
}

impl Backup {
    pub fn to_json(&self) -> String {
        let values: Vec<Value> = self
            .values
            .iter()
            .map(|v| {
                json!({
                    "key": v.key,
                    "name": v.name,
                    "original": v.original.as_ref().map(value_json),
                    "applied": value_json(&v.applied),
                })
            })
            .collect();
        let v = json!({
            "version": BACKUP_VERSION,
            "values": values,
            "missing_keys": self.missing_keys,
        });
        serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
    }

    /// 解析失败（格式、版本不符）时返回 `None`；调用方不得据此恢复，应提示用户。
    pub fn from_json(s: &str) -> Option<Backup> {
        let v: Value = serde_json::from_str(s).ok()?;
        if v.get("version")?.as_i64()? != BACKUP_VERSION {
            return None;
        }
        let mut b = Backup::default();
        for item in v.get("values")?.as_array()? {
            let original = match item.get("original")? {
                Value::Null => None,
                o => Some(value_from(o)?),
            };
            b.values.push(SavedValue {
                key: item.get("key")?.as_str()?.to_string(),
                name: item.get("name")?.as_str()?.to_string(),
                original,
                applied: value_from(item.get("applied")?)?,
            });
        }
        for k in v.get("missing_keys")?.as_array()? {
            b.missing_keys.push(k.as_str()?.to_string());
        }
        Some(b)
    }
}

// ---------- 内存实现（单元测试、演练） ----------

/// 内存中的注册表根，语义与 Windows 一致：键名、值名大小写不敏感；写值会创建祖先键。
#[derive(Clone, Debug, Default)]
pub struct MemStore {
    keys: BTreeSet<String>,
    values: BTreeMap<(String, String), RegValue>,
    /// 测试用：第 n 次 `set`（从 0 起）返回错误。
    pub fail_set_at: Option<usize>,
    sets: usize,
}

fn lc(s: &str) -> String {
    s.trim_matches('\\').to_ascii_lowercase()
}

impl MemStore {
    pub fn new() -> MemStore {
        MemStore::default()
    }

    /// 只建键，不写值。
    pub fn create_key(&mut self, key: &str) {
        for k in ancestors(&lc(key)) {
            self.keys.insert(k);
        }
    }
}

impl RegStore for MemStore {
    fn key_exists(&self, key: &str) -> io::Result<bool> {
        Ok(self.keys.contains(&lc(key)))
    }

    fn get(&self, key: &str, name: &str) -> io::Result<Option<RegValue>> {
        Ok(self.values.get(&(lc(key), lc(name))).cloned())
    }

    fn set(&mut self, key: &str, name: &str, value: &RegValue) -> io::Result<()> {
        let n = self.sets;
        self.sets += 1;
        if self.fail_set_at == Some(n) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "模拟写入失败",
            ));
        }
        self.create_key(key);
        self.values.insert((lc(key), lc(name)), value.clone());
        Ok(())
    }

    fn delete_value(&mut self, key: &str, name: &str) -> io::Result<()> {
        self.values.remove(&(lc(key), lc(name)));
        Ok(())
    }

    fn delete_key_if_empty(&mut self, key: &str) -> io::Result<bool> {
        let k = lc(key);
        if !self.keys.contains(&k) {
            return Ok(false);
        }
        let prefix = format!("{k}\\");
        let has_sub = self.keys.iter().any(|x| x.starts_with(&prefix));
        let has_val = self.values.keys().any(|(kk, _)| *kk == k);
        if has_sub || has_val {
            return Ok(false);
        }
        self.keys.remove(&k);
        Ok(true)
    }

    fn snapshot(&self, key: &str) -> io::Result<Snapshot> {
        let k = lc(key);
        let rel = |x: &str| -> Option<String> {
            if k.is_empty() {
                Some(x.to_string())
            } else {
                x.strip_prefix(&format!("{k}\\")).map(str::to_string)
            }
        };
        let mut s = Snapshot::default();
        for x in &self.keys {
            if let Some(r) = rel(x) {
                s.keys.insert(r);
            }
        }
        for ((kk, n), v) in &self.values {
            let r = if *kk == k {
                Some(String::new())
            } else {
                rel(kk)
            };
            if let Some(r) = r {
                s.values.insert((r, n.clone()), v.clone());
            }
        }
        Ok(s)
    }
}

/// 演练场景：在给定根里布置与本机真实情况同构的初始状态（用于测试“原来有／没有该键”等情况）。
///
/// - `Directory\shell` 键已存在但没有默认值，旁边有别的程序的动词（本机 Zed 的真实情况）；
/// - `Drive\shell` 键已存在且默认值为 `none`（REG_SZ），另有一个 REG_EXPAND_SZ 值；
/// - `CLSID\{52205fd8-...}` 完全不存在。
pub fn seed_rehearsal(store: &mut (impl RegStore + ?Sized)) -> io::Result<()> {
    store.set(
        "Directory\\shell\\Zed\\command",
        "",
        &RegValue::expand_sz("\"C:\\Zed\\Zed.exe\" \"%V\""),
    )?;
    store.set("Drive\\shell", "", &RegValue::sz("none"))?;
    store.set(
        "Drive\\shell",
        "Note",
        &RegValue::expand_sz("%SystemRoot%\\x"),
    )?;
    store.set("Drive\\shell", "Flags", &RegValue::dword(0x18))?;
    Ok(())
}

/// 通用演练：布置 → 快照 → 应用 → 检查生效 → 恢复 → 与快照逐值比对。供内存实现与真实测试根共用。
pub fn rehearse(store: &mut (impl RegStore + ?Sized), exe: &Path) -> Result<(), String> {
    let e = |x: io::Error| x.to_string();
    seed_rehearsal(store).map_err(e)?;
    let before = store.snapshot("").map_err(e)?;
    let p = plan(exe, &[Scope::FileSystemFolders, Scope::WinE]);
    if status(store, &p).map_err(e)? != Status::NotApplied {
        return Err("应用前状态不是“未接管”".into());
    }
    let pre = preflight(store, &p).map_err(e)?;
    // Drive\shell 的默认值 none 会被覆盖，应在应用前被指出
    if !pre
        .iter()
        .any(|(l, _)| l.eq_ignore_ascii_case("Drive\\shell\\(默认)"))
    {
        return Err(format!("应用前检查没有指出将被覆盖的值：{pre:?}"));
    }
    let mut persisted = None;
    let backup = apply(store, &p, |b| {
        persisted = Some(b.to_json());
        Ok(())
    })
    .map_err(|x| x.error.to_string())?;
    if status(store, &p).map_err(e)? != Status::Applied {
        return Err("应用后状态不是“已接管”".into());
    }
    // 模拟重启：从持久化的 JSON 恢复
    let reloaded = Backup::from_json(&persisted.unwrap_or_default()).ok_or("备份 JSON 无法解析")?;
    if reloaded != backup {
        return Err("备份 JSON 往返后不一致".into());
    }
    let report = restore(store, &reloaded).map_err(e)?;
    if !report.is_clean() {
        return Err(format!("恢复不完整：{report:?}"));
    }
    let after = store.snapshot("").map_err(e)?;
    if after != before {
        return Err(format!(
            "恢复后与原状不一致：\n原状 {before:?}\n现状 {after:?}"
        ));
    }
    if status(store, &p).map_err(e)? != Status::NotApplied {
        return Err("恢复后状态不是“未接管”".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\Program Files\Slint 文件管理器\slint-file-manager.exe")
    }

    #[test]
    fn plan_shapes() {
        let p = plan(&exe(), &[Scope::FileSystemFolders]);
        assert_eq!(p.len(), 6);
        // 先建动词与命令，最后切换默认动词
        assert_eq!(p[2].key, "Directory\\shell");
        assert_eq!(p[2].value.text().as_deref(), Some(VERB));
        assert_eq!(
            p[1].value.text().unwrap(),
            r#""C:\Program Files\Slint 文件管理器\slint-file-manager.exe" --entry "%1""#
        );
        assert!(
            p.iter()
                .all(|s| !s.key.to_ascii_lowercase().starts_with("folder"))
        );
        // 从不改写系统动词 open／explore
        assert!(
            p.iter()
                .all(|s| !s.key.to_ascii_lowercase().contains("\\open\\")
                    && !s.key.to_ascii_lowercase().ends_with("\\open"))
        );
        let w = plan(&exe(), &[Scope::WinE]);
        assert_eq!(w.len(), 2);
        assert!(w[0].key.contains(CLSID_WIN_E));
        assert_eq!(w[0].name, "DelegateExecute");
        assert_eq!(w[0].value.text().as_deref(), Some(""));
        assert!(plan(&exe(), &[]).is_empty());
    }

    #[test]
    fn reg_value_roundtrip() {
        let v = RegValue::sz("用 Slint");
        assert_eq!(v.text().as_deref(), Some("用 Slint"));
        assert_eq!(v.bytes.len() % 2, 0);
        assert_eq!(RegValue::dword(3).text(), None);
        assert_eq!(unhex(&hex(&[0, 255, 16])), Some(vec![0, 255, 16]));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }

    #[test]
    fn rehearsal_in_memory() {
        let mut m = MemStore::new();
        rehearse(&mut m, &exe()).unwrap();
    }

    #[test]
    fn restore_when_nothing_existed() {
        let mut m = MemStore::new();
        let before = m.snapshot("").unwrap();
        let p = plan(&exe(), &[Scope::FileSystemFolders, Scope::WinE]);
        let b = apply(&mut m, &p, |_| Ok(())).unwrap();
        assert!(b.missing_keys.iter().any(|k| k == "Directory"));
        assert!(b.values.iter().all(|v| v.original.is_none()));
        let r = restore(&mut m, &b).unwrap();
        assert!(r.is_clean(), "{r:?}");
        assert_eq!(m.snapshot("").unwrap(), before);
    }

    #[test]
    fn conflicting_change_is_not_overwritten() {
        let mut m = MemStore::new();
        let p = plan(&exe(), &[Scope::FileSystemFolders]);
        let b = apply(&mut m, &p, |_| Ok(())).unwrap();
        // 别的文件管理器随后接管了 Directory 的默认动词
        m.set("Directory\\shell", "", &RegValue::sz("openinother"))
            .unwrap();
        assert!(matches!(status(&m, &p).unwrap(), Status::Partial(_)));
        let r = restore(&mut m, &b).unwrap();
        assert_eq!(r.conflicts, vec!["Directory\\shell\\(默认)"]);
        assert!(r.mismatches.is_empty());
        // 别人的值保留，Directory\shell 键因有值而保留
        assert_eq!(
            m.get("Directory\\shell", "").unwrap(),
            Some(RegValue::sz("openinother"))
        );
        assert!(r.leftover_keys.iter().any(|k| k == "Directory\\shell"));
        // 本应用自己的动词键已删掉
        assert!(!m.key_exists(&format!("Directory\\shell\\{VERB}")).unwrap());
        assert!(!r.is_clean());
    }

    #[test]
    fn foreign_subkey_keeps_created_key() {
        let mut m = MemStore::new();
        let p = plan(&exe(), &[Scope::WinE]);
        let b = apply(&mut m, &p, |_| Ok(())).unwrap();
        m.set(
            &format!("CLSID\\{CLSID_WIN_E}\\other"),
            "",
            &RegValue::sz("x"),
        )
        .unwrap();
        let r = restore(&mut m, &b).unwrap();
        assert!(r.conflicts.is_empty());
        assert_eq!(
            r.leftover_keys,
            vec![format!("CLSID\\{CLSID_WIN_E}"), "CLSID".into()]
        );
        assert!(
            m.get(&format!("CLSID\\{CLSID_WIN_E}\\other"), "")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn failed_write_rolls_back() {
        let mut m = MemStore::new();
        seed_rehearsal(&mut m).unwrap();
        let before = m.snapshot("").unwrap();
        // seed_rehearsal 调用了 4 次 set；让计划的第 5 次写入失败
        m.fail_set_at = Some(4 + 4);
        let p = plan(&exe(), &[Scope::FileSystemFolders, Scope::WinE]);
        let err = apply(&mut m, &p, |_| Ok(())).unwrap_err();
        assert_eq!(err.error.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.rollback.unwrap().is_clean());
        assert_eq!(m.snapshot("").unwrap(), before);
    }

    #[test]
    fn persist_failure_writes_nothing() {
        let mut m = MemStore::new();
        let p = plan(&exe(), &[Scope::FileSystemFolders]);
        let err = apply(&mut m, &p, |_| Err(io::Error::other("磁盘满"))).unwrap_err();
        assert_eq!(err.error.to_string(), "磁盘满");
        assert_eq!(m.snapshot("").unwrap(), Snapshot::default());
    }

    #[test]
    fn restore_is_idempotent() {
        let mut m = MemStore::new();
        seed_rehearsal(&mut m).unwrap();
        let before = m.snapshot("").unwrap();
        let p = plan(&exe(), &[Scope::FileSystemFolders]);
        let b = apply(&mut m, &p, |_| Ok(())).unwrap();
        assert!(restore(&mut m, &b).unwrap().is_clean());
        assert!(restore(&mut m, &b).unwrap().is_clean());
        assert_eq!(m.snapshot("").unwrap(), before);
    }

    #[test]
    fn backup_json_rejects_bad_input() {
        assert!(Backup::from_json("not json").is_none());
        assert!(Backup::from_json(r#"{"version":2,"values":[],"missing_keys":[]}"#).is_none());
        let b = Backup {
            values: vec![SavedValue {
                key: "Drive\\shell".into(),
                name: String::new(),
                original: Some(RegValue::sz("none")),
                applied: RegValue::sz(VERB),
            }],
            missing_keys: vec!["CLSID".into()],
        };
        assert_eq!(Backup::from_json(&b.to_json()), Some(b));
    }
}
