//! [`RegStore`] 的 Windows 实现：挂在 `HKEY_CURRENT_USER\<base>` 下。
//!
//! - #17 产品接线时 `base = Software\Classes`（当前用户的文件关联，HKCR 合并视图中优先于 HKLM）。
//! - 测试与演练只用 [`TestRoot`]：`HKCU\Software\slint-file-manager-test\<随机>`，析构时整棵删除；
//!   整棵删除只允许发生在这个前缀下。
//!
//! 本 PR（Issue #16）里没有任何代码以 `Software\Classes` 为根调用写入。

use std::io;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, KEY_WRITE, REG_OPTION_NON_VOLATILE,
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteTreeW, RegDeleteValueW, RegEnumKeyExW,
    RegEnumValueW, RegOpenKeyExW, RegQueryInfoKeyW, RegQueryValueExW, RegSetValueExW,
};

use super::takeover::{RegStore, RegValue, Snapshot};

/// 测试根的前缀（相对 HKCU）。
pub const TEST_PREFIX: &str = r"Software\slint-file-manager-test";

/// `HKEY_CURRENT_USER\<base>` 下的注册表根。
pub struct WinReg {
    base: String,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn err(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

/// 关闭时自动 `RegCloseKey`。
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

impl WinReg {
    /// 以 `HKEY_CURRENT_USER\<base>` 为根。
    pub fn hkcu(base: &str) -> WinReg {
        WinReg {
            base: base.trim_matches('\\').to_string(),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn full(&self, key: &str) -> String {
        let key = key.trim_matches('\\');
        if key.is_empty() {
            self.base.clone()
        } else {
            format!("{}\\{key}", self.base)
        }
    }

    fn open(&self, key: &str, access: u32) -> io::Result<Option<Key>> {
        let path = wide(&self.full(key));
        let mut h: HKEY = null_mut();
        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, access, &mut h) };
        match rc {
            ERROR_SUCCESS => Ok(Some(Key(h))),
            ERROR_FILE_NOT_FOUND => Ok(None),
            e => Err(err(e)),
        }
    }

    fn snapshot_into(&self, h: &Key, rel: &str, out: &mut Snapshot) -> io::Result<()> {
        // 值
        let mut i = 0u32;
        loop {
            let mut name = vec![0u16; 16384];
            let mut name_len = name.len() as u32;
            let mut kind = 0u32;
            let mut data_len = 0u32;
            let rc = unsafe {
                RegEnumValueW(
                    h.0,
                    i,
                    name.as_mut_ptr(),
                    &mut name_len,
                    null(),
                    &mut kind,
                    null_mut(),
                    &mut data_len,
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc != ERROR_SUCCESS {
                return Err(err(rc));
            }
            let n = String::from_utf16_lossy(&name[..name_len as usize]);
            let v = query(h, &n)?.unwrap_or(RegValue {
                kind,
                bytes: Vec::new(),
            });
            out.values
                .insert((rel.to_string(), n.to_ascii_lowercase()), v);
            i += 1;
        }
        // 子键
        let mut subs = Vec::new();
        let mut i = 0u32;
        loop {
            let mut name = vec![0u16; 256];
            let mut name_len = name.len() as u32;
            let rc = unsafe {
                RegEnumKeyExW(
                    h.0,
                    i,
                    name.as_mut_ptr(),
                    &mut name_len,
                    null(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc != ERROR_SUCCESS {
                return Err(err(rc));
            }
            subs.push(String::from_utf16_lossy(&name[..name_len as usize]));
            i += 1;
        }
        for s in subs {
            let child_rel = if rel.is_empty() {
                s.to_ascii_lowercase()
            } else {
                format!("{rel}\\{}", s.to_ascii_lowercase())
            };
            out.keys.insert(child_rel.clone());
            let child_path = wide(&s);
            let mut ch: HKEY = null_mut();
            let rc = unsafe { RegOpenKeyExW(h.0, child_path.as_ptr(), 0, KEY_READ, &mut ch) };
            if rc != ERROR_SUCCESS {
                return Err(err(rc));
            }
            self.snapshot_into(&Key(ch), &child_rel, out)?;
        }
        Ok(())
    }
}

fn query(h: &Key, name: &str) -> io::Result<Option<RegValue>> {
    let n = wide(name);
    let mut kind = 0u32;
    let mut len = 0u32;
    let rc = unsafe { RegQueryValueExW(h.0, n.as_ptr(), null(), &mut kind, null_mut(), &mut len) };
    match rc {
        ERROR_SUCCESS => {}
        ERROR_FILE_NOT_FOUND => return Ok(None),
        e => return Err(err(e)),
    }
    let mut buf = vec![0u8; len as usize];
    let rc = unsafe {
        RegQueryValueExW(
            h.0,
            n.as_ptr(),
            null(),
            &mut kind,
            buf.as_mut_ptr(),
            &mut len,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(err(rc));
    }
    buf.truncate(len as usize);
    Ok(Some(RegValue { kind, bytes: buf }))
}

impl RegStore for WinReg {
    fn key_exists(&self, key: &str) -> io::Result<bool> {
        Ok(self.open(key, KEY_READ)?.is_some())
    }

    fn get(&self, key: &str, name: &str) -> io::Result<Option<RegValue>> {
        match self.open(key, KEY_READ)? {
            Some(h) => query(&h, name),
            None => Ok(None),
        }
    }

    fn set(&mut self, key: &str, name: &str, value: &RegValue) -> io::Result<()> {
        let path = wide(&self.full(key));
        let mut h: HKEY = null_mut();
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE | KEY_READ,
                null(),
                &mut h,
                null_mut(),
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(err(rc));
        }
        let h = Key(h);
        let n = wide(name);
        let rc = unsafe {
            RegSetValueExW(
                h.0,
                n.as_ptr(),
                0,
                value.kind,
                value.bytes.as_ptr(),
                value.bytes.len() as u32,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(err(rc));
        }
        Ok(())
    }

    fn delete_value(&mut self, key: &str, name: &str) -> io::Result<()> {
        let Some(h) = self.open(key, KEY_SET_VALUE)? else {
            return Ok(());
        };
        let n = wide(name);
        match unsafe { RegDeleteValueW(h.0, n.as_ptr()) } {
            ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
            e => Err(err(e)),
        }
    }

    fn delete_key_if_empty(&mut self, key: &str) -> io::Result<bool> {
        let Some(h) = self.open(key, KEY_READ)? else {
            return Ok(false);
        };
        let mut subkeys = 0u32;
        let mut values = 0u32;
        let rc = unsafe {
            RegQueryInfoKeyW(
                h.0,
                null_mut(),
                null_mut(),
                null(),
                &mut subkeys,
                null_mut(),
                null_mut(),
                &mut values,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        drop(h);
        if rc != ERROR_SUCCESS {
            return Err(err(rc));
        }
        if subkeys > 0 || values > 0 {
            return Ok(false);
        }
        let path = wide(&self.full(key));
        match unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, path.as_ptr()) } {
            ERROR_SUCCESS => Ok(true),
            ERROR_FILE_NOT_FOUND => Ok(false),
            e => Err(err(e)),
        }
    }

    fn snapshot(&self, key: &str) -> io::Result<Snapshot> {
        let mut out = Snapshot::default();
        if let Some(h) = self.open(key, KEY_READ)? {
            self.snapshot_into(&h, "", &mut out)?;
        }
        Ok(out)
    }
}

/// 测试专用根：`HKCU\Software\slint-file-manager-test\<pid>-<纳秒>`，析构时整棵删除（含空的前缀键）。
pub struct TestRoot {
    pub reg: WinReg,
}

impl TestRoot {
    pub fn new() -> io::Result<TestRoot> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base = format!("{TEST_PREFIX}\\{}-{nanos}", std::process::id());
        let mut reg = WinReg::hkcu(&base);
        // 建出空的根键，使“根下原来什么都没有”成为确定的起点
        reg.set("", "probe", &RegValue::sz(""))?;
        reg.delete_value("", "probe")?;
        Ok(TestRoot { reg })
    }

    /// 删除整棵测试根；只允许在 [`TEST_PREFIX`] 之下。
    pub fn cleanup(&self) -> io::Result<()> {
        delete_test_tree(self.reg.base())
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

/// 整棵删除 `HKCU\<base>`，并在前缀键已空时一并删除前缀键。`base` 必须位于 [`TEST_PREFIX`] 之下。
pub fn delete_test_tree(base: &str) -> io::Result<()> {
    let b = base.trim_matches('\\');
    let prefix = format!("{TEST_PREFIX}\\");
    if !b
        .to_ascii_lowercase()
        .starts_with(&prefix.to_ascii_lowercase())
        || b.len() <= prefix.len()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("拒绝删除测试前缀之外的键：{b}"),
        ));
    }
    let path = wide(b);
    match unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, path.as_ptr()) } {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => {}
        e => return Err(err(e)),
    }
    match unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, path.as_ptr()) } {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => {}
        e => return Err(err(e)),
    }
    // 前缀键为空时删除（并发测试的其他根还在时删除会失败，忽略即可）
    let mut parent = WinReg::hkcu(TEST_PREFIX);
    let _ = parent.delete_key_if_empty("");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::takeover::{self, Scope};
    use super::*;
    use std::path::PathBuf;

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\Program Files\Slint 文件管理器\slint-file-manager.exe")
    }

    #[test]
    fn refuses_to_delete_outside_test_prefix() {
        for bad in [
            r"Software\Classes",
            r"Software\Classes\Directory",
            TEST_PREFIX,
            r"Software\slint-file-manager-testX\a",
            "",
        ] {
            let e = delete_test_tree(bad).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{bad}");
        }
    }

    /// 在真实注册表的测试根里演练：布置（含“原来有键无值”“原来没有键”）→ 备份 → 应用 → 持久化往返 → 恢复 → 逐值比对。
    #[test]
    fn rehearsal_in_real_test_root() {
        let root = TestRoot::new().unwrap();
        let mut reg = WinReg::hkcu(root.reg.base());
        assert_eq!(reg.snapshot("").unwrap(), Snapshot::default());
        takeover::rehearse(&mut reg, &exe()).unwrap();
        // 布置内容本身仍在、且只剩布置内容
        assert!(reg.key_exists(r"Directory\shell\Zed\command").unwrap());
        assert!(!reg.key_exists(r"Directory\shell\slintfm.open").unwrap());
        assert!(!reg.key_exists("CLSID").unwrap());
        root.cleanup().unwrap();
        assert!(!WinReg::hkcu(root.reg.base()).key_exists("").unwrap());
    }

    #[test]
    fn real_test_root_conflict_and_types() {
        let root = TestRoot::new().unwrap();
        let mut reg = WinReg::hkcu(root.reg.base());
        // 保留类型：原值为 REG_EXPAND_SZ
        reg.set(
            r"Directory\shell",
            "",
            &RegValue::expand_sz("%SystemRoot%\\none"),
        )
        .unwrap();
        let p = takeover::plan(&exe(), &[Scope::FileSystemFolders]);
        let b = takeover::apply(&mut reg, &p, |_| Ok(())).unwrap();
        assert_eq!(
            takeover::status(&reg, &p).unwrap(),
            takeover::Status::Applied
        );
        // 别人改了 Drive 的默认动词
        reg.set(r"Drive\shell", "", &RegValue::sz("other")).unwrap();
        let r = takeover::restore(&mut reg, &b).unwrap();
        assert_eq!(r.conflicts, vec![r"Drive\shell\(默认)".to_string()]);
        assert_eq!(
            reg.get(r"Directory\shell", "").unwrap(),
            Some(RegValue::expand_sz("%SystemRoot%\\none"))
        );
        assert_eq!(
            reg.get(r"Drive\shell", "").unwrap(),
            Some(RegValue::sz("other"))
        );
        assert!(!reg.key_exists(r"Drive\shell\slintfm.open").unwrap());
    }
}
