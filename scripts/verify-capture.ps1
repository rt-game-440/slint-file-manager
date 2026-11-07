# 外部抓取可见性核实（Issue #15 引入；Issue #39 起为发布前必过检查，已接入 scripts/verify.ps1）：本应用窗口在桌面合成里是否真的可见？
# 对本应用自己的窗口，分别用几种外部手段取样，**只在内存中比对主题色，不保存任何截图**：
#   屏幕抓取    CopyFromScreen（Win+Shift+S、PrintScreen、整屏共享、录屏走桌面合成结果，与此等价）
#   WGC 窗口    Windows.Graphics.Capture 单窗口抓取（窗口共享）
#   WGC 显示器  Windows.Graphics.Capture 抓整个显示器后裁出本应用区域（Snipping Tool、屏幕共享）
#   PrintWindow PW_RENDERFULLCONTENT
# 取样点：侧栏空白处应为主题色 #eef0f3，内容区空白处应为 #fdfdfe（ui/theme.slint）。命中率 = 命中点数 / 取样点数。
# 启动用 --no-session、临时数据目录、临时夹具目录（不显示用户真实目录）。
# 判定：四种方式都“可见”（命中率 ≥ 60%）才通过。退出码 0 通过、1 有方式不可见、2 无法判断（找不到窗口、取不到帧等）。
# 需要已解锁的交互桌面（锁屏或远程会话断开时屏幕抓取全黑，会判为不可见）。
# 用法：powershell -File scripts/verify-capture.ps1 [-Software] [-Exe 其他构建的exe]
#   默认不加渲染器参数（产品默认启动方式）；-Software 用产品开关 --renderer=software（ADR 0010）。
param([switch]$Software, [string]$Exe)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Runtime.WindowsRuntime
[void][Windows.Graphics.Capture.GraphicsCaptureItem, Windows.Graphics.Capture, ContentType = WindowsRuntime]
[void][Windows.Graphics.Capture.Direct3D11CaptureFramePool, Windows.Graphics.Capture, ContentType = WindowsRuntime]
[void][Windows.Graphics.DirectX.Direct3D11.IDirect3DSurface, Windows.Graphics.DirectX.Direct3D11, ContentType = WindowsRuntime]
[void][Windows.Graphics.Imaging.SoftwareBitmap, Windows.Graphics.Imaging, ContentType = WindowsRuntime]
[void][Windows.Storage.Streams.Buffer, Windows.Storage.Streams, ContentType = WindowsRuntime]
Add-Type @'
using System; using System.Runtime.InteropServices; using System.Drawing;
public static class Cap {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
  [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr h, int idx);
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(int x, int y);
  [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint flags);
  [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int attr, out int v, int size);
  [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int attr, out RECT r, int size);
  [DllImport("user32.dll")] public static extern IntPtr MonitorFromWindow(IntPtr h, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct MONITORINFO { public int cbSize; public RECT rcMonitor; public RECT rcWork; public uint dwFlags; }
  [DllImport("user32.dll")] public static extern bool GetMonitorInfo(IntPtr m, ref MONITORINFO mi);
  [ComImport, Guid("3628E81B-3CAC-4C60-B7F4-23CE0E0C3356"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  public interface IGraphicsCaptureItemInterop { IntPtr CreateForWindow(IntPtr window, [In] ref Guid iid); IntPtr CreateForMonitor(IntPtr monitor, [In] ref Guid iid); }
  [DllImport("combase.dll")] static extern int RoGetActivationFactory(IntPtr cls, [In] ref Guid iid, out IntPtr f);
  [DllImport("combase.dll", CharSet = CharSet.Unicode)] static extern int WindowsCreateString(string s, int len, out IntPtr h);
  [DllImport("combase.dll")] static extern int WindowsDeleteString(IntPtr h);
  [DllImport("d3d11.dll")] static extern int D3D11CreateDevice(IntPtr adapter, int type, IntPtr sw, uint flags, IntPtr levels, uint n, uint sdk, out IntPtr dev, out int lvl, out IntPtr ctx);
  [DllImport("d3d11.dll")] static extern int CreateDirect3D11DeviceFromDXGIDevice(IntPtr dxgi, out IntPtr dev);
  // monitor 为 IntPtr.Zero 时抓窗口 hwnd，否则抓该显示器
  public static object Item(IntPtr hwnd, IntPtr monitor) {
    IntPtr hs; WindowsCreateString("Windows.Graphics.Capture.GraphicsCaptureItem", 44, out hs);
    Guid iidInterop = new Guid("3628E81B-3CAC-4C60-B7F4-23CE0E0C3356"); IntPtr f;
    int hr = RoGetActivationFactory(hs, ref iidInterop, out f); WindowsDeleteString(hs);
    if (hr != 0) throw new Exception("RoGetActivationFactory 0x" + hr.ToString("X"));
    var interop = (IGraphicsCaptureItemInterop)Marshal.GetObjectForIUnknown(f);
    Guid iidItem = new Guid("79C3F95B-31F7-4EC2-A464-632EF5D30760");
    return Marshal.GetObjectForIUnknown(monitor == IntPtr.Zero ? interop.CreateForWindow(hwnd, ref iidItem) : interop.CreateForMonitor(monitor, ref iidItem));
  }
  public static IntPtr NewDevice() {
    IntPtr dev, ctx; int lvl;
    int hr = D3D11CreateDevice(IntPtr.Zero, 1, IntPtr.Zero, 0x20, IntPtr.Zero, 0, 7, out dev, out lvl, out ctx);
    if (hr != 0) throw new Exception("D3D11CreateDevice 0x" + hr.ToString("X"));
    Guid iidDxgi = new Guid("54ec77fa-1377-44e6-8c32-88fd5f44c84c"); IntPtr dxgi; Marshal.QueryInterface(dev, ref iidDxgi, out dxgi);
    IntPtr w; hr = CreateDirect3D11DeviceFromDXGIDevice(dxgi, out w);
    if (hr != 0) throw new Exception("CreateDirect3D11DeviceFromDXGIDevice 0x" + hr.ToString("X"));
    return w;
  }
  delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc p, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
  public static IntPtr LargestWindow(uint pid) {
    IntPtr best = IntPtr.Zero; long area = 0;
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p); RECT r;
      if (p == pid && IsWindowVisible(h) && GetWindowRect(h, out r)) {
        long a = (long)(r.Right - r.Left) * (r.Bottom - r.Top); if (a > area) { area = a; best = h; }
      }
      return true;
    }, IntPtr.Zero);
    return best;
  }
}
'@
[void][Cap]::SetProcessDPIAware()

# 主题色（ui/theme.slint）
$sidebarRgb = @(0xee, 0xf0, 0xf3)
$contentRgb = @(0xfd, 0xfd, 0xfe)
function Near($c, $rgb) { ([math]::Abs($c.R - $rgb[0]) -le 3) -and ([math]::Abs($c.G - $rgb[1]) -le 3) -and ([math]::Abs($c.B - $rgb[2]) -le 3) }

# 取样：在 (w x h) 的位图上，侧栏空白与内容区空白各取若干点。返回 @{hit; total; opaque}
function Probe($bmp) {
    $w = $bmp.Width; $h = $bmp.Height
    # 窗口 1100x720 逻辑像素时侧栏 184px 宽：按比例取点（侧栏下部空白、内容区下部空白）
    $pts = @()
    foreach ($fy in 0.88, 0.92, 0.95) { foreach ($fx in 0.03, 0.06, 0.09, 0.12) { $pts += , @($fx, $fy, 's') } }
    foreach ($fy in 0.84, 0.87, 0.90) { foreach ($fx in 0.34, 0.5, 0.66) { $pts += , @($fx, $fy, 'c') } }
    $hit = 0; $opaque = 0; $hs = 0; $hc = 0; $seen = @{}
    foreach ($p in $pts) {
        $c = $bmp.GetPixel([int]($w * $p[0]), [int]($h * $p[1]))
        if ($c.A -gt 0) { $opaque++ }
        $rgb = if ($p[2] -eq 's') { $sidebarRgb } else { $contentRgb }
        if (Near $c $rgb) { $hit++; if ($p[2] -eq 's') { $hs++ } else { $hc++ } }
        else { $k = '{0:x2}{1:x2}{2:x2}' -f $c.R, $c.G, $c.B; $seen[$k] = 1 + [int]$seen[$k] }
    }
    @{ hit = $hit; total = $pts.Count; opaque = $opaque; side = $hs; content = $hc; other = (($seen.GetEnumerator() | Sort-Object Value -Descending | Select-Object -First 3 | ForEach-Object { "#$($_.Key)x$($_.Value)" }) -join ' ') }
}
function Report($name, $r) {
    $verdict = if ($r.hit -ge [math]::Ceiling($r.total * 0.6)) { '可见（与主题色一致）' } else { '不可见或内容不对' }
    Write-Host ("  {0,-26} 命中 {1}/{2}（侧栏 {3}/12，内容区 {4}/9），不透明 {5}/{2} -> {6}" -f $name, $r.hit, $r.total, $r.side, $r.content, $r.opaque, $verdict)
    if ($r.other) { Write-Host ("      未命中点的主要颜色：{0}" -f $r.other) }
    $script:results += , @($name, ($r.hit -ge [math]::Ceiling($r.total * 0.6)))
}
$script:results = @()
$script:code = 0


$asTask = [System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object { $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' } | Select-Object -First 1
function Await($op, $type) { $t = $asTask.MakeGenericMethod($type).Invoke($null, @($op)); [void]$t.Wait(5000); $t.Result }
# 用 WGC 抓一帧（窗口或整个显示器），返回内存位图；不落盘
function WgcFrame($hwnd, $monitor) {
    $item = [Windows.Graphics.Capture.GraphicsCaptureItem]([Cap]::Item($hwnd, $monitor))
    $dev = [System.Runtime.InteropServices.Marshal]::GetObjectForIUnknown([Cap]::NewDevice())
    $create = [Windows.Graphics.Capture.Direct3D11CaptureFramePool].GetMethod('CreateFreeThreaded')
    $pool = $create.Invoke($null, @($dev, [Windows.Graphics.DirectX.DirectXPixelFormat]::B8G8R8A8UIntNormalized, [int]2, $item.Size))
    $sess = $pool.CreateCaptureSession($item); $sess.IsCursorCaptureEnabled = $false; $sess.StartCapture()
    $frame = $null
    for ($i = 0; $i -lt 50 -and -not $frame; $i++) { Start-Sleep -Milliseconds 100; $frame = $pool.TryGetNextFrame() }
    $bmp = $null
    if ($frame) {
        $cm = [Windows.Graphics.Imaging.SoftwareBitmap].GetMethod('CreateCopyFromSurfaceAsync', [Type[]]@([Windows.Graphics.DirectX.Direct3D11.IDirect3DSurface]))
        $sb = Await ($cm.Invoke($null, @($frame.Surface))) ([Windows.Graphics.Imaging.SoftwareBitmap])
        $fw = $frame.ContentSize.Width; $fh = $frame.ContentSize.Height; $sw = $sb.PixelWidth
        $buf = New-Object Windows.Storage.Streams.Buffer ([uint32]($sw * $sb.PixelHeight * 4)); $sb.CopyToBuffer($buf)
        $bytes = New-Object byte[] ($sw * $sb.PixelHeight * 4)
        [System.Runtime.InteropServices.WindowsRuntime.WindowsRuntimeBufferExtensions]::CopyTo($buf, $bytes)
        $bmp = New-Object System.Drawing.Bitmap $fw, $fh, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
        $bd = $bmp.LockBits((New-Object System.Drawing.Rectangle 0, 0, $fw, $fh), 'WriteOnly', 'Format32bppArgb')
        for ($y = 0; $y -lt $fh; $y++) { [System.Runtime.InteropServices.Marshal]::Copy($bytes, $y * $sw * 4, [IntPtr]($bd.Scan0.ToInt64() + $y * $bd.Stride), $fw * 4) }
        $bmp.UnlockBits($bd); $sb.Dispose(); $frame.Dispose()
    }
    $sess.Dispose(); $pool.Dispose()
    $bmp
}

# ---- 启动 ----
$exe = if ($Exe) { (Resolve-Path $Exe).Path } else { Join-Path (Get-Location) 'target\release\slint-file-manager.exe' }
if (-not (Test-Path $exe)) { Write-Error '未找到 release 程序，请先运行 scripts/build.ps1'; exit 1 }
$work = Join-Path $env:TEMP ('sfm-capture-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
$fixture = Join-Path $work 'fixture'; $data = Join-Path $work 'data'
New-Item -ItemType Directory -Force $fixture, $data | Out-Null
1..5 | ForEach-Object { [IO.File]::WriteAllBytes((Join-Path $fixture "样例$_.txt"), [byte[]](1, 2, 3)) }
# 清掉可能影响渲染器选择的环境变量，确保测的是产品默认启动方式（或 -Software 的显式开关）
foreach ($v in 'SLINT_BACKEND', 'SFM_RENDERER') { [Environment]::SetEnvironmentVariable($v, $null, 'Process') }
$mode = if ($Software) { '软件渲染（--renderer=software）' } else { '默认启动方式（不加渲染器参数，femtovg/OpenGL）' }
Write-Host "渲染器：$mode"
$argv = @("--no-session", "--data-dir=`"$data`"", "--open=`"$fixture`"", '--smoke-ms=30000')
if ($Software) { $argv += '--renderer=software' }
$p = Start-Process $exe -ArgumentList $argv -PassThru
try {
    $hwnd = [IntPtr]::Zero
    for ($i = 0; $i -lt 80; $i++) {
        Start-Sleep -Milliseconds 250
        $hwnd = [Cap]::LargestWindow([uint32]$p.Id)
        if ($hwnd -ne [IntPtr]::Zero) { $rr = New-Object Cap+RECT; [void][Cap]::DwmGetWindowAttribute($hwnd, 9, [ref]$rr, [Runtime.InteropServices.Marshal]::SizeOf($rr)); if (($rr.Right - $rr.Left) -ge 400 -and ($rr.Bottom - $rr.Top) -ge 300) { break } }
    }
    if ($hwnd -eq [IntPtr]::Zero) { Write-Host '没有找到应用窗口；结论：无法判断'; exit 2 }
    Start-Sleep -Milliseconds 1500
    # 置顶，避免被别的窗口遮挡后把别人的内容当成“看不见”的证据
    [void][Cap]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0043)   # HWND_TOPMOST, NOMOVE|NOSIZE|SHOWWINDOW
    [void][Cap]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 800
    $r = New-Object Cap+RECT
    [void][Cap]::DwmGetWindowAttribute($hwnd, 9, [ref]$r, [Runtime.InteropServices.Marshal]::SizeOf($r))  # DWMWA_EXTENDED_FRAME_BOUNDS
    $w = $r.Right - $r.Left; $h = $r.Bottom - $r.Top
    Write-Host "窗口可见区域 ${w}x${h}（物理像素）"
    $ex = [Cap]::GetWindowLong($hwnd, -20); $st = [Cap]::GetWindowLong($hwnd, -16)
    $cloak = 0; [void][Cap]::DwmGetWindowAttribute($hwnd, 13, [ref]$cloak, 4)
    $top = [Cap]::GetAncestor([Cap]::WindowFromPoint([int]($r.Left + $w / 2), [int]($r.Top + $h * 0.9)), 2)
    $tpid = 0; [void][Cap]::GetWindowThreadProcessId($top, [ref]$tpid)
    Write-Host ("窗口属性：样式 0x{0:X8}，扩展样式 0x{1:X8}（LAYERED 0x80000、NOREDIRECTIONBITMAP 0x200000、TOPMOST 0x8、NOACTIVATE 0x8000000），cloaked={2}；窗口中下部最上层窗口{3}本应用" -f $st, $ex, $cloak, $(if ($tpid -eq $p.Id) { '是' } else { '不是' }))

    # 进程里加载的图形驱动模块（判断 OpenGL 走了哪块 GPU 的驱动、是否用了 DirectComposition）
    $mods = try { (Get-Process -Id $p.Id).Modules | ForEach-Object { $_.ModuleName.ToLower() } } catch { @() }
    $gpuMods = $mods | Where-Object { $_ -match '^(opengl32|nvoglv64|atio6axx|amdxx64|ig.*icd64|dcomp|nvwgf2umx|amdxc64|softbuffer)\.dll$' } | Sort-Object -Unique
    Write-Host ("图形模块：{0}" -f $(if ($gpuMods) { $gpuMods -join ', ' } else { '（无）' }))

    # 1. 屏幕抓取
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size)
    $g.Dispose()
    Report '屏幕抓取（截图/共享/录屏）' (Probe $bmp)
    # 对照：窗口左右外侧各一点的颜色（若与窗口内的“未命中色”相同，说明窗口内看到的就是窗口后面的桌面，即窗口是透明的）
    $one = New-Object System.Drawing.Bitmap 1, 1; $g1 = [System.Drawing.Graphics]::FromImage($one)
    $outs = foreach ($px in @(($r.Left - 12), ($r.Right + 12))) { $g1.CopyFromScreen($px, $r.Top + [int]($h * 0.9), 0, 0, $one.Size); $c = $one.GetPixel(0, 0); '#{0:x2}{1:x2}{2:x2}' -f $c.R, $c.G, $c.B }
    $g1.Dispose(); $one.Dispose()
    Write-Host ("      窗口外侧参照点颜色（左、右）：{0}" -f ($outs -join ' '))
    $bmp.Dispose()

    # 2. PrintWindow
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp); $hdc = $g.GetHdc()
    $ok = [Cap]::PrintWindow($hwnd, $hdc, 2)
    $g.ReleaseHdc($hdc); $g.Dispose()
    Report ("PrintWindow（返回 $ok）") (Probe $bmp); $bmp.Dispose()

    # 3. WGC 单窗口（窗口共享、Win+G）
    $bmp = WgcFrame $hwnd ([IntPtr]::Zero)
    if ($bmp) { Report 'WGC 窗口抓取' (Probe $bmp); $bmp.Dispose() } else { Write-Host '  WGC 窗口抓取               没有取到帧'; $script:code = 2 }
    # 4. WGC 整个显示器（Snipping Tool/Win+Shift+S、Teams／OBS 的屏幕共享用这类路径），裁出本应用窗口所在区域
    $mon = [Cap]::MonitorFromWindow($hwnd, 2)
    $mi = New-Object Cap+MONITORINFO; $mi.cbSize = [Runtime.InteropServices.Marshal]::SizeOf($mi); [void][Cap]::GetMonitorInfo($mon, [ref]$mi)
    $full = WgcFrame ([IntPtr]::Zero) $mon
    if ($full) {
        $crop = New-Object System.Drawing.Rectangle ($r.Left - $mi.rcMonitor.Left), ($r.Top - $mi.rcMonitor.Top), $w, $h
        $crop.Intersect((New-Object System.Drawing.Rectangle 0, 0, $full.Width, $full.Height))
        $part = $full.Clone($crop, $full.PixelFormat); $full.Dispose()
        Report 'WGC 显示器抓取' (Probe $part); $part.Dispose()
    } else { Write-Host '  WGC 显示器抓取             没有取到帧'; $script:code = 2 }

} finally {
    if (-not $p.HasExited) { $p.Kill() }
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
$bad = @($script:results | Where-Object { -not $_[1] } | ForEach-Object { $_[0] })
if ($script:code -eq 2) { Write-Host "结论：无法判断（$mode）"; exit 2 }
if ($script:results.Count -lt 4 -or $bad.Count -gt 0) { Write-Host ("结论：不通过，不可见的方式：{0}（$mode）" -f ($bad -join '、')); exit 1 }
Write-Host "结论：通过，四种方式都取到主题色（$mode）"
exit 0
