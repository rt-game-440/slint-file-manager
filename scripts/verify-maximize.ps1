# 最大化只占工作区检查（Issue #45）：无边框主窗口最大化后，窗口矩形必须等于所在显示器的工作区（MONITORINFO.rcWork），
# 不得盖住任务栏；还原后恢复最大化前的尺寸。默认渲染（femtovg/OpenGL）与 --renderer=software 都测。
# 用 Win32 读取窗口矩形与 rcWork 比对，只读数值，不保存任何截图。启动用 --no-session、临时数据目录、临时夹具目录。
# 最大化方式两种：ShowWindow(SW_MAXIMIZE)（与 winit／Slint 的 set_maximized 同一条路径），
# 以及 WM_SYSCOMMAND(SC_MAXIMIZE／SC_RESTORE)（Win+↑、双击标题栏走的系统路径）。
# 容差：窗口矩形与 rcWork 各边相差不超过 2 像素。
# 退出码：0 通过、1 失败、2 无法判断（找不到窗口等）。
# 用法：powershell -File scripts/verify-maximize.ps1 [-Mode all|default|software] [-Exe 其他构建的exe] [-Diag]
param([ValidateSet('all', 'default', 'software')][string]$Mode = 'all', [string]$Exe, [switch]$Diag)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class Mx {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct MONITORINFO { public int cbSize; public RECT rcMonitor; public RECT rcWork; public uint dwFlags; }
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsZoomed(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
  public static void Click(int x, int y, int times) { SetCursorPos(x, y); System.Threading.Thread.Sleep(150); for (int i = 0; i < times; i++) { mouse_event(2, 0, 0, 0, IntPtr.Zero); mouse_event(4, 0, 0, 0, IntPtr.Zero); System.Threading.Thread.Sleep(60); } }
  [DllImport("user32.dll")] public static extern IntPtr MonitorFromWindow(IntPtr h, uint flags);
  [DllImport("user32.dll")] public static extern bool GetMonitorInfo(IntPtr m, ref MONITORINFO mi);
  [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr h, int idx);
  delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc p, IntPtr l);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  public static IntPtr LargestWindow(uint pid) {
    IntPtr best = IntPtr.Zero; long area = 0;
    EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); RECT r;
      if (p == pid && IsWindowVisible(h) && GetWindowRect(h, out r)) { long a = (long)(r.Right - r.Left) * (r.Bottom - r.Top); if (a > area) { area = a; best = h; } }
      return true; }, IntPtr.Zero);
    return best;
  }
  [DllImport("user32.dll")] static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  [DllImport("dwmapi.dll")] static extern int DwmGetWindowAttribute(IntPtr h, int attr, out RECT r, int size);
  // 客户区（程序实际绘制的区域）的屏幕矩形
  public static RECT Client(IntPtr h) { RECT c; GetClientRect(h, out c); var p = new POINT(); ClientToScreen(h, ref p); var r = new RECT(); r.Left = p.X; r.Top = p.Y; r.Right = p.X + c.Right; r.Bottom = p.Y + c.Bottom; return r; }
  // DWM 视觉边界（不含不可见缩放边框）
  public static RECT Visible(IntPtr h) { RECT r; DwmGetWindowAttribute(h, 9, out r, Marshal.SizeOf(typeof(RECT))); return r; }
  public static RECT Rect(IntPtr h) { RECT r; GetWindowRect(h, out r); return r; }
  public static MONITORINFO Mon(IntPtr h) { var mi = new MONITORINFO(); mi.cbSize = Marshal.SizeOf(mi); GetMonitorInfo(MonitorFromWindow(h, 2), ref mi); return mi; }
}
'@
[void][Mx]::SetProcessDPIAware()
if (-not $Exe) { $Exe = Join-Path (Get-Location) 'target\release\slint-file-manager.exe' }
if (-not (Test-Path $Exe)) { Write-Host "找不到 $Exe，先运行 scripts/build.ps1"; exit 2 }

function R2S($r) { '({0},{1})-({2},{3}) {4}x{5}' -f $r.Left, $r.Top, $r.Right, $r.Bottom, ($r.Right - $r.Left), ($r.Bottom - $r.Top) }
function SameRect($a, $b, $tol) { ([math]::Abs($a.Left - $b.Left) -le $tol) -and ([math]::Abs($a.Top - $b.Top) -le $tol) -and ([math]::Abs($a.Right - $b.Right) -le $tol) -and ([math]::Abs($a.Bottom - $b.Bottom) -le $tol) }

function Run-One($software) {
    $name = if ($software) { '软件渲染（--renderer=software）' } else { '默认启动方式（femtovg/OpenGL）' }
    Write-Host "== $name"
    $work = Join-Path $env:TEMP ('sfm-maximize-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    $fixture = Join-Path $work 'fixture'; $data = Join-Path $work 'data'
    New-Item -ItemType Directory -Force $fixture, $data | Out-Null
    1..3 | ForEach-Object { [IO.File]::WriteAllBytes((Join-Path $fixture "样例$_.txt"), [byte[]](1, 2, 3)) }
    foreach ($v in 'SLINT_BACKEND', 'SFM_RENDERER') { [Environment]::SetEnvironmentVariable($v, $null, 'Process') }
    $argv = @('--no-session', "--data-dir=`"$data`"", "--open=`"$fixture`"", '--smoke-ms=60000')
    if ($software) { $argv += '--renderer=software' }
    $p = Start-Process $Exe -ArgumentList $argv -PassThru
    $ok = $true
    try {
        $hwnd = [IntPtr]::Zero
        for ($i = 0; $i -lt 80; $i++) {
            Start-Sleep -Milliseconds 250
            $hwnd = [Mx]::LargestWindow([uint32]$p.Id)
            if ($hwnd -ne [IntPtr]::Zero) { $r = [Mx]::Rect($hwnd); if (($r.Right - $r.Left) -ge 400) { break } }
        }
        if ($hwnd -eq [IntPtr]::Zero) { Write-Host '  没有找到应用窗口；无法判断'; return 2 }
        Start-Sleep -Milliseconds 1500
        [void][Mx]::SetWindowPos($hwnd, [IntPtr](-1), 0, 0, 0, 0, 0x0043)   # 置顶，保证真实点击落在本窗口上
        [void][Mx]::SetForegroundWindow($hwnd)
        Start-Sleep -Milliseconds 500
        $normal = [Mx]::Rect($hwnd)
        $style = [Mx]::GetWindowLong($hwnd, -16)
        Write-Host ('  样式 0x{0:X8}，普通窗口 {1}' -f $style, (R2S $normal))
        $sc = ($normal.Right - $normal.Left) / 1100.0   # 首次启动逻辑宽 1100，由此得缩放比例
        # 标题栏最大化按钮与右上角的距离（ui/main.slint：控制区右缩进 4px，按钮宽 44px、高 30px，顶部 2px）
        $btn = { $r = [Mx]::Rect($hwnd); [Mx]::Click([int]($r.Right - 70 * $sc), [int]($r.Top + 17 * $sc), 1) }
        $dbl = { $r = [Mx]::Rect($hwnd); [Mx]::Click($r.Left + [int](90 * $sc), $r.Top + [int](17 * $sc), 2) }
        $steps = @(
            @{ n = 'ShowWindow(SW_MAXIMIZE)'; max = { [void][Mx]::ShowWindow($hwnd, 3) }; res = { [void][Mx]::ShowWindow($hwnd, 9) } },
            @{ n = '点击标题栏最大化按钮（Slint maximized 属性路径）'; max = $btn; res = $btn },
            @{ n = '双击标题栏空白处'; max = $dbl; res = $dbl },
            @{ n = 'WM_SYSCOMMAND SC_MAXIMIZE'; max = { [void][Mx]::SendMessage($hwnd, 0x112, [IntPtr]0xF030, [IntPtr]::Zero) }; res = { [void][Mx]::SendMessage($hwnd, 0x112, [IntPtr]0xF120, [IntPtr]::Zero) } }
        )
        foreach ($s in $steps) {
            & $s.max
            Start-Sleep -Milliseconds 1200
            $m = [Mx]::Client($hwnd); $outer = [Mx]::Rect($hwnd); $vis = [Mx]::Visible($hwnd); $mi = [Mx]::Mon($hwnd); $wa = $mi.rcWork
            $zoomed = [Mx]::IsZoomed($hwnd)
            $good = $zoomed -and (SameRect $m $wa 2)
            Write-Host ('  {0}：客户区 {1}（窗口外框 {6}，DWM 可见边界 {7}），工作区 {2}，显示器 {3}，IsZoomed={4} -> {5}' -f $s.n, (R2S $m), (R2S $wa), (R2S $mi.rcMonitor), $zoomed, $(if ($good) { '占工作区' } else { '不符' }), (R2S $outer), (R2S $vis))
            if (-not $good) { $ok = $false }
            & $s.res
            Start-Sleep -Milliseconds 1200
            $b = [Mx]::Rect($hwnd)
            $back = (-not [Mx]::IsZoomed($hwnd)) -and (SameRect $b $normal 2)
            Write-Host ('    还原后：{0}（原 {1}）-> {2}' -f (R2S $b), (R2S $normal), $(if ($back) { '恢复原尺寸' } else { '未恢复' }))
            if (-not $back) { $ok = $false }
        }
    } finally {
        if (-not $p.HasExited) { $p.Kill() }
        Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    }
    if ($ok) { return 0 } else { return 1 }
}

$codes = @()
if ($Mode -in 'all', 'default') { $codes += Run-One $false }
if ($Mode -in 'all', 'software') { $codes += Run-One $true }
if ($codes -contains 2) { Write-Host '结论：无法判断'; exit 2 }
if ($codes -contains 1) { Write-Host '结论：不通过，最大化没有只占工作区或还原尺寸不对'; exit 1 }
Write-Host '结论：通过，两种渲染最大化都只占工作区，还原后恢复原尺寸'
exit 0
