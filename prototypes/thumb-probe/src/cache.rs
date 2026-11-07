//! 原型用的持久缩略图缓存：每个条目一个文件，按键的前两位十六进制分目录。
//!
//! 只为测出“重复访问”的下限并比较两种存储格式，不是 #10 的缓存设计：没有容量上限、淘汰、清空入口与损坏恢复。
//! 键 = FNV-1a(路径 | 大小 | 修改时间 | 缩略图边长)，文件重命名、内容变化都会换键（旧条目留在盘上，等待淘汰）。

use crate::common::Thumb;
use crate::webp::{self, DecodeOpts};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// 8 字节头 + 原始 RGBA：读回无需解码，体积大
    Raw,
    /// 有损 WebP（质量 80）：体积小一个数量级，读回需解码一张小图
    Webp,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        match s {
            "raw" => Some(Format::Raw),
            "webp" => Some(Format::Webp),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Format::Raw => "raw",
            Format::Webp => "webp",
        }
    }
}

pub struct DiskCache {
    dir: PathBuf,
    pub format: Format,
}

pub fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for &b in *p {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

impl DiskCache {
    pub fn new(dir: PathBuf, format: Format) -> DiskCache {
        let _ = fs::create_dir_all(&dir);
        DiskCache { dir, format }
    }

    pub fn clear(&self) {
        let _ = fs::remove_dir_all(&self.dir);
        let _ = fs::create_dir_all(&self.dir);
    }

    fn path(&self, key: u64) -> PathBuf {
        self.dir
            .join(format!("{:02x}", key >> 56))
            .join(format!("{key:016x}.{}", self.format.name()))
    }

    pub fn entries(&self) -> (usize, u64) {
        let (mut n, mut bytes) = (0, 0);
        if let Ok(rd) = fs::read_dir(&self.dir) {
            for d in rd.flatten() {
                if let Ok(rd2) = fs::read_dir(d.path()) {
                    for f in rd2.flatten() {
                        if let Ok(m) = f.metadata() {
                            n += 1;
                            bytes += m.len();
                        }
                    }
                }
            }
        }
        (n, bytes)
    }

    pub fn get(&self, key: u64) -> Option<Thumb> {
        let b = fs::read(self.path(key)).ok()?;
        match self.format {
            Format::Raw => {
                if b.len() < 8 {
                    return None;
                }
                let w = u32::from_le_bytes(b[0..4].try_into().ok()?);
                let h = u32::from_le_bytes(b[4..8].try_into().ok()?);
                if b.len() != 8 + w as usize * h as usize * 4 {
                    return None; // 长度对不上：视为损坏，当作未命中
                }
                Some(Thumb {
                    w,
                    h,
                    rgba: b[8..].to_vec(),
                })
            }
            Format::Webp => {
                let f = webp::features(&b).ok()?;
                webp::decode(&b, f.w, f.h, DecodeOpts::default()).ok()
            }
        }
    }

    /// 写临时文件再改名，避免读到写了一半的条目。失败静默（缓存只是加速）。
    pub fn put(&self, key: u64, t: &Thumb) {
        let p = self.path(key);
        let _ = fs::create_dir_all(p.parent().unwrap());
        let bytes = match self.format {
            Format::Raw => {
                let mut v = Vec::with_capacity(8 + t.rgba.len());
                v.extend(t.w.to_le_bytes());
                v.extend(t.h.to_le_bytes());
                v.extend(&t.rgba);
                v
            }
            Format::Webp => webp::encode(&t.rgba, t.w, t.h, 80.0),
        };
        let tmp = p.with_extension("tmp");
        if fs::File::create(&tmp)
            .and_then(|mut f| f.write_all(&bytes))
            .is_ok()
        {
            let _ = fs::rename(&tmp, &p);
        }
    }
}
