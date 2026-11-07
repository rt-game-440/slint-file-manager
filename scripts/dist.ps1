# 生成可分发产物：dist/slint-file-manager-<版本>-win-x64.zip（含 exe、LICENSE）。
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
& (Join-Path $PSScriptRoot 'build.ps1')
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$ver = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$name = "slint-file-manager-$ver-win-x64"
$stage = Join-Path 'dist' $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item 'target\release\slint-file-manager.exe' $stage
Copy-Item 'LICENSE' $stage
$zip = Join-Path 'dist' "$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $stage -DestinationPath $zip
Write-Host "产物：$((Resolve-Path $zip).Path)"
