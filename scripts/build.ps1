# Builds EchoBridge into dist\: one self-contained EchoBridge.exe with its notices.
#Requires -Version 7
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    $cargo = if ($cargo) { $cargo.Source } else { Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }
    & $cargo build --release --locked -p echobridge
    if ($LASTEXITCODE -ne 0) { throw 'Build failed.' }
    $dist = Join-Path $root 'dist'
    New-Item -ItemType Directory -Force $dist | Out-Null
    Copy-Item -LiteralPath (Join-Path $root 'target\release\EchoBridge.exe') -Destination $dist
    Copy-Item -LiteralPath (Join-Path $root 'START-HERE.txt') -Destination $dist
    Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination $dist
    & (Join-Path $PSScriptRoot 'notices.ps1') -Output (Join-Path $dist 'THIRD-PARTY-NOTICES.txt')
    $size = (Get-Item (Join-Path $dist 'EchoBridge.exe')).Length / 1MB
    Write-Host ('EchoBridge.exe: {0:N1} MB' -f $size)
} finally {
    Pop-Location
}
