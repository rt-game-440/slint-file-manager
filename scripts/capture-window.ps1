# 真实窗口截图入口（补充手段）：以 --debug-perf --demo-autoplay 启动 release 程序，
# 按间隔对窗口区域截屏，并把每张图的本机时间（HH:mm:ss.fff）写入 captures.txt，
# 与 logs/*.log 行首的时间对应，用于复现只在真实渲染下出现的问题。
# 用法：powershell -File scripts/capture-window.ps1 [-Count 6] [-IntervalMs 250] [-RunMs 4000]
# 需要交互式桌面，且窗口不被遮挡。截图用 GDI 抓屏，受显示器缩放影响；它不是确定性截图，不能用于基线比较。
param(
    [int]$Count = 6,
    [int]$IntervalMs = 250,
    [int]$RunMs = 4000
)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Runtime.InteropServices;
public class Win32Rect {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
  delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc p, IntPtr l);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  // 进程可能有多个顶层窗口（辅助窗口），取面积最大的可见窗口
  public static IntPtr LargestWindow(uint pid) {
    IntPtr best = IntPtr.Zero; long area = 0;
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      RECT r;
      if (p == pid && IsWindowVisible(h) && GetWindowRect(h, out r)) {
        long a = (long)(r.Right - r.Left) * (r.Bottom - r.Top);
        if (a > area) { area = a; best = h; }
      }
      return true;
    }, IntPtr.Zero);
    return best;
  }
}
'@
[void][Win32Rect]::SetProcessDPIAware()

$exe = Join-Path (Get-Location) 'target\release\slint-file-manager.exe'
if (-not (Test-Path $exe)) { Write-Error '未找到 release 程序，请先运行 scripts/build.ps1'; exit 1 }

$stamp = Get-Date -Format 'yyyy-MM-dd HH-mm-ss'
$out = Join-Path (Get-Location) "artifacts\real\$stamp"
New-Item -ItemType Directory -Force $out | Out-Null

$p = Start-Process $exe -ArgumentList "--debug-perf", "--demo-autoplay", "--smoke-ms=$RunMs" -PassThru
$deadline = (Get-Date).AddSeconds(10)
$hwnd = [IntPtr]::Zero
# 等到窗口面积大于 200x200（排除启动时的小辅助窗口）
$rr = New-Object Win32Rect+RECT
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 100
    $hwnd = [Win32Rect]::LargestWindow([uint32]$p.Id)
    if ($hwnd -ne [IntPtr]::Zero -and [Win32Rect]::GetWindowRect($hwnd, [ref]$rr) -and (($rr.Right - $rr.Left) -gt 200) -and (($rr.Bottom - $rr.Top) -gt 200)) { break }
    $hwnd = [IntPtr]::Zero
}
if ($hwnd -eq [IntPtr]::Zero) { $p.Kill(); Write-Error '没有等到窗口'; exit 1 }
# 置顶，避免被其他窗口遮挡后抓到屏幕上别的内容（抓屏会包含遮挡物，注意截图可能含桌面私人内容，勿外传）
[void][Win32Rect]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0003)  # HWND_TOPMOST, NOMOVE|NOSIZE
Start-Sleep -Milliseconds 300

$lines = @()
for ($i = 0; $i -lt $Count; $i++) {
    $r = New-Object Win32Rect+RECT
    [void][Win32Rect]::GetWindowRect($hwnd, [ref]$r)
    $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $t = Get-Date -Format 'HH:mm:ss.fff'
    $g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size $w, $h))
    $name = 'shot_{0:D2}.png' -f $i
    $bmp.Save((Join-Path $out $name), [System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    $lines += "$t $name"
    Start-Sleep -Milliseconds $IntervalMs
}
$lines | Set-Content (Join-Path $out 'captures.txt') -Encoding utf8
if (-not $p.WaitForExit(15000)) { $p.Kill() }
Write-Host "已保存 $Count 张截图与 captures.txt 到 $out"
Write-Host "对应日志：logs 目录中最新的 .log（行首为 HH:mm:ss.fff 本机时间）"
