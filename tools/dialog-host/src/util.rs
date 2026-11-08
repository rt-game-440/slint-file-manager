use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::PWSTR;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 取出并释放 COM 分配的宽字符串。
pub fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = unsafe { p.to_string().unwrap_or_default() };
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// 缓冲区里从头到第一个 NUL 之前的文本。
pub fn until_nul(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

pub fn bitness() -> u32 {
    (std::mem::size_of::<usize>() * 8) as u32
}
