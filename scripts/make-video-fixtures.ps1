# 生成视频缩略图兼容测试夹具（Issue #13）：小型 MP4／MKV，覆盖常见编码组合与异常文件。
# 需要 ffmpeg（本机用 winget 的 Gyan.FFmpeg）。夹具写入 %TEMP%\sfm-video-fixtures（不读写用户文件，不入库）。
# 用法：powershell -File scripts/make-video-fixtures.ps1 [-Out 目录] [-Force]
# 每个夹具 3 秒、有明显色块与时间码的测试图案（testsrc2），因此“缩略图是否有内容”可以按像素方差判断。
param(
    [string]$Out = (Join-Path $env:TEMP 'sfm-video-fixtures'),
    [switch]$Force
)
$ErrorActionPreference = 'Stop'
$ff = (Get-Command ffmpeg -ErrorAction SilentlyContinue).Source
if (-not $ff) {
    $ff = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages" -Recurse -Filter ffmpeg.exe -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty FullName
}
if (-not $ff) { Write-Error '找不到 ffmpeg；请先 winget install Gyan.FFmpeg'; exit 2 }
if ((Test-Path $Out) -and -not $Force -and (Test-Path (Join-Path $Out 'manifest.json'))) {
    Write-Host "夹具已存在：$Out（-Force 重建）"; exit 0
}
if (Test-Path $Out) { Remove-Item $Out -Recurse -Force }
New-Item -ItemType Directory -Force $Out | Out-Null

$src = 'testsrc2=size=320x180:rate=15:duration=3'
$aud = 'sine=frequency=440:duration=3'
$manifest = @()

# 名称、容器、视频编码参数、音频编码参数（空为无音轨）、说明
$cases = @(
    @('mp4-h264-baseline-aac', 'mp4', '-c:v libx264 -profile:v baseline -pix_fmt yuv420p', '-c:a aac', 'H.264 Baseline 8 位 + AAC'),
    @('mp4-h264-high-aac', 'mp4', '-c:v libx264 -profile:v high -pix_fmt yuv420p', '-c:a aac', 'H.264 High 8 位 + AAC'),
    @('mp4-h264-high10', 'mp4', '-c:v libx264 -profile:v high10 -pix_fmt yuv420p10le', '', 'H.264 High 10（10 位）'),
    @('mp4-h264-444', 'mp4', '-c:v libx264 -profile:v high444 -pix_fmt yuv444p', '', 'H.264 High 4:4:4'),
    @('mp4-h264-noaudio', 'mp4', '-c:v libx264 -pix_fmt yuv420p', '', 'H.264，无音轨'),
    @('mp4-h264-moov-at-end', 'mp4', '-c:v libx264 -pix_fmt yuv420p', '-c:a aac', 'H.264，moov 在文件尾（未 faststart）'),
    @('mp4-h264-faststart', 'mp4', '-c:v libx264 -pix_fmt yuv420p -movflags +faststart', '-c:a aac', 'H.264，moov 在文件头（faststart）'),
    @('mp4-hevc-8bit-aac', 'mp4', '-c:v libx265 -pix_fmt yuv420p -tag:v hvc1 -x265-params log-level=error', '-c:a aac', 'HEVC Main 8 位 + AAC'),
    @('mp4-hevc-10bit', 'mp4', '-c:v libx265 -pix_fmt yuv420p10le -tag:v hvc1 -x265-params log-level=error', '', 'HEVC Main10（10 位）'),
    @('mp4-vp9-opus', 'mp4', '-c:v libvpx-vp9 -pix_fmt yuv420p -b:v 300k', '-c:a libopus', 'VP9 + Opus（MP4 容器）'),
    @('mp4-av1-aac', 'mp4', '-c:v libaom-av1 -pix_fmt yuv420p -cpu-used 8 -crf 40 -b:v 0', '-c:a aac', 'AV1 + AAC（MP4 容器）'),
    @('mp4-mpeg4-aac', 'mp4', '-c:v mpeg4 -q:v 5 -pix_fmt yuv420p', '-c:a aac', 'MPEG-4 Part 2（DivX/Xvid 类）+ AAC'),
    @('mkv-h264-aac', 'mkv', '-c:v libx264 -pix_fmt yuv420p', '-c:a aac', 'H.264 + AAC（MKV）'),
    @('mkv-h264-flac', 'mkv', '-c:v libx264 -pix_fmt yuv420p', '-c:a flac', 'H.264 + FLAC（MKV）'),
    @('mkv-hevc-aac', 'mkv', '-c:v libx265 -pix_fmt yuv420p -x265-params log-level=error', '-c:a aac', 'HEVC 8 位 + AAC（MKV）'),
    @('mkv-hevc-10bit', 'mkv', '-c:v libx265 -pix_fmt yuv420p10le -x265-params log-level=error', '', 'HEVC Main10（MKV）'),
    @('mkv-vp9-opus', 'mkv', '-c:v libvpx-vp9 -pix_fmt yuv420p -b:v 300k', '-c:a libopus', 'VP9 + Opus（MKV）'),
    @('mkv-vp8-vorbis', 'mkv', '-c:v libvpx -pix_fmt yuv420p -b:v 300k', '-c:a libvorbis', 'VP8 + Vorbis（MKV）'),
    @('mkv-av1-opus', 'mkv', '-c:v libaom-av1 -pix_fmt yuv420p -cpu-used 8 -crf 40 -b:v 0', '-c:a libopus', 'AV1 + Opus（MKV）'),
    @('mkv-mpeg4-aac', 'mkv', '-c:v mpeg4 -q:v 5 -pix_fmt yuv420p', '-c:a aac', 'MPEG-4 Part 2 + AAC（MKV）'),
    @('mkv-mpeg2-mp2', 'mkv', '-c:v mpeg2video -q:v 5 -pix_fmt yuv420p', '-c:a mp2', 'MPEG-2 Video + MP2（MKV）')
)

foreach ($c in $cases) {
    $name, $ext, $v, $a, $desc = $c
    $file = Join-Path $Out "$name.$ext"
    $ffargs = @('-hide_banner', '-loglevel', 'error', '-y', '-f', 'lavfi', '-i', $src)
    if ($a) { $ffargs += @('-f', 'lavfi', '-i', $aud) }
    $ffargs += ($v -split ' ')
    if ($a) { $ffargs += ($a -split ' ') }
    $ffargs += @('-t', '3', $file)
    try { & $ff @ffargs 2>&1 | Out-Null } catch { }
    $ok = (Test-Path $file) -and ((Get-Item $file).Length -gt 0)
    Write-Host ("{0,-28} {1}" -f $name, $(if ($ok) { 'OK' } else { '编码器不可用或失败，跳过' }))
    if ($ok) { $manifest += [ordered]@{ file = "$name.$ext"; kind = 'valid'; desc = $desc } } elseif (Test-Path $file) { Remove-Item $file }
}

# 大分辨率（4K，1 秒，低码率）
try { & $ff -hide_banner -loglevel error -y -f lavfi -i 'testsrc2=size=3840x2160:rate=5:duration=1' -c:v libx264 -pix_fmt yuv420p -crf 35 (Join-Path $Out 'mp4-h264-4k.mp4') 2>&1 | Out-Null } catch { }
if (Test-Path (Join-Path $Out 'mp4-h264-4k.mp4')) { $manifest += [ordered]@{ file = 'mp4-h264-4k.mp4'; kind = 'valid'; desc = 'H.264 3840x2160' } }

# 超长（1 小时，1 帧/秒，160x90，低码率）：元数据与索引较大，采帧不应线性耗时
try { & $ff -hide_banner -loglevel error -y -f lavfi -i 'testsrc2=size=160x90:rate=1:duration=3600' -c:v libx264 -pix_fmt yuv420p -crf 40 -g 30 (Join-Path $Out 'mp4-h264-long-1h.mp4') 2>&1 | Out-Null } catch { }
if (Test-Path (Join-Path $Out 'mp4-h264-long-1h.mp4')) { $manifest += [ordered]@{ file = 'mp4-h264-long-1h.mp4'; kind = 'valid'; desc = 'H.264 时长 1 小时' } }

# 异常文件
$good = Join-Path $Out 'mp4-h264-high-aac.mp4'
if (Test-Path $good) {
    $bytes = [System.IO.File]::ReadAllBytes($good)
    [System.IO.File]::WriteAllBytes((Join-Path $Out 'bad-truncated.mp4'), $bytes[0..([int]($bytes.Length * 0.4))])
    $manifest += [ordered]@{ file = 'bad-truncated.mp4'; kind = 'corrupt'; desc = 'MP4 被截断到 40%' }
    $mid = [byte[]]$bytes.Clone()
    for ($i = [int]($mid.Length * 0.3); $i -lt [int]($mid.Length * 0.7); $i++) { $mid[$i] = 0xFF }
    [System.IO.File]::WriteAllBytes((Join-Path $Out 'bad-midcorrupt.mp4'), $mid)
    $manifest += [ordered]@{ file = 'bad-midcorrupt.mp4'; kind = 'tolerant'; desc = 'MP4 中段被覆盖为 0xFF（文件头与首帧完好，播放器会容错，出图不算异常）' }
}
[System.IO.File]::WriteAllBytes((Join-Path $Out 'bad-empty.mp4'), [byte[]]@())
$manifest += [ordered]@{ file = 'bad-empty.mp4'; kind = 'corrupt'; desc = '0 字节 MP4' }
$rnd = New-Object byte[] 65536
(New-Object System.Random 13).NextBytes($rnd)
[System.IO.File]::WriteAllBytes((Join-Path $Out 'bad-garbage.mkv'), $rnd)
$manifest += [ordered]@{ file = 'bad-garbage.mkv'; kind = 'corrupt'; desc = '随机字节冒充 MKV' }
$goodmkv = Join-Path $Out 'mkv-h264-aac.mkv'
if (Test-Path $goodmkv) {
    $bytes = [System.IO.File]::ReadAllBytes($goodmkv)
    [System.IO.File]::WriteAllBytes((Join-Path $Out 'bad-truncated.mkv'), $bytes[0..([int]($bytes.Length * 0.5))])
    $manifest += [ordered]@{ file = 'bad-truncated.mkv'; kind = 'corrupt'; desc = 'MKV 被截断到 50%' }
}
Set-Content -Path (Join-Path $Out 'bad-text.mp4') -Value 'not a video' -Encoding UTF8
$manifest += [ordered]@{ file = 'bad-text.mp4'; kind = 'corrupt'; desc = '文本冒充 MP4' }

$manifest | ConvertTo-Json | Set-Content -Path (Join-Path $Out 'manifest.json') -Encoding UTF8
Write-Host "已生成 $($manifest.Count) 个夹具 -> $Out"
