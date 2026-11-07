# 观测框架定向验证：harness 特性下的 clippy、交互脚本（含重复一致性）、截图对比基线、
# 动画逐帧采集两次并核对产物逐字节一致。编译并行度上限 6。
# 截图对比退出码 2（环境与基线不同，无法判断）只给警告，不算失败；1（回归）和 3（不稳定）算失败。
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

function Step($name, [scriptblock]$cmd, [int[]]$ok = @(0)) {
    Write-Host "== $name"
    & $cmd
    if ($ok -notcontains $LASTEXITCODE) { Write-Error "$name 失败（退出码 $LASTEXITCODE）"; exit $LASTEXITCODE }
    if ($LASTEXITCODE -ne 0) { Write-Warning "$name 退出码 $LASTEXITCODE（环境与基线不同，需在本机 bless 后才能比较）" }
}

$h = Join-Path $PSScriptRoot 'harness.ps1'
Step 'cargo clippy --features harness' { cargo clippy --locked --jobs 6 --features harness --all-targets -- -D warnings }
Step '交互脚本' { & $h run }
Step '磁盘／收藏／目录变化同步' { & $h live }
Step '右键菜单真实窗口自测（需交互式桌面）' { & (Join-Path $PSScriptRoot 'verify-menu.ps1') }
Step '搜索（目录／全盘、取消、过期结果、增强后端故障回退）' { & $h search }
Step '截图对比基线' { & $h compare } @(0, 2)
Step '动画采集（第 1 次）' { & $h frames --out artifacts/verify/a }
Step '动画采集（第 2 次）' { & $h frames --out artifacts/verify/b }

Write-Host '== 两次采集产物逐字节一致'
foreach ($f in 'curve.json', 'curve.csv', 'contact-sheet.png', 'anim.gif', 'frame_0010_t0160ms.png') {
    $a = (Get-FileHash "artifacts/verify/a/$f").Hash
    $b = (Get-FileHash "artifacts/verify/b/$f").Hash
    if ($a -ne $b) { Write-Error "$f 两次采集不一致"; exit 1 }
}
Write-Host '观测框架验证通过'
