//! 执行一次请求：记录请求、弹框、记录结果，组装稳定结构的 JSON。

use crate::cli::{Api, Request};
use crate::util::bitness;
use crate::{gofn, ifd, log, outcome::Outcome, sbf};
use serde_json::{Value, json};
use std::time::Instant;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetCurrentProcessId;

/// 返回完整记录与退出码（0 = 弹框并正常结束，含取消；1 = 系统接口报错）。
pub fn execute(req: &Request, owner: Option<HWND>) -> (Value, i32) {
    log::line(&format!(
        "request {}",
        serde_json::to_string(&req.to_json()).unwrap_or_default()
    ));
    let t0 = Instant::now();
    let out: Outcome = match req.api {
        Api::Ifd => ifd::run(req, owner),
        Api::Gofn => gofn::run(req, owner),
        Api::Sbf => sbf::run(req, owner),
    };
    let elapsed = t0.elapsed().as_millis() as u64;
    let code = if out.error.is_some() { 1 } else { 0 };
    let record = json!({
        "schema": 1,
        "tool": "dialog-host",
        "api": req.api.name(),
        "scenario": req.scenario.name(),
        "bitness": bitness(),
        "request": req.to_json(),
        "result": out.to_json(),
        // meta 每次运行都不同（进程号、耗时），比对时忽略
        "meta": { "pid": unsafe { GetCurrentProcessId() }, "elapsed_ms": elapsed },
    });
    log::line(&format!(
        "result {}",
        serde_json::to_string(&record["result"]).unwrap_or_default()
    ));
    (record, code)
}
