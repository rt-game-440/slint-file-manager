# A2 视觉核对（Issue #15）：把 A2 原图与实现截图左右并排，输出到 -Out 目录。
# 实现截图来自 harness 的离屏渲染（scripts/harness.ps1 snap，软件渲染器、模拟数据），不是真实窗口。
# 用法：powershell -File scripts/compose-a2-review.ps1 -Snap artifacts\snapshots\current -Out artifacts\issue-15-review
param(
    [string]$Snap = 'artifacts\snapshots\current',
    [string]$Out = 'artifacts\issue-15-review'
)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
Add-Type -AssemblyName System.Drawing
New-Item -ItemType Directory -Force $Out | Out-Null

$a2 = [System.Drawing.Image]::FromFile((Resolve-Path 'docs\concepts\file-manager\round-2\A2.png'))
# 编号、场景、缩放、说明（说明写进图片上方的标题条）
$items = @(
    @('01', 'dual-default', 100, '双栏等宽，左面板活动'),
    @('02', 'thumbs-ready', 100, '缩略图网格'),
    @('03', 'search-results', 100, '搜索（目录范围）'),
    @('04', 'preview-fit', 100, '大图预览'),
    @('05', 'not-found', 100, '错误态：位置不存在'),
    @('06', 'access-denied', 100, '错误态：无权限'),
    @('07', 'empty-folder', 100, '空目录'),
    @('08', 'long-path', 100, '长路径'),
    @('09', 'dual-default', 150, '双栏，文字缩放 150%'),
    @('10', 'dual-text-scale-150', 150, '双栏 + 文字缩放 150%，右面板活动'),
    @('11', 'search-results', 150, '搜索，150%'),
    @('12', 'loading', 100, '加载中')
)
$font = New-Object System.Drawing.Font('Microsoft YaHei UI', 15, [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)
$small = New-Object System.Drawing.Font('Microsoft YaHei UI', 13, [System.Drawing.GraphicsUnit]::Pixel)
$made = @()
foreach ($it in $items) {
    $no, $scn, $pct, $desc = $it
    $file = Join-Path $Snap ("{0}_1100x720_{1}.png" -f $scn, $pct)
    if (-not (Test-Path $file)) { Write-Warning "缺少截图 $file"; continue }
    $impl = [System.Drawing.Image]::FromFile((Resolve-Path $file))
    $h = $impl.Height
    $w2 = [int]($a2.Width * $h / $a2.Height)
    $bar = 34; $gap = 12
    $bmp = New-Object System.Drawing.Bitmap ($w2 + $gap + $impl.Width), ($h + $bar)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::FromArgb(255, 245, 245, 247))
    $g.InterpolationMode = 'HighQualityBicubic'; $g.TextRenderingHint = 'ClearTypeGridFit'
    $g.DrawImage($a2, 0, $bar, $w2, $h)
    $g.DrawImage($impl, $w2 + $gap, $bar, $impl.Width, $h)
    $ink = [System.Drawing.Brushes]::Black
    $g.DrawString("[$no] 左：A2 原图（占位内容）", $font, $ink, 6, 8)
    $g.DrawString("右：实现 $scn @ ${pct}%  —  $desc", $font, $ink, $w2 + $gap + 6, 8)
    $g.Dispose()
    $name = "{0}-a2-vs-{1}_{2}.png" -f $no, $scn, $pct
    $bmp.Save((Join-Path $Out $name), [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose(); $impl.Dispose()
    $made += $name
}
$a2.Dispose()
$made | ForEach-Object { Write-Host $_ }
