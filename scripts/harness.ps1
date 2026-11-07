# 调试 harness 入口：构建（--features harness，jobs ≤ 6）后运行子命令。
# 用法：powershell -File scripts/harness.ps1 <tree|run|snap|bless|compare|frames|review> [--选项 值 ...]
# 详见 docs/debug-harness.md。退出码由子命令决定（compare：0 一致，1 回归，2 无法判断，3 截图不稳定）。
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

cargo build --features harness --bin harness --locked --jobs 6 --quiet
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& (Join-Path (Get-Location) 'target\debug\harness.exe') @args
exit $LASTEXITCODE
