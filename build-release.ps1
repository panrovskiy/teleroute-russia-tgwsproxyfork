$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
Write-Host '[TeleRoute] Building release...' -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'Cargo build failed.' }
Write-Host ''
Write-Host '[OK] Build completed.' -ForegroundColor Green
Write-Host (Join-Path $PWD 'target\release\tele-route.exe')
Write-Host (Join-Path $PWD 'target\release\call-relay.exe')
