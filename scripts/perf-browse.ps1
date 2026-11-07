# 进入目录耗时基准（Issue #1 目标：进入目录到可操作 ≤ 200ms）。
# 在 %TEMP%\sfm-perf 生成 60／1000／10000 个文件的目录，以 release 程序的 --bench-enter 反复进入，
# 从 logs 中最新日志读取每次进入的 dir-loaded-us（读取完成回到界面线程）与 dir-ready-us（新内容第一帧渲染完成），
# 汇总写入 artifacts/perf/browse-<时间>.json。需要交互式桌面（真实窗口、真实渲染器）。
# 用法：powershell -File scripts/perf-browse.ps1 [-Counts 60,1000,10000] [-Runs 10]
param(
    [int[]]$Counts = @(60, 1000, 10000),
    [int]$Runs = 10
)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

$exe = Join-Path (Get-Location) 'target\release\slint-file-manager.exe'
if (-not (Test-Path $exe)) { Write-Error '未找到 release 程序，请先运行 scripts/build.ps1'; exit 1 }

$root = Join-Path $env:TEMP 'sfm-perf'
function Ensure-Dir([int]$n) {
    $d = Join-Path $root "$n-files"
    if ((Test-Path $d) -and ((Get-ChildItem -LiteralPath $d -File).Count -eq $n)) { return $d }
    if (Test-Path $d) { Remove-Item -LiteralPath $d -Recurse -Force }
    New-Item -ItemType Directory -Force $d | Out-Null
    $bytes = New-Object byte[] 256
    for ($i = 1; $i -le $n; $i++) {
        [System.IO.File]::WriteAllBytes((Join-Path $d ('图片{0:D5}.webp' -f $i)), $bytes)
    }
    return $d
}

function Stats([double[]]$v) {
    if ($v.Count -eq 0) { return $null }
    $s = $v | Sort-Object
    $pick = { param($q) $s[[math]::Min($s.Count - 1, [math]::Floor($q * ($s.Count - 1) + 0.5))] }
    [ordered]@{
        n      = $s.Count
        min    = [math]::Round($s[0], 1)
        median = [math]::Round((& $pick 0.5), 1)
        p90    = [math]::Round((& $pick 0.9), 1)
        max    = [math]::Round($s[-1], 1)
    }
}

$results = @()
foreach ($n in $Counts) {
    $dir = Ensure-Dir $n
    $before = Get-Date
    $p = Start-Process $exe -ArgumentList "--bench-enter=`"$dir`"", "--bench-runs=$Runs" -PassThru
    if (-not $p.WaitForExit(120000)) { $p.Kill(); Write-Error "$n 个文件：基准超时"; exit 1 }
    $log = Get-ChildItem logs -Filter *.log | Where-Object { $_.LastWriteTime -ge $before } |
        Sort-Object LastWriteTime | Select-Object -Last 1
    if (-not $log) { Write-Error "$n 个文件：没有找到日志"; exit 1 }

    # 每个 bench-iter 之后的第一条 dir-loaded-us / dir-ready-us 属于该次进入
    $loaded = @(); $ready = @(); $inIter = $false; $gotL = $false; $gotR = $false
    foreach ($line in Get-Content -LiteralPath $log.FullName -Encoding utf8) {
        if ($line -match 'event bench-iter') { $inIter = $true; $gotL = $false; $gotR = $false; continue }
        if (-not $inIter) { continue }
        if (-not $gotL -and $line -match 'event dir-loaded-us value=(\d+)') { $loaded += [double]$Matches[1] / 1000; $gotL = $true }
        if (-not $gotR -and $line -match 'event dir-ready-us value=(\d+)') { $ready += [double]$Matches[1] / 1000; $gotR = $true }
        if ($gotL -and $gotR) { $inIter = $false }
    }
    $r = [ordered]@{
        files            = $n
        runs             = $Runs
        log              = $log.Name
        loaded_ms        = Stats $loaded
        ready_ms         = Stats $ready
        target_ms        = 200
        ready_p90_within = ($ready.Count -gt 0) -and ((Stats $ready).p90 -le 200)
    }
    $results += $r
    Write-Host ("{0,6} 个文件：可操作 中位 {1}ms / p90 {2}ms / 最大 {3}ms（读取完成 中位 {4}ms），日志 {5}" -f `
            $n, $r.ready_ms.median, $r.ready_ms.p90, $r.ready_ms.max, $r.loaded_ms.median, $log.Name)
}

$os = Get-CimInstance Win32_OperatingSystem
$cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name
$out = Join-Path (Get-Location) ("artifacts\perf\browse-{0}.json" -f (Get-Date -Format 'yyyy-MM-dd HH-mm-ss'))
New-Item -ItemType Directory -Force (Split-Path $out) | Out-Null
[ordered]@{
    machine = [ordered]@{ os = "$($os.Caption) build $($os.BuildNumber)"; cpu = $cpu }
    note    = '本地 SSD、系统文件缓存已预热（同一目录反复进入）；冷缓存与网络位置未覆盖'
    results = $results
} | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $out -Encoding utf8
Write-Host "结果：$out"
