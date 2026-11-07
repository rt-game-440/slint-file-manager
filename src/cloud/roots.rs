//! 发现 OneDrive／iCloud 在本机的同步目录。
//!
//! 优先读系统的同步根注册表（`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager\<提供方>!<SID>!<账户>\UserSyncRoots`，
//! 只读）：它给出提供方实际登记的目录，iCloud 被用户移到别的盘（如 `E:\icloud\iCloudDrive`）时也能找到。
//! 注册表里没有时才回退到环境变量／默认位置（`%OneDrive%`、`%USERPROFILE%\iCloudDrive`）。
//! 不登录、不创建账户，只看本机已经存在的同步目录。

use std::path::{Path, PathBuf};

/// 本机识别到的云盘同步目录。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloudRoots {
    pub onedrive: Option<PathBuf>,
    pub icloud: Option<PathBuf>,
    pub icloud_photos: Option<PathBuf>,
}

impl CloudRoots {
    pub fn any(&self) -> bool {
        self.onedrive.is_some() || self.icloud.is_some() || self.icloud_photos.is_some()
    }

    pub fn all(&self) -> impl Iterator<Item = &PathBuf> {
        [&self.onedrive, &self.icloud, &self.icloud_photos]
            .into_iter()
            .flatten()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    OneDrive,
    ICloudDrive,
    ICloudPhotos,
}

/// 同步根注册表项名（`提供方!SID!账户[|标识]`）里的提供方类别。
fn kind_of(key: &str) -> Option<Kind> {
    let prov = key.split('!').next()?.to_ascii_lowercase();
    match prov.as_str() {
        "onedrive" => Some(Kind::OneDrive),
        "iclouddrive" => Some(Kind::ICloudDrive),
        "icloudphotos" => Some(Kind::ICloudPhotos),
        _ => None,
    }
}

/// 由（注册表项名，登记的目录）列表选出各提供方的目录：目录必须存在；同类有多个时个人账户优先，其次按项名排序取第一个。
pub fn pick(entries: &[(String, PathBuf)], is_dir: &dyn Fn(&Path) -> bool) -> CloudRoots {
    let mut sorted: Vec<&(String, PathBuf)> = entries.iter().collect();
    // 项名里带 “Personal” 的排前面（OneDrive 个人版），其余按名称
    sorted.sort_by_key(|(k, _)| {
        (
            !k.to_ascii_lowercase().contains("!personal"),
            k.to_lowercase(),
        )
    });
    let mut out = CloudRoots::default();
    for (k, p) in sorted {
        let slot = match kind_of(k) {
            Some(Kind::OneDrive) => &mut out.onedrive,
            Some(Kind::ICloudDrive) => &mut out.icloud,
            Some(Kind::ICloudPhotos) => &mut out.icloud_photos,
            None => continue,
        };
        if slot.is_none() && is_dir(p) {
            *slot = Some(p.clone());
        }
    }
    out
}

/// 回退：环境变量与默认位置（注册表读不到时）。
fn fallback(is_dir: &dyn Fn(&Path) -> bool) -> CloudRoots {
    let onedrive = ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"]
        .iter()
        .filter_map(|k| std::env::var_os(k).map(PathBuf::from))
        .find(|p| is_dir(p));
    let icloud = std::env::var_os("USERPROFILE")
        .map(|h| PathBuf::from(h).join("iCloudDrive"))
        .filter(|p| is_dir(p));
    CloudRoots {
        onedrive,
        icloud,
        icloud_photos: None,
    }
}

/// 读取注册表里的同步根登记：（项名，目录）。读不到返回空。
fn registry_entries() -> Vec<(String, PathBuf)> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumKeyExW, RegEnumValueW,
        RegOpenKeyExW,
    };
    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let mut out = Vec::new();
    let base = w("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\SyncRootManager");
    let mut hbase: HKEY = std::ptr::null_mut();
    // SAFETY: 以下均为只读注册表调用，缓冲区长度如实传入，句柄在使用后关闭。
    unsafe {
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, base.as_ptr(), 0, KEY_READ, &mut hbase) != 0 {
            return out;
        }
        let mut i = 0u32;
        loop {
            let mut name = [0u16; 512];
            let mut n = name.len() as u32;
            let r = RegEnumKeyExW(
                hbase,
                i,
                name.as_mut_ptr(),
                &mut n,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if r != 0 {
                break;
            }
            i += 1;
            let key = String::from_utf16_lossy(&name[..n as usize]);
            if kind_of(&key).is_none() {
                continue;
            }
            let path = w(&format!(
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\SyncRootManager\\{key}\\UserSyncRoots"
            ));
            let mut hsub: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut hsub) != 0 {
                continue;
            }
            let mut j = 0u32;
            loop {
                let mut vname = [0u16; 256];
                let mut vn = vname.len() as u32;
                let mut data = vec![0u8; 2048];
                let mut dn = data.len() as u32;
                let mut ty = 0u32;
                let r = RegEnumValueW(
                    hsub,
                    j,
                    vname.as_mut_ptr(),
                    &mut vn,
                    std::ptr::null(),
                    &mut ty,
                    data.as_mut_ptr(),
                    &mut dn,
                );
                if r != 0 {
                    break;
                }
                j += 1;
                // REG_SZ = 1
                if ty == 1 && dn >= 2 {
                    let u: Vec<u16> = data[..dn as usize]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c))
                        .take_while(|&c| c != 0)
                        .collect();
                    out.push((
                        key.clone(),
                        PathBuf::from(std::ffi::OsString::from_wide(&u)),
                    ));
                }
            }
            RegCloseKey(hsub);
        }
        RegCloseKey(hbase);
    }
    out
}

/// 发现本机的云盘同步目录：注册表优先，缺失的提供方再用环境变量／默认位置补。
pub fn discover() -> CloudRoots {
    let is_dir = |p: &Path| p.is_dir();
    let mut r = pick(&registry_entries(), &is_dir);
    let fb = fallback(&is_dir);
    r.onedrive = r.onedrive.or(fb.onedrive);
    r.icloud = r.icloud.or(fb.icloud);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(k: &str, p: &str) -> (String, PathBuf) {
        (k.to_string(), PathBuf::from(p))
    }

    #[test]
    fn picks_each_provider_from_registry_entries() {
        let entries = vec![
            e(
                "iCloudDrive!S-1-5-21-1!Personal",
                "E:\\移动过的\\iCloudDrive",
            ),
            e(
                "iCloudPhotos!S-1-5-21-1!Personal",
                "E:\\图\\iCloud Photos\\Photos",
            ),
            e(
                "OneDrive!S-1-5-21-1!Personal|abc",
                "C:\\Users\\某人\\OneDrive",
            ),
            e(
                "OneDrive!S-1-5-21-1!Business1|def",
                "C:\\Users\\某人\\OneDrive - 公司",
            ),
            e("Dropbox!S-1-5-21-1!x", "C:\\Dropbox"),
        ];
        let r = pick(&entries, &|_| true);
        assert_eq!(r.icloud, Some("E:\\移动过的\\iCloudDrive".into()));
        assert_eq!(
            r.icloud_photos,
            Some("E:\\图\\iCloud Photos\\Photos".into())
        );
        assert_eq!(
            r.onedrive,
            Some("C:\\Users\\某人\\OneDrive".into()),
            "个人版优先于商业版"
        );
        assert!(r.any());
    }

    #[test]
    fn missing_directories_are_skipped() {
        let entries = vec![
            e("OneDrive!S-1!Personal", "C:\\不存在"),
            e("OneDrive!S-1!Business1", "C:\\存在"),
        ];
        let r = pick(&entries, &|p| p == Path::new("C:\\存在"));
        assert_eq!(
            r.onedrive,
            Some("C:\\存在".into()),
            "个人版目录不存在时退到下一个"
        );
        assert!(r.icloud.is_none());
        assert!(!pick(&[], &|_| true).any());
    }

    #[test]
    fn real_registry_read_does_not_panic() {
        // 本机有没有云盘都不应崩溃；只检查返回的目录确实存在
        for p in discover().all() {
            assert!(p.is_dir());
        }
    }
}
