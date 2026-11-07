//! 目录监视：通知“某个目录里有东西变了”，具体读取由调用方在工作线程完成。依据 Issue #7。
//!
//! 两种模式：
//! - [`WatchMode::Notify`]：`ReadDirectoryChangesW`（不递归）。每次通知只发出一个“已变化”信号，不解析文件名，
//!   爆发的通知由接收方合并后再读取一次。目录句柄以全部共享方式打开，不阻止他人重命名或删除该目录。
//! - [`WatchMode::Poll`]：不持有任何句柄，每隔约 1.5 秒看目录自身的修改时间。用于 U 盘／移动硬盘等可弹出的磁盘：
//!   对卷上的目录持有打开的句柄会让“安全删除硬件”报告设备正在使用。代价是只能发现新建、删除、重命名
//!   （目录修改时间变化），看不到已有文件的单纯内容修改——此时用 F5 刷新。
//!
//! 事件经 `mpsc` 通道交给界面线程，附带监视器编号：换目录后旧监视器被丢弃，其迟到的通知因编号失效被忽略。
//! 目录被删除、磁盘断开或监视出错时发出一次 [`WatchEvent::Gone`] 后线程结束。

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::CancelIoEx;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchMode {
    Notify,
    Poll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    /// 目录内容可能变了。
    Changed,
    /// 监视结束：目录没了、磁盘断开或出错。
    Gone,
}

/// 监视器编号 + 事件。
pub type WatchMsg = (u64, WatchEvent);

const POLL_INTERVAL: Duration = Duration::from_millis(1500);
const POLL_SLICE: Duration = Duration::from_millis(100);

/// 一个目录的监视。丢弃即停止（不阻塞：线程自行收尾并关闭句柄）。
pub struct DirWatcher {
    stop: Arc<AtomicBool>,
    /// `Notify` 模式下的目录句柄（以整数保存）；线程退出前取走并关闭，`Drop` 在锁内取消阻塞中的读取。
    handle: Arc<Mutex<Option<usize>>>,
}

impl DirWatcher {
    pub fn start(
        path: &Path,
        id: u64,
        tx: Sender<WatchMsg>,
        mode: WatchMode,
    ) -> io::Result<DirWatcher> {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = Arc::new(Mutex::new(None));
        match mode {
            WatchMode::Notify => {
                let w: Vec<u16> = path
                    .as_os_str()
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect();
                // SAFETY: 以 NUL 结尾的路径；其余参数按 CreateFileW 文档传入。
                let h = unsafe {
                    CreateFileW(
                        w.as_ptr(),
                        FILE_LIST_DIRECTORY,
                        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        FILE_FLAG_BACKUP_SEMANTICS,
                        std::ptr::null_mut(),
                    )
                };
                if h == INVALID_HANDLE_VALUE {
                    return Err(io::Error::last_os_error());
                }
                let hu = h as usize;
                *handle.lock().unwrap() = Some(hu);
                let (stop2, handle2) = (stop.clone(), handle.clone());
                std::thread::spawn(move || notify_loop(hu, id, tx, stop2, handle2));
            }
            WatchMode::Poll => {
                let stamp = poll_stamp(path)?;
                let (stop2, p) = (stop.clone(), path.to_path_buf());
                std::thread::spawn(move || poll_loop(p, stamp, id, tx, stop2));
            }
        }
        Ok(DirWatcher { stop, handle })
    }
}

impl Drop for DirWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(g) = self.handle.lock()
            && let Some(h) = *g
        {
            // SAFETY: 句柄在锁内仍有效（线程关闭句柄前先在锁内取走）；取消该句柄上阻塞的读取。
            unsafe { CancelIoEx(h as _, std::ptr::null()) };
        }
    }
}

fn notify_loop(
    h: usize,
    id: u64,
    tx: Sender<WatchMsg>,
    stop: Arc<AtomicBool>,
    handle: Arc<Mutex<Option<usize>>>,
) {
    // 8 字节对齐的 64 KiB 缓冲；内容不解析，只用来承接系统写入
    let mut buf = vec![0u64; 8192];
    let filter = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE;
    let gone = loop {
        if stop.load(Ordering::SeqCst) {
            break false;
        }
        let mut returned = 0u32;
        // SAFETY: 同步调用；缓冲区长度如实传入，句柄在本线程关闭前有效。
        let ok = unsafe {
            ReadDirectoryChangesW(
                h as _,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                0,
                filter,
                &mut returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if stop.load(Ordering::SeqCst) {
            break false;
        }
        if ok == 0 {
            // SAFETY: 读取线程局部错误码。
            let _ = unsafe { GetLastError() };
            break true;
        }
        // returned == 0 表示缓冲区溢出（丢了通知）：同样当作“已变化”
        if tx.send((id, WatchEvent::Changed)).is_err() {
            break false;
        }
    };
    // 先在锁内取走并关闭句柄，再通知：接收方随后读目录时，目录已不被本监视持有（不会处于“待删除”状态）
    if let Ok(mut g) = handle.lock()
        && let Some(h) = g.take()
    {
        // SAFETY: 句柄由本线程独占关闭一次。
        unsafe { CloseHandle(h as _) };
    }
    if gone {
        let _ = tx.send((id, WatchEvent::Gone));
    }
}

/// 目录自身的修改时间；目录不存在或读不了时返回错误。
fn poll_stamp(path: &Path) -> io::Result<Option<std::time::SystemTime>> {
    let md = std::fs::metadata(path)?;
    if !md.is_dir() {
        return Err(io::Error::other("不是目录"));
    }
    Ok(md.modified().ok())
}

fn poll_loop(
    path: PathBuf,
    mut last: Option<std::time::SystemTime>,
    id: u64,
    tx: Sender<WatchMsg>,
    stop: Arc<AtomicBool>,
) {
    loop {
        let mut waited = Duration::ZERO;
        while waited < POLL_INTERVAL {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(POLL_SLICE);
            waited += POLL_SLICE;
        }
        match poll_stamp(&path) {
            Ok(now) => {
                if now != last {
                    last = now;
                    if tx.send((id, WatchEvent::Changed)).is_err() {
                        return;
                    }
                }
            }
            Err(_) => {
                if !stop.load(Ordering::SeqCst) {
                    let _ = tx.send((id, WatchEvent::Gone));
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Instant;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sfm-watch-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn wait(rx: &Receiver<WatchMsg>, want: WatchEvent, secs: u64) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if let Ok((_, ev)) = rx.recv_timeout(Duration::from_millis(100))
                && ev == want
            {
                return true;
            }
        }
        false
    }

    #[test]
    fn notify_reports_create_rename_modify_delete() {
        let d = tmp("notify");
        let (tx, rx) = channel();
        let _w = DirWatcher::start(&d, 1, tx, WatchMode::Notify).unwrap();
        fs::write(d.join("a.txt"), b"1").unwrap();
        assert!(wait(&rx, WatchEvent::Changed, 3), "新建");
        fs::rename(d.join("a.txt"), d.join("b.txt")).unwrap();
        assert!(wait(&rx, WatchEvent::Changed, 3), "重命名");
        fs::write(d.join("b.txt"), b"22222").unwrap();
        assert!(wait(&rx, WatchEvent::Changed, 3), "修改");
        fs::remove_file(d.join("b.txt")).unwrap();
        assert!(wait(&rx, WatchEvent::Changed, 3), "删除");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn notify_reports_directory_removed_and_releases_handle() {
        let d = tmp("gone");
        let (tx, rx) = channel();
        let _w = DirWatcher::start(&d, 7, tx, WatchMode::Notify).unwrap();
        fs::remove_dir_all(&d).unwrap();
        assert!(wait(&rx, WatchEvent::Gone, 3), "目录被删应发 Gone");
        assert!(!d.exists(), "监视器不应让被删目录继续存在");
    }

    #[test]
    fn dropping_stops_without_events_and_allows_delete() {
        let d = tmp("drop");
        let (tx, rx) = channel();
        let w = DirWatcher::start(&d, 3, tx, WatchMode::Notify).unwrap();
        drop(w);
        std::thread::sleep(Duration::from_millis(200));
        fs::write(d.join("x"), b"1").unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "丢弃后不应再有通知"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn poll_mode_sees_entry_changes_and_removal() {
        let d = tmp("poll");
        let (tx, rx) = channel();
        let _w = DirWatcher::start(&d, 2, tx, WatchMode::Poll).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        fs::write(d.join("a.txt"), b"1").unwrap();
        assert!(wait(&rx, WatchEvent::Changed, 5), "新建");
        fs::remove_dir_all(&d).unwrap();
        assert!(wait(&rx, WatchEvent::Gone, 5), "目录被删");
    }

    #[test]
    fn start_fails_for_missing_directory() {
        let (tx, _rx) = channel();
        let missing = std::env::temp_dir().join("sfm-watch-definitely-missing-dir");
        assert!(DirWatcher::start(&missing, 1, tx.clone(), WatchMode::Notify).is_err());
        assert!(DirWatcher::start(&missing, 1, tx, WatchMode::Poll).is_err());
    }
}
