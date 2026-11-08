# Issue #23：测试宿主的构建、静态检查、带按钮窗口冒烟、与基准的比对（不强制进 scripts/verify.ps1）。
# 用法：powershell -File scripts/verify-dialog-host.ps1 [-Quick] [-SkipBuild]
#   默认：fmt、clippy、x64 与 x86 发布构建、比对入口自测（selftest）、带按钮窗口冒烟、全部用例与基准比对。
#   -Quick：只比对 8 个代表用例（方式×位数各覆盖），约 1 分钟。
# 需要已解锁的交互桌面（会短暂弹出系统文件选择对话框；只操作本测试宿主进程弹出的窗口）。
# 退出码：0 通过；1 失败；2 该系统没有基准（未覆盖，比对无法判断）。
param([switch]$Quick, [switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$HostDir = Join-Path $Root 'tools\dialog-host'
$Utf8 = New-Object System.Text.UTF8Encoding($false)
$ps = (Get-Process -Id $PID).Path

function Step($name, [scriptblock]$cmd) {
    Write-Host "== $name"
    & $cmd
    if ($LASTEXITCODE -ne 0) { Write-Host "$name 失败（退出码 $LASTEXITCODE）" -ForegroundColor Red; exit 1 }
}

if (-not $SkipBuild) {
    Push-Location $HostDir
    try {
        Step 'cargo fmt --check (dialog-host)' { cargo fmt --check }
        Step 'cargo clippy (dialog-host, x64)' { cargo clippy --locked --jobs 6 -- -D warnings }
        Step '构建 x64' { cargo build --release --locked --jobs 6 }
        Step '构建 x86 (i686-pc-windows-msvc)' { cargo build --release --locked --jobs 6 --target i686-pc-windows-msvc }
    }
    finally { Pop-Location }
}

Step '比对入口自测（人为制造字段差异，不弹框）' { & $ps -NoProfile -File (Join-Path $PSScriptRoot 'dialog-baseline.ps1') -Mode selftest }

# ---- 带按钮窗口冒烟：点第 1 个预设按钮，驱动弹出的系统对话框，检查结果文本框与日志
Write-Host '== 带按钮窗口冒烟（x64 与 x86）'
Add-Type -TypeDefinition ([IO.File]::ReadAllText((Join-Path $PSScriptRoot 'dialog-driver.cs'), $Utf8))
foreach ($b in 'x64', 'x86') {
    $exe = if ($b -eq 'x86') { Join-Path $HostDir 'target\i686-pc-windows-msvc\release\dialog-host.exe' } else { Join-Path $HostDir 'target\release\dialog-host.exe' }
    $work = Join-Path $env:TEMP 'sfm-dialog-host\gui-smoke'
    $log = Join-Path $Root ('logs\' + (Get-Date).ToString('yyyy-MM-dd HH-mm-ss') + ' gui-smoke.log')
    $psi = New-Object Diagnostics.ProcessStartInfo $exe
    $psi.Arguments = "gui `"--fixture=$work\fixture`" `"--log-file=$log`""
    $psi.UseShellExecute = $false
    $p = [Diagnostics.Process]::Start($psi)
    try {
        $id = [uint32]$p.Id
        $main = [DialogDriver]::FindTop($id, 'DialogHostMain', 10000)
        if ($main -eq [IntPtr]::Zero) { throw "[$b] 带按钮窗口没有出现" }
        if (-not [DialogDriver]::Click($main, 100)) { throw "[$b] 找不到第 1 个预设按钮" }
        $dlg = [DialogDriver]::WaitDialog($id, 15000, [IntPtr]::Zero)
        if ($dlg -eq [IntPtr]::Zero) { throw "[$b] 点按钮后没有弹出对话框" }
        Start-Sleep -Milliseconds 600
        $edit = [DialogDriver]::FileNameEdit($dlg)
        [void][DialogDriver]::SetText($edit, 'alpha.txt')
        [void][DialogDriver]::Click($dlg, 1)
        $text = ''
        $end = [Environment]::TickCount + 8000
        while ([Environment]::TickCount -lt $end -and $text -notmatch 'alpha\.txt') {
            Start-Sleep -Milliseconds 200
            $e = [DialogDriver]::Child($main, 0, 'Edit')
            if ($e -ne [IntPtr]::Zero) { $text = [DialogDriver]::EditText($e) }
        }
        if ($text -notmatch 'alpha\.txt' -or $text -notmatch '"cancelled": false') { throw "[$b] 结果文本框里没有 alpha.txt 的返回结果：$text" }
        Write-Host "  [$b] 点击预设 → 系统对话框 → 结果 JSON 显示在窗口里，通过"
    }
    finally { if (-not $p.HasExited) { $p.Kill() } }
    if (-not (Test-Path $log) -or (Select-String -Path $log -Pattern 'alpha.txt' -Quiet) -ne $true) { Write-Host "[$b] 日志里没有结果" -ForegroundColor Red; exit 1 }
}

# ---- 与基准比对
$args2 = @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'dialog-baseline.ps1'), '-Mode', 'compare', '-SkipBuild')
if ($Quick) { $args2 += @('-Filter', '^(ifd-open-multi|ifd-save-overwrite-yes|ifd-folder-cancel|gofn-open-filter-switch|gofn-save-default-ext|gofn-save-overwrite-no|sbf-folder-new|sbf-folder-legacy-initdir)$') }
Write-Host '== 与基准比对（真实对话框）'
& $ps @args2
$code = $LASTEXITCODE
if ($code -eq 2) { Write-Host '该系统没有基准：未覆盖（不算失败，但不能宣称已验证）。' -ForegroundColor Yellow; exit 2 }
if ($code -ne 0) { Write-Host '与基准存在差异或用例执行失败（见上）。' -ForegroundColor Red; exit 1 }
Write-Host '测试宿主验证通过。'
exit 0
