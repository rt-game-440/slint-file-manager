//! 复制、移动、新建与重命名的操作层：与界面无关的计划、冲突处理、文件系统引擎与逐项汇报。
//!
//! 设计要点（见 docs/create-rename-transfer.md）：
//! - **先计划后执行**：[`plan`] 在不改动任何东西的前提下检查目标目录、逐项判断（源已不存在、文件夹放进自己里面、
//!   移动到原位置、同名冲突），得到可直接执行的 [`Job`]、需要用户决定的 [`Conflict`] 与已判定失败的项。
//!   同名冲突由界面询问用户（跳过 / 保留两者），**没有“替换”**，所以不会无提示覆盖任何文件。
//! - **目标一律不存在才写**：新建、重命名、复制、移动都用“目标已存在则失败”的系统调用
//!   （`MoveFileExW` 不带替换标志、`CopyFileExW` 带 `COPY_FILE_FAIL_IF_EXISTS`、`create_new`），
//!   计划与执行之间有人抢先创建同名项目时也只会失败并如实汇报，不会覆盖。
//! - **跨卷移动 = 复制 + 校验 + 删除源**：同卷直接改名；系统报“不在同一设备”（17）时退到复制，复制与校验都成功后才删除源，
//!   复制失败会清理自己刚创建的不完整副本、源原样保留；删除源失败则两份都留下并如实说明。
//! - [`run`] 逐项执行，遇到“已取消”后不再继续，剩余项记为“操作被取消”，汇报区分已完成与未执行。
//! - 产品的复制与移动由系统的 `IFileOperation` 执行（进度、权限提示用系统界面，见 ADR 0006 与 `crate::shellop`），
//!   本模块的文件系统引擎用于 harness 与测试后端（可注入失败），以及新建与重命名。

use crate::fileops::{FailKind, OpError, OpKind, OpResult, Outcome, Report};
use crate::naming::{self, Style};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Copy,
    Move,
}

impl Mode {
    pub fn kind(self) -> OpKind {
        match self {
            Mode::Copy => OpKind::Copy,
            Mode::Move => OpKind::Move,
        }
    }
    pub fn verb(self) -> &'static str {
        match self {
            Mode::Copy => "复制",
            Mode::Move => "移动",
        }
    }
}

/// 一项可直接执行的传输：`dest` 是完整的目标路径（保证执行前不存在）。
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub src: PathBuf,
    pub dest: PathBuf,
}

/// 目标位置已有同名项目，需要用户决定。
#[derive(Clone, Debug, PartialEq)]
pub struct Conflict {
    pub src: PathBuf,
    pub existing: PathBuf,
}

/// 对全部冲突统一的处理方式。没有“替换”：不覆盖、不删除任何已有项目。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// 保留两者：新来的改名为“名称 (2)”。
    KeepBoth,
    /// 跳过冲突项。
    Skip,
}

#[derive(Debug)]
pub struct Plan {
    pub mode: Mode,
    pub dest_dir: PathBuf,
    pub jobs: Vec<Job>,
    pub conflicts: Vec<Conflict>,
    /// 计划阶段就已判定失败的项（源已不存在、文件夹放进自己里面……）。
    pub rejected: Vec<Outcome>,
    /// 无需处理的项：（名称，原因），例如移动到本来就在的文件夹。
    pub skipped: Vec<(String, String)>,
}

impl Plan {
    /// 是否没有任何可做的事（也没有冲突）。
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty() && self.conflicts.is_empty()
    }
}

/// 路径比较用的键：小写、统一分隔符、去掉末尾分隔符。
pub fn key(p: &Path) -> String {
    p.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// `dir` 是否就是 `src` 本身或在 `src` 里面。
pub fn is_inside(dir: &Path, src: &Path) -> bool {
    let (d, s) = (key(dir), key(src));
    d == s || d.starts_with(&(s + "\\"))
}

fn display_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// 目标目录是否可作为放置位置：存在、是目录、有“新建文件／子文件夹”的权限（只探测，不创建任何东西）。
pub fn check_dest_dir(dir: &Path) -> Result<(), OpError> {
    let md = std::fs::metadata(dir).map_err(|e| OpError::from_io(&e))?;
    if !md.is_dir() {
        return Err(OpError::new(FailKind::DestMissing, ""));
    }
    crate::recycle::probe_add_access(dir, true).map_err(|e| OpError::from_io(&e))?;
    crate::recycle::probe_add_access(dir, false).map_err(|e| OpError::from_io(&e))
}

/// 为 `srcs` 到 `dest_dir` 的复制或移动做计划。目标目录本身不可用时整体失败。不改动任何东西。
pub fn plan(srcs: &[PathBuf], dest_dir: &Path, mode: Mode) -> Result<Plan, OpError> {
    check_dest_dir(dest_dir)?;
    let mut p = Plan {
        mode,
        dest_dir: dest_dir.to_path_buf(),
        jobs: Vec::new(),
        conflicts: Vec::new(),
        rejected: Vec::new(),
        skipped: Vec::new(),
    };
    let mut taken: Vec<String> = Vec::new();
    for src in srcs {
        let name = display_name(src);
        let reject = |p: &mut Plan, e: OpError| {
            p.rejected.push(Outcome {
                name: name.clone(),
                result: Err(e),
            })
        };
        let md = match std::fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(e) => {
                reject(&mut p, OpError::from_io(&e));
                continue;
            }
        };
        let Some(file_name) = src.file_name() else {
            reject(&mut p, OpError::new(FailKind::Other, "不能传输磁盘根目录"));
            continue;
        };
        if md.is_dir() && is_inside(dest_dir, src) {
            reject(&mut p, OpError::new(FailKind::Nested, ""));
            continue;
        }
        let same_parent = src.parent().is_some_and(|d| key(d) == key(dest_dir));
        let target = dest_dir.join(file_name);
        if same_parent {
            match mode {
                Mode::Move => {
                    p.skipped
                        .push((name, "项目本来就在这个文件夹里".to_string()));
                }
                Mode::Copy => {
                    // 在同一文件夹里复制：像资源管理器一样生成“- 副本”，不算冲突
                    let (stem, ext) = naming::split_ext(&name, md.is_dir());
                    let uniq = naming::unique_name(stem, ext, Style::Duplicate, &|n| {
                        dest_dir.join(n).symlink_metadata().is_ok()
                            || taken.iter().any(|t| *t == n.to_lowercase())
                    });
                    taken.push(uniq.to_lowercase());
                    p.jobs.push(Job {
                        src: src.clone(),
                        dest: dest_dir.join(uniq),
                    });
                }
            }
            continue;
        }
        let lname = target
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if target.symlink_metadata().is_ok() || taken.contains(&lname) {
            p.conflicts.push(Conflict {
                src: src.clone(),
                existing: target,
            });
            continue;
        }
        taken.push(lname);
        p.jobs.push(Job {
            src: src.clone(),
            dest: target,
        });
    }
    Ok(p)
}

/// 按用户对冲突的选择补全任务清单。`Skip` 把冲突项记为跳过，`KeepBoth` 给新来的项目改名。
pub fn resolve(plan: &mut Plan, policy: Policy) {
    let conflicts = std::mem::take(&mut plan.conflicts);
    let mut taken: Vec<String> = plan
        .jobs
        .iter()
        .filter_map(|j| j.dest.file_name())
        .map(|n| n.to_string_lossy().to_lowercase())
        .collect();
    for c in conflicts {
        let name = display_name(&c.src);
        match policy {
            Policy::Skip => plan
                .skipped
                .push((name, "目标位置已有同名项目".to_string())),
            Policy::KeepBoth => {
                let is_dir = c
                    .src
                    .symlink_metadata()
                    .map(|m| m.is_dir())
                    .unwrap_or(false);
                let (stem, ext) = naming::split_ext(&name, is_dir);
                let uniq = naming::unique_name(stem, ext, Style::KeepBoth, &|n| {
                    plan.dest_dir.join(n).symlink_metadata().is_ok()
                        || taken.contains(&n.to_lowercase())
                });
                taken.push(uniq.to_lowercase());
                plan.jobs.push(Job {
                    dest: plan.dest_dir.join(uniq),
                    src: c.src,
                });
            }
        }
    }
}

/// 执行计划里的任务，得到完整汇报。计划阶段判定失败与跳过的项也计入汇报。
pub fn run(backend: &dyn crate::fileops::Backend, plan: Plan) -> Report {
    let mut outcomes = plan.rejected;
    let mut aborted = false;
    for j in &plan.jobs {
        let name = display_name(&j.src);
        let result = if aborted {
            Err(OpError::new(FailKind::Aborted, ""))
        } else {
            backend.transfer(&j.src, &j.dest, plan.mode)
        };
        if matches!(&result, Err(e) if e.kind == FailKind::Aborted) {
            aborted = true;
        }
        outcomes.push(Outcome { name, result });
    }
    Report {
        kind: plan.mode.kind(),
        outcomes,
        skipped: plan.skipped,
    }
}

/// 一批操作涉及的、内容会变化的目录：目标目录，以及移动时的源目录。
pub fn affected_dirs(plan_dest: &Path, srcs: &[PathBuf], mode: Mode) -> Vec<PathBuf> {
    let mut v = vec![plan_dest.to_path_buf()];
    if mode == Mode::Move {
        for d in crate::fileops::parent_dirs(srcs.iter().map(PathBuf::as_path)) {
            if !v.iter().any(|x| key(x) == key(&d)) {
                v.push(d);
            }
        }
    }
    v
}

// ---------- 卷判断与默认语义 ----------

/// 路径所在卷的标识：能取到卷 GUID 路径时用它，否则（subst、网络、取不到）用盘符或 UNC 共享根的小写形式。
pub fn volume_key(p: &Path) -> String {
    use std::path::Component;
    let Some(Component::Prefix(pre)) = p.components().next() else {
        return String::new();
    };
    let mut root: Vec<u16> = pre.as_os_str().encode_wide().collect();
    let fallback = String::from_utf16_lossy(&root).to_lowercase();
    root.push(b'\\' as u16);
    root.push(0);
    let mut buf = [0u16; 64];
    // SAFETY: root 以 NUL 结尾，buf 长度如实传入。
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW(
            root.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    };
    if ok != 0 {
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n]).to_lowercase()
    } else {
        fallback
    }
}

pub fn same_volume(a: &Path, b: &Path) -> bool {
    let (ka, kb) = (volume_key(a), volume_key(b));
    !ka.is_empty() && ka == kb
}

/// Windows 的默认拖放语义：全部在同一卷上则移动，否则复制。
pub fn default_mode(srcs: &[PathBuf], dest_dir: &Path) -> Mode {
    if !srcs.is_empty() && srcs.iter().all(|s| same_volume(s, dest_dir)) {
        Mode::Move
    } else {
        Mode::Copy
    }
}

// ---------- 文件系统引擎 ----------

/// 引擎的测试开关（产品不使用）。
#[derive(Clone, Debug, Default)]
pub struct FsOpts {
    /// 移动时不走同卷改名，直接按跨卷处理（复制 + 校验 + 删除源）。
    pub force_cross_volume: bool,
    /// 复制到名为此值的文件时失败，模拟中途出错（磁盘满、设备断开……）。
    pub fail_on_file: Option<(String, FailKind)>,
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 改名／移动，**目标已存在时失败**（`MoveFileExW` 不带替换标志；对文件夹同样有效）。
pub fn move_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let (a, b) = (wide(from), wide(to));
    // SAFETY: 两个缓冲区都以 NUL 结尾。flags=0：不替换已存在的目标。
    let ok =
        unsafe { windows_sys::Win32::Storage::FileSystem::MoveFileExW(a.as_ptr(), b.as_ptr(), 0) };
    if ok != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// 复制单个文件，**目标已存在时失败**，保留修改时间。
pub fn copy_file_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{COPY_FILE_FAIL_IF_EXISTS, CopyFileExW};
    let (a, b) = (wide(from), wide(to));
    // SAFETY: 缓冲区以 NUL 结尾；不使用进度回调与取消标志。
    let ok = unsafe {
        CopyFileExW(
            a.as_ptr(),
            b.as_ptr(),
            None,
            std::ptr::null(),
            std::ptr::null_mut(),
            COPY_FILE_FAIL_IF_EXISTS,
        )
    };
    if ok != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn is_reparse(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes() & 0x400 != 0
}

fn copy_tree(src: &Path, dest: &Path, opts: &FsOpts) -> OpResult {
    let md = std::fs::symlink_metadata(src).map_err(|e| OpError::from_io(&e))?;
    if is_reparse(&md) {
        return Err(OpError::new(
            FailKind::Other,
            format!("“{}”是符号链接或目录联接，暂不支持复制", display_name(src)),
        ));
    }
    if md.is_dir() {
        std::fs::create_dir(dest).map_err(|e| OpError::from_io(&e))?;
        for entry in std::fs::read_dir(src).map_err(|e| OpError::from_io(&e))? {
            let entry = entry.map_err(|e| OpError::from_io(&e))?;
            copy_tree(&entry.path(), &dest.join(entry.file_name()), opts)?;
        }
        // 目录修改时间尽量保持（失败不影响结果）
        if let Ok(t) = md.modified() {
            set_dir_time(dest, t);
        }
        Ok(())
    } else {
        if let Some((name, kind)) = &opts.fail_on_file
            && display_name(src) == *name
        {
            return Err(OpError::new(*kind, ""));
        }
        copy_file_no_replace(src, dest).map_err(|e| OpError::from_io(&e))
    }
}

fn set_dir_time(dir: &Path, t: std::time::SystemTime) {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    if let Ok(f) = std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
    {
        let _ = f.set_modified(t);
    }
}

/// 复制后的校验：目录结构与文件大小一致。
fn verify_tree(src: &Path, dest: &Path) -> bool {
    let (Ok(a), Ok(b)) = (
        std::fs::symlink_metadata(src),
        std::fs::symlink_metadata(dest),
    ) else {
        return false;
    };
    if a.is_dir() != b.is_dir() {
        return false;
    }
    if !a.is_dir() {
        return a.len() == b.len();
    }
    let (Ok(ra), Ok(rb)) = (std::fs::read_dir(src), std::fs::read_dir(dest)) else {
        return false;
    };
    let mut names_a: Vec<_> = ra.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
    let mut names_b: Vec<_> = rb.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
    names_a.sort();
    names_b.sort();
    names_a == names_b
        && names_a
            .iter()
            .all(|n| verify_tree(&src.join(n), &dest.join(n)))
}

/// 复制或移动一项到完整目标路径 `dest`。目标已存在则失败（不覆盖）；目标所在目录必须已存在。
pub fn fs_transfer(src: &Path, dest: &Path, mode: Mode, opts: &FsOpts) -> OpResult {
    if dest.symlink_metadata().is_ok() {
        return Err(OpError::new(FailKind::Exists, ""));
    }
    let src_md = std::fs::symlink_metadata(src).map_err(|e| OpError::from_io(&e))?;
    if src_md.is_dir() && dest.parent().is_some_and(|d| is_inside(d, src)) {
        return Err(OpError::new(FailKind::Nested, ""));
    }
    if mode == Mode::Move && !opts.force_cross_volume {
        match move_no_replace(src, dest) {
            Ok(()) => return Ok(()),
            // ERROR_NOT_SAME_DEVICE：跨卷，退到复制 + 校验 + 删除源
            Err(e) if e.raw_os_error() == Some(17) => {}
            Err(e) => return Err(OpError::from_io(&e)),
        }
    }
    if let Err(e) = copy_tree(src, dest, opts) {
        // dest 是本次调用刚创建的（开头已确认不存在），清理不完整的副本；源没有被动过
        let _ = crate::fileops::delete_permanent_fs(dest);
        return Err(e);
    }
    if mode == Mode::Move {
        if !verify_tree(src, dest) {
            let _ = crate::fileops::delete_permanent_fs(dest);
            return Err(OpError::new(
                FailKind::Other,
                "复制后校验不一致，已放弃移动，原项目保持不变",
            ));
        }
        if let Err(e) = crate::fileops::delete_permanent_fs(src) {
            return Err(OpError::new(
                FailKind::Other,
                format!(
                    "已复制到目标位置，但原项目未能删除（{}）；两处现在都有这个项目",
                    e.text()
                ),
            ));
        }
    }
    Ok(())
}

/// 新建文件并写入内容；已存在则失败（`create_new`，不会覆盖）。
pub fn fs_create_file(path: &Path, content: &[u8]) -> OpResult {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| OpError::from_io(&e))?;
    if let Err(e) = f.write_all(content) {
        drop(f);
        let _ = std::fs::remove_file(path);
        return Err(OpError::from_io(&e));
    }
    Ok(())
}

/// 新建文件夹；已存在则失败。
pub fn fs_create_dir(path: &Path) -> OpResult {
    std::fs::create_dir(path).map_err(|e| OpError::from_io(&e))
}

/// 重命名（同一文件夹内）。目标已存在则失败；只改大小写时允许（同一项目）。
pub fn fs_rename(from: &Path, to: &Path) -> OpResult {
    let case_only = from.parent().map(key) == to.parent().map(key)
        && from.file_name().map(|n| n.to_string_lossy().to_lowercase())
            == to.file_name().map(|n| n.to_string_lossy().to_lowercase());
    if !case_only && to.symlink_metadata().is_ok() {
        return Err(OpError::new(FailKind::Exists, ""));
    }
    if case_only {
        // 大小写不同的同名：先确认原项目确实存在，再改名（系统把它视为同一项目，不会覆盖别的东西）
        std::fs::symlink_metadata(from).map_err(|e| OpError::from_io(&e))?;
    }
    move_no_replace(from, to).map_err(|e| OpError::from_io(&e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join("sfm-unit")
            .join(format!("xfer-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn plan_basic_conflict_nested_and_noop() {
        let root = temp("plan");
        let (a, b) = (root.join("a"), root.join("b"));
        write(&a.join("x.txt"), "x");
        write(&a.join("y.txt"), "y");
        write(&a.join("文件夹").join("内.txt"), "n");
        write(&b.join("x.txt"), "已有");
        // 复制 x（冲突）、y（可执行）、文件夹、不存在项到 b
        let srcs = vec![
            a.join("x.txt"),
            a.join("y.txt"),
            a.join("文件夹"),
            a.join("没有.txt"),
        ];
        let p = plan(&srcs, &b, Mode::Copy).unwrap();
        assert_eq!(p.jobs.len(), 2);
        assert_eq!(p.conflicts.len(), 1);
        assert_eq!(p.conflicts[0].existing, b.join("x.txt"));
        assert_eq!(p.rejected.len(), 1);
        assert_eq!(
            p.rejected[0].result.as_ref().unwrap_err().kind,
            FailKind::NotFound
        );
        // 移动到原位置：跳过
        let p = plan(&[a.join("x.txt")], &a, Mode::Move).unwrap();
        assert!(p.jobs.is_empty() && p.conflicts.is_empty());
        assert_eq!(p.skipped.len(), 1);
        // 把文件夹放进自己里面 / 自己的子孙里
        let p = plan(std::slice::from_ref(&a), &a.join("文件夹"), Mode::Move).unwrap();
        assert_eq!(
            p.rejected[0].result.as_ref().unwrap_err().kind,
            FailKind::Nested
        );
        let p = plan(&[a.join("文件夹")], &a.join("文件夹"), Mode::Copy).unwrap();
        assert_eq!(
            p.rejected[0].result.as_ref().unwrap_err().kind,
            FailKind::Nested,
            "文件夹复制到自己里面"
        );
        // 目标目录不存在 / 是文件
        assert!(plan(&srcs, &root.join("不存在"), Mode::Copy).is_err());
        assert_eq!(
            plan(&srcs, &b.join("x.txt"), Mode::Copy).unwrap_err().kind,
            FailKind::DestMissing
        );
    }

    #[test]
    fn copy_in_same_folder_makes_duplicate_names() {
        let root = temp("dup");
        write(&root.join("报告.txt"), "r");
        write(&root.join("报告 - 副本.txt"), "old");
        let p = plan(&[root.join("报告.txt")], &root, Mode::Copy).unwrap();
        assert!(p.conflicts.is_empty());
        assert_eq!(p.jobs[0].dest, root.join("报告 - 副本 (2).txt"));
    }

    #[test]
    fn resolve_keep_both_and_skip() {
        let root = temp("resolve");
        let (a, b) = (root.join("a"), root.join("b"));
        write(&a.join("x.txt"), "新");
        write(&b.join("x.txt"), "旧");
        write(&b.join("x (2).txt"), "旧2");
        let mut p = plan(&[a.join("x.txt")], &b, Mode::Copy).unwrap();
        resolve(&mut p, Policy::KeepBoth);
        assert_eq!(p.jobs.len(), 1);
        assert_eq!(p.jobs[0].dest, b.join("x (3).txt"));
        let mut p = plan(&[a.join("x.txt")], &b, Mode::Copy).unwrap();
        resolve(&mut p, Policy::Skip);
        assert!(p.jobs.is_empty());
        assert_eq!(p.skipped.len(), 1);
    }

    #[test]
    fn fs_copy_file_and_folder_never_overwrite() {
        let root = temp("copy");
        write(&root.join("s").join("f.txt"), "内容");
        write(&root.join("s").join("d").join("g.txt"), "g");
        let o = FsOpts::default();
        fs_transfer(
            &root.join("s").join("f.txt"),
            &root.join("f.txt"),
            Mode::Copy,
            &o,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "内容");
        assert!(root.join("s").join("f.txt").exists(), "复制不动源");
        // 目标已存在：失败且内容不变
        write(&root.join("占用.txt"), "原内容");
        let e = fs_transfer(
            &root.join("s").join("f.txt"),
            &root.join("占用.txt"),
            Mode::Copy,
            &o,
        )
        .unwrap_err();
        assert_eq!(e.kind, FailKind::Exists);
        assert_eq!(fs::read_to_string(root.join("占用.txt")).unwrap(), "原内容");
        // 文件夹递归
        fs_transfer(&root.join("s").join("d"), &root.join("d2"), Mode::Copy, &o).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("d2").join("g.txt")).unwrap(),
            "g"
        );
    }

    #[test]
    fn fs_move_same_volume_renames_and_refuses_overwrite() {
        let root = temp("move");
        write(&root.join("s").join("f.txt"), "内容");
        write(&root.join("s").join("d").join("g.txt"), "g");
        let o = FsOpts::default();
        fs_transfer(
            &root.join("s").join("f.txt"),
            &root.join("f.txt"),
            Mode::Move,
            &o,
        )
        .unwrap();
        assert!(!root.join("s").join("f.txt").exists());
        assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "内容");
        fs_transfer(&root.join("s").join("d"), &root.join("d"), Mode::Move, &o).unwrap();
        assert!(root.join("d").join("g.txt").exists() && !root.join("s").join("d").exists());
        // 目标已存在：失败，源与目标都不变
        write(&root.join("s").join("h.txt"), "H");
        write(&root.join("h.txt"), "已有 H");
        let e = fs_transfer(
            &root.join("s").join("h.txt"),
            &root.join("h.txt"),
            Mode::Move,
            &o,
        )
        .unwrap_err();
        assert_eq!(e.kind, FailKind::Exists);
        assert_eq!(fs::read_to_string(root.join("h.txt")).unwrap(), "已有 H");
        assert_eq!(
            fs::read_to_string(root.join("s").join("h.txt")).unwrap(),
            "H"
        );
    }

    #[test]
    fn cross_volume_move_removes_source_only_after_success() {
        let root = temp("cross");
        write(&root.join("s").join("d").join("a.txt"), "aaa");
        write(&root.join("s").join("d").join("sub").join("b.txt"), "bbb");
        let ok = FsOpts {
            force_cross_volume: true,
            ..FsOpts::default()
        };
        fs_transfer(&root.join("s").join("d"), &root.join("d"), Mode::Move, &ok).unwrap();
        assert!(!root.join("s").join("d").exists(), "成功后源被移除");
        assert_eq!(
            fs::read_to_string(root.join("d").join("sub").join("b.txt")).unwrap(),
            "bbb"
        );
        // 复制到一半失败：目标清理、源原样保留
        write(&root.join("s2").join("d").join("a.txt"), "aaa");
        write(
            &root.join("s2").join("d").join("sub").join("bad.txt"),
            "bbb",
        );
        let bad = FsOpts {
            force_cross_volume: true,
            fail_on_file: Some(("bad.txt".into(), FailKind::DiskFull)),
        };
        let e = fs_transfer(
            &root.join("s2").join("d"),
            &root.join("d2"),
            Mode::Move,
            &bad,
        )
        .unwrap_err();
        assert_eq!(e.kind, FailKind::DiskFull);
        assert!(!root.join("d2").exists(), "不完整的副本应被清理");
        assert_eq!(
            fs::read_to_string(root.join("s2").join("d").join("a.txt")).unwrap(),
            "aaa",
            "源必须保持原样"
        );
        assert!(
            root.join("s2")
                .join("d")
                .join("sub")
                .join("bad.txt")
                .exists()
        );
    }

    #[test]
    fn move_folder_into_itself_is_refused() {
        let root = temp("nested");
        write(&root.join("d").join("x.txt"), "x");
        let e = fs_transfer(
            &root.join("d"),
            &root.join("d").join("子").join("d"),
            Mode::Move,
            &FsOpts::default(),
        );
        assert!(e.is_err());
        assert!(root.join("d").join("x.txt").exists());
    }

    #[test]
    fn rename_refuses_collision_and_allows_case_change() {
        let root = temp("rename");
        write(&root.join("a.txt"), "A");
        write(&root.join("b.txt"), "B");
        let e = fs_rename(&root.join("a.txt"), &root.join("b.txt")).unwrap_err();
        assert_eq!(e.kind, FailKind::Exists);
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "B");
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "A");
        // 与另一个项目只差大小写：仍是冲突
        let e = fs_rename(&root.join("a.txt"), &root.join("B.TXT")).unwrap_err();
        assert_eq!(e.kind, FailKind::Exists, "{e:?}");
        // 只改自己的大小写：允许
        fs_rename(&root.join("a.txt"), &root.join("A.TXT")).unwrap();
        let names: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"A.TXT".to_string()), "{names:?}");
        assert_eq!(fs::read_to_string(root.join("A.TXT")).unwrap(), "A");
        // 普通改名
        fs_rename(&root.join("A.TXT"), &root.join("新名字.txt")).unwrap();
        assert!(root.join("新名字.txt").exists());
        // 源不存在
        let e = fs_rename(&root.join("不存在.txt"), &root.join("x.txt")).unwrap_err();
        assert_eq!(e.kind, FailKind::NotFound);
    }

    #[test]
    fn create_dir_and_file_never_replace() {
        let root = temp("mkdir");
        fs_create_dir(&root.join("新建文件夹")).unwrap();
        let e = fs_create_dir(&root.join("新建文件夹")).unwrap_err();
        assert_eq!(e.kind, FailKind::Exists);
        fs_create_file(&root.join("a.txt"), b"").unwrap();
        fs_create_file(&root.join("b.txt"), b"hello").unwrap();
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"hello");
        let e = fs_create_file(&root.join("b.txt"), b"x").unwrap_err();
        assert_eq!(e.kind, FailKind::Exists);
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"hello");
    }

    #[test]
    fn volume_rules() {
        let t = std::env::temp_dir();
        assert!(same_volume(&t.join("a"), &t.join("b").join("c")));
        assert!(!volume_key(&t).is_empty());
        let none: PathBuf = "relative".into();
        assert!(!same_volume(&none, &t));
        assert_eq!(default_mode(&[t.join("a")], &t.join("x")), Mode::Move);
        // 同机另一盘符（如果存在）→ 复制
        let other = ["C:\\", "D:\\", "E:\\"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists() && !same_volume(p, &t));
        if let Some(o) = other {
            assert_eq!(default_mode(&[t.join("a")], &o), Mode::Copy);
        }
    }

    #[test]
    fn run_stops_after_abort_and_reports_remaining() {
        use crate::fileops::{Inject, StashBackend};
        let root = temp("abort");
        let b = StashBackend::new(root.join("bin"));
        let (s, d) = (root.join("s"), root.join("d"));
        for n in ["1.txt", "2.txt", "3.txt"] {
            write(&s.join(n), n);
        }
        fs::create_dir_all(&d).unwrap();
        b.inject(s.join("2.txt"), Inject::Transfer(FailKind::Aborted));
        let srcs: Vec<_> = ["1.txt", "2.txt", "3.txt"]
            .iter()
            .map(|n| s.join(n))
            .collect();
        let p = plan(&srcs, &d, Mode::Copy).unwrap();
        let r = run(&*b, p);
        assert_eq!((r.ok_count(), r.fail_count()), (1, 2));
        assert!(d.join("1.txt").exists() && !d.join("2.txt").exists() && !d.join("3.txt").exists());
        assert_eq!(r.summary().line, "已取消：已复制 1 项，其余 2 项没有复制");
    }

    #[test]
    fn summary_for_skips_and_partial() {
        let ok = |n: &str| Outcome {
            name: n.into(),
            result: Ok(()),
        };
        let mut r = Report::new(OpKind::Copy, vec![ok("a"), ok("b")]);
        r.skipped.push(("c".into(), "目标位置已有同名项目".into()));
        assert_eq!(r.summary().line, "已复制 2 项，跳过 1 项");
        let r = Report {
            kind: OpKind::Move,
            outcomes: vec![],
            skipped: vec![("c".into(), "项目本来就在这个文件夹里".into())],
        };
        assert_eq!(
            r.summary().line,
            "没有移动任何项目：项目本来就在这个文件夹里"
        );
        let bad = Outcome {
            name: "x".into(),
            result: Err(OpError::new(FailKind::DiskFull, "")),
        };
        let r = Report::new(OpKind::Copy, vec![ok("a"), bad]);
        assert_eq!(r.summary().line, "部分完成：1 项已复制，1 项未能复制");
        assert!(r.summary().detail.unwrap().contains("x：磁盘空间不足"));
    }
}
