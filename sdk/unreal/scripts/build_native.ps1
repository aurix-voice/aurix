<#
.SYNOPSIS
  Build the aurix-client native library (Win64) and stage it with the headers into the Unreal
  plugin's ThirdParty module.

.EXAMPLE
  .\sdk\unreal\scripts\build_native.ps1
  .\sdk\unreal\scripts\build_native.ps1 -PluginDir C:\MyGame\Plugins\AurixVoice
#>
param(
    [string]$PluginDir = "",
    [string]$Target = "x86_64-pc-windows-msvc",
    [switch]$Debug
)

$ErrorActionPreference = "Stop"
$Root = Resolve-Path (Join-Path $PSScriptRoot "..\..\..")
if ($PluginDir -eq "") { $PluginDir = Join-Path $Root "sdk\unreal\AurixVoice" }
$ThirdParty = Join-Path $PluginDir "Source\ThirdParty\AurixClientLibrary"
$Profile = if ($Debug) { "debug" } else { "release" }

if ($Target -notlike "*-pc-windows-msvc") {
    throw "build_native.ps1 targets Windows MSVC only; use build_native.sh for Linux/macOS"
}

$CargoArgs = @("build", "-p", "aurix-client", "--lib", "--target", $Target)
if (-not $Debug) { $CargoArgs += "--release" }

Write-Host "building aurix-client for $Target ($Profile)"
Push-Location $Root
try {
    # libopus 1.6 (DRED/OSCE) is built from the sources bundled with opusic-sys (needs cmake) and
    # linked statically into the DLL.
    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
} finally {
    Pop-Location
}

$Out = Join-Path $Root "target\$Target\$Profile"
$Dest = Join-Path $ThirdParty "lib\Win64"
New-Item -ItemType Directory -Force -Path $Dest | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $ThirdParty "include") | Out-Null

foreach ($f in @("aurix_client.dll", "aurix_client.dll.lib")) {
    $Src = Join-Path $Out $f
    if (-not (Test-Path $Src)) { throw "expected artifact missing: $Src" }
    Copy-Item -Force $Src (Join-Path $Dest $f)
    Write-Host "  $(Join-Path $Dest $f)"
}

foreach ($h in @("aurix_client.h", "aurix_client.hpp")) {
    Copy-Item -Force (Join-Path $Root "crates\aurix-client\include\$h") (Join-Path $ThirdParty "include\$h")
    Write-Host "  $(Join-Path $ThirdParty "include\$h")"
}
Write-Host "done: $ThirdParty"
