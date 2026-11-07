# 缩略图路线与首屏性能测量（Issue #5）。依据 docs/research/0005-thumbnail-route.md。
#
# 做三件事，结果合并写入 artifacts/perf/thumbs-<时间>.json（原始分项 JSON 在同名目录），运行摘要写 logs/<时间>.log：
#   1. 原型（prototypes/thumb-probe）：路线对比（解码／缩放／内存）、冷／热／重启三种缓存条件下的首屏、
#      滚动取消、换目录、慢文件隔离、异常文件。无头：不含 Slint 上传与渲染，不是整机 UI 成绩。
#   2. 产品界面：用 slint-file-manager.exe 的 --bench-enter 复用 #4 的“可操作”计时（scripts/perf-browse.ps1），
#      并读取日志里的事件 thumbs-first-screen-us（产品实现缩略图后由 #10 写入；没有该事件时显示“未测量”，绝不写 0）。
#   3. 环境记录：系统、CPU、内存、窗口、首屏项数、缓存条件。
#
# 用法：
#   powershell -File scripts/perf-thumbs.ps1                         合成夹具（%TEMP%\sfm-thumbs，可丢弃）
#   powershell -File scripts/perf-thumbs.ps1 -Dir D:\我的图片目录     只读你自己的目录，公开记录只含统计
#   参数：-Runs 5（重复次数）-Workers 4 -FirstScreen 28 -SkipProduct（无交互桌面时跳过产品计时）-Quick（Runs=2）
#
# 匿名：输出与日志只含数值与类别，不含路径、文件名、图像。-Dir 目录不会被修改（不写缓存、不改时间戳）；
# 缩略图缓存写在 %TEMP%\sfm-thumbs\cache-*。Shell 路线会让系统写入自己的 thumbcache，且无法清除，见研究文档。
param(
    [string]$Dir = '',
    [int]$Runs = 5,
    [int]$Workers = 4,
    [int]$FirstScreen = 28,
    [switch]$SkipProduct,
    [switch]$ProductOnly,
    [switch]$Quick
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Set-Location (Join-Path $PSScriptRoot '..')
if ($Quick) { $Runs = 2 }

$stamp = Get-Date -Format 'yyyy-MM-dd HH-mm-ss'
$work = Join-Path (Get-Location) "artifacts\perf\thumbs-$stamp"
New-Item -ItemType Directory -Force $work | Out-Null
New-Item -ItemType Directory -Force 'logs' | Out-Null
$logFile = Join-Path (Get-Location) "logs\$stamp.log"
function Log([string]$m) {
    Write-Host $m
    Add-Content -LiteralPath $logFile -Value ("{0} {1}" -f (Get-Date -Format 'HH:mm:ss.fff'), $m) -Encoding utf8
}

function Stat($vals) {
    $v = @($vals | Where-Object { $null -ne $_ } | ForEach-Object { [double]$_ })
    if ($v.Count -eq 0) { return $null }
    $s = $v | Sort-Object
    $pick = { param($q) $s[[math]::Min($s.Count - 1, [math]::Floor($q * ($s.Count - 1) + 0.5))] }
    [ordered]@{
        n      = $s.Count
        min    = [math]::Round($s[0], 2)
        median = [math]::Round((& $pick 0.5), 2)
        p90    = [math]::Round((& $pick 0.9), 2)
        max    = [math]::Round($s[-1], 2)
    }
}

# ---------------------------------------------------------------- 构建与夹具
Log '== 构建原型（release，jobs 6）'
cargo build --release --locked --jobs 6 --manifest-path prototypes\thumb-probe\Cargo.toml
if ($LASTEXITCODE -ne 0) { Write-Error '原型构建失败'; exit 1 }
$probe = Join-Path (Get-Location) 'prototypes\thumb-probe\target\release\thumb-probe.exe'
$fx = Join-Path $env:TEMP 'sfm-thumbs'

function Probe([string]$name, [string[]]$a) {
    $out = Join-Path $work "$name.json"
    & $probe @a "--out=$out" | Out-Null
    $code = $LASTEXITCODE
    if (-not (Test-Path $out)) { Write-Error "$name 没有产生结果（退出码 $code）"; exit 1 }
    $j = Get-Content -LiteralPath $out -Raw -Encoding utf8 | ConvertFrom-Json
    if ($code -ne 0 -and $null -ne $j.valid -and $j.valid) { Write-Error "$name 退出码 $code"; exit 1 }
    return $j
}

$synthetic = [string]::IsNullOrEmpty($Dir)
if ($synthetic) {
    Log '== 生成／复用合成夹具（%TEMP%\sfm-thumbs）'
    & $probe gen | ForEach-Object { Log $_ }
    if ($LASTEXITCODE -ne 0) { Write-Error '夹具生成失败'; exit 1 }
    $huge = Join-Path $fx 'huge-1'
    New-Item -ItemType Directory -Force $huge | Out-Null
    Copy-Item -LiteralPath (Join-Path $fx 'mixed\IMG_0029.webp') -Destination $huge -Force
    $primary = Join-Path $fx 'webp-photo-60'
    $label = 'webp-photo-60'
} else {
    if (-not (Test-Path -LiteralPath $Dir -PathType Container)) { Write-Error '指定目录不存在'; exit 1 }
    $primary = $Dir
    $label = 'user-dir'
}
$nFiles = @(Get-ChildItem -LiteralPath $primary -File | Where-Object { -not $_.Name.StartsWith('.') }).Count

# Shell 缩略图会进入系统 thumbcache，键含修改时间：对合成夹具改一下时间戳即可得到“冷”的 Shell 状态；用户目录绝不改动
function Touch-Fixture([string]$d) {
    if (-not $synthetic) { return }
    $now = Get-Date
    Get-ChildItem -LiteralPath $d -File | ForEach-Object { $_.LastWriteTime = $now }
}

# ---------------------------------------------------------------- 环境
$os = Get-CimInstance Win32_OperatingSystem
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$disk = 'unknown'
try {
    $dl = (Split-Path -Qualifier $primary).TrimEnd(':')
    $pd = Get-Partition -DriveLetter $dl -ErrorAction Stop | Get-Disk -ErrorAction Stop | Get-PhysicalDisk -ErrorAction Stop
    $disk = "$($pd.MediaType) $($pd.BusType)"
} catch { }
$env_ = [ordered]@{
    os              = "$($os.Caption) build $($os.BuildNumber)"
    cpu             = $cpu.Name.Trim()
    logical_cores   = [int]$cpu.NumberOfLogicalProcessors
    ram_gib         = [math]::Round($os.TotalVisibleMemorySize / 1MB, 1)
    target_disk     = $disk
    rustc           = (rustc --version)
    dataset         = if ($synthetic) { 'synthetic' } else { 'user-dir（匿名）' }
    dataset_label   = $label
    files           = $nFiles
    thumb_size_px   = 256
    window          = '1100x720（#6 之前的单面板布局：网格 7 列，完整可见 4 行 = 28 项，含半露出一行 35 项；双栏下需重新计数并用 -FirstScreen 传入，见 0005 研究文档）'
    first_screen    = $FirstScreen
    workers         = $Workers
    runs            = $Runs
    cpu_load_pct_at_start = [int](Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average
    load_note       = '基线应在空闲机器上采集；后台有编译或其他任务时数字会明显变慢（见 0005 第 7 节）'
    os_file_cache   ='未控制（夹具刚生成或刚读过；真正冷读需管理员清备用列表，本次未测量）'
}
Log ("环境：{0} / {1} / {2} 核 / {3} GiB / 磁盘 {4}" -f $env_.os, $env_.cpu, $env_.logical_cores, $env_.ram_gib, $env_.target_disk)

$result = [ordered]@{ env = $env_; routes = [ordered]@{}; conditions = [ordered]@{}; isolation = [ordered]@{}; product = $null; not_measured = @() }
$notMeasured = New-Object System.Collections.Generic.List[string]

if (-not $ProductOnly) {
# ---------------------------------------------------------------- 1. 路线对比
function RouteRow($j) {
    [ordered]@{
        ok                 = $j.ok
        failed             = $j.failed
        wall_ms            = $j.per_file_ms.wall
        read_ms            = $j.per_file_ms.read
        decode_ms          = $j.per_file_ms.decode
        scale_ms           = $j.per_file_ms.scale
        throughput_per_s   = $j.throughput_per_s
        peak_commit_over_baseline_mib = $j.memory.peak_commit_over_baseline_mib
        source_megapixels_mean = $j.source_megapixels_mean
        source_bytes_mean  = $j.source_bytes_mean
        shell_hresults     = $j.shell_hresults
    }
}
Log '== 路线对比（每条路线独立进程，1 个线程，256px）'
foreach ($r in 'libwebp-scaled', 'libwebp-fast', 'libwebp-full', 'rust-image') {
    $j = Probe "route-$r" @('bench', "--dir=$primary", "--route=$r", '--workers=1')
    $result.routes[$r] = RouteRow $j
    Log ("  {0,-15} 每张 中位 {1}ms / p90 {2}ms，峰值内存增量 {3} MiB" -f $r, $j.per_file_ms.wall.median, $j.per_file_ms.wall.p90, $j.memory.peak_commit_over_baseline_mib)
}
# Shell：冷（thumbcache 里没有）与热（刚生成过）
Touch-Fixture $primary
$j = Probe 'route-shell-cold' @('bench', "--dir=$primary", '--route=shell', '--workers=1')
$result.routes['shell-cold'] = RouteRow $j
$result.routes['shell-cold']['note'] = if ($synthetic) { '已改夹具时间戳，系统 thumbcache 无该项' } else { '用户目录 thumbcache 状态不可控：可能是冷也可能是热' }
if ($j.per_file_ms.wall) { Log ("  shell-cold       每张 中位 {0}ms / p90 {1}ms" -f $j.per_file_ms.wall.median, $j.per_file_ms.wall.p90) }
$j = Probe 'route-shell-hot' @('bench', "--dir=$primary", '--route=shell', '--workers=1')
$result.routes['shell-hot'] = RouteRow $j
if ($j.per_file_ms.wall) { Log ("  shell-hot        每张 中位 {0}ms / p90 {1}ms" -f $j.per_file_ms.wall.median, $j.per_file_ms.wall.p90) }
# 并行：产品形态（多线程）
$j = Probe "route-auto-w$Workers" @('bench', "--dir=$primary", '--route=auto', "--workers=$Workers", '--repeat=3')
$result.routes["auto-workers-$Workers"] = RouteRow $j
Touch-Fixture $primary
$j = Probe "route-shell-cold-w$Workers" @('bench', "--dir=$primary", '--route=shell', "--workers=$Workers")
$result.routes["shell-cold-workers-$Workers"] = RouteRow $j
if ($j.per_file_ms.wall) { Log ("  shell-cold {0} 线程 每张 中位 {1}ms / p90 {2}ms，吞吐 {3}/s" -f $Workers, $j.per_file_ms.wall.median, $j.per_file_ms.wall.p90, $j.throughput_per_s) }

if ($synthetic) {
    Log '== 体积不能推断耗时：同分辨率，细节密集的 WebP（体积约大 18 倍）'
    $dense = Join-Path $fx 'webp-dense-12'
    $result['dense'] = [ordered]@{}
    foreach ($r in 'libwebp-fast', 'rust-image') {
        $j = Probe "dense-$r" @('bench', "--dir=$dense", "--route=$r", '--workers=1')
        $result.dense[$r] = RouteRow $j
        Log ("  dense {0,-13} 每张 中位 {1}ms，源文件均 {2:N0} 字节" -f $r, $j.per_file_ms.wall.median, $j.source_bytes_mean)
    }
    Touch-Fixture $dense
    $j = Probe 'dense-shell-cold' @('bench', "--dir=$dense", '--route=shell', '--workers=1')
    $result.dense['shell-cold'] = RouteRow $j
    Log ("  dense shell-cold    每张 中位 {0}ms / p90 {1}ms" -f $j.per_file_ms.wall.median, $j.per_file_ms.wall.p90)

    Log '== 其他格式（各一张 1200 万像素 JPEG／PNG／BMP，GIF 300 万；rust-image 与 Shell）'
    $fm = Join-Path $fx 'formats'
    $result['formats'] = [ordered]@{}
    foreach ($ext in 'jpg', 'png', 'bmp', 'gif') {
        $j = Probe "fmt-$ext-rust" @('bench', "--dir=$fm", "--ext=$ext", '--route=rust-image', '--repeat=3')
        $row = [ordered]@{ rust_image = RouteRow $j }
        Touch-Fixture $fm
        $j2 = Probe "fmt-$ext-shell" @('bench', "--dir=$fm", "--ext=$ext", '--route=shell')
        $row['shell_cold'] = RouteRow $j2
        $result.formats[$ext] = $row
        Log ("  {0}: rust-image 中位 {1}ms / 峰值内存增量 {2} MiB；shell-cold {3}ms" -f $ext, $j.per_file_ms.wall.median, $j.memory.peak_commit_over_baseline_mib, $(if ($j2.per_file_ms.wall) { $j2.per_file_ms.wall.median } else { 'null(失败)' }))
    }

    Log '== 超大尺寸（1.44 亿像素 WebP，体积仅 0.4 MB）：关闭像素上限后各路线的内存'
    $result['oversize'] = [ordered]@{}
    foreach ($r in 'libwebp-scaled', 'libwebp-full', 'rust-image') {
        $j = Probe "huge-$r" @('bench', "--dir=$(Join-Path $fx 'huge-1')", "--route=$r", '--no-limit')
        $result.oversize[$r] = RouteRow $j
        Log ("  {0,-15} {1}ms，峰值内存增量 {2} MiB" -f $r, $j.per_file_ms.wall.median, $j.memory.peak_commit_over_baseline_mib)
    }

    Log '== WebP 变体（同一幅 1200 万像素图：无损、有损带透明）：缩放解码是否同样省内存'
    $result['webp_variants'] = [ordered]@{}
    foreach ($v in 'lossless', 'lossy-alpha') {
        $vd = Join-Path $fx "webp-variants"
        foreach ($r in 'libwebp-scaled', 'libwebp-full', 'rust-image') {
            $j = Probe "variant-$v-$r" @('bench', "--dir=$vd", "--only=$v.webp", "--route=$r", '--repeat=3')
            $result.webp_variants["$v/$r"] = RouteRow $j
            Log ("  {0,-11} {1,-15} 中位 {2}ms，峰值内存增量 {3} MiB" -f $v, $r, $j.per_file_ms.wall.median, $j.memory.peak_commit_over_baseline_mib)
        }
    }

    Log '== 画质与透明'
    $q = Probe 'quality' @('quality', "--dir=$primary", '--limit=6')
    $result['quality'] = $q.routes
    $a = Probe 'alpha' @('alpha', "--file=$(Join-Path $fx 'formats\alpha.webp')")
    $result['alpha'] = $a.routes
}

# ---------------------------------------------------------------- 2. 冷／热／重启
function Cond([string]$name, [string]$d, [int]$first, [string]$fmt, [int]$workers, [int]$runs) {
    $cold = @(); $warm = @(); $restart = @(); $bg = @(); $dec = @(); $mem = @(); $bytes = $null; $entries = $null; $rvalid = $true
    for ($i = 1; $i -le $runs; $i++) {
        $w = Probe "$name-warm-$i" @('scenario', "--dir=$d", '--kind=enter', '--condition=warm', "--cache-format=$fmt", "--workers=$workers", "--first-screen=$first")
        $cold += $w.passes[0].normal_ready_ms
        $warm += $w.passes[1].normal_ready_ms
        $bg += $w.passes[0].background_done_ms
        if ($w.passes[0].stage_ms_decoded_items -and $w.passes[0].stage_ms_decoded_items.decode) { $dec += $w.passes[0].stage_ms_decoded_items.decode.median }
        $mem += $w.memory.peak_commit_over_baseline_mib
        if ($fmt -ne 'none') {
            $r = Probe "$name-restart-$i" @('scenario', "--dir=$d", '--kind=enter', '--condition=restart', "--cache-format=$fmt", "--workers=$workers", "--first-screen=$first")
            if (-not $r.valid) { $rvalid = $false }
            $restart += $r.passes[0].normal_ready_ms
            $bytes = $r.cache_after.bytes; $entries = $r.cache_after.entries
        }
    }
    [ordered]@{
        first_screen_items = $first
        cache_format       = $fmt
        workers            = $workers
        runs               = $runs
        cold_ms            = Stat $cold
        warm_same_process_ms = Stat $warm
        restart_ms         = if ($fmt -eq 'none') { '未测量（无磁盘缓存）' } elseif ($rvalid) { Stat $restart } else { '无效：磁盘缓存为空' }
        cold_background_done_ms = Stat $bg
        cold_decode_median_ms_per_item = Stat $dec
        peak_commit_over_baseline_mib = Stat $mem
        disk_cache_bytes   = $bytes
        disk_cache_entries = $entries
    }
}

Log "== 冷／热／重启（首屏 $FirstScreen 项，$Workers 线程，重复 $Runs 次）"
foreach ($fmt in 'webp', 'raw') {
    $c = Cond "cond-$label-$fmt" $primary $FirstScreen $fmt $Workers $Runs
    $result.conditions["$label/cache-$fmt"] = $c
    Log ("  cache={0}：冷 中位 {1}ms（最大 {2}）/ 同进程热 中位 {3}ms / 重启后 中位 {4}ms（最大 {5}）；磁盘缓存 {6} 项 {7:N0} 字节" -f $fmt, $c.cold_ms.median, $c.cold_ms.max, $c.warm_same_process_ms.median, $c.restart_ms.median, $c.restart_ms.max, $c.disk_cache_entries, $c.disk_cache_bytes)
}
$c = Cond "cond-$label-nocache" $primary $FirstScreen 'none' $Workers $Runs
$result.conditions["$label/no-disk-cache"] = $c
Log ("  无磁盘缓存：冷 中位 {0}ms" -f $c.cold_ms.median)

Log '== 首屏项数与线程数敏感性（冷，无磁盘缓存）'
$result['sweep'] = [ordered]@{}
foreach ($w in 1, 2, 4, 6) {
    $c = Cond "sweep-w$w" $primary $FirstScreen 'none' $w ([math]::Max(2, [math]::Min($Runs, 3)))
    $result.sweep["workers-$w"] = $c.cold_ms
    Log ("  {0} 线程：冷 中位 {1}ms / 最大 {2}ms" -f $w, $c.cold_ms.median, $c.cold_ms.max)
}
$c = Cond 'sweep-first35' $primary 35 'none' $Workers ([math]::Max(2, [math]::Min($Runs, 3)))
$result.sweep['first-screen-35'] = $c.cold_ms
Log ("  首屏 35 项（含半露出一行）：冷 中位 {0}ms / 最大 {1}ms" -f $c.cold_ms.median, $c.cold_ms.max)
if ($synthetic) {
    $c = Cond 'cond-dense' (Join-Path $fx 'webp-dense-12') 12 'webp' $Workers ([math]::Max(2, [math]::Min($Runs, 3)))
    $result.conditions['webp-dense-12/cache-webp'] = $c
    Log ("  细节密集 12 张（首屏 12）：冷 中位 {0}ms / 同进程热 {1}ms / 重启 {2}ms" -f $c.cold_ms.median, $c.warm_same_process_ms.median, $c.restart_ms.median)
}

# ---------------------------------------------------------------- 3. 滚动取消、换目录、慢文件、异常文件（仅合成夹具，位置固定）
if ($synthetic) {
    Log '== 滚动取消 / 换目录 / 慢文件隔离 / 异常文件'
    $s = Probe 'iso-scroll' @('scenario', "--dir=$(Join-Path $fx 'many-600')", '--kind=scroll', '--cache-format=none', "--workers=$Workers", "--first-screen=$FirstScreen", '--scroll-after-ms=40')
    $result.isolation['scroll'] = $s.passes[0]
    Log ("  滚动：排队中取消 {0} 个，在途无法取消 {1} 个，滚动后才开始的视口外任务 {2} 个；新首屏 {3}ms 内齐全（{4}/{5}）" -f $s.passes[0].counters_at_scroll.cancelled_queued, $s.passes[0].counters_at_scroll.stale_inflight, $s.passes[0].started_unwanted_total, $s.passes[0].new_first_screen_ready_ms_after_scroll, $s.passes[0].new_first_screen_ready_count, $s.passes[0].new_first_screen_items)
    $s = Probe 'iso-dirswitch' @('scenario', "--dir=$primary", '--kind=dirswitch', '--cache-format=none', "--workers=$Workers", "--first-screen=$FirstScreen")
    $result.isolation['dirswitch'] = $s.passes[0]
    Log ("  换目录：旧 epoch 结果丢弃 {0} 个，绑定错位 {1} 个" -f $s.passes[0].dropped_old_epoch, $s.passes[0].key_mismatch)
    $s = Probe 'iso-slow1' @('scenario', "--dir=$primary", '--kind=enter', '--cache-format=none', "--workers=$Workers", "--first-screen=$FirstScreen", '--slow=2:2600')
    $result.isolation['slow-1-of-first-screen'] = $s.passes[0]
    Log ("  1 个慢文件（注入 2600ms，硬超时 2000ms）：其余首屏 {0}ms 齐全；慢项状态 {1}，最终 {2}" -f $s.passes[0].normal_ready_ms, $s.passes[0].injected_slow_items[0].state, $s.passes[0].injected_slow_final_states[0].state)
    $s = Probe 'iso-slow4' @('scenario', "--dir=$primary", '--kind=enter', '--cache-format=none', "--workers=$Workers", "--first-screen=$FirstScreen", '--slow=0:2600,1:2600,2:2600,3:2600')
    $result.isolation['slow-4-of-first-screen'] = $s.passes[0]
    Log ("  4 个同时慢（占满全部线程）：其余首屏 {0}ms 齐全" -f $s.passes[0].normal_ready_ms)
    $s = Probe 'iso-mixed' @('scenario', "--dir=$(Join-Path $fx 'mixed')", '--kind=enter', '--cache-format=none', "--workers=$Workers", '--first-screen=60', '--prefetch=0', '--report-indices=6,12,20,28,35,44')
    $result.isolation['faults-mixed-60'] = [ordered]@{ ready = $s.passes[0].ready; failed = $s.passes[0].failed; all_resolved_ms = $s.passes[0].all_resolved_ms; faults = $s.passes[0].index_states; fault_kinds = '6 截断 / 12 空文件 / 20 PNG 内容错用 .webp / 28 1.44 亿像素 / 35 仅有 RIFF 头的垃圾 / 44 主体比特翻转' }
    Log ("  60 个文件含 6 个异常：成功 {0}，失败 {1}" -f $s.passes[0].ready, (($s.passes[0].failed.PSObject.Properties | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ' '))
}

}
# ---------------------------------------------------------------- 4. 产品界面：可操作 + 首屏缩略图事件
function Parse-ProductLog([string]$path) {
    # 每个 bench-iter 之后的第一条 dir-ready-us 与 thumbs-first-screen-us 属于该次进入
    $its = @(); $cur = $null
    foreach ($line in Get-Content -LiteralPath $path -Encoding utf8) {
        if ($line -match 'event bench-iter') { $cur = [ordered]@{ operable_ms = $null; thumbs_ms = $null }; $its += , $cur; continue }
        if ($null -eq $cur) { continue }
        if ($null -eq $cur.operable_ms -and $line -match 'event dir-ready-us value=(\d+)') { $cur.operable_ms = [double]$Matches[1] / 1000 }
        if ($null -eq $cur.thumbs_ms -and $line -match 'event thumbs-first-screen-us value=(\d+)') { $cur.thumbs_ms = [double]$Matches[1] / 1000 }
    }
    return , $its
}

if ($SkipProduct) {
    $result.product = '未测量：-SkipProduct'
    $notMeasured.Add('产品界面“可操作”与首屏缩略图计时（-SkipProduct）')
} else {
    Log '== 产品界面：可操作（复用 perf-browse.ps1 / --bench-enter）与首屏缩略图事件'
    $exe = Join-Path (Get-Location) 'target\release\slint-file-manager.exe'
    if (-not (Test-Path $exe)) { & powershell -NoProfile -File scripts\build.ps1; if ($LASTEXITCODE -ne 0) { Write-Error '主程序构建失败'; exit 1 } }
    # 约定（给 #10）：缩略图缓存目录由环境变量 SFM_THUMB_CACHE_DIR 指定；冷 = 清空该目录，重启后 = 保留目录并新开进程
    $pcache = Join-Path $fx 'product-cache'
    New-Item -ItemType Directory -Force $fx | Out-Null
    if (Test-Path $pcache) { Remove-Item -LiteralPath $pcache -Recurse -Force }
    $env:SFM_THUMB_CACHE_DIR = $pcache
    $benchRuns = 10
    & powershell -NoProfile -File scripts\perf-browse.ps1 -Dir $primary -Runs $benchRuns | ForEach-Object { Log "  $_" }
    if ($LASTEXITCODE -ne 0) { Write-Error 'perf-browse 失败'; exit 1 }
    $pb = Get-ChildItem artifacts\perf -Filter 'browse-*.json' | Sort-Object LastWriteTime | Select-Object -Last 1
    $pj = Get-Content -LiteralPath $pb.FullName -Raw -Encoding utf8 | ConvertFrom-Json
    $log1 = Join-Path 'logs' $pj.results[0].log
    $it1 = Parse-ProductLog $log1
    # 重启后：保留缓存目录，新开进程
    $before = Get-Date
    $p = Start-Process $exe -ArgumentList "--bench-enter=`"$primary`"", "--bench-runs=$benchRuns" -PassThru
    if (-not $p.WaitForExit(120000)) { $p.Kill(); Write-Error '重启后基准超时'; exit 1 }
    $log2f = Get-ChildItem logs -Filter *.log | Where-Object { $_.LastWriteTime -ge $before -and $_.Name -ne "$stamp.log" } | Sort-Object LastWriteTime | Select-Object -Last 1
    $it2 = if ($log2f) { Parse-ProductLog $log2f.FullName } else { @() }
    Remove-Item Env:\SFM_THUMB_CACHE_DIR -ErrorAction SilentlyContinue

    $unmeasured = '未测量：产品尚未实现缩略图（#10 实现并写入事件 thumbs-first-screen-us 后自动出现）'
    function Thumbs($its) { $v = @($its | ForEach-Object { $_.thumbs_ms } | Where-Object { $null -ne $_ }); if ($v.Count -eq 0) { return $unmeasured } else { return Stat $v } }
    $first1 = if ($it1.Count -gt 0) { @($it1[0]) } else { @() }
    $rest1 = if ($it1.Count -gt 1) { @($it1[1..($it1.Count - 1)]) } else { @() }
    $first2 = if ($it2.Count -gt 0) { @($it2[0]) } else { @() }
    $result.product = [ordered]@{
        method          = '主程序 --bench-enter：在目录与其上级间往返，每次进入记录 dir-ready-us（新内容第一帧渲染完成 = 可操作）；首屏缩略图事件 thumbs-first-screen-us'
        window          = $env_.window
        first_screen_items = $FirstScreen
        files           = $nFiles
        operable_ms     = [ordered]@{
            cold_first_enter     = Stat ($first1 | ForEach-Object { $_.operable_ms })
            warm_same_process    = Stat ($rest1 | ForEach-Object { $_.operable_ms })
            restart_first_enter  = Stat ($first2 | ForEach-Object { $_.operable_ms })
            note = '“冷”指进程内第一次进入（含首次渲染等一次性开销），样本只有 1 个；多次冷启动请重复运行本脚本取分布。尚无缩略图，所以这是不含缩略图的可操作时间'
        }
        thumbs_first_screen_ms = [ordered]@{
            cold_first_enter    = Thumbs $first1
            warm_same_process   = Thumbs $rest1
            restart_first_enter = Thumbs $first2
        }
        cache_condition = '产品尚无缩略图缓存；约定 SFM_THUMB_CACHE_DIR 指定缓存目录，冷=清空，重启后=保留并新开进程'
        perf_browse_result = $pb.Name
    }
    Log ("  可操作：冷首次进入 {0}ms；同进程再进入 中位 {1}ms（p90 {2}ms）；重启后首次进入 {3}ms" -f $result.product.operable_ms.cold_first_enter.median, $result.product.operable_ms.warm_same_process.median, $result.product.operable_ms.warm_same_process.p90, $result.product.operable_ms.restart_first_enter.median)
    if ($result.product.thumbs_first_screen_ms.cold_first_enter -is [string]) { Log '  首屏缩略图：未测量（产品尚未实现缩略图）' }
    $notMeasured.Add('产品界面首屏缩略图时间（产品尚无缩略图）')
}

$notMeasured.Add('操作系统文件缓存冷态（需要管理员清备用列表）')
$notMeasured.Add('Windows 11（无环境）')
if (-not $synthetic) {
    $notMeasured.Add('用户目录：异常文件、慢文件注入等隔离项只在合成夹具上运行')
    $notMeasured.Add('用户目录：Shell 路线的“冷”不可控（不改你的时间戳），shell-cold 实际是系统 thumbcache 当前状态，不是冷态')
}
$notMeasured.Add('Slint 图像上传与渲染耗时（无头原型不含；整机成绩看产品部分）')
$result.not_measured = $notMeasured.ToArray()

$out = Join-Path (Get-Location) "artifacts\perf\thumbs-$stamp.json"
$result | ConvertTo-Json -Depth 14 | Set-Content -LiteralPath $out -Encoding utf8
Log "结果：$out"
Log "日志：$logFile"
