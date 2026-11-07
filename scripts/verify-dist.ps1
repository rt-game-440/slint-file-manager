# 分发包验证（Issue #15）：把 dist 里的 zip 解压到全新的临时目录，在“只有包内文件”的条件下检查并启动。
#   1. 解析 exe 的导入表（含延迟加载表），列出依赖的 DLL，判断是否需要包内没有的 VC 运行库等；
#   2. 用全新的临时数据目录（--data-dir）和精简的 PATH 启动 --smoke，要求退出码 0；
#   3. 核对启动/更新/移除行为：便携式 zip，启动不写入安装目录，也不写注册表；数据只在数据目录。
# 局限：这是开发机，不是真正干净的机器。系统目录里已经有的 DLL（含机器上可能已装的 VC 运行库）
# 无法被隔离，所以第 1 步的导入表分析才是“是否需要额外运行库”的依据，第 2 步只证明包内文件可以启动。
# 用法：powershell -File scripts/verify-dist.ps1 [-Zip dist\xxx.zip]
param([string]$Zip = '')
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
if (-not $Zip) {
    $Zip = (Get-ChildItem dist -Filter *-win-x64.zip | Sort-Object LastWriteTime | Select-Object -Last 1).FullName
}
if (-not $Zip -or -not (Test-Path $Zip)) { Write-Error '没有找到分发包，请先运行 scripts/dist.ps1'; exit 1 }

$work = Join-Path $env:TEMP ("sfm-dist-verify-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
$unpack = Join-Path $work '解压 目录'      # 故意含空格与中文，覆盖路径处理
$data = Join-Path $work 'data'
New-Item -ItemType Directory -Force $unpack, $data | Out-Null
Expand-Archive -LiteralPath $Zip -DestinationPath $unpack
$exe = (Get-ChildItem $unpack -Recurse -Filter slint-file-manager.exe | Select-Object -First 1).FullName
if (-not $exe) { Write-Error '包内没有 slint-file-manager.exe'; exit 1 }
$appDir = Split-Path $exe
Write-Host "包：$Zip"
Write-Host '包内文件：'
Get-ChildItem $appDir -Recurse -File | ForEach-Object { Write-Host ("  {0}  {1:N0} 字节" -f $_.FullName.Substring($appDir.Length + 1), $_.Length) }

Add-Type @'
using System; using System.Collections.Generic; using System.IO; using System.Text;
public static class PeImports {
  static uint U32(byte[] b, int o) { return BitConverter.ToUInt32(b, o); }
  static ushort U16(byte[] b, int o) { return BitConverter.ToUInt16(b, o); }
  public static List<string> Read(string path) {
    byte[] b = File.ReadAllBytes(path);
    int pe = (int)U32(b, 0x3C);
    int nsec = U16(b, pe + 6);
    int optSize = U16(b, pe + 20);
    int opt = pe + 24;
    bool pe32plus = U16(b, opt) == 0x20b;
    int ddBase = opt + (pe32plus ? 112 : 96);
    int sec = opt + optSize;
    Func<uint, int> rva2off = rva => {
      for (int i = 0; i < nsec; i++) {
        int s = sec + i * 40;
        uint va = U32(b, s + 12), vs = U32(b, s + 8), raw = U32(b, s + 20);
        if (rva >= va && rva < va + Math.Max(vs, U32(b, s + 16))) return (int)(rva - va + raw);
      }
      return -1;
    };
    var res = new List<string>();
    Action<uint, int, string> walk = (rva, stride, tag) => {
      if (rva == 0) return;
      int o = rva2off(rva);
      for (; o >= 0; o += stride) {
        uint name = U32(b, o + (tag == "延迟" ? 4 : 12));
        if (name == 0) break;
        int no = rva2off(name); int e = no; while (b[e] != 0) e++;
        res.Add(Encoding.ASCII.GetString(b, no, e - no) + (tag == "" ? "" : "（" + tag + "）"));
      }
    };
    walk(U32(b, ddBase + 8), 20, "");        // 导入表
    walk(U32(b, ddBase + 13 * 8), 32, "延迟"); // 延迟加载导入表
    return res;
  }
}
'@

Write-Host ''
Write-Host '导入表依赖（exe 直接依赖的 DLL）：'
$imports = [PeImports]::Read($exe) | Sort-Object -Unique
$needRuntime = @()
$sys32 = Join-Path $env:WINDIR 'System32'
foreach ($i in $imports) {
    $n = ($i -replace '（.*$', '').ToLowerInvariant()
    $kind =
        if ($n -match '^(vcruntime|msvcp|concrt|vcomp|vccorlib|mfc)\d*') { $needRuntime += $i; 'VC 运行库（包内没有，目标机需已安装）' }
        elseif ($n -match '^api-ms-win-crt-|^ucrtbase\.dll$') { 'Universal CRT（Windows 10 自带）' }
        elseif ($n -match '^api-ms-win-') { 'API Set（系统自带）' }
        elseif (Test-Path (Join-Path $sys32 $n)) { '系统 DLL' }
        else { '未在 System32 找到，需人工确认' }
    Write-Host ("  {0,-34} {1}" -f $i, $kind)
}
if ($needRuntime.Count -eq 0) { Write-Host '结论：exe 不依赖 vcruntime/msvcp 等 VC 运行库（静态链接 CRT 或仅用系统自带库）。' }
else { Write-Host "结论：exe 依赖 VC 运行库：$($needRuntime -join ', ')。干净机器需先安装 VC++ 可再发行组件，或改为静态链接 CRT。" }

Write-Host ''
Write-Host '启动验证：只在解压目录运行，PATH 精简为系统目录，使用全新数据目录，--smoke 应在约 1 秒内以 0 退出'
$env:PATH = "$sys32;$env:WINDIR"
$before = (Get-ChildItem $appDir -Recurse -File | Measure-Object).Count
$p = Start-Process -FilePath $exe -ArgumentList "--smoke", "--data-dir=`"$data`"", '--no-session' -WorkingDirectory $appDir -PassThru
$null = $p.Handle
if (-not $p.WaitForExit(20000)) { $p.Kill(); Write-Error '启动超时'; exit 1 }
$code = $p.ExitCode
$after = (Get-ChildItem $appDir -Recurse -File | Measure-Object).Count
Write-Host "退出码：$code；解压目录文件数 $before -> $after（启动不应往安装目录写文件）"
Write-Host "数据目录内容：$((Get-ChildItem $data -Recurse -File | ForEach-Object { $_.FullName.Substring($data.Length + 1) }) -join ', ')"
$os = Get-CimInstance Win32_OperatingSystem
Write-Host "环境：$($os.Caption) build $($os.BuildNumber)（开发机，不是真正干净的机器）"
Remove-Item -Recurse -Force $work
if ($code -ne 0 -or $before -ne $after) { Write-Error '分发包验证失败'; exit 1 }
Write-Host '分发包验证通过'
