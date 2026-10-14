# 打包 Windows 完整分发包：ds-pet.exe + assets → zip
# 用法：pwsh scripts/package-windows.ps1 <target-triple> <version>
param(
    [Parameter(Mandatory = $true)][string]$Target,
    [Parameter(Mandatory = $true)][string]$Version
)
$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
$Arch = if ($Target -like "x86_64*") { "x64" } else { "arm64" }
$Out = Join-Path $Root "ds-pet-$Version-windows-$Arch.zip"

$Package = Join-Path $Root "package"
if (Test-Path $Package) { Remove-Item -Recurse -Force $Package }
New-Item -ItemType Directory -Force $Package | Out-Null
Copy-Item (Join-Path $Root "target/$Target/release/ds-pet.exe") $Package
Copy-Item -Recurse -Force (Join-Path $Root "assets") (Join-Path $Package "assets")
Compress-Archive -Path "$Package/*" -DestinationPath $Out -Force
Remove-Item -Recurse -Force $Package
Write-Output "产物：$Out"
