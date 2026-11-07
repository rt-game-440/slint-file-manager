# 右键菜单真实窗口自测（Issue #9）：以 harness 特性构建 menu-selftest，打开真实主窗口，
# 用真实 Win32 输入呼出系统右键菜单，核对选择语义、NanaZip 压缩/解压/打开的产物、
# 菜单期间事件循环行为、GDI 增长与菜单结束后无残留状态。编译并行度上限 6。
#
# 前提：交互式桌面会话（需要真实窗口与前台焦点）；已安装 NanaZip 时才会覆盖压缩类命令（否则相关项标为跳过）。
# 只在 %TEMP% 下的临时夹具上执行命令，不触碰用户文件；截图只含本进程自己的窗口。
# 用法：powershell -File scripts/verify-menu.ps1 [-Out 目录] [-GdiRuns N] [-SlintInput]
# 退出码：0 全部通过；1 有检查失败；3 看门狗超时（180 秒无进展）。
param(
    [string]$Out = 'artifacts/verify/menu',
    [int]$GdiRuns = 30,
    [switch]$SlintInput
)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

Write-Host '== cargo build --features harness --bin menu-selftest'
cargo build --features harness --bin menu-selftest --locked --jobs 6 --quiet
if ($LASTEXITCODE -ne 0) { Write-Error "构建失败（退出码 $LASTEXITCODE）"; exit $LASTEXITCODE }

$argv = @("--out=$Out", "--gdi-runs=$GdiRuns")
if ($SlintInput) { $argv += '--slint-input' }
Write-Host "== menu-selftest $($argv -join ' ')"
& (Join-Path (Get-Location) 'target\debug\menu-selftest.exe') @argv
if ($LASTEXITCODE -ne 0) { Write-Error "右键菜单自测失败（退出码 $LASTEXITCODE），详见 $Out\selftest.json"; exit $LASTEXITCODE }
Write-Host '右键菜单自测通过'
