# 定向验证：格式、clippy、构建、--smoke 启动退出。编译并行度上限 6。
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

function Step($name, [scriptblock]$cmd) {
    Write-Host "== $name"
    & $cmd
    if ($LASTEXITCODE -ne 0) { Write-Error "$name 失败（退出码 $LASTEXITCODE）"; exit $LASTEXITCODE }
}

Step 'cargo fmt --check' { cargo fmt --check }
Step 'cargo clippy' { cargo clippy --locked --jobs 6 -- -D warnings }
Step 'cargo build --release' { cargo build --release --locked --jobs 6 }

Write-Host '== 冒烟：启动并自动退出（15 秒超时）'
$exe = Join-Path (Get-Location) 'target\release\slint-file-manager.exe'
$p = Start-Process -FilePath $exe -ArgumentList '--smoke' -PassThru -NoNewWindow
$null = $p.Handle  # 缓存句柄，否则 WaitForExit(ms) 后 ExitCode 可能为空
if (-not $p.WaitForExit(15000)) { $p.Kill(); Write-Error '冒烟超时，进程未退出'; exit 1 }
if ($p.ExitCode -ne 0) { Write-Error "冒烟退出码 $($p.ExitCode)"; exit 1 }
$os = (Get-CimInstance Win32_OperatingSystem)
Write-Host "冒烟通过：$($os.Caption) build $($os.BuildNumber)"
