# Packs dist\ (from build.ps1) into dist\EchoBridge-Setup.exe with Inno Setup 6 or later.
param([string]$Compiler = 'ISCC.exe')
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not (Test-Path -LiteralPath (Join-Path $root 'dist\EchoBridge.exe'))) {
    throw 'Run scripts\build.ps1 before building the installer.'
}
$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
& $Compiler "/DAppVersion=$version" (Join-Path $root 'installer\EchoBridge.iss')
if ($LASTEXITCODE -ne 0) { throw 'Installer compilation failed.' }
