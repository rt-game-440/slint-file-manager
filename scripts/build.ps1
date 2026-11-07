# 构建 release 产物。编译并行度上限 6（本机 12 核的一半）。
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
cargo build --release --locked --jobs 6
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
