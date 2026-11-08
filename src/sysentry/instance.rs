//! 单实例与请求转发（Issue #17）。
//!
//! 每个数据目录一个实例：命名管道 `\\.\pipe\slint-file-manager-<会话号>-<数据目录哈希>`。
//! 不同数据目录（测试用的临时目录）是不同的实例，互不转发。
//!
//! - **主实例**用 `FILE_FLAG_FIRST_PIPE_INSTANCE` 原子地占用管道名（[`claim`]），之后在后台线程接受连接（[`Listener::serve`]）。
//!   每个连接读一行 JSON 请求，交给回调（界面线程处理并答复），写回一行 JSON 答复后断开。拒绝远程客户端。
//! - **第二个进程**占用失败即知道已有主实例，把请求转发过去（[`forward`]）并退出。连接前允许主实例把窗口
//!   切到前台（`AllowSetForegroundWindow`）。整个转发有超时：主实例无响应（卡死、正在退出）时返回
//!   [`Outcome::Unresponsive`]，调用方改为自己正常启动，不会让用户的双击石沉大海。
//!
//! 协议只有两种请求（[`Request`]），不传任何可执行的内容；未知或格式错误的请求被拒绝。

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::request::{Reply, Request};

/// 转发的默认超时（连接、写入、等待答复合计）。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(5000);
/// 只用于测试：覆盖转发超时（毫秒）。
pub const TIMEOUT_ENV: &str = "SFM_INSTANCE_TIMEOUT_MS";
/// 单行消息上限。
const MAX_LINE: usize = 64 * 1024;

/// 转发超时：[`TIMEOUT_ENV`] 或默认值。
pub fn timeout() -> Duration {
    std::env::var(TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&ms| (100..=60_000).contains(&ms))
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TIMEOUT)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// FNV-1a 64 位。
fn fnv64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// 数据目录对应的管道名。路径先转绝对、规范化、统一小写，所以 `C:\D\`、`c:\d` 是同一个实例。
pub fn pipe_name(data_dir: &Path) -> String {
    let abs = std::path::absolute(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let norm = crate::browse::location::normalize(&abs);
    let key = norm.to_string_lossy().trim_end_matches('\\').to_lowercase();
    format!(
        r"\\.\pipe\slint-file-manager-{}-{:016x}",
        session_id(),
        fnv64(&key)
    )
}

fn session_id() -> u32 {
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let mut s = 0u32;
    // SAFETY: 输出参数为本地变量。
    unsafe {
        ProcessIdToSessionId(std::process::id(), &mut s);
    }
    s
}

/// 创建一个管道实例。`first` 为真时要求是该名称的第一个实例（已有主实例时失败，错误码 ERROR_ACCESS_DENIED）。
fn create_instance(name: &str, first: bool) -> io::Result<File> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    let w = wide(name);
    let mode = PIPE_ACCESS_DUPLEX
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    // SAFETY: 名称缓冲区在调用期间有效；默认安全描述符（只有本用户、管理员与系统可写）。
    let h = unsafe {
        CreateNamedPipeW(
            w.as_ptr(),
            mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            0,
            std::ptr::null(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 句柄刚创建且有效，所有权交给 File（析构时关闭）。
    Ok(unsafe { File::from_raw_handle(h as _) })
}

/// 已占用管道名的主实例（尚未开始接受连接）。
pub struct Listener {
    name: String,
    first: File,
}

/// 尝试成为主实例：成功返回 [`Listener`]；已有主实例返回 `Ok(None)`；其他错误原样返回。
pub fn claim(name: &str) -> io::Result<Option<Listener>> {
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY};
    match create_instance(name, true) {
        Ok(first) => Ok(Some(Listener {
            name: name.to_string(),
            first,
        })),
        // 已有同名管道：ACCESS_DENIED（对方允许多个实例）或 PIPE_BUSY（对方实例数已满）
        Err(e)
            if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
                || e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// 读一行（不含换行）；连接关闭或超过上限时返回错误。
fn read_line(f: &mut File) -> io::Result<String> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "连接已关闭"));
        }
        if let Some(i) = buf[..n].iter().position(|&b| b == b'\n') {
            out.extend_from_slice(&buf[..i]);
            break;
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() > MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "消息过长"));
        }
    }
    String::from_utf8(out).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "不是 UTF-8"))
}

impl Listener {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 在后台线程接受连接；每个连接一个线程，调用 `handler` 得到答复。线程随进程结束。
    pub fn serve(self, handler: impl Fn(Request) -> Reply + Send + Sync + 'static) {
        let handler = Arc::new(handler);
        let Listener { name, first } = self;
        std::thread::Builder::new()
            .name("instance-accept".into())
            .spawn(move || accept_loop(name, first, handler))
            .ok();
    }
}

fn accept_loop(name: String, first: File, handler: Arc<dyn Fn(Request) -> Reply + Send + Sync>) {
    use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError};
    use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
    let mut cur = first;
    loop {
        // SAFETY: 句柄由 cur 持有且有效；同步模式不需要 OVERLAPPED。
        let connected = unsafe {
            ConnectNamedPipe(cur.as_raw_handle() as _, std::ptr::null_mut()) != 0
                || GetLastError() == ERROR_PIPE_CONNECTED
        };
        // 先建好下一个实例再把当前连接交出去，保证管道名一直存在（其他客户端看到的是“忙”而不是“没有主实例”）
        let next = loop {
            match create_instance(&name, false) {
                Ok(f) => break f,
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
        };
        let conn = std::mem::replace(&mut cur, next);
        if connected {
            let h = handler.clone();
            let _ = std::thread::Builder::new()
                .name("instance-conn".into())
                .spawn(move || serve_one(conn, &*h));
        }
    }
}

fn serve_one(mut conn: File, handler: &(dyn Fn(Request) -> Reply + Send + Sync)) {
    use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;
    use windows_sys::Win32::System::Pipes::DisconnectNamedPipe;
    let reply = match read_line(&mut conn) {
        Ok(line) => match Request::from_json(line.trim()) {
            Some(req) => handler(req),
            None => Reply::fail("请求格式无效"),
        },
        Err(_) => return,
    };
    let _ = conn.write_all((reply.to_json() + "\n").as_bytes());
    // SAFETY: 句柄由 conn 持有且有效。
    unsafe {
        FlushFileBuffers(conn.as_raw_handle() as _);
        DisconnectNamedPipe(conn.as_raw_handle() as _);
    }
}

/// 转发结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 主实例已收到并答复。
    Delivered(Reply),
    /// 没有主实例（管道不存在，例如主实例刚刚退出）。
    NoPrimary,
    /// 有主实例但在超时内没有完成答复，或连接出错（附原因）。
    Unresponsive(String),
}

/// 把请求转发给主实例，最多等待 `timeout`。
pub fn forward(name: &str, req: &Request, timeout: Duration) -> Outcome {
    let (tx, rx) = mpsc::channel();
    let name = name.to_string();
    let line = req.to_json() + "\n";
    let spawned = std::thread::Builder::new()
        .name("instance-forward".into())
        .spawn(move || {
            let _ = tx.send(forward_blocking(&name, &line, timeout));
        });
    if spawned.is_err() {
        return Outcome::Unresponsive("无法创建转发线程".into());
    }
    match rx.recv_timeout(timeout) {
        Ok(o) => o,
        // 工作线程可能仍阻塞在读写上；进程随后自行启动或退出，线程随之结束
        Err(_) => Outcome::Unresponsive(format!("{} 毫秒内没有答复", timeout.as_millis())),
    }
}

fn forward_blocking(name: &str, line: &str, timeout: Duration) -> Outcome {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};
    use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
    let deadline = Instant::now() + timeout;
    let mut f = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
        {
            Ok(f) => break f,
            Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
                return Outcome::NoPrimary;
            }
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Outcome::Unresponsive("主实例一直忙".into());
                }
                let w = wide(name);
                // SAFETY: 名称缓冲区在调用期间有效。
                unsafe {
                    WaitNamedPipeW(w.as_ptr(), left.as_millis().clamp(1, 2000) as u32);
                }
            }
            Err(e) => return Outcome::Unresponsive(format!("连接失败：{e}")),
        }
    };
    let mut pid = 0u32;
    // SAFETY: 句柄由 f 持有且有效；输出参数为本地变量。
    unsafe {
        if GetNamedPipeServerProcessId(f.as_raw_handle() as _, &mut pid) != 0 && pid != 0 {
            // 本进程由用户操作启动、有前台权限：把它让给主实例，使其能把窗口切到前台
            AllowSetForegroundWindow(pid);
        }
    }
    if let Err(e) = f.write_all(line.as_bytes()) {
        return Outcome::Unresponsive(format!("写入失败：{e}"));
    }
    match read_line(&mut f) {
        Ok(s) => match Reply::from_json(s.trim()) {
            Some(r) => Outcome::Delivered(r),
            None => Outcome::Unresponsive("答复格式无效".into()),
        },
        Err(e) => Outcome::Unresponsive(format!("读取答复失败：{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn unique(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "sfm-instance-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn pipe_name_is_per_data_dir() {
        let a = pipe_name(Path::new(r"C:\Data\SFM"));
        assert_eq!(a, pipe_name(Path::new(r"c:\data\sfm\")));
        assert_eq!(a, pipe_name(Path::new(r"C:\Data\.\SFM")));
        assert_ne!(a, pipe_name(Path::new(r"C:\Data\SFM2")));
        assert!(a.starts_with(r"\\.\pipe\slint-file-manager-"));
    }

    #[test]
    fn second_claim_fails_and_forward_is_delivered() {
        let name = pipe_name(&unique("deliver"));
        let l = claim(&name).unwrap().expect("第一个实例");
        assert!(claim(&name).unwrap().is_none(), "已有主实例时不能再占用");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        l.serve(move |req| {
            s2.lock().unwrap().push(req.clone());
            match req {
                Request::Activate => Reply::ok("已激活"),
                Request::Open { .. } => Reply::fail("找不到"),
            }
        });
        let open = Request::Open {
            folder: PathBuf::from(r"C:\甲 乙"),
            select: None,
        };
        assert_eq!(
            forward(&name, &Request::Activate, Duration::from_secs(5)),
            Outcome::Delivered(Reply::ok("已激活"))
        );
        assert_eq!(
            forward(&name, &open, Duration::from_secs(5)),
            Outcome::Delivered(Reply::fail("找不到"))
        );
        // 并发转发都能送达
        let hs: Vec<_> = (0..4)
            .map(|_| {
                let n = name.clone();
                std::thread::spawn(move || forward(&n, &Request::Activate, Duration::from_secs(5)))
            })
            .collect();
        for h in hs {
            assert!(matches!(h.join().unwrap(), Outcome::Delivered(_)));
        }
        assert_eq!(seen.lock().unwrap().len(), 6);
        assert_eq!(seen.lock().unwrap()[1], open);
    }

    #[test]
    fn no_primary_is_reported() {
        let name = pipe_name(&unique("none"));
        assert_eq!(
            forward(&name, &Request::Activate, Duration::from_secs(2)),
            Outcome::NoPrimary
        );
    }

    #[test]
    fn unresponsive_primary_times_out() {
        // 占用管道名但从不读写（模拟卡死的主实例）
        let name = pipe_name(&unique("hung"));
        let _hung = claim(&name).unwrap().unwrap();
        let t0 = Instant::now();
        let o = forward(&name, &Request::Activate, Duration::from_millis(600));
        assert!(matches!(o, Outcome::Unresponsive(_)), "{o:?}");
        assert!(t0.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn single_instance_holder_counts_as_primary() {
        // 别的程序以“最多 1 个实例”占用了同名管道：建管道得到 PIPE_BUSY，也应视为已有主实例并转发（随后超时）
        use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT};
        let name = pipe_name(&unique("busy"));
        let w = wide(&name);
        // SAFETY: 名称缓冲区在调用期间有效；句柄交给 File 关闭。
        let h = unsafe {
            CreateNamedPipeW(
                w.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(h, windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE);
        let _holder = unsafe { File::from_raw_handle(h as _) };
        assert!(claim(&name).unwrap().is_none());
        let o = forward(&name, &Request::Activate, Duration::from_millis(400));
        assert!(matches!(o, Outcome::Unresponsive(_)), "{o:?}");
    }

    #[test]
    fn slow_handler_times_out_but_garbage_is_rejected() {
        let name = pipe_name(&unique("slow"));
        let l = claim(&name).unwrap().unwrap();
        l.serve(|_| {
            std::thread::sleep(Duration::from_secs(3));
            Reply::ok("迟到")
        });
        let o = forward(&name, &Request::Activate, Duration::from_millis(500));
        assert!(matches!(o, Outcome::Unresponsive(_)), "{o:?}");

        let name2 = pipe_name(&unique("garbage"));
        let l2 = claim(&name2).unwrap().unwrap();
        l2.serve(|_| Reply::ok("不应调用"));
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name2)
            .unwrap();
        f.write_all(b"{\"type\":\"run\",\"cmd\":\"calc\"}\n")
            .unwrap();
        let line = read_line(&mut f).unwrap();
        assert_eq!(Reply::from_json(&line), Some(Reply::fail("请求格式无效")));
    }
}
