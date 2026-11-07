# 真实回收站测试的看门狗运行器：只在 %TEMP%\sfm-unit 夹具上操作，超时会强制结束进程并报告弹出的窗口标题，
# 结束后统计回收站里是否残留本测试夹具（原位置在 %TEMP%\sfm-unit\recycle- 下）的项目；残留用 sweep_stale_fixtures 清扫。
# 用法：powershell -File scripts/run-recycle-tests.ps1 [-Filter recycle] [-TimeoutSec 90]
param([string]$Filter = 'recycle', [int]$TimeoutSec = 90, [switch]$Ignored)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

$ErrorActionPreference = 'Continue'
cargo test --lib --jobs 6 --no-run 2>&1 | Select-Object -Last 2
$ErrorActionPreference = 'Stop'
$exe = Get-ChildItem target\debug\deps\slint_file_manager-*.exe | Where-Object { $_.Extension -eq '.exe' } |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1

$sh = New-Object -ComObject Shell.Application
function Count-Bin {
    $b = $sh.NameSpace(10)
    $all = @($b.Items())
    $mine = @($all | Where-Object { $b.GetDetailsOf($_, 1) -like '*sfm-unit\recycle-*' })
    "回收站总数 $($all.Count)，本测试夹具残留 $($mine.Count)"
}

$out = Join-Path $env:TEMP 'sfm-recycle-test.out'
$err = Join-Path $env:TEMP 'sfm-recycle-test.err'
Count-Bin
$p = Start-Process -FilePath $exe.FullName -ArgumentList (@($Filter, '--nocapture', '--test-threads=1') + $(if ($Ignored) { @('--ignored') } else { @() })) `
    -PassThru -RedirectStandardOutput $out -RedirectStandardError $err -WindowStyle Hidden
$ok = $p.WaitForExit($TimeoutSec * 1000)
if (-not $ok) {
    $p.Refresh()
    "超时：进程窗口标题 = [$($p.MainWindowTitle)]"
    Stop-Process -Id $p.Id -Force
    $p.WaitForExit()
}
Get-Content $err -ErrorAction SilentlyContinue | Select-Object -Last 15
Get-Content $out -ErrorAction SilentlyContinue | Select-Object -Last 15
Remove-Item $out, $err -ErrorAction SilentlyContinue
Count-Bin
if ($ok) { exit $p.ExitCode } else { exit 124 }
