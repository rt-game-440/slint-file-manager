# 真实窗口截图入口（补充手段）：以 --debug-perf 启动 release 程序（-Open 指定起始目录，默认主目录），
# 用 Windows.Graphics.Capture（WGC）直接抓取本应用窗口自己的合成画面，并把每张图的本机时间（HH:mm:ss.fff）
# 写入 captures.txt，与 logs/*.log 行首的时间对应，用于复现只在真实渲染下出现的问题。
# 用法：powershell -File scripts/capture-window.ps1 [-Open 目录] [-Count 6] [-IntervalMs 250] [-RunMs 4000]
#
# 为什么不再按屏幕坐标抓屏（Issue #22 的隐私问题）：
#   旧实现用 CopyFromScreen 抓“窗口矩形所在的屏幕像素”，得到的是桌面上此刻显示的任何东西。
#   本应用是无边框窗口，GL（femtovg）画面若没有被合成为不透明像素（窗口内容 alpha=0 或尚未呈现），
#   屏幕上该矩形里就是别的窗口——即使窗口是前台、WindowFromPoint 也报“属于本窗口”，抓屏照样拍到桌面。
#   （Issue #39 已查明 alpha=0 的来源是 Slint 默认的透明窗口属性，产品已改为不透明窗口，默认启动即可抓到；
#    这些防护仍保留，防止任何回归导致保存桌面内容。）
#   旧脚本的“置顶”无法解决这一点；另外 GetWindowRect 还带一圈不可见的缩放边框，边缘永远是桌面像素。
# 现在的做法：
#   1. WGC 只读取目标窗口自己的画面，与遮挡、前台、屏幕位置无关；最小化时没有帧，直接拒绝。
#   2. 截图后做一致性检查：帧尺寸与窗口一致且不小于 200x200；至少一半采样点不透明（alpha>0）；
#      不是单一颜色。任何一项不满足就**不保存**，报错并以非 0 退出。
#   3. 不抓取光标。启动使用 --no-session 和临时数据目录，不读写用户真实会话。
# 它不是确定性截图，不能用于基线比较。需要 Windows 10 1903+ 与交互式桌面。
param(
    [string]$Open = '',
    [int]$Count = 6,
    [int]$IntervalMs = 250,
    [int]$RunMs = 4000,
    [switch]$Software
)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Runtime.WindowsRuntime
[void][Windows.Graphics.Capture.GraphicsCaptureItem, Windows.Graphics.Capture, ContentType = WindowsRuntime]
[void][Windows.Graphics.Capture.Direct3D11CaptureFramePool, Windows.Graphics.Capture, ContentType = WindowsRuntime]
[void][Windows.Graphics.DirectX.Direct3D11.IDirect3DSurface, Windows.Graphics.DirectX.Direct3D11, ContentType = WindowsRuntime]
[void][Windows.Graphics.Imaging.SoftwareBitmap, Windows.Graphics.Imaging, ContentType = WindowsRuntime]
[void][Windows.Storage.Streams.Buffer, Windows.Storage.Streams, ContentType = WindowsRuntime]
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class Wgc {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
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

  [ComImport, Guid("3628E81B-3CAC-4C60-B7F4-23CE0E0C3356"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  public interface IGraphicsCaptureItemInterop {
    IntPtr CreateForWindow(IntPtr window, [In] ref Guid iid);
    IntPtr CreateForMonitor(IntPtr monitor, [In] ref Guid iid);
  }
  [DllImport("combase.dll")] static extern int RoGetActivationFactory(IntPtr cls, [In] ref Guid iid, out IntPtr f);
  [DllImport("combase.dll", CharSet = CharSet.Unicode)] static extern int WindowsCreateString(string s, int len, out IntPtr h);
  [DllImport("combase.dll")] static extern int WindowsDeleteString(IntPtr h);
  [DllImport("d3d11.dll")] static extern int D3D11CreateDevice(IntPtr adapter, int type, IntPtr sw, uint flags, IntPtr levels, uint n, uint sdk, out IntPtr dev, out int lvl, out IntPtr ctx);
  [DllImport("d3d11.dll")] static extern int CreateDirect3D11DeviceFromDXGIDevice(IntPtr dxgi, out IntPtr dev);
  public static object ItemForWindow(IntPtr hwnd) {
    IntPtr hs; WindowsCreateString("Windows.Graphics.Capture.GraphicsCaptureItem", 44, out hs);
    Guid iidInterop = new Guid("3628E81B-3CAC-4C60-B7F4-23CE0E0C3356"); IntPtr f;
    int hr = RoGetActivationFactory(hs, ref iidInterop, out f); WindowsDeleteString(hs);
    if (hr != 0) throw new Exception("RoGetActivationFactory 0x" + hr.ToString("X"));
    var interop = (IGraphicsCaptureItemInterop)Marshal.GetObjectForIUnknown(f);
    Guid iidItem = new Guid("79C3F95B-31F7-4EC2-A464-632EF5D30760");
    return Marshal.GetObjectForIUnknown(interop.CreateForWindow(hwnd, ref iidItem));
  }
  // 返回 WinRT IDirect3DDevice 的 IUnknown 指针（PowerShell 里用 GetObjectForIUnknown 取对象）
  public static IntPtr NewDevice() {
    IntPtr dev, ctx; int lvl;
    int hr = D3D11CreateDevice(IntPtr.Zero, 1, IntPtr.Zero, 0x20, IntPtr.Zero, 0, 7, out dev, out lvl, out ctx);
    if (hr != 0) throw new Exception("D3D11CreateDevice 0x" + hr.ToString("X"));
    Guid iidDxgi = new Guid("54ec77fa-1377-44e6-8c32-88fd5f44c84c");
    IntPtr dxgi; Marshal.QueryInterface(dev, ref iidDxgi, out dxgi);
    IntPtr w; hr = CreateDirect3D11DeviceFromDXGIDevice(dxgi, out w);
    if (hr != 0) throw new Exception("CreateDirect3D11DeviceFromDXGIDevice 0x" + hr.ToString("X"));
    return w;
  }
}
'@

[void][Wgc]::SetProcessDPIAware()  # 窗口矩形用物理像素，才能与帧尺寸比较
$exe = Join-Path (Get-Location) 'target\release\slint-file-manager.exe'
if (-not (Test-Path $exe)) { Write-Error '未找到 release 程序，请先运行 scripts/build.ps1'; exit 1 }

$asTask = [System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object { $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' } | Select-Object -First 1
function Await($op, $type) { $t = $asTask.MakeGenericMethod($type).Invoke($null, @($op)); [void]$t.Wait(5000); $t.Result }

# 把一帧转成 System.Drawing.Bitmap（BGRA，保留 alpha）
function ConvertFrame($frame) {
    $cm = [Windows.Graphics.Imaging.SoftwareBitmap].GetMethod('CreateCopyFromSurfaceAsync', [Type[]]@([Windows.Graphics.DirectX.Direct3D11.IDirect3DSurface]))
    $sb = Await ($cm.Invoke($null, @($frame.Surface))) ([Windows.Graphics.Imaging.SoftwareBitmap])
    $w = $frame.ContentSize.Width; $h = $frame.ContentSize.Height
    $sw = $sb.PixelWidth
    $buf = New-Object Windows.Storage.Streams.Buffer ([uint32]($sw * $sb.PixelHeight * 4))
    $sb.CopyToBuffer($buf)
    $bytes = New-Object byte[] ($sw * $sb.PixelHeight * 4)
    [System.Runtime.InteropServices.WindowsRuntime.WindowsRuntimeBufferExtensions]::CopyTo($buf, $bytes)
    $bmp = New-Object System.Drawing.Bitmap $w, $h, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $bd = $bmp.LockBits((New-Object System.Drawing.Rectangle 0, 0, $w, $h), 'WriteOnly', 'Format32bppArgb')
    for ($y = 0; $y -lt $h; $y++) {
        [System.Runtime.InteropServices.Marshal]::Copy($bytes, $y * $sw * 4, [IntPtr]($bd.Scan0.ToInt64() + $y * $bd.Stride), $w * 4)
    }
    $bmp.UnlockBits($bd); $sb.Dispose()
    $bmp
}

# 一致性检查：返回 $null 表示通过，否则返回拒绝原因
function CheckBitmap($bmp, $expectW, $expectH) {
    if ($bmp.Width -lt 200 -or $bmp.Height -lt 200) { return "帧尺寸过小 $($bmp.Width)x$($bmp.Height)" }
    if ([math]::Abs($bmp.Width - $expectW) -gt 64 -or [math]::Abs($bmp.Height - $expectH) -gt 64) { return "帧尺寸 $($bmp.Width)x$($bmp.Height) 与窗口 ${expectW}x${expectH} 不一致" }
    $opaque = 0; $total = 0; $colors = @{}
    for ($y = 3; $y -lt $bmp.Height; $y += [math]::Max(1, [int]($bmp.Height / 24))) {
        for ($x = 3; $x -lt $bmp.Width; $x += [math]::Max(1, [int]($bmp.Width / 32))) {
            $c = $bmp.GetPixel($x, $y); $total++
            if ($c.A -gt 0) { $opaque++ }
            $colors["$($c.R),$($c.G),$($c.B)"] = 1
        }
    }
    if ($opaque * 2 -lt $total) { return "窗口画面几乎全透明（不透明采样 $opaque/$total）：窗口内容没有被合成到桌面，抓屏会拍到桌面其他内容" }
    if ($colors.Count -lt 3) { return "画面只有 $($colors.Count) 种颜色，疑似空白或全黑" }
    return $null
}

$stamp = Get-Date -Format 'yyyy-MM-dd HH-mm-ss'
$out = Join-Path (Get-Location) "artifacts\real\$stamp"
New-Item -ItemType Directory -Force $out | Out-Null
$dataDir = Join-Path $env:TEMP "sfm-capture-data-$PID"
New-Item -ItemType Directory -Force $dataDir | Out-Null

$argv = @("--debug-perf", "--smoke-ms=$RunMs", "--no-session", "--data-dir=`"$dataDir`"")
if ($Open) { $argv += "--open=`"$Open`"" }
# 默认测产品默认启动方式：清掉可能影响渲染器的环境变量；-Software 用产品开关（ADR 0010）
foreach ($v in 'SLINT_BACKEND', 'SFM_RENDERER') { [Environment]::SetEnvironmentVariable($v, $null, 'Process') }
if ($Software) { $argv += '--renderer=software' }
$p = Start-Process $exe -ArgumentList $argv -PassThru
$fail = $null
$saved = 0
$pool = $null; $sess = $null
try {
    # 等到窗口面积大于 200x200（排除启动时的小辅助窗口）
    $deadline = (Get-Date).AddSeconds(10)
    $hwnd = [IntPtr]::Zero
    $rr = New-Object Wgc+RECT
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 100
        $hwnd = [Wgc]::LargestWindow([uint32]$p.Id)
        if ($hwnd -ne [IntPtr]::Zero -and [Wgc]::GetWindowRect($hwnd, [ref]$rr) -and (($rr.Right - $rr.Left) -gt 200) -and (($rr.Bottom - $rr.Top) -gt 200)) { break }
        $hwnd = [IntPtr]::Zero
    }
    if ($hwnd -eq [IntPtr]::Zero) { throw '没有等到窗口' }
    if ([Wgc]::IsIconic($hwnd)) { throw '窗口处于最小化，没有可抓取的画面，拒绝截图' }
    Start-Sleep -Milliseconds 500

    $item = [Windows.Graphics.Capture.GraphicsCaptureItem]([Wgc]::ItemForWindow($hwnd))
    $dev = [System.Runtime.InteropServices.Marshal]::GetObjectForIUnknown([Wgc]::NewDevice())
    $create = [Windows.Graphics.Capture.Direct3D11CaptureFramePool].GetMethod('CreateFreeThreaded')
    $pool = $create.Invoke($null, @($dev, [Windows.Graphics.DirectX.DirectXPixelFormat]::B8G8R8A8UIntNormalized, [int]2, $item.Size))
    $sess = $pool.CreateCaptureSession($item)
    $sess.IsCursorCaptureEnabled = $false
    $sess.StartCapture()

    $lines = @()
    $last = $null
    for ($i = 0; $i -lt $Count; $i++) {
        # WGC 只在画面变化时出帧：等一小会儿，没有新帧就沿用上一帧
        $frame = $null
        $until = (Get-Date).AddMilliseconds([math]::Max(1000, $IntervalMs * 2))
        while (-not $frame -and (Get-Date) -lt $until) {
            $frame = $pool.TryGetNextFrame()
            if (-not $frame) { Start-Sleep -Milliseconds 30 }
        }
        $t = Get-Date -Format 'HH:mm:ss.fff'
        if ($frame) {
            if ($last) { $last.Dispose() }
            $last = ConvertFrame $frame
            $frame.Dispose()
        }
        elseif ([Wgc]::IsIconic($hwnd)) { throw '窗口被最小化，没有新的画面，拒绝截图' }
        elseif (-not $last) { throw '窗口没有产生任何画面帧（最小化、被隐藏或未呈现），拒绝截图' }
        [void][Wgc]::GetWindowRect($hwnd, [ref]$rr)
        $why = CheckBitmap $last ($rr.Right - $rr.Left) ($rr.Bottom - $rr.Top)
        if ($why) { throw "截图未通过检查，未保存：$why" }
        $name = 'shot_{0:D2}.png' -f $i
        $path = Join-Path $out $name
        $last.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
        $saved++
        $lines += "$t $name"
        Start-Sleep -Milliseconds $IntervalMs
    }
    $lines | Set-Content (Join-Path $out 'captures.txt') -Encoding utf8
}
catch { $fail = $_.Exception.Message }
finally {
    if ($sess) { $sess.Dispose() }
    if ($pool) { $pool.Dispose() }
    if ($p -and -not $p.WaitForExit($RunMs + 15000)) { $p.Kill() }
    Remove-Item $dataDir -Recurse -Force -ErrorAction SilentlyContinue
}
if ($fail) {
    # 任何失败都不留图片，避免把可疑截图交给后续步骤
    Remove-Item $out -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "截图失败：$fail"
    exit 1
}
Write-Host "已保存 $saved 张截图与 captures.txt 到 $out"
Write-Host "对应日志：logs 目录中最新的 .log（行首为 HH:mm:ss.fff 本机时间）"
