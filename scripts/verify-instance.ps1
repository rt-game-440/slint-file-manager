# 单实例与外部打开请求的真实程序验证（Issue #17）：用 release 版 exe 加 --data-dir=<临时目录>。
# - 主实例运行时，第二个进程用 `--entry <目标>` 转发：同一文件夹复用标签、新文件夹新开标签、不存在的路径答复失败、
#   不带目标（Win+E）只激活窗口、explorer 风格参数（/select,）打开所在文件夹；主实例退出时写回的 session.json 核对标签。
# - 没有主实例时 `--entry` 自己成为主实例并打开目标；不同数据目录互不干扰。
# - 主实例无响应（假管道服务端只接受连接不答复）：按 SFM_INSTANCE_TIMEOUT_MS 超时后独立启动，且不写会话。
# 全部使用 %TEMP%\sfm-instance-test；注册表根用 SFM_TEST_REG_ROOT 指到不存在的测试键（只读，不创建），
# 绝不读写用户真实的 %LOCALAPPDATA%\slint-file-manager 与 HKCU\Software\Classes。
# 前置：已构建 target\release\slint-file-manager.exe（scripts/verify.ps1 会先构建）。
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

$exe = Join-Path (Get-Location) 'target\release\slint-file-manager.exe'
if (-not (Test-Path $exe)) { Write-Error "找不到 $exe，请先 cargo build --release"; exit 1 }

$root = Join-Path $env:TEMP 'sfm-instance-test'
if (Test-Path $root) { Remove-Item $root -Recurse -Force }
$fs = Join-Path $root 'fs'
foreach ($n in '甲目录', '乙 目录', '丙目录') { New-Item -ItemType Directory -Force (Join-Path $fs $n) | Out-Null }
$A = Join-Path $fs '甲目录'; $B = Join-Path $fs '乙 目录'; $C = Join-Path $fs '丙目录'
$file = Join-Path $A '说明 文本.txt'
[System.IO.File]::WriteAllText($file, 'x')
$gone = Join-Path $fs '不存在的目录'
$out = Join-Path $root 'out'
New-Item -ItemType Directory -Force $out | Out-Null

$env:SFM_TEST_REG_ROOT = "Software\slint-file-manager-test\verify-instance-$PID"
Remove-Item Env:SFM_DATA_DIR -ErrorAction SilentlyContinue
Remove-Item Env:SFM_INSTANCE_TIMEOUT_MS -ErrorAction SilentlyContinue

$utf8 = [System.Text.UTF8Encoding]::new($false)
$script:n = 0
$script:failed = 0
function Check($ok, $msg) {
    if ($ok) { Write-Host "  通过：$msg" } else { Write-Host "  失败：$msg" -ForegroundColor Red; $script:failed++ }
}
# 启动 exe；参数原样拼成命令行（调用方负责引号）。返回进程与输出文件。
function Start-App([string]$cmdline) {
    $script:n++
    $o = Join-Path $out "$($script:n).out.txt"
    $p = Start-Process -FilePath $exe -ArgumentList $cmdline -PassThru -NoNewWindow -RedirectStandardOutput $o -RedirectStandardError "$o.err"
    $null = $p.Handle
    @{ p = $p; out = $o }
}
function Wait-App($r, [int]$ms = 20000) {
    if (-not $r.p.WaitForExit($ms)) { $r.p.Kill(); Write-Error "进程未在 $ms 毫秒内退出"; exit 1 }
    $r.p.WaitForExit()
    $text = if (Test-Path $r.out) { [System.IO.File]::ReadAllText($r.out, $utf8) } else { '' }
    @{ code = $r.p.ExitCode; text = $text }
}
function Run-App([string]$cmdline, [int]$ms = 20000) { Wait-App (Start-App $cmdline) $ms }
# 读取仍被进程写入的输出文件（共享读写打开）
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
function Read-Session($dir) {
    [System.IO.File]::ReadAllText((Join-Path $dir 'session.json'), $utf8) | ConvertFrom-Json
}
function Locs($s, $p) { @($s.panes[$p].tabs | ForEach-Object { $_.location }) }

# ---- 1. 主实例运行时转发 ----
Write-Host '== 1. 第二个进程转发请求：复用／新建标签、失败答复、激活、explorer 风格参数'
$d1 = Join-Path $root 'data1'
$primary = Start-App "--smoke-ms=12000 --data-dir=`"$d1`" --open=`"$C`""
if (-not (Wait-Line $primary 'smoke: 启动成功')) { Write-Error '主实例没有启动'; exit 1 }
Check ((Read-Shared $primary.out) -match 'instance: 主实例') '第一个进程成为主实例'

$r = Run-App "--data-dir=`"$d1`" --entry `"$A`""
Check ($r.code -eq 0 -and $r.text -match 'instance: 已转交（ok）') "打开甲目录：已转交并成功（$($r.text.Trim())）"
$r = Run-App "--data-dir=`"$d1`" --entry `"$A`""
Check ($r.text -match 'instance: 已转交（ok）') '再次打开甲目录：已转交'
$r = Run-App "--data-dir=`"$d1`" --entry `"$B`""
Check ($r.text -match 'instance: 已转交（ok）') '打开“乙 目录”（含空格）：已转交'
$r = Run-App "--data-dir=`"$d1`" --entry `"$gone`""
Check ($r.code -eq 0 -and $r.text -match 'instance: 已转交（fail）.*不存在的目录') "不存在的路径：主实例答复失败并说明（$($r.text.Trim())）"
$r = Run-App "--data-dir=`"$d1`" --entry"
Check ($r.text -match 'instance: 已转交（ok）：已激活窗口') 'Win+E 形式（无目标）：激活窗口'
# explorer 风格：没有任何 -- 开关，数据目录只能用环境变量指定
$env:SFM_DATA_DIR = $d1
$r = Run-App "/select,`"$file`""
Remove-Item Env:SFM_DATA_DIR
Check ($r.text -match 'instance: 已转交（ok）') "explorer 风格 /select,：已转交（$($r.text.Trim())）"

# ---- 2. 不同数据目录互不干扰；没有主实例时 --entry 自己打开 ----
Write-Host '== 2. 不同数据目录是不同实例；没有主实例时 --entry 自己成为主实例'
$d2 = Join-Path $root 'data2'
$r = Run-App "--smoke-ms=1500 --data-dir=`"$d2`" --entry `"$B`""
Check ($r.code -eq 0 -and $r.text -match 'instance: 主实例') '另一个数据目录独立成为主实例（不转发给数据目录 1）'
Check ($r.text -match 'entry: 已打开') "自己处理外部请求（$(($r.text -split "`n" | Where-Object { $_ -match '^entry:' }) -join ' ')）"
$s2 = Read-Session $d2
Check ((Locs $s2 0) -contains $B) '数据目录 2 的会话里打开了“乙 目录”'
Check (-not ((Locs $s2 0) -contains $A)) '数据目录 1 收到的请求不出现在数据目录 2'

$p1 = Wait-App $primary 30000
Check ($p1.code -eq 0) '主实例正常退出'
$s1 = Read-Session $d1
$left = Locs $s1 0
Check ($left.Count -eq 3 -and $left[0] -eq $C -and $left[1] -eq $A -and $left[2] -eq $B) "左面板标签为 丙、甲、乙（甲复用未重复，不存在的路径没有新建标签）：$($left -join ' | ')"
Check ($s1.panes[0].active -eq 1) '最后的 /select, 请求切回甲目录所在标签'

# ---- 3. 主实例无响应 ----
Write-Host '== 3. 主实例无响应：超时后独立启动且不写会话'
$d3 = Join-Path $root 'data3'
$name = (Run-App "--data-dir=`"$d3`" --print-instance-name").text.Trim()
Check ($name -like '\\.\pipe\slint-file-manager-*') "管道名：$name"
$short = $name.Substring('\\.\pipe\'.Length)
$server = [System.IO.Pipes.NamedPipeServerStream]::new($short, [System.IO.Pipes.PipeDirection]::InOut, 1,
    [System.IO.Pipes.PipeTransmissionMode]::Byte, [System.IO.Pipes.PipeOptions]::Asynchronous)
$wait = $server.WaitForConnectionAsync()
$env:SFM_INSTANCE_TIMEOUT_MS = '800'
$sw = [Diagnostics.Stopwatch]::StartNew()
$r = Run-App "--smoke-ms=1500 --data-dir=`"$d3`" --entry `"$A`""
$sw.Stop()
Remove-Item Env:SFM_INSTANCE_TIMEOUT_MS
Check ($wait.IsCompleted) '第二个进程连上了（假的）主实例'
Check ($r.code -eq 0 -and $r.text -match 'instance: 主实例无响应，独立启动') "超时后独立启动（$(($r.text -split "`n" | Where-Object { $_ -match '^instance:' }) -join ' ')）"
Check ($sw.ElapsedMilliseconds -lt 10000) "整体耗时 $($sw.ElapsedMilliseconds) ms（超时 800 ms + 运行 1500 ms）"
Check (-not (Test-Path (Join-Path $d3 'session.json'))) '独立运行时不写会话（不覆盖主实例的会话）'
$server.Dispose()

Remove-Item Env:SFM_TEST_REG_ROOT
if ($script:failed -gt 0) { Write-Error "单实例验证失败 $($script:failed) 项（各进程输出保留在 $out）"; exit 1 }
Remove-Item $root -Recurse -Force
Write-Host '单实例验证通过'
