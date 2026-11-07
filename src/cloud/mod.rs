//! OneDrive／iCloud 云盘的“无隐式下载”浏览边界（Issue #14，依据 ADR 0004、研究 0003、`docs/cloud-browsing.md`）。
//!
//! 一句话规则：**浏览、搜索、缩略图、预览、目录监视、右键菜单弹出都不读取云文件的数据流**；只有用户明确的
//! 打开、复制／拖放、提供方菜单命令才允许让同步客户端下载原文件，并且界面要让用户看得出来。
//!
//! 本模块提供：
//! - [`CloudState`]：仅在线／仅在线但客户端不可用／本地可用／始终保留／不在云盘。状态用**云文件 API**
//!   （`CfGetSyncRootInfoByPath`、`CfGetPlaceholderInfo`）判定，而不是只看属性位——默认情况下已下载的云文件与普通文件
//!   属性完全一样（研究 0003 3.1）。云文件 API 只读元数据，不打开数据流，不触发下载。
//! - [`CloudProbe`]：读取同步根与占位信息的接缝。产品用 [`SystemCloud`]，测试与 harness 用 [`MockCloud`] 模拟提供方。
//! - [`open_local_only`]：读内容前的守卫（属性检查 → 打开 → 句柄上再检查），缩略图与预览共用，
//!   把“目录枚举之后文件被释放空间”的竞态窗口压到最小。
//! - [`roots`]：用系统同步根注册表发现 OneDrive／iCloud 云盘／iCloud 照片的本机目录（iCloud 常被用户移到别的盘）。
//! - [`count_online_only`]：复制／拖放前统计会触发下载的仅在线文件数，用于让用户感知。

mod mock;
pub mod roots;
mod sys;

pub use mock::MockCloud;
pub use sys::{SYSTEM, SystemCloud};

use std::fs::{File, Metadata};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const ATTR_DIRECTORY: u32 = 0x10;
pub const ATTR_OFFLINE: u32 = 0x1000;
pub const ATTR_RECALL_ON_OPEN: u32 = 0x4_0000;
pub const ATTR_PINNED: u32 = 0x8_0000;
pub const ATTR_UNPINNED: u32 = 0x10_0000;
pub const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;

/// 读取内容会让同步客户端下载的属性位。
const RECALL_MASK: u32 = ATTR_OFFLINE | ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS;

/// 属性里带“按需下载”标记：读取内容（对 `RECALL_ON_OPEN` 甚至只是打开）会触发下载。
pub fn is_recall(attrs: u32) -> bool {
    attrs & RECALL_MASK != 0
}

/// 一个条目在云盘里的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CloudState {
    /// 不在云盘里，或是云盘里尚未被提供方接管的普通文件。
    #[default]
    None,
    /// 仅在线：内容不在本机，打开／复制才会下载。
    OnlineOnly,
    /// 仅在线，且提供方当前不可用（同步客户端未运行、断网或出错），打开暂时无法下载。
    OnlineUnavailable,
    /// 本地可用：内容已在本机，提供方可在空间不足时释放。
    Local,
    /// 始终保留在本设备。
    AlwaysKeep,
}

impl CloudState {
    pub fn is_cloud(self) -> bool {
        self != CloudState::None
    }

    /// 仅在线（含客户端不可用）：读取内容会触发下载，或暂时下载不了。
    pub fn is_online_only(self) -> bool {
        matches!(self, CloudState::OnlineOnly | CloudState::OnlineUnavailable)
    }

    /// 稳定短代号（harness 与日志用）。
    pub fn code(self) -> &'static str {
        match self {
            CloudState::None => "none",
            CloudState::OnlineOnly => "online",
            CloudState::OnlineUnavailable => "online-unavailable",
            CloudState::Local => "local",
            CloudState::AlwaysKeep => "pinned",
        }
    }

    /// 无障碍描述与状态栏用的说明；`None` 为空串。
    pub fn text(self) -> &'static str {
        match self {
            CloudState::None => "",
            CloudState::OnlineOnly => "仅在线（未下载到本机）",
            CloudState::OnlineUnavailable => "仅在线，同步客户端当前不可用，暂时无法下载",
            CloudState::Local => "本地可用（已下载，空间不足时可被释放）",
            CloudState::AlwaysKeep => "始终保留在本设备",
        }
    }
}

/// 同步提供方的运行状态（`CF_SYNC_PROVIDER_STATUS` 归并）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderStatus {
    /// 正常连接（空闲、同步中都算）。
    Connected,
    /// 提供方没有连接：同步客户端没有运行（或已退出）。
    Disconnected,
    /// 已连接但丢失网络连接。
    ConnectivityLost,
    /// 提供方报告错误或已终止。
    Error,
}

impl ProviderStatus {
    /// 从 `CF_SYNC_PROVIDER_STATUS` 的原始值归并：0 为断开，0xC0000001／0xC0000002 为终止／错误，位 0x40 为网络丢失。
    pub fn from_raw(v: u32) -> ProviderStatus {
        match v {
            0 => ProviderStatus::Disconnected,
            0xC000_0001 | 0xC000_0002 => ProviderStatus::Error,
            v if v & 0x40 != 0 => ProviderStatus::ConnectivityLost,
            _ => ProviderStatus::Connected,
        }
    }

    pub fn is_ok(self) -> bool {
        self == ProviderStatus::Connected
    }

    pub fn code(self) -> &'static str {
        match self {
            ProviderStatus::Connected => "connected",
            ProviderStatus::Disconnected => "disconnected",
            ProviderStatus::ConnectivityLost => "connectivity-lost",
            ProviderStatus::Error => "error",
        }
    }
}

/// 目录所在的同步根。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncRoot {
    /// 提供方显示名（如 “OneDrive”“iCloudDrive”）。
    pub provider: String,
    pub status: ProviderStatus,
}

impl SyncRoot {
    /// 状态栏说明：提供方不可用时给出原因和影响；正常时为空。
    pub fn warning(&self) -> Option<String> {
        let p = provider_label(&self.provider);
        match self.status {
            ProviderStatus::Connected => None,
            ProviderStatus::Disconnected => Some(format!(
                "{p} 同步客户端未运行：已下载的文件照常可用，仅在线文件暂时无法下载。启动 {p} 后会自动恢复"
            )),
            ProviderStatus::ConnectivityLost => Some(format!(
                "{p} 已断开网络连接：已下载的文件照常可用，仅在线文件暂时无法下载"
            )),
            ProviderStatus::Error => Some(format!(
                "{p} 同步出现错误：仅在线文件可能无法下载，请在 {p} 客户端里查看原因"
            )),
        }
    }
}

/// 提供方名称的界面显示名。
pub fn provider_label(provider: &str) -> &str {
    let l = provider.to_ascii_lowercase();
    if l.contains("onedrive") {
        "OneDrive"
    } else if l.contains("icloud") {
        "iCloud"
    } else {
        provider
    }
}

/// `CF_PIN_STATE`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pin {
    Unspecified,
    Pinned,
    Unpinned,
    Excluded,
    Inherit,
}

impl Pin {
    pub fn from_raw(v: i32) -> Pin {
        match v {
            1 => Pin::Pinned,
            2 => Pin::Unpinned,
            3 => Pin::Excluded,
            4 => Pin::Inherit,
            _ => Pin::Unspecified,
        }
    }
}

/// 占位信息（只读，来自 `CfGetPlaceholderInfo`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placeholder {
    /// 本机磁盘上实际有内容的字节数；0 表示未下载。
    pub on_disk: u64,
    pub pin: Pin,
    /// 与云端一致。
    pub in_sync: bool,
}

/// 读取同步根与占位信息的接缝。实现必须只读元数据，**不得打开数据流**。
pub trait CloudProbe: Send + Sync {
    /// `dir` 所在的同步根；不在任何同步根内返回 `None`。一次一个目录，开销小。
    fn sync_root(&self, dir: &Path) -> Option<SyncRoot>;
    /// `path` 的占位信息；不是云占位文件（或读取失败）返回 `None`。
    fn placeholder(&self, path: &Path) -> Option<Placeholder>;
}

/// 判定状态（纯函数，便于穷举测试）。
///
/// - 有按需下载属性 → 仅在线（目录则表示未填充）；客户端不可用时升级为 [`CloudState::OnlineUnavailable`]。
/// - 否则：属性带“始终保留”→ 始终保留；
/// - 否则只有在同步根内才用占位信息区分：固定 → 始终保留；是占位文件 → 本地可用；不是占位文件 → 普通。
/// - 目录不调用占位信息：本地目录只有被固定时才标记，否则不标记。
pub fn classify(
    attrs: u32,
    is_dir: bool,
    in_root: bool,
    provider_ok: bool,
    ph: impl FnOnce() -> Option<Placeholder>,
) -> CloudState {
    if is_recall(attrs) {
        return if provider_ok {
            CloudState::OnlineOnly
        } else {
            CloudState::OnlineUnavailable
        };
    }
    if attrs & ATTR_PINNED != 0 && attrs & ATTR_UNPINNED == 0 {
        // 同时带固定与取消固定位的是 desktop.ini 之类的特殊文件，不归入“始终保留”
        return CloudState::AlwaysKeep;
    }
    if !in_root || is_dir {
        return CloudState::None;
    }
    match ph() {
        Some(p) if p.pin == Pin::Pinned => CloudState::AlwaysKeep,
        Some(_) => CloudState::Local,
        None => CloudState::None,
    }
}

/// 一个目录枚举时使用的云上下文：每个目录只问一次同步根。
pub struct DirCloud<'a> {
    probe: &'a dyn CloudProbe,
    dir: PathBuf,
    root: Option<SyncRoot>,
}

impl<'a> DirCloud<'a> {
    pub fn new(probe: &'a dyn CloudProbe, dir: &Path) -> DirCloud<'a> {
        DirCloud {
            probe,
            dir: dir.to_path_buf(),
            root: probe.sync_root(dir),
        }
    }

    pub fn root(&self) -> Option<&SyncRoot> {
        self.root.as_ref()
    }

    pub fn into_root(self) -> Option<SyncRoot> {
        self.root
    }

    /// 目录内一个条目的状态。
    pub fn state(&self, name: &std::ffi::OsStr, attrs: u32, is_dir: bool) -> CloudState {
        let in_root = self.root.is_some();
        let ok = self.root.as_ref().is_none_or(|r| r.status.is_ok());
        classify(attrs, is_dir, in_root, ok, || {
            self.probe.placeholder(&self.dir.join(name))
        })
    }
}

/// 为什么拒绝读取内容。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guard {
    /// 仅在线的云占位文件：读取会触发下载。
    Cloud,
    /// 不是普通文件（目录等）。
    NotFile,
    /// 其他读取错误（不存在、权限……）。
    Io,
}

/// 读内容前的守卫：只有**本地已有内容**的文件才会被打开。
///
/// 1. 用只读属性的元数据检查按需下载位（`std::fs::metadata` 以 `FILE_READ_ATTRIBUTES` 打开，不触发下载）；
/// 2. 通过检查才以读权限打开（对带 `RECALL_ON_OPEN` 的文件，打开本身就会触发下载，所以必须先查）；
/// 3. 在已打开的句柄上再查一次：目录枚举与读取之间若文件被“释放空间”，在读取（才会真正下载）之前发现。
///
/// 返回打开的文件与首次检查时的元数据（大小、修改时间用于缓存身份）。第 3 步与随后的读取之间仍有微秒级窗口，
/// 这是用户态无法消除的（提供方可在任意时刻释放），见 `docs/cloud-browsing.md`。
pub fn open_local_only(path: &Path) -> Result<(File, Metadata), Guard> {
    let meta = std::fs::metadata(path).map_err(|_| Guard::Io)?;
    if is_recall(meta.file_attributes()) {
        return Err(Guard::Cloud);
    }
    if !meta.is_file() {
        return Err(Guard::NotFile);
    }
    let f = File::open(path).map_err(|_| Guard::Io)?;
    let again = f.metadata().map_err(|_| Guard::Io)?;
    if is_recall(again.file_attributes()) {
        return Err(Guard::Cloud);
    }
    Ok((f, meta))
}

/// 统计 `paths`（含文件夹内容）里仅在线文件的个数，用于复制／拖放前提示“将触发下载”。
///
/// 只读目录枚举自带的属性，不打开任何文件；最多枚举 `max_entries` 个条目、最多花 `budget`，
/// 超限则 `truncated` 为真（个数是下限）。
pub fn count_online_only(paths: &[PathBuf], max_entries: usize, budget: Duration) -> OnlineCount {
    let start = Instant::now();
    let mut out = OnlineCount::default();
    let mut seen = 0usize;
    let mut stack: Vec<PathBuf> = Vec::new();
    for p in paths {
        let Ok(md) = std::fs::symlink_metadata(p) else {
            continue;
        };
        let a = md.file_attributes();
        if md.is_dir() {
            if is_recall(a) {
                // 未填充的云文件夹：里面的文件都是仅在线，个数未知（不展开，展开会触发网络枚举）
                out.unpopulated_dirs += 1;
            } else {
                stack.push(p.clone());
            }
        } else if is_recall(a) {
            out.files += 1;
            out.bytes += md.len();
        }
    }
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for de in rd.flatten() {
            seen += 1;
            if seen > max_entries || start.elapsed() > budget {
                out.truncated = true;
                return out;
            }
            let Ok(md) = de.metadata() else { continue };
            let a = md.file_attributes();
            if md.is_dir() {
                if is_recall(a) {
                    out.unpopulated_dirs += 1;
                } else if a & 0x400 == 0 {
                    // 不跟随符号链接／联接
                    stack.push(de.path());
                }
            } else if is_recall(a) {
                out.files += 1;
                out.bytes += md.len();
            }
        }
    }
    out
}

/// [`count_online_only`] 的结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OnlineCount {
    pub files: u64,
    pub bytes: u64,
    /// 未填充（仅在线）的云文件夹个数：其内容在复制时才会被枚举并下载。
    pub unpopulated_dirs: u64,
    /// 枚举被上限截断：`files` 是下限。
    pub truncated: bool,
}

impl OnlineCount {
    pub fn any(&self) -> bool {
        self.files > 0 || self.unpopulated_dirs > 0
    }

    /// 给用户看的提示（不含动词前缀）：“其中 3 个仅在线文件（共 1.2 MB）会由云盘下载”。
    pub fn hint(&self) -> String {
        let mut parts = Vec::new();
        if self.files > 0 {
            parts.push(format!(
                "{}{} 个仅在线文件（约 {}）",
                if self.truncated { "至少 " } else { "" },
                self.files,
                crate::browse::format::size_text(self.bytes)
            ));
        }
        if self.unpopulated_dirs > 0 {
            parts.push(format!("{} 个仅在线文件夹里的内容", self.unpopulated_dirs));
        }
        format!("包含 {}，将通过云盘客户端下载到本机", parts.join("和"))
    }

    /// 操作完成后的说明：其中几个仅在线文件已由云盘客户端下载到了本机。
    pub fn done_text(&self) -> String {
        let mut parts = Vec::new();
        if self.files > 0 {
            parts.push(format!(
                "{}{} 个仅在线文件",
                if self.truncated { "至少 " } else { "" },
                self.files
            ));
        }
        if self.unpopulated_dirs > 0 {
            parts.push(format!("{} 个仅在线文件夹里的内容", self.unpopulated_dirs));
        }
        format!("其中 {}已由云盘客户端下载到本机", parts.join("和"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ph(pin: Pin) -> Placeholder {
        Placeholder {
            on_disk: 10,
            pin,
            in_sync: true,
        }
    }

    #[test]
    fn recall_attributes_mean_online_only() {
        for a in [
            ATTR_OFFLINE,
            ATTR_RECALL_ON_OPEN,
            ATTR_RECALL_ON_DATA_ACCESS,
        ] {
            assert_eq!(
                classify(a | 0x20, false, true, true, || panic!("不应读占位信息")),
                CloudState::OnlineOnly
            );
        }
        // 提供方不可用时仍是仅在线，但标出无法下载
        assert_eq!(
            classify(ATTR_RECALL_ON_DATA_ACCESS, false, true, false, || None),
            CloudState::OnlineUnavailable
        );
        // 不在同步根里的脱机文件（如磁带库、自带 OFFLINE 的文件）也不读内容；提供方视为正常
        assert_eq!(
            classify(ATTR_OFFLINE, false, false, true, || None),
            CloudState::OnlineOnly
        );
        // 未填充的云文件夹
        assert_eq!(
            classify(
                ATTR_DIRECTORY | ATTR_RECALL_ON_DATA_ACCESS,
                true,
                true,
                true,
                || None
            ),
            CloudState::OnlineOnly
        );
    }

    #[test]
    fn pinned_and_local() {
        // 属性带固定位：不必再问云文件 API
        assert_eq!(
            classify(0x20 | ATTR_PINNED, false, true, true, || panic!(
                "不应读占位信息"
            )),
            CloudState::AlwaysKeep
        );
        // 固定+取消固定同时置位：特殊文件，不算始终保留
        assert_eq!(
            classify(
                0x20 | ATTR_PINNED | ATTR_UNPINNED,
                false,
                false,
                true,
                || None
            ),
            CloudState::None
        );
        // 在同步根内，已下载：靠占位信息区分
        assert_eq!(
            classify(0x20, false, true, true, || Some(ph(Pin::Unspecified))),
            CloudState::Local
        );
        assert_eq!(
            classify(0x20, false, true, true, || Some(ph(Pin::Unpinned))),
            CloudState::Local
        );
        assert_eq!(
            classify(0x20, false, true, true, || Some(ph(Pin::Pinned))),
            CloudState::AlwaysKeep
        );
        // 在同步根内但不是占位文件（刚新建、尚未被接管）：普通
        assert_eq!(classify(0x20, false, true, true, || None), CloudState::None);
        // 不在同步根：普通文件，且不会去读占位信息
        assert_eq!(
            classify(0x20, false, false, true, || panic!("不应读占位信息")),
            CloudState::None
        );
        // 目录：不读占位信息，已填充的目录不标记
        assert_eq!(
            classify(ATTR_DIRECTORY, true, true, true, || panic!(
                "不应读占位信息"
            )),
            CloudState::None
        );
    }

    #[test]
    fn provider_status_mapping() {
        assert_eq!(ProviderStatus::from_raw(0), ProviderStatus::Disconnected);
        assert_eq!(ProviderStatus::from_raw(1), ProviderStatus::Connected);
        assert_eq!(ProviderStatus::from_raw(0x10), ProviderStatus::Connected);
        assert_eq!(
            ProviderStatus::from_raw(0x40),
            ProviderStatus::ConnectivityLost
        );
        assert_eq!(ProviderStatus::from_raw(0xC000_0001), ProviderStatus::Error);
        assert_eq!(ProviderStatus::from_raw(0xC000_0002), ProviderStatus::Error);
    }

    #[test]
    fn warnings_only_when_provider_not_ok() {
        let mk = |s| SyncRoot {
            provider: "OneDrive".into(),
            status: s,
        };
        assert!(mk(ProviderStatus::Connected).warning().is_none());
        for s in [
            ProviderStatus::Disconnected,
            ProviderStatus::ConnectivityLost,
            ProviderStatus::Error,
        ] {
            let w = mk(s).warning().unwrap();
            assert!(w.contains("OneDrive") && w.contains("无法下载"), "{w}");
        }
        assert_eq!(provider_label("iCloudDrive"), "iCloud");
    }

    #[test]
    fn dir_cloud_asks_root_once_and_placeholder_only_for_candidates() {
        let m = MockCloud::default();
        m.add_root("C:\\云", "OneDrive", ProviderStatus::Connected);
        m.set_placeholder("C:\\云\\本地.txt", ph(Pin::Unspecified));
        m.set_placeholder("C:\\云\\固定.txt", ph(Pin::Pinned));
        let d = DirCloud::new(&m, Path::new("C:\\云"));
        assert_eq!(m.root_calls(), 1);
        let n = |s: &str| std::ffi::OsString::from(s);
        assert_eq!(d.state(&n("本地.txt"), 0x20, false), CloudState::Local);
        assert_eq!(d.state(&n("固定.txt"), 0x20, false), CloudState::AlwaysKeep);
        assert_eq!(d.state(&n("普通.txt"), 0x20, false), CloudState::None);
        assert_eq!(
            d.state(&n("在线.txt"), 0x20 | ATTR_RECALL_ON_DATA_ACCESS, false),
            CloudState::OnlineOnly
        );
        // 仅在线文件不读占位信息（只有三个候选读了）
        assert_eq!(m.placeholder_calls(), 3);
        assert_eq!(m.root_calls(), 1);
        // 不在任何同步根的目录：不读占位信息
        let other = DirCloud::new(&m, Path::new("D:\\普通"));
        assert!(other.root().is_none());
        assert_eq!(other.state(&n("a.txt"), 0x20, false), CloudState::None);
        assert_eq!(m.placeholder_calls(), 3);
    }

    #[test]
    fn open_local_only_refuses_recall_files() {
        let d = std::env::temp_dir()
            .join("sfm-unit")
            .join(format!("cloud-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let plain = d.join("普通.txt");
        std::fs::write(&plain, b"abc").unwrap();
        let (mut f, md) = open_local_only(&plain).unwrap();
        assert_eq!(md.len(), 3);
        let mut s = String::new();
        std::io::Read::read_to_string(&mut f, &mut s).unwrap();
        assert_eq!(s, "abc");
        // 脱机属性：模拟仅在线占位文件，守卫拒绝打开
        let off = d.join("在线.txt");
        std::fs::write(&off, b"abc").unwrap();
        assert!(
            std::process::Command::new("attrib")
                .arg("+O")
                .arg(&off)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(open_local_only(&off).err(), Some(Guard::Cloud));
        assert_eq!(
            open_local_only(&d).err(),
            Some(Guard::NotFile),
            "目录不是文件"
        );
        assert_eq!(open_local_only(&d.join("不存在")).err(), Some(Guard::Io));
        // 统计：一个仅在线文件在子目录里
        std::fs::create_dir(d.join("子")).unwrap();
        std::fs::copy(&off, d.join("子").join("在线2.txt")).ok();
        let c = count_online_only(std::slice::from_ref(&d), 1000, Duration::from_secs(5));
        assert!(c.files >= 1 && c.any() && !c.truncated, "{c:?}");
        assert!(c.hint().contains("仅在线文件"));
        let _ = std::process::Command::new("attrib")
            .arg("-O")
            .arg(&off)
            .status();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn count_online_only_truncates() {
        let d = std::env::temp_dir()
            .join("sfm-unit")
            .join(format!("cloud-count-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..20 {
            std::fs::write(d.join(format!("f{i}.txt")), b"x").unwrap();
        }
        let c = count_online_only(std::slice::from_ref(&d), 5, Duration::from_secs(5));
        assert!(c.truncated);
        let c = count_online_only(std::slice::from_ref(&d), 100, Duration::from_secs(5));
        assert!(!c.truncated && !c.any());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn hint_text() {
        let c = OnlineCount {
            files: 3,
            bytes: 2048,
            unpopulated_dirs: 1,
            truncated: true,
        };
        let h = c.hint();
        assert!(
            h.contains("至少 3 个仅在线文件") && h.contains("1 个仅在线文件夹"),
            "{h}"
        );
        assert!(!OnlineCount::default().any());
    }
}
