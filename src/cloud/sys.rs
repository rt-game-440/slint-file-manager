//! 产品实现：云文件 API（`cldapi.dll`）。只读元数据，不打开数据流、不触发下载。
//!
//! - 同步根：`CfGetSyncRootInfoByPath`（路径不在同步根内时返回 `ERROR_NOT_A_CLOUD_FILE`）。
//! - 占位信息：以 `FILE_READ_ATTRIBUTES | FILE_FLAG_OPEN_REPARSE_POINT` 打开句柄（只读属性、不跟随重解析点、不读数据），
//!   再 `CfGetPlaceholderInfo`。研究 0003 3.1 用同样方法在 OneDrive／iCloud 上验证过不会水合。

use super::{CloudProbe, Pin, Placeholder, ProviderStatus, SyncRoot};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::CloudFilters::{
    CF_PLACEHOLDER_INFO_STANDARD, CF_PLACEHOLDER_STANDARD_INFO, CF_SYNC_ROOT_INFO_STANDARD,
    CF_SYNC_ROOT_STANDARD_INFO, CfGetPlaceholderInfo, CfGetSyncRootInfoByPath,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::core::PCWSTR;

/// 系统云文件 API。
pub struct SystemCloud;

/// 供“没有注入探针”的调用点使用的全局实例。
pub static SYSTEM: SystemCloud = SystemCloud;

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn from_wide(s: &[u16]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n])
}

/// `STANDARD_INFO` 之后紧跟可变长的文件标识，缓冲区留足并保证按 8 字节对齐。
#[repr(C, align(8))]
struct Buf([u8; 4096]);

impl CloudProbe for SystemCloud {
    fn sync_root(&self, dir: &Path) -> Option<SyncRoot> {
        let w = wide(dir);
        let mut buf = Buf([0; 4096]);
        let mut ret = 0u32;
        // SAFETY: w 以 NUL 结尾；缓冲区足够大、对齐，长度如实传入。
        let r = unsafe {
            CfGetSyncRootInfoByPath(
                PCWSTR(w.as_ptr()),
                CF_SYNC_ROOT_INFO_STANDARD,
                buf.0.as_mut_ptr().cast(),
                buf.0.len() as u32,
                Some(&mut ret),
            )
        };
        r.ok()?;
        // SAFETY: 调用成功后缓冲区开头是 CF_SYNC_ROOT_STANDARD_INFO。
        let info = unsafe { &*(buf.0.as_ptr() as *const CF_SYNC_ROOT_STANDARD_INFO) };
        Some(SyncRoot {
            provider: from_wide(&info.ProviderName),
            status: ProviderStatus::from_raw(info.ProviderStatus.0),
        })
    }

    fn placeholder(&self, path: &Path) -> Option<Placeholder> {
        let w = wide(path);
        // SAFETY: 只申请读属性权限，不读数据流；句柄在函数内关闭。
        let h: HANDLE = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .ok()?;
        let mut buf = Buf([0; 4096]);
        let mut ret = 0u32;
        // SAFETY: 句柄有效；缓冲区足够大、对齐。
        let r = unsafe {
            CfGetPlaceholderInfo(
                h,
                CF_PLACEHOLDER_INFO_STANDARD,
                buf.0.as_mut_ptr().cast(),
                buf.0.len() as u32,
                Some(&mut ret),
            )
        };
        // SAFETY: 句柄由本函数打开。
        unsafe {
            let _ = CloseHandle(h);
        }
        if r.is_err() {
            return None;
        }
        // SAFETY: 调用成功后缓冲区开头是 CF_PLACEHOLDER_STANDARD_INFO。
        let info = unsafe { &*(buf.0.as_ptr() as *const CF_PLACEHOLDER_STANDARD_INFO) };
        Some(Placeholder {
            on_disk: info.OnDiskDataSize.max(0) as u64,
            pin: Pin::from_raw(info.PinState.0),
            in_sync: info.InSyncState.0 != 0,
        })
    }
}
