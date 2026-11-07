//! 删除、回收站浏览与恢复：与界面无关的操作层。
//!
//! - [`Backend`]：删除到回收站、永久删除、枚举回收站、恢复单项。产品用 [`SystemBackend`]（Windows 回收站，
//!   见 [`crate::recycle`]）；harness 与单元测试用 [`StashBackend`]（把“回收站”放在一个临时目录里，
//!   可注入各类失败，绝不碰用户真实的回收站）。
//! - [`run_delete`] / [`run_restore`]：逐项执行并逐项记录结果，得到 [`Report`]。成功、失败、部分成功都按真实结果
//!   汇报；调用方刷新列表后也不能把“列表变了”当成“全部成功”。
//! - 删除到回收站失败（含不支持回收站的位置）时**不会**改成永久删除，只报告失败与原因。
//! - 恢复遇到同名项目、原目录已不存在、权限不足时，该项原样留在回收站，并说明原因。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// 回收站里的一项。`id` 是外壳给出的项目标识（对调用方不透明，恢复时原样交回）。
#[derive(Clone, Debug, PartialEq)]
pub struct BinItem {
    pub id: Vec<u8>,
    /// 删除前的文件名（含扩展名）。
    pub name: String,
    /// 删除前所在的文件夹（完整路径）。
    pub original_dir: String,
    pub deleted: Option<SystemTime>,
    pub size: u64,
    pub is_dir: bool,
}

impl BinItem {
    /// 原位置的完整路径。
    pub fn original_path(&self) -> PathBuf {
        Path::new(&self.original_dir).join(&self.name)
    }
}

/// 失败原因的类别：决定提示文字。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailKind {
    /// 项目已不存在（可能已被其他程序移动或删除）。
    NotFound,
    /// 没有权限。
    AccessDenied,
    /// 项目正被其他程序占用。
    InUse,
    /// 所在位置不支持回收站（可移动磁盘、网络位置、路径过长等），没有任何改动。
    Unsupported,
    /// 恢复：原位置已有同名项目。
    Conflict,
    /// 恢复：原目录已不存在。
    DestMissing,
    /// 被系统取消（例如用户在系统提示中拒绝）。
    Aborted,
    /// 新建、重命名、复制、移动：目标位置已有同名项目，没有覆盖。
    Exists,
    /// 磁盘空间不足。
    DiskFull,
    /// 设备已断开、网络位置不可用。
    DeviceGone,
    /// 不能把文件夹放进它自己里面。
    Nested,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpError {
    pub kind: FailKind,
    /// 系统给出的补充说明（可为空）。
    pub detail: String,
}

impl OpError {
    pub fn new(kind: FailKind, detail: impl Into<String>) -> OpError {
        OpError {
            kind,
            detail: detail.into(),
        }
    }

    /// 给用户看的原因。
    pub fn text(&self) -> String {
        let base = match self.kind {
            FailKind::NotFound => "项目已不存在",
            FailKind::AccessDenied => "没有权限",
            FailKind::InUse => "正被其他程序使用",
            FailKind::Unsupported => "此位置不支持回收站，未做任何改动",
            FailKind::Conflict => "原位置已有同名项目，未覆盖",
            FailKind::DestMissing => "原文件夹已不存在",
            FailKind::Aborted => "操作被取消",
            FailKind::Exists => "目标位置已有同名项目，未覆盖",
            FailKind::DiskFull => "磁盘空间不足",
            FailKind::DeviceGone => "设备已断开或暂时不可用",
            FailKind::Nested => "不能把文件夹放进它自己里面",
            FailKind::Other => "操作失败",
        };
        if self.detail.is_empty() || self.kind == FailKind::Unsupported {
            base.to_string()
        } else {
            format!("{base}（{}）", self.detail)
        }
    }

    /// 由系统 I/O 错误归类。
    pub fn from_io(e: &std::io::Error) -> OpError {
        let kind = match e.raw_os_error() {
            Some(2 | 3 | 123) => FailKind::NotFound,
            Some(5 | 19) => FailKind::AccessDenied,
            Some(32 | 33) => FailKind::InUse,
            Some(80 | 183) => FailKind::Exists,
            Some(39 | 112) => FailKind::DiskFull,
            Some(21 | 31 | 53 | 55 | 64 | 67 | 1117 | 1167) => FailKind::DeviceGone,
            _ => match e.kind() {
                std::io::ErrorKind::NotFound => FailKind::NotFound,
                std::io::ErrorKind::PermissionDenied => FailKind::AccessDenied,
                _ => FailKind::Other,
            },
        };
        OpError::new(
            kind,
            if kind == FailKind::Other {
                e.to_string()
            } else {
                String::new()
            },
        )
    }
}

pub type OpResult = Result<(), OpError>;

/// 文件操作后端。每个方法只处理一个项目，批量与汇总由 [`run_delete`] / [`run_restore`] 负责。
pub trait Backend: Send + Sync {
    /// 移到回收站。位置不支持时返回 [`FailKind::Unsupported`]，不得退化为永久删除。
    fn recycle(&self, path: &Path) -> OpResult;
    /// 该位置能否使用回收站（不改动任何东西）；`Err(原因)` 表示不支持。
    fn can_recycle(&self, path: &Path) -> Result<(), String>;
    /// 永久删除（文件夹连同内容）。链接只删链接本身。
    fn delete_permanent(&self, path: &Path) -> OpResult {
        delete_permanent_fs(path)
    }
    /// 枚举回收站。
    fn list_bin(&self) -> Result<Vec<BinItem>, String>;
    /// 把一项恢复到原位置。不覆盖已有项目，不自动创建缺失的原目录。
    fn restore(&self, item: &BinItem) -> OpResult;
    /// 复制或移动一项到完整目标路径 `dest`（调用方已在计划阶段确认其不存在）。目标已存在时必须失败，不得覆盖。
    /// 默认用文件系统引擎；产品后端改用系统的 `IFileOperation`。
    fn transfer(&self, src: &Path, dest: &Path, mode: crate::xfer::Mode) -> OpResult {
        crate::xfer::fs_transfer(src, dest, mode, &crate::xfer::FsOpts::default())
    }
    /// 新建文件夹；已存在则失败。
    fn create_dir(&self, path: &Path) -> OpResult {
        crate::xfer::fs_create_dir(path)
    }
    /// 新建文件并写入 `content`（空文件传空切片）；已存在则失败。
    fn create_file(&self, path: &Path, content: &[u8]) -> OpResult {
        crate::xfer::fs_create_file(path, content)
    }
    /// 同一文件夹内重命名；目标已存在则失败（只改大小写除外）。
    fn rename(&self, from: &Path, to: &Path) -> OpResult {
        crate::xfer::fs_rename(from, to)
    }
}

/// 永久删除：文件、文件夹（递归）、符号链接与目录联接（只删链接本身）。
pub fn delete_permanent_fs(path: &Path) -> OpResult {
    let md = std::fs::symlink_metadata(path).map_err(|e| OpError::from_io(&e))?;
    let r = if md.file_type().is_symlink() {
        // 目录符号链接 / 联接需要 remove_dir，文件符号链接用 remove_file
        std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path))
    } else if md.is_dir() {
        if is_junction(&md) {
            std::fs::remove_dir(path)
        } else {
            std::fs::remove_dir_all(path)
        }
    } else {
        std::fs::remove_file(path)
    };
    r.map_err(|e| OpError::from_io(&e))
}

fn is_junction(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// 产品使用的后端：Windows 回收站。
pub struct SystemBackend;

impl Backend for SystemBackend {
    fn transfer(&self, src: &Path, dest: &Path, mode: crate::xfer::Mode) -> OpResult {
        crate::shellop::transfer(src, dest, mode)
    }

    fn recycle(&self, path: &Path) -> OpResult {
        crate::recycle::recycle(path)
    }
    fn can_recycle(&self, path: &Path) -> Result<(), String> {
        crate::recycle::can_recycle(path)
    }
    fn list_bin(&self) -> Result<Vec<BinItem>, String> {
        crate::recycle::list()
    }
    fn restore(&self, item: &BinItem) -> OpResult {
        crate::recycle::restore(item)
    }
}

// ---------- 批量执行与汇报 ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteMode {
    Recycle,
    Permanent,
}

/// 一项的执行结果。
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    /// 显示名称。
    pub name: String,
    pub result: OpResult,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    Recycle,
    Permanent,
    Restore,
    Copy,
    Move,
    /// 新建文件夹或文件（名称为新项目的名称）。
    Create,
    /// 重命名（名称为改名后的名称）。
    Rename,
}

/// 一次批量操作的结果。
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub kind: OpKind,
    pub outcomes: Vec<Outcome>,
    /// 没有执行也不算失败的项（用户选择跳过同名项、移动到原位置）：（名称，原因）。
    pub skipped: Vec<(String, String)>,
}

/// 同一份结果的用户可见汇报。
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    /// 状态栏一行文字。
    pub line: String,
    /// 有失败项时给出的详细说明（对话框正文），全部成功时为 `None`。
    pub detail: Option<String>,
}

/// 对话框与状态栏最多列出的失败项数。
const LIST_LIMIT: usize = 8;

impl Report {
    pub fn new(kind: OpKind, outcomes: Vec<Outcome>) -> Report {
        Report {
            kind,
            outcomes,
            skipped: Vec::new(),
        }
    }

    pub fn ok_count(&self) -> usize {
        self.outcomes.iter().filter(|o| o.result.is_ok()).count()
    }
    pub fn fail_count(&self) -> usize {
        self.outcomes.len() - self.ok_count()
    }
    pub fn failures(&self) -> impl Iterator<Item = (&str, &OpError)> {
        self.outcomes
            .iter()
            .filter_map(|o| o.result.as_ref().err().map(|e| (o.name.as_str(), e)))
    }

    /// 汇报文字：全部成功、全部失败、部分成功都如实说明。
    pub fn summary(&self) -> Summary {
        let (ok, fail) = (self.ok_count(), self.fail_count());
        let skipped = self.skipped.len();
        let done = match self.kind {
            OpKind::Recycle => "移到回收站",
            OpKind::Permanent => "永久删除",
            OpKind::Restore => "还原到原位置",
            OpKind::Copy => "复制",
            OpKind::Move => "移动",
            OpKind::Create => "新建",
            OpKind::Rename => "重命名",
        };
        let first_name = self.outcomes.first().map(|o| o.name.as_str()).unwrap_or("");
        let skip_note = if skipped > 0 {
            format!("，跳过 {skipped} 项")
        } else {
            String::new()
        };
        if fail == 0 {
            if ok == 0 {
                // 没有任何项目被处理（全部跳过）
                let why = self
                    .skipped
                    .first()
                    .map(|(_, w)| w.as_str())
                    .unwrap_or("没有需要处理的项目");
                return Summary {
                    line: format!("没有{done}任何项目：{why}"),
                    detail: None,
                };
            }
            let line = if ok == 1 {
                match self.kind {
                    OpKind::Recycle => format!("已将“{first_name}”移到回收站"),
                    OpKind::Permanent => format!("已永久删除“{first_name}”"),
                    OpKind::Restore => format!("已将“{first_name}”还原到原位置"),
                    OpKind::Copy => format!("已复制“{first_name}”"),
                    OpKind::Move => format!("已移动“{first_name}”"),
                    OpKind::Create => format!("已新建“{first_name}”"),
                    OpKind::Rename => format!("已重命名为“{first_name}”"),
                }
            } else {
                match self.kind {
                    OpKind::Recycle => format!("已将 {ok} 项移到回收站"),
                    OpKind::Permanent => format!("已永久删除 {ok} 项"),
                    OpKind::Restore => format!("已将 {ok} 项还原到原位置"),
                    OpKind::Copy => format!("已复制 {ok} 项"),
                    OpKind::Move => format!("已移动 {ok} 项"),
                    OpKind::Create => format!("已新建 {ok} 项"),
                    OpKind::Rename => format!("已重命名 {ok} 项"),
                }
            };
            return Summary {
                line: line + &skip_note,
                detail: None,
            };
        }
        let failed_do = match self.kind {
            OpKind::Recycle => "未能移到回收站",
            OpKind::Permanent => "未能删除",
            OpKind::Restore => "未能还原",
            OpKind::Copy => "未能复制",
            OpKind::Move => "未能移动",
            OpKind::Create => "未能新建",
            OpKind::Rename => "未能重命名",
        };
        let all_aborted = self.failures().all(|(_, e)| e.kind == FailKind::Aborted);
        let line = if all_aborted && matches!(self.kind, OpKind::Copy | OpKind::Move) {
            if ok == 0 {
                format!("已取消，没有{done}任何项目")
            } else {
                format!("已取消：已{done} {ok} 项，其余 {fail} 项没有{done}")
            }
        } else if ok == 0 {
            if fail == 1 {
                let (n, e) = self.failures().next().expect("有失败项");
                format!("{failed_do}“{n}”：{}", e.text())
            } else {
                format!("{fail} 项全部{failed_do}")
            }
        } else {
            format!("部分完成：{ok} 项已{done}，{fail} 项{failed_do}{skip_note}")
        };
        let mut d = String::new();
        if ok > 0 {
            d += &format!("已{done} {ok} 项。\n");
        }
        if skipped > 0 {
            d += &format!("跳过 {skipped} 项（{}）。\n", self.skipped[0].1);
        }
        d += &format!("{failed_do} {fail} 项，这些项目保持原样：\n");
        for (n, e) in self.failures().take(LIST_LIMIT) {
            d += &format!("・{n}：{}\n", e.text());
        }
        if fail > LIST_LIMIT {
            d += &format!("……另有 {} 项未列出。\n", fail - LIST_LIMIT);
        }
        Summary {
            line,
            detail: Some(d.trim_end().to_string()),
        }
    }
}

fn display_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// 逐项删除。回收失败的项目不会被永久删除。
pub fn run_delete(backend: &dyn Backend, paths: &[PathBuf], mode: DeleteMode) -> Report {
    let outcomes = paths
        .iter()
        .map(|p| Outcome {
            name: display_name(p),
            result: match mode {
                DeleteMode::Recycle => backend.recycle(p),
                DeleteMode::Permanent => backend.delete_permanent(p),
            },
        })
        .collect();
    Report::new(
        match mode {
            DeleteMode::Recycle => OpKind::Recycle,
            DeleteMode::Permanent => OpKind::Permanent,
        },
        outcomes,
    )
}

/// 逐项恢复。
pub fn run_restore(backend: &dyn Backend, items: &[BinItem]) -> Report {
    Report::new(
        OpKind::Restore,
        items
            .iter()
            .map(|it| Outcome {
                name: it.name.clone(),
                result: backend.restore(it),
            })
            .collect(),
    )
}

/// 一批路径涉及的目录（去重，保持出现顺序）：删除后需要刷新显示这些目录的标签。
pub fn parent_dirs<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        if let Some(d) = p.parent()
            && !out.iter().any(|x| x == d)
        {
            out.push(d.to_path_buf());
        }
    }
    out
}

// ---------- 临时目录“回收站”（harness 与测试用） ----------

/// 注入的失败。
#[derive(Clone, Debug)]
pub enum Inject {
    /// 对该路径的删除（回收或永久）失败。
    Delete(FailKind),
    /// 该路径所在位置不支持回收站。
    Unsupported,
    /// 恢复该名称的项目时失败。
    Restore(FailKind),
    /// 以该路径为源的复制／移动失败（`Aborted` 即模拟用户取消）。
    Transfer(FailKind),
    /// 复制到名为此值的文件时失败，模拟中途出错（磁盘满、设备断开……），已复制的部分会被清理。
    TransferFile(String, FailKind),
    /// 在该路径新建文件或文件夹失败。
    Create(FailKind),
    /// 重命名该路径失败。
    Rename(FailKind),
}

#[derive(Default)]
struct StashState {
    next: u64,
    /// id → (项目, 暂存位置)
    items: BTreeMap<u64, (BinItem, PathBuf)>,
    inject: HashMap<PathBuf, Inject>,
    /// 固定“删除时间”（截图要确定性）；`None` 用当前时间
    clock: Option<SystemTime>,
    /// 用于核对“没有碰真实回收站”的调用计数
    recycled: usize,
    /// 传输引擎开关：强制按跨卷处理
    force_cross_volume: bool,
}

/// 把“回收站”放在 `stash` 目录里的后端：回收 = 移入暂存目录，恢复 = 移回原位置。
/// 行为与规格一致（不覆盖、不建目录、失败原样保留），并能注入失败。只用于 harness 与测试。
pub struct StashBackend {
    stash: PathBuf,
    state: Mutex<StashState>,
}

impl StashBackend {
    /// `stash` 必须是可丢弃目录；调用时不清空，由调用方准备。
    pub fn new(stash: PathBuf) -> Arc<StashBackend> {
        let _ = std::fs::create_dir_all(&stash);
        Arc::new(StashBackend {
            stash,
            state: Mutex::new(StashState::default()),
        })
    }

    /// 让后续对 `path` 的操作按 `inject` 失败。
    pub fn inject(&self, path: impl Into<PathBuf>, inject: Inject) {
        self.state
            .lock()
            .unwrap()
            .inject
            .insert(path.into(), inject);
    }

    /// 之后进入“回收站”的项目都记为这个删除时间（harness 截图用，让“删除日期”列确定）。
    pub fn set_clock(&self, t: SystemTime) {
        self.state.lock().unwrap().clock = Some(t);
    }

    /// 让移动一律按跨卷处理（复制 + 校验 + 删除源），测试跨卷语义。
    pub fn set_force_cross_volume(&self, on: bool) {
        self.state.lock().unwrap().force_cross_volume = on;
    }

    pub fn clear_injections(&self) {
        self.state.lock().unwrap().inject.clear();
    }

    pub fn bin_len(&self) -> usize {
        self.state.lock().unwrap().items.len()
    }

    pub fn recycled_count(&self) -> usize {
        self.state.lock().unwrap().recycled
    }
}

impl Backend for StashBackend {
    fn recycle(&self, path: &Path) -> OpResult {
        let mut st = self.state.lock().unwrap();
        match st.inject.get(path) {
            Some(Inject::Unsupported) => return Err(OpError::new(FailKind::Unsupported, "")),
            Some(Inject::Delete(k)) => return Err(OpError::new(*k, "")),
            _ => {}
        }
        let md = std::fs::symlink_metadata(path).map_err(|e| OpError::from_io(&e))?;
        let n = st.next;
        st.next += 1;
        let slot = self.stash.join(format!("{n}"));
        std::fs::create_dir_all(&slot).map_err(|e| OpError::from_io(&e))?;
        let name = display_name(path);
        let target = slot.join(&name);
        std::fs::rename(path, &target).map_err(|e| OpError::from_io(&e))?;
        let item = BinItem {
            id: n.to_le_bytes().to_vec(),
            name,
            original_dir: path
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            deleted: Some(st.clock.unwrap_or_else(SystemTime::now)),
            size: if md.is_dir() { 0 } else { md.len() },
            is_dir: md.is_dir(),
        };
        st.items.insert(n, (item, target));
        st.recycled += 1;
        Ok(())
    }

    fn can_recycle(&self, path: &Path) -> Result<(), String> {
        match self.state.lock().unwrap().inject.get(path) {
            Some(Inject::Unsupported) => Err("注入：此位置不支持回收站".into()),
            _ => Ok(()),
        }
    }

    fn delete_permanent(&self, path: &Path) -> OpResult {
        if let Some(Inject::Delete(k)) = self.state.lock().unwrap().inject.get(path) {
            return Err(OpError::new(*k, ""));
        }
        delete_permanent_fs(path)
    }

    fn list_bin(&self) -> Result<Vec<BinItem>, String> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .items
            .values()
            .map(|(i, _)| i.clone())
            .collect())
    }

    fn restore(&self, item: &BinItem) -> OpResult {
        let mut st = self.state.lock().unwrap();
        let key = u64::from_le_bytes(item.id.clone().try_into().unwrap_or([0; 8]));
        let Some((_, stash_path)) = st.items.get(&key).cloned() else {
            return Err(OpError::new(FailKind::NotFound, ""));
        };
        let dest = item.original_path();
        if let Some(Inject::Restore(k)) = st.inject.get(&dest) {
            return Err(OpError::new(*k, ""));
        }
        let dir = Path::new(&item.original_dir);
        if !dir.is_dir() {
            return Err(OpError::new(FailKind::DestMissing, ""));
        }
        if std::fs::symlink_metadata(&dest).is_ok() {
            return Err(OpError::new(FailKind::Conflict, ""));
        }
        std::fs::rename(&stash_path, &dest).map_err(|e| OpError::from_io(&e))?;
        st.items.remove(&key);
        Ok(())
    }

    fn transfer(&self, src: &Path, dest: &Path, mode: crate::xfer::Mode) -> OpResult {
        let mut opts = crate::xfer::FsOpts::default();
        {
            let st = self.state.lock().unwrap();
            opts.force_cross_volume = st.force_cross_volume;
            match st.inject.get(src) {
                Some(Inject::Transfer(k)) => return Err(OpError::new(*k, "")),
                Some(Inject::TransferFile(n, k)) => opts.fail_on_file = Some((n.clone(), *k)),
                _ => {}
            }
        }
        // 锁在复制期间不持有：慢速传输不应阻塞 harness 读状态
        crate::xfer::fs_transfer(src, dest, mode, &opts)
    }

    fn create_dir(&self, path: &Path) -> OpResult {
        if let Some(Inject::Create(k)) = self.state.lock().unwrap().inject.get(path) {
            return Err(OpError::new(*k, ""));
        }
        crate::xfer::fs_create_dir(path)
    }

    fn create_file(&self, path: &Path, content: &[u8]) -> OpResult {
        if let Some(Inject::Create(k)) = self.state.lock().unwrap().inject.get(path) {
            return Err(OpError::new(*k, ""));
        }
        crate::xfer::fs_create_file(path, content)
    }

    fn rename(&self, from: &Path, to: &Path) -> OpResult {
        if let Some(Inject::Rename(k)) = self.state.lock().unwrap().inject.get(from) {
            return Err(OpError::new(*k, ""));
        }
        crate::xfer::fs_rename(from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join("sfm-unit")
            .join(format!("fileops-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn setup(name: &str) -> (PathBuf, Arc<StashBackend>) {
        let root = temp(name);
        let work = root.join("work");
        fs::create_dir_all(&work).unwrap();
        let b = StashBackend::new(root.join("bin"));
        (work, b)
    }

    #[test]
    fn recycle_then_restore_roundtrip() {
        let (work, b) = setup("roundtrip");
        fs::write(work.join("报告.txt"), b"x").unwrap();
        fs::create_dir(work.join("文件夹")).unwrap();
        fs::write(work.join("文件夹").join("里面.txt"), b"y").unwrap();
        let paths = vec![work.join("报告.txt"), work.join("文件夹")];
        let r = run_delete(&*b, &paths, DeleteMode::Recycle);
        assert_eq!(r.ok_count(), 2);
        assert!(!work.join("报告.txt").exists() && !work.join("文件夹").exists());
        assert_eq!(b.bin_len(), 2);
        let items = b.list_bin().unwrap();
        let r = run_restore(&*b, &items);
        assert_eq!(r.fail_count(), 0, "{:?}", r);
        assert!(work.join("报告.txt").is_file());
        assert!(work.join("文件夹").join("里面.txt").is_file());
        assert_eq!(b.bin_len(), 0);
    }

    #[test]
    fn unsupported_location_is_not_permanently_deleted() {
        let (work, b) = setup("unsupported");
        let f = work.join("留着.txt");
        fs::write(&f, b"x").unwrap();
        b.inject(&f, Inject::Unsupported);
        let r = run_delete(&*b, std::slice::from_ref(&f), DeleteMode::Recycle);
        assert_eq!(r.fail_count(), 1);
        assert_eq!(
            r.outcomes[0].result.as_ref().unwrap_err().kind,
            FailKind::Unsupported
        );
        assert!(f.exists(), "不支持回收站时不得退化为永久删除");
        assert!(r.summary().line.contains("未能移到回收站"));
    }

    #[test]
    fn partial_success_is_reported_accurately() {
        let (work, b) = setup("partial");
        for n in ["a.txt", "b.txt", "c.txt"] {
            fs::write(work.join(n), n).unwrap();
        }
        b.inject(work.join("b.txt"), Inject::Delete(FailKind::AccessDenied));
        let paths: Vec<_> = ["a.txt", "b.txt", "c.txt", "不存在.txt"]
            .iter()
            .map(|n| work.join(n))
            .collect();
        let r = run_delete(&*b, &paths, DeleteMode::Recycle);
        assert_eq!((r.ok_count(), r.fail_count()), (2, 2));
        assert!(work.join("b.txt").exists(), "失败项保持原样");
        let s = r.summary();
        assert_eq!(s.line, "部分完成：2 项已移到回收站，2 项未能移到回收站");
        let d = s.detail.unwrap();
        assert!(
            d.contains("b.txt：没有权限") && d.contains("不存在.txt：项目已不存在"),
            "{d}"
        );
    }

    #[test]
    fn summary_wording() {
        let ok = |n: &str| Outcome {
            name: n.into(),
            result: Ok(()),
        };
        let r = Report::new(OpKind::Permanent, vec![ok("甲.txt")]);
        assert_eq!(r.summary().line, "已永久删除“甲.txt”");
        let r = Report::new(OpKind::Restore, vec![ok("甲"), ok("乙")]);
        assert_eq!(r.summary().line, "已将 2 项还原到原位置");
        assert!(r.summary().detail.is_none());
        let bad = Outcome {
            name: "丙".into(),
            result: Err(OpError::new(FailKind::Conflict, "")),
        };
        let r = Report::new(OpKind::Restore, vec![bad]);
        assert_eq!(r.summary().line, "未能还原“丙”：原位置已有同名项目，未覆盖");
    }

    #[test]
    fn long_failure_list_is_truncated() {
        let outcomes = (0..12)
            .map(|i| Outcome {
                name: format!("f{i}"),
                result: Err(OpError::new(FailKind::InUse, "")),
            })
            .collect();
        let s = Report::new(OpKind::Permanent, outcomes).summary();
        assert_eq!(s.line, "12 项全部未能删除");
        assert!(s.detail.unwrap().contains("另有 4 项未列出"));
    }

    #[test]
    fn restore_conflict_missing_dir_and_permission() {
        let (work, b) = setup("restore-fail");
        let sub = work.join("子");
        fs::create_dir(&sub).unwrap();
        for n in ["冲突.txt", "无目录.txt", "无权限.txt"] {
            fs::write(sub.join(n), n).unwrap();
        }
        let paths: Vec<_> = ["冲突.txt", "无目录.txt", "无权限.txt"]
            .iter()
            .map(|n| sub.join(n))
            .collect();
        assert_eq!(run_delete(&*b, &paths, DeleteMode::Recycle).fail_count(), 0);
        // 冲突：原位置出现同名文件
        fs::write(sub.join("冲突.txt"), "新的").unwrap();
        // 权限：注入
        b.inject(
            sub.join("无权限.txt"),
            Inject::Restore(FailKind::AccessDenied),
        );
        let items = b.list_bin().unwrap();
        // 原目录消失：先恢复其中一项到别处不可能，改为只对“无目录”移走整个原目录
        let missing = items
            .iter()
            .find(|i| i.name == "无目录.txt")
            .unwrap()
            .clone();
        let mut moved = missing.clone();
        moved.original_dir = work.join("已删目录").display().to_string();
        let r = run_restore(&*b, &[items[0].clone(), moved, items[2].clone()]);
        let kinds: Vec<_> = r
            .outcomes
            .iter()
            .map(|o| o.result.as_ref().err().map(|e| e.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                Some(FailKind::Conflict),
                Some(FailKind::DestMissing),
                Some(FailKind::AccessDenied)
            ]
        );
        assert_eq!(b.bin_len(), 3, "失败项都仍在回收站");
        assert_eq!(
            fs::read_to_string(sub.join("冲突.txt")).unwrap(),
            "新的",
            "未覆盖"
        );
        // 解除条件后可重试成功
        b.clear_injections();
        fs::remove_file(sub.join("冲突.txt")).unwrap();
        let r = run_restore(&*b, &[items[0].clone(), items[2].clone()]);
        assert_eq!(r.fail_count(), 0);
        assert_eq!(b.bin_len(), 1);
    }

    #[test]
    fn permanent_delete_files_dirs_and_missing() {
        let (work, b) = setup("permanent");
        fs::create_dir_all(work.join("d").join("e")).unwrap();
        fs::write(work.join("d").join("e").join("x.txt"), b"x").unwrap();
        fs::write(work.join("f.txt"), b"x").unwrap();
        let paths = vec![work.join("d"), work.join("f.txt"), work.join("没有.txt")];
        let r = run_delete(&*b, &paths, DeleteMode::Permanent);
        assert_eq!((r.ok_count(), r.fail_count()), (2, 1));
        assert!(!work.join("d").exists() && !work.join("f.txt").exists());
        assert_eq!(b.bin_len(), 0, "永久删除不进回收站");
    }

    /// 目录联接只删联接本身，目标里的内容必须原样保留。
    #[test]
    fn permanent_delete_junction_keeps_target() {
        let (work, b) = setup("junction");
        let target = work.join("目标");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("保留.txt"), b"keep").unwrap();
        let link = work.join("联接");
        let out = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(out.status.success(), "创建目录联接失败");
        let r = run_delete(&*b, std::slice::from_ref(&link), DeleteMode::Permanent);
        assert_eq!(r.fail_count(), 0, "{:?}", r.summary().detail);
        assert!(!link.exists(), "联接应已删除");
        assert_eq!(
            fs::read(target.join("保留.txt")).unwrap(),
            b"keep",
            "联接目标里的内容不能被删除"
        );
    }

    #[test]
    fn parent_dirs_dedup() {
        let v = [
            PathBuf::from(r"C:\a\1"),
            PathBuf::from(r"C:\a\2"),
            PathBuf::from(r"C:\b\3"),
        ];
        let d = parent_dirs(v.iter().map(|p| p.as_path()));
        assert_eq!(d, [PathBuf::from(r"C:\a"), PathBuf::from(r"C:\b")]);
    }
}
