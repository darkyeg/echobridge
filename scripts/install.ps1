# Installs EchoBridge for the current Windows user, without administrator rights:
#
#   irm https://github.com/darkyeg/echobridge/releases/latest/download/install.ps1 | iex
#   .\install.ps1 -Uninstall
#
# It downloads EchoBridge.exe from the release, checks it against the release's checksum
# list, places it in %LOCALAPPDATA%\Programs\EchoBridge and adds a Start menu shortcut.
# Use it where running EchoBridge-Setup.exe is not possible, such as a managed account or a
# scripted setup. VB-CABLE is a separate driver that needs administrator approval once.
#Requires -Version 5.1
[CmdletBinding()]
param(
    [switch] $Uninstall,
    # A release number such as 1.0.0; the latest release by default.
    [string] $Version = 'latest',
    # Install this EchoBridge.exe instead of downloading one (it is not checked).
    [string] $From,
    [string] $Repository = $(if ($env:ECHOBRIDGE_REPO) { $env:ECHOBRIDGE_REPO } else { 'darkyeg/echobridge' })
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$folder = Join-Path $env:LOCALAPPDATA 'Programs\EchoBridge'
$exe = Join-Path $folder 'EchoBridge.exe'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'EchoBridge.lnk'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

if ($Uninstall) {
    Get-Process -Name EchoBridge -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $exe } | Stop-Process -Force
    Remove-Item -LiteralPath $shortcut -Force -ErrorAction SilentlyContinue
    Remove-ItemProperty -Path $runKey -Name EchoBridge -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $folder -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "EchoBridge removed. Your settings stay in $env:LOCALAPPDATA\EchoBridge."
    return
}

if (-not [Environment]::Is64BitOperatingSystem) {
    throw 'EchoBridge is built for 64-bit Windows.'
}

$work = Join-Path ([IO.Path]::GetTempPath()) ('echobridge-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
try {
    if ($From) {
        $download = (Resolve-Path -LiteralPath $From).Path
    } else {
        $base = if ($Version -eq 'latest') {
            "https://github.com/$Repository/releases/latest/download"
        } else {
            "https://github.com/$Repository/releases/download/v$($Version.TrimStart('v'))"
        }
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        Write-Host 'Downloading EchoBridge...'
        $sums = Join-Path $work 'SHA256SUMS-windows.txt'
        $download = Join-Path $work 'EchoBridge.exe'
        Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS-windows.txt" -OutFile $sums
        Invoke-WebRequest -UseBasicParsing -Uri "$base/EchoBridge.exe" -OutFile $download
        $line = Get-Content -LiteralPath $sums | Where-Object { $_ -match '^([0-9a-fA-F]{64})\s+\*?EchoBridge\.exe\s*$' } | Select-Object -First 1
        if (-not $line) { throw 'The release has no checksum for EchoBridge.exe; not installing.' }
        $expected = ($line -split '\s+')[0]
        $actual = (Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash
        if ($expected -ne $actual) { throw 'The download does not match its checksum; not installing.' }
    }

    # An update replaces the running program, so close it first.
    Get-Process -Name EchoBridge -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $exe } | Stop-Process -Force
    New-Item -ItemType Directory -Force -Path $folder | Out-Null
    Copy-Item -LiteralPath $download -Destination $exe -Force

    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut($shortcut)
    $link.TargetPath = $exe
    $link.WorkingDirectory = $folder
    $link.Description = 'Removes headphone sound that leaks into your microphone'
    $link.Save()
    Write-Host "Installed EchoBridge in $folder and added it to the Start menu."
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}

# VB-CABLE carries the clean microphone into call apps.
$cable = Get-CimInstance Win32_SoundDevice -ErrorAction SilentlyContinue | Where-Object { $_.Name -like '*VB-Audio*' -or $_.Name -like 'CABLE*' }
if (-not $cable) {
    Write-Host 'Next: install VB-CABLE (free) from https://vb-audio.com/Cable/ - it needs administrator approval once.'
}
Write-Host 'Open EchoBridge from the Start menu. Windows may warn that it is unrecognized (not code-signed yet): choose More info, then Run anyway.'
