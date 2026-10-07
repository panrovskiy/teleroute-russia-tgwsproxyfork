$ErrorActionPreference = 'Stop'

cargo build --release

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$release = Join-Path $root 'release'
Remove-Item $release -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $release, (Join-Path $release 'config'), (Join-Path $release 'wintun') | Out-Null
Copy-Item (Join-Path $root 'target\release\tele-route.exe') (Join-Path $release 'TeleRoute.exe')
Copy-Item (Join-Path $root 'config.example.toml') (Join-Path $release 'config\config.example.toml')
Copy-Item (Join-Path $root 'README.md') $release
Copy-Item (Join-Path $root 'BUILD.md') $release
Copy-Item (Join-Path $root 'DEVELOPMENT.md') $release
Copy-Item (Join-Path $root 'LICENSE') $release

$dll = Join-Path $root 'wintun\wintun.dll'
if (!(Test-Path $dll)) { throw 'Place the official signed wintun.dll in .\wintun before packaging.' }
Copy-Item $dll (Join-Path $release 'wintun\wintun.dll')

Write-Host "Release created at $release"
