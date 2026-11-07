# 会话恢复的真实程序验证：用 release 版 exe 加 --data-dir=<临时目录> 反复启动并自动退出，
# 检查退出时写回的 session.json（恢复 -> 退出 -> 保存的闭环），以及缺失路径、会话损坏、版本不兼容的降级。
# 全部使用 %TEMP%\sfm-session-test，不读写用户真实的 %LOCALAPPDATA%\slint-file-manager。
# 前置：已构建 target\release\slint-file-manager.exe（scripts/verify.ps1 会先构建）。
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

$exe = Join-Path (Get-Location) 'target\release\slint-file-manager.exe'
if (-not (Test-Path $exe)) { Write-Error "找不到 $exe，请先 cargo build --release"; exit 1 }

$root = Join-Path $env:TEMP 'sfm-session-test'
if (Test-Path $root) { Remove-Item $root -Recurse -Force }
$fs = Join-Path $root 'fs'
foreach ($n in '甲目录', '乙 目录', '丙目录') { New-Item -ItemType Directory -Force (Join-Path $fs $n) | Out-Null }
$A = Join-Path $fs '甲目录'; $B = Join-Path $fs '乙 目录'; $C = Join-Path $fs '丙目录'
$gone = Join-Path $fs '已删除的目录'
$noDrive = 'Z:\sfm-不存在的磁盘\目录'

$utf8 = [System.Text.UTF8Encoding]::new($false)
function Tab($loc, $key = 'name', $asc = $true, $view = 'grid') {
    @{ location = $loc; sort = @{ key = $key; ascending = $asc }; view = $view; back = @(); forward = @() }
}
function Write-Session($dir, $obj) {
    New-Item -ItemType Directory -Force $dir | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $dir 'session.json'), ($obj | ConvertTo-Json -Depth 10), $utf8)
}
function Run-App($dir) {
    $p = Start-Process -FilePath $exe -ArgumentList '--smoke-ms=1500', "--data-dir=$dir" -PassThru -NoNewWindow
    $null = $p.Handle
    if (-not $p.WaitForExit(20000)) { $p.Kill(); Write-Error '进程未在 20 秒内退出'; exit 1 }
    if ($p.ExitCode -ne 0) { Write-Error "退出码 $($p.ExitCode)"; exit 1 }
}
function Read-Session($dir) {
    $text = [System.IO.File]::ReadAllText((Join-Path $dir 'session.json'), $utf8)
    $text | ConvertFrom-Json
}
$script:failed = 0
function Check($ok, $msg) {
    if ($ok) { Write-Host "  通过：$msg" } else { Write-Host "  失败：$msg" -ForegroundColor Red; $script:failed++ }
}

# ---- 1. 恢复 -> 退出 -> 保存 闭环 ----
Write-Host '== 1. 位置／排序／视图／活动标签／单栏隐藏侧'
$d1 = Join-Path $root 'roundtrip'
Write-Session $d1 @{
    version = 1; split = $false; active_pane = 1
    panes = @(
        @{ active = 1; tabs = @((Tab $A 'size' $false 'list'), (Tab $B)) },
        @{ active = 0; tabs = @((Tab $C 'modified' $true 'grid'), (Tab $A)) }
    )
}
Run-App $d1
$s = Read-Session $d1
Check ($s.version -eq 1) '版本号为 1'
Check ((-not $s.split) -and $s.active_pane -eq 1) '单栏与活动面板（右）保留'
Check ($s.panes[0].tabs.Count -eq 2 -and $s.panes[1].tabs.Count -eq 2) '两侧各 2 个标签（隐藏侧也保留）'
Check ($s.panes[0].active -eq 1 -and $s.panes[1].active -eq 0) '各面板活动标签保留'
Check ($s.panes[0].tabs[0].location -eq $A -and $s.panes[0].tabs[1].location -eq $B -and $s.panes[1].tabs[0].location -eq $C) '位置保留（含中文与空格）'
Check ($s.panes[0].tabs[0].sort.key -eq 'size' -and (-not $s.panes[0].tabs[0].sort.ascending) -and $s.panes[0].tabs[0].view -eq 'list') '标签 1 的排序与视图独立保留'
Check ($s.panes[1].tabs[0].sort.key -eq 'modified' -and $s.panes[0].tabs[1].view -eq 'grid') '其他标签的排序与视图不受影响'

# ---- 2. 位置缺失 ----
Write-Host '== 2. 位置缺失（已删除目录、不存在的磁盘）：不崩溃、标签保留'
$d2 = Join-Path $root 'missing'
Write-Session $d2 @{
    version = 1; split = $true; active_pane = 0
    panes = @(
        @{ active = 0; tabs = @((Tab $gone), (Tab $noDrive), (Tab $A)) },
        @{ active = 1; tabs = @((Tab $B), (Tab $gone)) }
    )
}
Run-App $d2
$s = Read-Session $d2
Check ($s.panes[0].tabs.Count -eq 3 -and $s.panes[1].tabs.Count -eq 2) '失败的标签没有丢'
Check ($s.panes[0].tabs[0].location -eq $gone -and $s.panes[0].tabs[1].location -eq $noDrive) '失效位置原样保留，等路径恢复后可再用'
Check ($s.split -and $s.panes[1].active -eq 1) '双栏与活动标签保留'

# ---- 3. 会话损坏 ----
Write-Host '== 3. 会话文件损坏：备份并使用默认会话'
$d3 = Join-Path $root 'corrupt'
New-Item -ItemType Directory -Force $d3 | Out-Null
$bad = '{ 这不是合法的 JSON, "version": 1'
[System.IO.File]::WriteAllText((Join-Path $d3 'session.json'), $bad, $utf8)
Run-App $d3
$bk = @(Get-ChildItem $d3 -Filter 'session.backup-*-corrupt.json')
Check ($bk.Count -eq 1) '原文件被移成 session.backup-…-corrupt.json'
Check ($bk.Count -eq 1 -and [System.IO.File]::ReadAllText($bk[0].FullName, $utf8) -eq $bad) '备份保留原始内容'
$s = Read-Session $d3
Check ($s.version -eq 1 -and $s.panes[0].tabs.Count -eq 1 -and $s.panes[1].tabs.Count -eq 1) '随后写入的是新的有效默认会话'

# ---- 4. 版本不兼容 ----
Write-Host '== 4. 会话来自更新版本：保留原文件，使用默认会话'
$d4 = Join-Path $root 'newer'
New-Item -ItemType Directory -Force $d4 | Out-Null
$newer = '{"version": 99, "panes": [], "future": true}'
[System.IO.File]::WriteAllText((Join-Path $d4 'session.json'), $newer, $utf8)
Run-App $d4
$bk = @(Get-ChildItem $d4 -Filter 'session.backup-*-v99.json')
Check ($bk.Count -eq 1 -and [System.IO.File]::ReadAllText($bk[0].FullName, $utf8) -eq $newer) '原文件完整备份为 session.backup-…-v99.json'

# ---- 5. 无会话时默认行为；--no-session 不写盘 ----
Write-Host '== 5. 首次运行与 --no-session'
$d5 = Join-Path $root 'first'
Run-App $d5
Check (Test-Path (Join-Path $d5 'session.json')) '首次运行退出时写入会话'
$s = Read-Session $d5
Check ($s.split -and $s.panes[0].tabs.Count -eq 1 -and $s.panes[1].tabs.Count -eq 1) '默认双栏，每侧一个标签'
$d6 = Join-Path $root 'nosession'
$p = Start-Process -FilePath $exe -ArgumentList '--smoke-ms=1000', '--no-session', "--data-dir=$d6" -PassThru -NoNewWindow
$null = $p.Handle
if (-not $p.WaitForExit(20000)) { $p.Kill(); Write-Error '进程未退出'; exit 1 }
Check ($p.ExitCode -eq 0 -and -not (Test-Path (Join-Path $d6 'session.json'))) '--no-session 不读写会话'

Remove-Item $root -Recurse -Force
if ($script:failed -gt 0) { Write-Error "会话验证失败 $($script:failed) 项"; exit 1 }
Write-Host '会话验证通过'
