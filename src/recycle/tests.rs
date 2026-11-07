//! 真实回收站闭环。**只在 `%TEMP%\sfm-unit` 下的临时夹具上操作**：每个夹具目录名带进程号与纳秒时间戳，
//! 断言与清理都只认“原位置在该目录内”的回收站项目，绝不触碰用户已有的回收站内容。

use super::*;
use crate::fileops::{DeleteMode, SystemBackend, run_delete};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn fixture(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir()
        .join("sfm-unit")
        .join(format!("recycle-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&d).unwrap();
    d
}

/// 回收站里原位置位于 `dir` 之内的项目（只看自己的）。
fn mine(dir: &Path) -> Vec<BinItem> {
    let key = dir.display().to_string().to_lowercase();
    list()
        .unwrap()
        .into_iter()
        .filter(|i| i.original_dir.to_lowercase().starts_with(&key))
        .collect()
}

/// 清理：永久删除自己放进回收站的项目与夹具目录，并确认没有残留。
fn cleanup(dir: &Path) {
    let left = mine(dir);
    if !left.is_empty() {
        purge(&left).expect("清理自己的回收站项目");
    }
    assert!(mine(dir).is_empty(), "清理后回收站里不应再有本测试的项目");
    let _ = Command::new("icacls")
        .args([dir.to_str().unwrap(), "/remove:d", "*S-1-1-0", "/q"])
        .output();
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn can_recycle_rules() {
    assert!(can_recycle(&std::env::temp_dir().join("a.txt")).is_ok());
    assert!(can_recycle(Path::new(r"\\server\share\a.txt")).is_err());
    let long = format!(r"C:\{}", "长".repeat(300));
    assert!(can_recycle(Path::new(&long)).is_err());
    assert_eq!(
        recycle(Path::new(&long)).unwrap_err().kind,
        FailKind::NotFound,
        "不存在的项目先报不存在"
    );
}

#[test]
fn missing_item_is_not_found() {
    let d = fixture("missing");
    let e = recycle(&d.join("没有这个文件.txt")).unwrap_err();
    assert_eq!(e.kind, FailKind::NotFound);
    cleanup(&d);
}

/// 删入 → 浏览 → 恢复 → 冲突 → 原目录消失 → 权限不足 的完整闭环。
#[test]
fn real_recycle_bin_roundtrip() {
    let d = fixture("loop");
    let file = d.join("报告 最终版.txt");
    fs::write(&file, "内容甲").unwrap();
    let folder = d.join("资料夹");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("里面.txt"), "内容乙").unwrap();

    // 删入：回收站里出现两项，原路径消失
    eprintln!("[步骤] 删入");
    recycle(&file).expect("文件进回收站");
    recycle(&folder).expect("文件夹进回收站");
    assert!(!file.exists() && !folder.exists());

    // 浏览：名称、原位置、删除时间、大小、是否文件夹
    eprintln!("[步骤] 浏览");
    let items = mine(&d);
    assert_eq!(items.len(), 2, "{items:?}");
    let f = items.iter().find(|i| !i.is_dir).unwrap();
    assert_eq!(f.name, "报告 最终版.txt");
    assert_eq!(
        f.original_dir.to_lowercase(),
        d.display().to_string().to_lowercase()
    );
    assert_eq!(f.size, "内容甲".len() as u64);
    let age = SystemTime::now()
        .duration_since(f.deleted.expect("有删除时间"))
        .unwrap_or_default();
    assert!(age < Duration::from_secs(600), "删除时间应是刚才：{age:?}");
    let dir_item = items.iter().find(|i| i.is_dir).unwrap();
    assert_eq!(dir_item.name, "资料夹");

    // 恢复单项：内容完整，回收站里该项消失，另一项仍在
    eprintln!("[步骤] 恢复单项");
    restore(f).expect("恢复文件");
    assert_eq!(fs::read_to_string(&file).unwrap(), "内容甲");
    assert_eq!(mine(&d).len(), 1);

    // 恢复文件夹（带内容）
    eprintln!("[步骤] 恢复文件夹");
    restore(dir_item).expect("恢复文件夹");
    assert_eq!(
        fs::read_to_string(folder.join("里面.txt")).unwrap(),
        "内容乙"
    );
    assert!(mine(&d).is_empty());

    // 同名冲突：不覆盖，项目留在回收站；清掉冲突后可恢复
    eprintln!("[步骤] 同名冲突");
    recycle(&file).unwrap();
    fs::write(&file, "新文件").unwrap();
    let it = mine(&d).remove(0);
    assert_eq!(restore(&it).unwrap_err().kind, FailKind::Conflict);
    assert_eq!(fs::read_to_string(&file).unwrap(), "新文件", "没有被覆盖");
    assert_eq!(mine(&d).len(), 1, "冲突项仍在回收站");
    fs::remove_file(&file).unwrap();
    restore(&it).expect("冲突解除后恢复");
    assert_eq!(fs::read_to_string(&file).unwrap(), "内容甲");

    // 原目录消失：不自动建目录，项目留在回收站；目录回来后可恢复
    eprintln!("[步骤] 原目录消失");
    let sub = d.join("会消失的目录");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("孤儿.txt"), "孤").unwrap();
    recycle(&sub.join("孤儿.txt")).unwrap();
    fs::remove_dir(&sub).unwrap();
    let it = mine(&d).remove(0);
    assert_eq!(restore(&it).unwrap_err().kind, FailKind::DestMissing);
    assert!(!sub.exists(), "不得自动重建原目录");
    assert_eq!(mine(&d).len(), 1);
    fs::create_dir(&sub).unwrap();
    restore(&it).expect("目录回来后恢复");
    assert!(sub.join("孤儿.txt").is_file());

    // 权限不足：目录拒绝新建文件，恢复失败且项目留在回收站；放开后可恢复
    eprintln!("[步骤] 权限不足");
    recycle(&sub.join("孤儿.txt")).unwrap();
    let it = mine(&d).remove(0);
    let out = Command::new("icacls")
        .args([sub.to_str().unwrap(), "/deny", "*S-1-1-0:(WD,AD)", "/q"])
        .output()
        .unwrap();
    assert!(out.status.success(), "设置拒绝写入的 ACL 失败");
    let err = restore(&it).unwrap_err();
    assert!(
        matches!(err.kind, FailKind::AccessDenied | FailKind::Other),
        "权限不足应失败：{err:?}"
    );
    assert!(!sub.join("孤儿.txt").exists(), "失败项不得出现在原位置");
    assert_eq!(mine(&d).len(), 1, "失败项仍在回收站，不假消失");
    let _ = Command::new("icacls")
        .args([sub.to_str().unwrap(), "/remove:d", "*S-1-1-0", "/q"])
        .output();
    restore(&it).expect("放开权限后恢复");
    assert_eq!(fs::read_to_string(sub.join("孤儿.txt")).unwrap(), "孤");

    // 已不在回收站的项目：再次恢复报“已不存在”
    eprintln!("[步骤] 已不在回收站的项目");
    recycle(&sub.join("孤儿.txt")).unwrap();
    let it = mine(&d).remove(0);
    restore(&it).unwrap();
    // 原位置已有该项（刚还原回来）：如实报同名冲突，不覆盖
    assert_eq!(restore(&it).unwrap_err().kind, FailKind::Conflict);
    // 原位置腾空后再还原这个已过期的标识：回收站里已经没有它
    fs::remove_file(sub.join("孤儿.txt")).unwrap();
    assert_eq!(restore(&it).unwrap_err().kind, FailKind::NotFound);

    cleanup(&d);
}

/// 一批里混有可回收与不存在的项目：逐项如实汇报，成功的进回收站，失败的保持原样。
#[test]
fn real_partial_batch_report() {
    let d = fixture("batch");
    let ok1 = d.join("甲.txt");
    let ok2 = d.join("乙.txt");
    fs::write(&ok1, "1").unwrap();
    fs::write(&ok2, "2").unwrap();
    let ghost = d.join("丙（不存在）.txt");
    let r = run_delete(
        &SystemBackend,
        &[ok1.clone(), ghost, ok2.clone()],
        DeleteMode::Recycle,
    );
    assert_eq!((r.ok_count(), r.fail_count()), (2, 1));
    assert!(!ok1.exists() && !ok2.exists());
    assert_eq!(mine(&d).len(), 2);
    let s = r.summary();
    assert!(
        s.line.starts_with("部分完成：2 项已移到回收站，1 项"),
        "{}",
        s.line
    );
    cleanup(&d);
}

/// 永久删除不进回收站。
#[test]
fn real_permanent_delete_skips_bin() {
    let d = fixture("perm");
    let f = d.join("永久.txt");
    fs::write(&f, "x").unwrap();
    let r = run_delete(
        &SystemBackend,
        std::slice::from_ref(&f),
        DeleteMode::Permanent,
    );
    assert_eq!(r.fail_count(), 0);
    assert!(!f.exists());
    assert!(mine(&d).is_empty(), "永久删除的项目不应出现在回收站");
    cleanup(&d);
}

/// 清扫历史遗留：早先异常中断的运行可能把夹具项目留在回收站。只处理原位置在
/// `%TEMP%\sfm-unit\recycle-` 之下且不属于当前进程的项目；用户自己的回收站内容一概不碰。
/// 手动运行：`cargo test --lib -- --ignored sweep_stale_fixtures`。
#[test]
#[ignore]
fn sweep_stale_fixtures() {
    let prefix = std::env::temp_dir()
        .join("sfm-unit")
        .join("recycle-")
        .display()
        .to_string()
        .to_lowercase();
    let me = format!("-{}-", std::process::id());
    let stale: Vec<BinItem> = list()
        .unwrap()
        .into_iter()
        .filter(|i| {
            let d = i.original_dir.to_lowercase();
            d.starts_with(&prefix) && !d.contains(&me)
        })
        .collect();
    println!("清扫 {} 项历史夹具遗留", stale.len());
    for i in &stale {
        println!("  {} <- {}", i.name, i.original_dir);
    }
    if !stale.is_empty() {
        purge(&stale).expect("清扫");
    }
}
