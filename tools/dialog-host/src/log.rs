//! 日志：`logs/yyyy-MM-dd HH-mm-ss.log`，一次启动复用同一个文件（首次写入时创建）。
//! 行格式沿用主程序：`HH:mm:ss.fff +启动后毫秒 内容`。
//! 位置：`--log-file` 指定文件优先；其次 `--log-dir` 或环境变量 `SFM_LOG_DIR`；默认当前目录下的 `logs`。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use windows::Win32::System::SystemInformation::GetLocalTime;

struct Log {
    target: Target,
    file: Option<File>,
    start: Instant,
}

enum Target {
    File(PathBuf),
    Dir(PathBuf),
}

static LOG: OnceLock<Mutex<Log>> = OnceLock::new();

pub fn init(file: Option<PathBuf>, dir: Option<PathBuf>) {
    let target = match (file, dir) {
        (Some(f), _) => Target::File(f),
        (None, Some(d)) => Target::Dir(d),
        (None, None) => Target::Dir(
            std::env::var_os("SFM_LOG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("logs")),
        ),
    };
    let _ = LOG.set(Mutex::new(Log {
        target,
        file: None,
        start: Instant::now(),
    }));
}

pub fn line(msg: &str) {
    let Some(m) = LOG.get() else { return };
    let Ok(mut g) = m.lock() else { return };
    let st = unsafe { GetLocalTime() };
    if g.file.is_none() {
        let p = match &g.target {
            Target::File(p) => p.clone(),
            Target::Dir(d) => {
                let _ = std::fs::create_dir_all(d);
                // 一次启动一个文件：以首次写入的本地时间命名，之后复用
                let name = format!(
                    "{:04}-{:02}-{:02} {:02}-{:02}-{:02}.log",
                    st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
                );
                let p = d.join(name);
                g.target = Target::File(p.clone());
                p
            }
        };
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match OpenOptions::new().create(true).append(true).open(&p) {
            Ok(f) => g.file = Some(f),
            Err(e) => {
                eprintln!("警告：无法打开日志文件 {}：{e}", p.display());
                return;
            }
        }
    }
    let ms = g.start.elapsed().as_millis();
    let text = format!(
        "{:02}:{:02}:{:02}.{:03} +{}ms {}\n",
        st.wHour, st.wMinute, st.wSecond, st.wMilliseconds, ms, msg
    );
    if let Some(f) = g.file.as_mut() {
        let _ = f.write_all(text.as_bytes());
    }
}
