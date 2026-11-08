# “在文件夹中显示”接管的真实程序验证（Issue #18）：release 版 exe + reveal-probe 测试调用器。
# - 已接管（测试根）：主程序把标签登记为外壳窗口；测试调用器按 Chromium 的方式调用 SHOpenFolderAndSelectItems：
#   已打开的文件夹在本应用对应标签选中（不开资源管理器）、多个文件追加选中、调用后被删除的文件给出提示、
#   未打开的文件夹回退到资源管理器、转发新标签后可被选中；退出时注销。
# - 停用接管（同一测试根恢复原设置）：不登记，调用回退到资源管理器（与 #17 停用后的行为一致）。
# 注册表只用 SFM_TEST_REG_ROOT 指向的 HKCU 测试键（结束时整棵删除），绝不写真实的 HKCU\Software\Classes、HKCR、HKLM；
# 文件只用 %TEMP%\sfm-reveal-test；回退时新开的资源管理器窗口只关闭标题等于目标文件夹的那一个。
# 前置：已构建 target\release\slint-file-manager.exe 与 reveal-probe.exe（scripts/verify.ps1 会先构建）。
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

$exe = Join-Path (Get-Location) 'target\release\slint-file-manager.exe'
$probe = Join-Path (Get-Location) 'target\release\reveal-probe.exe'
foreach ($f in $exe, $probe) { if (-not (Test-Path $f)) { Write-Error "找不到 $f，请先 cargo build --release"; exit 1 } }

$root = Join-Path $env:TEMP 'sfm-reveal-test'
if (Test-Path $root) { Remove-Item $root -Recurse -Force }
$fs = Join-Path $root 'fs'
$A = Join-Path $fs '甲目录'; $B = Join-Path $fs '乙 目录'; $C = Join-Path $fs '丙目录'
foreach ($d in $A, $B, $C) { New-Item -ItemType Directory -Force $d | Out-Null }
$fa = Join-Path $A '说明 文本.txt'; $fa2 = Join-Path $A '第二个.txt'; $fdel = Join-Path $A '将删除.txt'
$fb = Join-Path $B 'b.txt'; $fc = Join-Path $C 'c.txt'
foreach ($f in $fa, $fa2, $fdel, $fb, $fc) { [System.IO.File]::WriteAllText($f, 'x') }
$out = Join-Path $root 'out'
New-Item -ItemType Directory -Force $out | Out-Null
$data = Join-Path $root 'data'
New-Item -ItemType Directory -Force $data | Out-Null
$backup = Join-Path $data 'entry-takeover-backup.json'

$env:SFM_TEST_REG_ROOT = "Software\slint-file-manager-test\verify-reveal-$PID"
Remove-Item Env:SFM_DATA_DIR -ErrorAction SilentlyContinue
Remove-Item Env:SFM_REVEAL_TRACE -ErrorAction SilentlyContinue

$utf8 = [System.Text.UTF8Encoding]::new($false)
$script:n = 0
$script:failed = 0
function Check($ok, $msg) {
    if ($ok) { Write-Host "  通过：$msg" } else { Write-Host "  失败：$msg" -ForegroundColor Red; $script:failed++ }
}
function Start-Exe([string]$file, [string]$cmdline) {
    $script:n++
    $o = Join-Path $out "$($script:n).out.txt"
    $p = Start-Process -FilePath $file -ArgumentList $cmdline -PassThru -NoNewWindow -RedirectStandardOutput $o -RedirectStandardError "$o.err"
    $null = $p.Handle
    @{ p = $p; out = $o }
}
function Wait-Exe($r, [int]$ms = 20000) {
    if (-not $r.p.WaitForExit($ms)) { $r.p.Kill(); Write-Error "进程未在 $ms 毫秒内退出"; exit 1 }
    $r.p.WaitForExit()
    $text = if (Test-Path $r.out) { [System.IO.File]::ReadAllText($r.out, $utf8) } else { '' }
    @{ code = $r.p.ExitCode; text = $text }
}
function Probe([string]$cmdline, [int]$ms = 30000) { Wait-Exe (Start-Exe $probe $cmdline) $ms }
function Read-Shared([string]$path) {
    if (-not (Test-Path $path)) { return '' }
    $f = [System.IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
    try { ([System.IO.StreamReader]::new($f, $utf8)).ReadToEnd() } finally { $f.Dispose() }
}
function Wait-Line($r, [string]$pattern, [int]$ms = 15000) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $ms) {
        if ((Read-Shared $r.out) -match $pattern) { return $true }
        if ($r.p.HasExited) { return $false }
        Start-Sleep -Milliseconds 100
    }
    $false
}
# 统计主程序输出里匹配的行数（等待新的一行出现时用）
function Count-Lines($r, [string]$pattern) { @((Read-Shared $r.out) -split "`n" | Where-Object { $_ -match $pattern }).Count }
function Wait-More($r, [string]$pattern, [int]$before, [int]$ms = 8000) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $ms) {
        if ((Count-Lines $r $pattern) -gt $before) { return $true }
        Start-Sleep -Milliseconds 100
    }
    $false
}
function Last-Line($r, [string]$pattern) { ((Read-Shared $r.out) -split "`n" | Where-Object { $_ -match $pattern } | Select-Object -Last 1) }
function Read-Session($dir) { [System.IO.File]::ReadAllText((Join-Path $dir 'session.json'), $utf8) | ConvertFrom-Json }

try {
    # ---- 1. 已接管 ----
    Write-Host '== 1. 已接管（测试根）：已打开的文件夹在本应用选中，未打开的回退到资源管理器'
    $r = Probe "test-root take `"--exe=$exe`" `"--backup=$backup`""
    Check ($r.code -eq 0 -and $r.text -match '状态 Ok\(Taken\)') "测试根进入已接管状态（$($r.text.Trim())）"

    $app = Start-Exe $exe "--smoke-ms=40000 --data-dir=`"$data`" --open=`"$A`""
    if (-not (Wait-Line $app 'reveal: 已登记 \d+ 个标签')) { Write-Error "主程序没有登记外壳窗口：$(Read-Shared $app.out)"; exit 1 }
    Check ((Read-Shared $app.out) -match 'reveal: 已启用') '已接管时启用“在文件夹中显示”'

    $sel = 'reveal: 已在标签 \d+ 选中'
    $k = Count-Lines $app $sel
    $r = Probe "call `"--file=$fa`" --wait-ms=1500"
    Check ($r.text -match 'hr=0x0' -and $r.text -match 'new_explorer=0') "甲目录（已打开）：调用成功且没有新开资源管理器（$(($r.text -split "`n" | Where-Object { $_ -match '^call:' }) -join ' ')）"
    Check ((Wait-More $app $sel $k) -and (Last-Line $app $sel) -match '说明 文本\.txt') "本应用选中 说明 文本.txt（$(Last-Line $app $sel)）"

    # 转发新标签：转发后新标签也被登记
    $reg = 'reveal: 已登记 \d+ 个标签'
    $k2 = Count-Lines $app $reg
    $f = Wait-Exe (Start-Exe $exe "--data-dir=`"$data`" --entry `"$B`"")
    Check ($f.text -match 'instance: 已转交（ok）') '转发打开“乙 目录”（新标签）'
    Check (Wait-More $app $reg $k2) "新标签被登记（$(Last-Line $app $reg)）"

    $k = Count-Lines $app $sel
    $r = Probe "call `"--file=$fb`" --wait-ms=1500"
    Check ($r.text -match 'hr=0x0' -and $r.text -match 'new_explorer=0') '“乙 目录”（新标签）：没有新开资源管理器'
    Check ((Wait-More $app $sel $k) -and (Last-Line $app $sel) -match 'b\.txt') "本应用切到对应标签并选中 b.txt（$(Last-Line $app $sel)）"

    # 多个文件：第一个替换选中并切回甲目录标签，其余追加
    $k = Count-Lines $app $sel
    $r = Probe "call `"--file=$fa`" `"--file=$fa2`" --wait-ms=1500"
    Check ($r.text -match 'new_explorer=0') '甲目录两个文件：没有新开资源管理器'
    $ok = Wait-More $app $sel ($k + 1)
    $lines = @((Read-Shared $app.out) -split "`n" | Where-Object { $_ -match $sel } | Select-Object -Last 2)
    Check ($ok -and $lines[0] -match '说明 文本\.txt' -and $lines[1] -match '第二个\.txt') "依次选中两个文件（$($lines -join ' / ')）"

    # 调用方取得 PIDL 后文件被删除（被移动／删除的竞态）：本应用提示已不存在
    $gone = 'reveal: “将删除\.txt”已不存在'
    $r = Probe "call `"--file=$fdel`" --delete-before-call --wait-ms=1500"
    Check ($r.text -match 'new_explorer=0') '调用前文件被删除：没有新开资源管理器'
    Check (Wait-Line $app $gone 5000) '本应用提示“将删除.txt”已不存在（可能已被移动或删除）'

    # 文件本来就不存在：调用方（Chromium 方式）解析 PIDL 失败，不会发出调用
    $r = Probe "call `"--file=$(Join-Path $A '从未存在.txt')`" --wait-ms=500"
    Check ($r.code -eq 3) '路径无效：调用方解析失败、不发出调用（本应用与资源管理器都不参与）'

    # 未打开的文件夹：回退到资源管理器（新开窗口由调用器关闭）
    $k = Count-Lines $app $sel
    $r = Probe "call `"--file=$fc`" --wait-ms=5000"
    Check ($r.text -match 'new_explorer=1' -and $r.text -match '已关闭新开的资源管理器窗口“丙目录”') "丙目录（未打开）：回退到资源管理器（$(($r.text -split "`n" | Where-Object { $_ -match '^(call|cleanup):' }) -join ' ')）"
    Start-Sleep -Milliseconds 300
    Check ((Count-Lines $app $sel) -eq $k) '未打开的文件夹不在本应用选中'

    # 后期绑定（Issue #51）：脚本经 Shell.Application.Windows() 枚举，读到本应用标签的属性
    Write-Host '  -- 后期绑定枚举（Shell.Application.Windows()）'
    # 资源管理器的 LocationURL 对非 ASCII 字符不转义（与本应用一致），比较前统一解码
    function Unesc([string]$u) { [uri]::UnescapeDataString($u) }
    $shell = New-Object -ComObject Shell.Application
    $mine = @($shell.Windows() | Where-Object { try { (Unesc $_.LocationURL) -eq (Unesc ([uri]$A).AbsoluteUri) } catch { $false } })
    Check ($mine.Count -eq 1) "枚举到 1 个 LocationURL 为甲目录的项（$($mine.Count)）"
    if ($mine.Count -ne 1) {
        foreach ($x in $shell.Windows()) {
            $u = try { $x.LocationURL } catch { "读取失败：$($_.Exception.Message)" }
            Write-Host "    诊断：枚举项 LocationURL=$u（期望 $(([uri]$A).AbsoluteUri)）"
        }
    }
    if ($mine.Count -ge 1) {
        $w = $mine[0]
        Check ($w.HWND -ne 0 -and $w.HWND -ne $null) "HWND 非空（$($w.HWND)）"
        $app.p.Refresh()
        $mainHwnd = [int64]$app.p.MainWindowHandle
        if ($mainHwnd -ne 0) { Check ([int64]([uint32]([int]$w.HWND)) -eq $mainHwnd) "HWND 等于主程序窗口句柄（$mainHwnd）" }
        Check ($w.LocationName -eq '甲目录') "LocationName=甲目录（$($w.LocationName)）"
        Check ($w.FullName -eq $exe) "FullName=主程序路径（$($w.FullName)）"
        Check ($w.Visible -eq $true) 'Visible=True'
        Check ($w.Document.Folder.Self.Path -eq $A) "Document.Folder.Self.Path=甲目录（$($w.Document.Folder.Self.Path)）"
        Check ($w.Document.Folder.Title -eq '甲目录') "Document.Folder.Title=甲目录（$($w.Document.Folder.Title)）"
        Check ($w.Document.Folder.Self.Name -eq '甲目录') 'Document.Folder.Self.Name=甲目录'
        # 不支持的成员：明确失败（不崩溃，主程序继续运行）
        $bad = try { $null = $w.Document.FocusedItem; 'noerror' } catch { 'error' }
        Check ($bad -ne 'crash') "不支持的 Document 成员读取不崩溃（$bad）"
        Check (-not $app.p.HasExited) '枚举之后主程序仍在运行'
    }
    $other = @($shell.Windows() | Where-Object { try { (Unesc $_.LocationURL) -eq (Unesc ([uri]$B).AbsoluteUri) } catch { $false } })
    Check ($other.Count -eq 1 -and $other[0].LocationName -eq '乙 目录') '乙 目录（另一标签）同样可枚举'
    [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($shell)
    $k = Count-Lines $app $sel
    $r = Probe "call `"--file=$fa`" --wait-ms=1500"
    Check ($r.text -match 'hr=0x0' -and $r.text -match 'new_explorer=0' -and (Wait-More $app $sel $k)) '枚举之后 SHOpenFolderAndSelectItems 仍能定位到本应用'

    $res = Wait-Exe $app 60000
    Check ($res.code -eq 0) '主程序正常退出'
    Check ($res.text -match 'reveal: 已注销') '退出时注销外壳窗口'
    $s = Read-Session $data
    $left = @($s.panes[0].tabs | ForEach-Object { $_.location })
    Check ($left.Count -eq 2 -and $left[0] -eq $A -and $left[1] -eq $B) "左面板标签为 甲、乙（选中不新建标签）：$($left -join ' | ')"
    Check ($s.panes[0].active -eq 0) '最后的请求切回甲目录所在标签'

    # ---- 2. 停用接管 ----
    Write-Host '== 2. 停用接管（恢复原设置）：不登记，调用回退到资源管理器'
    $r = Probe "test-root disable `"--backup=$backup`""
    Check ($r.code -eq 0 -and $r.text -match '干净=true') "停用并恢复原设置（$($r.text.Trim())）"
    $app = Start-Exe $exe "--smoke-ms=9000 --data-dir=`"$data`" --open=`"$A`""
    if (-not (Wait-Line $app 'smoke: 启动成功')) { Write-Error '主程序没有启动'; exit 1 }
    Start-Sleep -Milliseconds 1500
    $r = Probe "call `"--file=$fa`" --wait-ms=5000"
    Check ($r.text -match 'new_explorer=1') "甲目录已在本应用打开，但停用接管后回退到资源管理器（$(($r.text -split "`n" | Where-Object { $_ -match '^call:' }) -join ' ')）"
    $res = Wait-Exe $app 30000
    Check ($res.code -eq 0) '主程序正常退出'
    Check ($res.text -notmatch 'reveal:') '停用接管时没有任何登记'
}
finally {
    $r = Probe 'test-root delete'
    Write-Host "  清理：$($r.text.Trim())"
    Remove-Item Env:SFM_TEST_REG_ROOT
}
if ($script:failed -gt 0) { Write-Error "在文件夹中显示：验证失败 $($script:failed) 项（各进程输出保留在 $out）"; exit 1 }
Remove-Item $root -Recurse -Force
Write-Host '在文件夹中显示：验证通过'
