//! 共享类型、统计与进程内存读取。输出只含数值与类别，不含路径、文件名或图像内容。

use std::time::Duration;

/// 缩略图：RGBA8，非预乘 alpha（与 Slint `Rgba8Pixel` 一致）。
pub struct Thumb {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// 单次生成的分段耗时（微秒）。`decode_us` 对“解码时缩放”的路线包含缩放，此时 `scale_us` 为 0。
#[derive(Clone, Copy, Default, Debug)]
pub struct Timing {
    pub read_us: u64,
    pub decode_us: u64,
    pub scale_us: u64,
}

impl Timing {
    pub fn total_us(&self) -> u64 {
        self.read_us + self.decode_us + self.scale_us
    }
}

/// 源图信息（只记录统计用的数值）。
#[derive(Clone, Copy, Default, Debug)]
pub struct SrcInfo {
    pub w: u32,
    pub h: u32,
    pub bytes: u64,
}

/// 失败类别：只暴露类别，不带 OS 错误文本（避免泄露路径）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Fail {
    /// 读取失败（不存在、无权限、I/O 错误）
    Io,
    /// 文件内容损坏或被截断
    Corrupt,
    /// 格式不支持（含动图、未知内容）
    Unsupported,
    /// 像素或文件体积超过上限，未解码
    TooLarge,
    /// 超过硬超时，显示回退图标
    Timeout,
}

impl Fail {
    pub fn as_str(self) -> &'static str {
        match self {
            Fail::Io => "io",
            Fail::Corrupt => "corrupt",
            Fail::Unsupported => "unsupported",
            Fail::TooLarge => "too_large",
            Fail::Timeout => "timeout",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// 源图像素上限（宽×高）。超过则读完头部后拒绝，不解码。
    pub max_pixels: u64,
    /// 文件体积上限（字节）。超过则不读取。
    pub max_file_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_pixels: 120_000_000,
            max_file_bytes: 256 * 1024 * 1024,
        }
    }
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// 分位数统计（毫秒）。样本为空时返回 null，而不是 0。
pub fn stats_ms(us: &[u64]) -> serde_json::Value {
    if us.is_empty() {
        return serde_json::Value::Null;
    }
    let mut v: Vec<f64> = us.iter().map(|&x| x as f64 / 1000.0).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pick = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    let r = |x: f64| (x * 100.0).round() / 100.0;
    serde_json::json!({
        "n": v.len(),
        "min": r(v[0]),
        "median": r(pick(0.5)),
        "p90": r(pick(0.9)),
        "max": r(*v.last().unwrap()),
        "mean": r(v.iter().sum::<f64>() / v.len() as f64),
    })
}

/// 当前进程内存（MiB）。`peak_commit` 为峰值已提交私有内存（PeakPagefileUsage），最接近“分配过多少”。
pub struct Mem {
    pub working_set: f64,
    pub peak_working_set: f64,
    pub commit: f64,
    pub peak_commit: f64,
}

pub fn mem() -> Mem {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb);
    }
    let mib = |b: usize| (b as f64 / 1048576.0 * 10.0).round() / 10.0;
    Mem {
        working_set: mib(c.WorkingSetSize),
        peak_working_set: mib(c.PeakWorkingSetSize),
        commit: mib(c.PagefileUsage),
        peak_commit: mib(c.PeakPagefileUsage),
    }
}

pub fn mem_json(m: &Mem) -> serde_json::Value {
    serde_json::json!({
        "working_set_mib": m.working_set,
        "peak_working_set_mib": m.peak_working_set,
        "commit_mib": m.commit,
        "peak_commit_mib": m.peak_commit,
    })
}

pub fn arg(args: &[String], name: &str) -> Option<String> {
    let pre = format!("--{name}=");
    args.iter()
        .find_map(|a| a.strip_prefix(&pre).map(|s| s.to_string()))
}

pub fn flag(args: &[String], name: &str) -> bool {
    let f = format!("--{name}");
    args.contains(&f)
}

pub fn arg_parse<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    arg(args, name)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub fn temp_root() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("TEMP").unwrap_or_else(|_| ".".into()))
        .join("sfm-thumbs")
}
