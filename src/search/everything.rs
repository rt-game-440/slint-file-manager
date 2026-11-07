//! 可选搜索增强：Everything（voidtools）的窗口消息 IPC（`WM_COPYDATA`）。依据 ADR 0007。
//!
//! 为什么走 IPC 而不是 SDK DLL 或 es.exe：本应用不随附也不下载任何第三方二进制；
//! Everything 已运行时，直接给它的隐藏窗口发查询即可。未安装、未运行、索引未载入、查询失败都能判定，见 [`Availability`]。
//!
//! 重要：本模块的协议常量与结构布局来自 Everything SDK（`everything_ipc.h`）的公开说明，
//! 本机**没有安装 Everything**（只读核查，见 docs/research/0011-search-backend.md），
//! 所以真实 IPC 传输（[`EverythingIpc`]）**尚未与真实的 Everything 联调**；
//! 编码／解码、查询串构造、回退逻辑都用字节级单元测试与 [`MockEnhancer`] 覆盖，联调列入人工验收清单。
//!
//! 与基本搜索的语义一致：增强返回的结果交回 [`engine`](super::engine) 后，再按本应用自己的 [`Query`] 规则
//! 二次过滤、按范围裁剪、剔除磁盘上已不存在的陈旧索引项，所以两种后端看到的名称／扩展名／路径语义相同。

use super::{Cancel, Hit, Query, Scope};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 增强后端当前状态。必须能区分，不能一律当作“无结果”。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    /// 找不到 Everything 窗口，也没有发现安装痕迹
    NotInstalled,
    /// 已安装但没有运行（或没有以可被访问的方式运行）
    NotRunning,
    /// 正在运行，但数据库尚未载入完成（索引未就绪）
    IndexLoading,
    /// 可查询，附带版本号文字
    Ready(String),
    /// 找到窗口但通信失败（无响应、被权限隔离等）
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnhancerError {
    /// 查询开始前已不可用
    Unavailable(Availability),
    /// 查询过程中失败（超时、回复格式异常、通信错误）
    Failed(String),
    /// 被取消
    Cancelled,
}

/// 可选增强后端。实现必须线程安全，`query` 在搜索线程中阻塞调用。
pub trait Enhancer: Send + Sync {
    fn name(&self) -> &'static str;
    fn probe(&self) -> Availability;
    /// 返回匹配的结果（粗筛，调用方会按本应用规则二次过滤）。至多 `limit` 条。
    fn query(
        &self,
        q: &Query,
        scope: &Scope,
        limit: usize,
        cancel: &Cancel,
    ) -> Result<Vec<Hit>, EnhancerError>;
}

// ---------- 协议（纯函数，可测） ----------

/// `EVERYTHING_IPC_COPYDATAQUERYW`
pub const IPC_COPYDATA_QUERYW: usize = 2;
/// `EVERYTHING_IPC_MATCHPATH`：查询词同时匹配完整路径
pub const IPC_MATCHPATH: u32 = 0x4;
/// 本应用定义的“查询完成”回复消息编号（写进查询里，Everything 原样放进回复的 dwData）
pub const REPLY_ID: u32 = 0x5346_4D31; // "SFM1"
const ITEM_FOLDER: u32 = 1;
const ITEM_DRIVE: u32 = 2;

/// 构造发给 Everything 的粗筛查询串与搜索标志。
///
/// 粗筛必须是本应用语义的**超集**（再由本应用二次过滤）：
/// - 名称／通配词原样交给 Everything（它按子串、不分大小写匹配）；
/// - 扩展名写成 `ext:a;b`；路径词原样放入，并打开“匹配完整路径”标志；
/// - 目录范围写成 `"<目录>\"` 前缀词，同样依赖“匹配完整路径”。
pub fn build_query(q: &Query, scope: &Scope) -> (String, u32) {
    use super::query::Term;
    let quote = |s: &str| {
        if s.contains(' ') {
            format!("\"{s}\"")
        } else {
            s.to_string()
        }
    };
    let mut parts: Vec<String> = Vec::new();
    let mut flags = 0u32;
    if let Scope::Dir(d) = scope {
        let mut p = d.to_string_lossy().into_owned();
        if !p.ends_with('\\') {
            p.push('\\');
        }
        parts.push(format!("\"{p}\""));
        flags |= IPC_MATCHPATH;
    }
    for t in &q.terms {
        match t {
            Term::Name(s) => parts.push(quote(s)),
            Term::Wild(c) => parts.push(quote(&c.iter().collect::<String>())),
            Term::Ext(list) => parts.push(format!("ext:{}", list.join(";"))),
            Term::Path(s) => {
                parts.push(quote(s));
                flags |= IPC_MATCHPATH;
            }
        }
    }
    (parts.join(" "), flags)
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(|u| u.to_le_bytes())
        .collect()
}

/// `EVERYTHING_IPC_QUERYW`：reply_hwnd、reply_copydata_message、search_flags、offset、max_results 各 4 字节，后接 UTF-16 查询串（含结尾 NUL）。
pub fn encode_query(reply_hwnd: u32, text: &str, flags: u32, max_results: u32) -> Vec<u8> {
    let mut v = Vec::new();
    for x in [reply_hwnd, REPLY_ID, flags, 0, max_results] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.extend(utf16z(text));
    v
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn str_at(b: &[u8], off: usize) -> Option<String> {
    let mut units = Vec::new();
    let mut i = off;
    loop {
        let s = b.get(i..i + 2)?;
        let u = u16::from_le_bytes([s[0], s[1]]);
        if u == 0 {
            break;
        }
        units.push(u);
        i += 2;
        if units.len() > 32_768 {
            return None;
        }
    }
    Some(String::from_utf16_lossy(&units))
}

/// 解码 `EVERYTHING_IPC_LISTW`：7 个 DWORD 的头（totfolders、totfiles、totitems、numfolders、numfiles、numitems、offset），
/// 后接 `numitems` 个 {flags, filename_offset, path_offset}（偏移相对列表起点，指向 NUL 结尾的 UTF-16）。
pub fn decode_list(b: &[u8]) -> Result<Vec<Hit>, String> {
    let bad = || "Everything 回复格式异常".to_string();
    let numitems = u32_at(b, 20).ok_or_else(bad)? as usize;
    if numitems > 5_000_000 {
        return Err(bad());
    }
    let mut out = Vec::with_capacity(numitems.min(super::MAX_HITS));
    for i in 0..numitems {
        let at = 28 + i * 12;
        let flags = u32_at(b, at).ok_or_else(bad)?;
        let name = str_at(b, u32_at(b, at + 4).ok_or_else(bad)? as usize).ok_or_else(bad)?;
        let dir = str_at(b, u32_at(b, at + 8).ok_or_else(bad)? as usize).ok_or_else(bad)?;
        let is_drive = flags & ITEM_DRIVE != 0;
        let path = if is_drive {
            PathBuf::from(format!("{}\\", name.trim_end_matches('\\')))
        } else if dir.is_empty() {
            PathBuf::from(&name)
        } else {
            PathBuf::from(dir).join(&name)
        };
        out.push(Hit {
            path,
            is_dir: flags & (ITEM_FOLDER | ITEM_DRIVE) != 0,
        });
    }
    Ok(out)
}

// ---------- 真实 IPC 传输 ----------

/// 通过 Everything 的隐藏窗口查询（未与真实 Everything 联调，见模块说明）。
pub struct EverythingIpc;

impl EverythingIpc {
    pub fn new() -> EverythingIpc {
        EverythingIpc
    }
}

impl Default for EverythingIpc {
    fn default() -> Self {
        EverythingIpc::new()
    }
}

impl Enhancer for EverythingIpc {
    fn name(&self) -> &'static str {
        "Everything"
    }

    fn probe(&self) -> Availability {
        ipc::probe()
    }

    fn query(
        &self,
        q: &Query,
        scope: &Scope,
        limit: usize,
        cancel: &Cancel,
    ) -> Result<Vec<Hit>, EnhancerError> {
        if cancel.is_cancelled() {
            return Err(EnhancerError::Cancelled);
        }
        match self.probe() {
            Availability::Ready(_) => {}
            other => return Err(EnhancerError::Unavailable(other)),
        }
        let (text, flags) = build_query(q, scope);
        let bytes = ipc::query(&text, flags, limit as u32)?;
        if cancel.is_cancelled() {
            return Err(EnhancerError::Cancelled);
        }
        decode_list(&bytes).map_err(EnhancerError::Failed)
    }
}

mod ipc {
    use super::{Availability, EnhancerError, IPC_COPYDATA_QUERYW, REPLY_ID, encode_query};
    use std::cell::RefCell;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowW,
        MSGFLT_ALLOW, RegisterClassW, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_COPYDATA, WM_USER,
        WNDCLASSW,
    };

    const CLASSES: [&str; 2] = [
        "EVERYTHING_TASKBAR_NOTIFICATION",
        "EVERYTHING_TASKBAR_NOTIFICATION_(1.5a)",
    ];
    const IPC_IS_DB_LOADED: usize = 401;
    const QUERY_TIMEOUT_MS: u32 = 20_000;

    thread_local! {
        static REPLY: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    }

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }

    fn find() -> Option<HWND> {
        for c in CLASSES {
            let w = wide(c);
            // SAFETY: 以 NUL 结尾的类名；第二个参数为空表示不按标题过滤。
            let h = unsafe { FindWindowW(w.as_ptr(), std::ptr::null()) };
            if !h.is_null() {
                return Some(h);
            }
        }
        None
    }

    fn installed_hint() -> bool {
        let mut dirs: Vec<std::path::PathBuf> = Vec::new();
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(p) = std::env::var_os(var) {
                dirs.push(std::path::PathBuf::from(&p).join("Everything"));
                dirs.push(std::path::PathBuf::from(&p).join("Everything 1.5a"));
            }
        }
        if let Some(p) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(
                std::path::PathBuf::from(p)
                    .join("Programs")
                    .join("Everything"),
            );
        }
        dirs.iter().any(|d| d.join("Everything.exe").is_file())
    }

    pub fn probe() -> Availability {
        let Some(h) = find() else {
            return if installed_hint() {
                Availability::NotRunning
            } else {
                Availability::NotInstalled
            };
        };
        let mut res = 0usize;
        // SAFETY: 向 Everything 的窗口同步发送无指针参数的 IPC 消息，带超时。
        let ok = unsafe {
            SendMessageTimeoutW(
                h,
                WM_USER,
                IPC_IS_DB_LOADED,
                0,
                SMTO_ABORTIFHUNG,
                2000,
                &mut res,
            )
        };
        if ok == 0 {
            return Availability::Failed("Everything 窗口无响应".into());
        }
        if res == 0 {
            return Availability::IndexLoading;
        }
        let ver = |n: usize| {
            let mut r = 0usize;
            // SAFETY: 同上。
            unsafe { SendMessageTimeoutW(h, WM_USER, n, 0, SMTO_ABORTIFHUNG, 2000, &mut r) };
            r
        };
        Availability::Ready(format!("{}.{}.{}.{}", ver(0), ver(1), ver(2), ver(3)))
    }

    unsafe extern "system" fn reply_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        if m == WM_COPYDATA {
            // SAFETY: WM_COPYDATA 的 lParam 指向系统保证有效的 COPYDATASTRUCT，仅在本次消息处理期间有效，这里立即复制。
            let cds = unsafe { &*(l as *const COPYDATASTRUCT) };
            if cds.dwData == REPLY_ID as usize && !cds.lpData.is_null() {
                let bytes = unsafe {
                    std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize)
                }
                .to_vec();
                REPLY.with(|r| *r.borrow_mut() = Some(bytes));
                return 1;
            }
        }
        // SAFETY: 默认窗口过程。
        unsafe { DefWindowProcW(h, m, w, l) }
    }

    pub fn query(text: &str, flags: u32, max_results: u32) -> Result<Vec<u8>, EnhancerError> {
        let target = find().ok_or(EnhancerError::Unavailable(Availability::NotRunning))?;
        let class = wide("SfmEverythingReply");
        // SAFETY: 注册窗口类与创建仅用于接收回复的隐藏窗口；类名重复注册会失败但不影响使用；句柄在返回前销毁。
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(reply_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return Err(EnhancerError::Failed("无法创建接收回复的窗口".into()));
            }
            // Everything 以更高权限运行时，回复默认会被界面特权隔离拦截
            ChangeWindowMessageFilterEx(hwnd, WM_COPYDATA, MSGFLT_ALLOW, std::ptr::null_mut());
            REPLY.with(|r| *r.borrow_mut() = None);
            let mut buf = encode_query(hwnd as usize as u32, text, flags, max_results);
            let cds = COPYDATASTRUCT {
                dwData: IPC_COPYDATA_QUERYW,
                cbData: buf.len() as u32,
                lpData: buf.as_mut_ptr().cast(),
            };
            let mut res = 0usize;
            let ok = SendMessageTimeoutW(
                target,
                WM_COPYDATA,
                hwnd as WPARAM,
                &cds as *const COPYDATASTRUCT as LPARAM,
                SMTO_ABORTIFHUNG,
                QUERY_TIMEOUT_MS,
                &mut res,
            );
            let reply = REPLY.with(|r| r.borrow_mut().take());
            DestroyWindow(hwnd);
            if ok == 0 {
                return Err(EnhancerError::Failed("Everything 查询超时或无响应".into()));
            }
            reply.ok_or(EnhancerError::Failed("Everything 没有返回结果".into()))
        }
    }
}

// ---------- 测试用的模拟后端 ----------

/// 可编程的模拟增强后端：状态、结果、故障都可注入。harness 与单元测试使用。
pub struct MockEnhancer {
    pub availability: Mutex<Availability>,
    pub hits: Mutex<Vec<Hit>>,
    /// 设置后，下一次（及之后的）`query` 返回该错误
    pub fail_with: Mutex<Option<EnhancerError>>,
    pub queries: AtomicUsize,
    /// 记录最近一次收到的粗筛查询串
    pub last_query: Mutex<String>,
}

impl MockEnhancer {
    pub fn new(a: Availability, hits: Vec<Hit>) -> MockEnhancer {
        MockEnhancer {
            availability: Mutex::new(a),
            hits: Mutex::new(hits),
            fail_with: Mutex::new(None),
            queries: AtomicUsize::new(0),
            last_query: Mutex::new(String::new()),
        }
    }
    pub fn set_availability(&self, a: Availability) {
        *self.availability.lock().unwrap() = a;
    }
    pub fn fail(&self, e: Option<EnhancerError>) {
        *self.fail_with.lock().unwrap() = e;
    }
    pub fn query_count(&self) -> usize {
        self.queries.load(Ordering::SeqCst)
    }
}

impl Enhancer for MockEnhancer {
    fn name(&self) -> &'static str {
        "Everything（模拟）"
    }
    fn probe(&self) -> Availability {
        self.availability.lock().unwrap().clone()
    }
    fn query(
        &self,
        q: &Query,
        scope: &Scope,
        limit: usize,
        cancel: &Cancel,
    ) -> Result<Vec<Hit>, EnhancerError> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        *self.last_query.lock().unwrap() = build_query(q, scope).0;
        if cancel.is_cancelled() {
            return Err(EnhancerError::Cancelled);
        }
        if let Some(e) = self.fail_with.lock().unwrap().clone() {
            return Err(e);
        }
        match self.probe() {
            Availability::Ready(_) => {}
            other => return Err(EnhancerError::Unavailable(other)),
        }
        // 粗筛：模拟 Everything 对名称子串的行为（不做本应用的二次过滤，故意返回超集）
        let hits: Vec<Hit> = self
            .hits
            .lock()
            .unwrap()
            .iter()
            .take(limit)
            .cloned()
            .collect();
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u8> {
        utf16z(s)
    }

    /// 按协议拼出一个回复：头 28 字节 + 条目 + 字符串区。
    fn build_reply(items: &[(u32, &str, &str)]) -> Vec<u8> {
        let n = items.len();
        let mut strings: Vec<u8> = Vec::new();
        let base = 28 + 12 * n;
        let mut table: Vec<(u32, u32, u32)> = Vec::new();
        for (flags, name, path) in items {
            let no = base + strings.len();
            strings.extend(utf16(name));
            let po = base + strings.len();
            strings.extend(utf16(path));
            table.push((*flags, no as u32, po as u32));
        }
        let mut v = Vec::new();
        for x in [0u32, 0, n as u32, 0, 0, n as u32, 0] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        for (f, a, b) in table {
            for x in [f, a, b] {
                v.extend_from_slice(&x.to_le_bytes());
            }
        }
        v.extend(strings);
        v
    }

    #[test]
    fn query_encoding_layout() {
        let b = encode_query(0x1234, "ab", IPC_MATCHPATH, 100);
        assert_eq!(u32_at(&b, 0), Some(0x1234));
        assert_eq!(u32_at(&b, 4), Some(REPLY_ID));
        assert_eq!(u32_at(&b, 8), Some(4));
        assert_eq!(u32_at(&b, 12), Some(0));
        assert_eq!(u32_at(&b, 16), Some(100));
        assert_eq!(&b[20..], &[b'a', 0, b'b', 0, 0, 0]);
    }

    #[test]
    fn list_decoding_files_folders_and_drives() {
        let r = build_reply(&[
            (0, "报告.txt", "C:\\用户\\文档"),
            (1, "照片", "D:\\"),
            (3, "E:", ""),
        ]);
        let hits = decode_list(&r).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].path, PathBuf::from("C:\\用户\\文档\\报告.txt"));
        assert!(!hits[0].is_dir);
        assert_eq!(hits[1].path, PathBuf::from("D:\\照片"));
        assert!(hits[1].is_dir);
        assert_eq!(hits[2].path, PathBuf::from("E:\\"));
        assert!(hits[2].is_dir);
    }

    #[test]
    fn truncated_or_garbage_reply_is_an_error_not_empty() {
        let r = build_reply(&[(0, "a.txt", "C:\\")]);
        assert!(decode_list(&r[..30]).is_err());
        assert!(decode_list(&r[..r.len() - 4]).is_err(), "字符串缺结尾 NUL");
        assert!(decode_list(&[1, 2, 3]).is_err());
        assert!(
            decode_list(&build_reply(&[])).unwrap().is_empty(),
            "合法的空回复才是无结果"
        );
    }

    #[test]
    fn coarse_query_building() {
        let q = Query::parse("报告 .pdf path:项目 \"a b\"");
        let (s, f) = build_query(&q, &Scope::AllDisks(vec![]));
        assert_eq!(s, "报告 ext:pdf 项目 \"a b\"");
        assert_eq!(f, IPC_MATCHPATH);
        let (s, f) = build_query(
            &Query::parse("x"),
            &Scope::Dir(PathBuf::from("C:\\我的 文档")),
        );
        assert_eq!(s, "\"C:\\我的 文档\\\" x");
        assert_eq!(f, IPC_MATCHPATH);
        let (s, f) = build_query(&Query::parse("x"), &Scope::AllDisks(vec![]));
        assert_eq!((s.as_str(), f), ("x", 0));
    }

    #[test]
    fn real_probe_never_panics_and_is_classified() {
        // 本机未必装有 Everything：只要求返回可判定的状态
        let a = EverythingIpc::new().probe();
        assert!(matches!(
            a,
            Availability::NotInstalled
                | Availability::NotRunning
                | Availability::IndexLoading
                | Availability::Ready(_)
                | Availability::Failed(_)
        ));
    }
}
