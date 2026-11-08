# 定向验证：格式、clippy、浏览核心单元测试、构建、--smoke 启动退出、会话恢复、窗口可见性（发布前必过）。编译并行度上限 6。
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

function Step($name, [scriptblock]$cmd) {
    Write-Host "== $name"
    & $cmd
    if ($LASTEXITCODE -ne 0) { Write-Error "$name 失败（退出码 $LASTEXITCODE）"; exit $LASTEXITCODE }
}

Step 'cargo fmt --check' { cargo fmt --check }
Step 'cargo clippy' { cargo clippy --locked --jobs 6 -- -D warnings }
Step 'cargo test --lib' { cargo test --lib --locked --jobs 6 }
Step 'cargo build --release' { cargo build --release --locked --jobs 6 }

Write-Host '== 冒烟：启动并自动退出（15 秒超时）'
$exe = Join-Path (Get-Location) 'target\release\slint-file-manager.exe'
$p = Start-Process -FilePath $exe -ArgumentList '--smoke' -PassThru -NoNewWindow
$null = $p.Handle  # 缓存句柄，否则 WaitForExit(ms) 后 ExitCode 可能为空
if (-not $p.WaitForExit(15000)) { $p.Kill(); Write-Error '冒烟超时，进程未退出'; exit 1 }
if ($p.ExitCode -ne 0) { Write-Error "冒烟退出码 $($p.ExitCode)"; exit 1 }
$os = (Get-CimInstance Win32_OperatingSystem)
Write-Host "冒烟通过：$($os.Caption) build $($os.BuildNumber)"

Step '会话恢复（真实程序，临时数据目录）' { & (Join-Path $PSScriptRoot 'verify-session.ps1') }
# Issue #17：单实例转发、标签复用、不可用路径答复、主实例无响应时超时独立启动（临时数据目录，注册表只读测试根）
Step '单实例与外部打开请求（真实程序，临时数据目录）' { & (Join-Path $PSScriptRoot 'verify-instance.ps1') }
# Issue #18：“在文件夹中显示”路由（测试根接管 + reveal-probe 调用 SHOpenFolderAndSelectItems；停用后回退资源管理器）。
# 回退用例会短暂打开并关闭临时文件夹的资源管理器窗口；需要已解锁的交互桌面。
Step '在文件夹中显示（真实程序，测试注册表根）' { & (Join-Path $PSScriptRoot 'verify-reveal.ps1') }
# Issue #39：默认启动方式下窗口必须在桌面合成里可见（屏幕抓取、PrintWindow、WGC 窗口／显示器都取到主题色）。
# 只在内存里比对颜色，不保存截图；需要已解锁的交互桌面。
Step '窗口可见性（默认启动方式，发布前必过）' { & (Join-Path $PSScriptRoot 'verify-capture.ps1') }
# Issue #45：无边框窗口最大化只占工作区（两种渲染；真实鼠标点击标题栏，需要已解锁的交互桌面）。
Step '窗口最大化只占工作区（默认与软件渲染）' { & (Join-Path $PSScriptRoot 'verify-maximize.ps1') }
