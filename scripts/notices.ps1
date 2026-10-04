# Writes the license notices for everything linked into the Rust EchoBridge program
# (for the Windows build unless -Target names another):
# every crate reached through normal dependencies, plus the vendored WebRTC sources.
# Needs PowerShell 7: cargo metadata has keys that differ only in case.
#Requires -Version 7
param([Parameter(Mandatory)] [string] $Output, [string] $Target = 'x86_64-pc-windows-msvc')
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
$cargo = if ($cargo) { $cargo.Source } else { Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }

$metadata = & $cargo metadata --format-version 1 --locked --filter-platform $Target --manifest-path (Join-Path $root 'Cargo.toml') |
    ConvertFrom-Json -AsHashtable -Depth 64
if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed.' }
$packages = @{}
foreach ($package in $metadata.packages) { $packages[$package.id] = $package }
$nodes = @{}
foreach ($node in $metadata.resolve.nodes) { $nodes[$node.id] = $node }

# Walk normal (linked) dependencies from the app; build scripts and tests are not shipped.
$app = ($metadata.packages | Where-Object { $_.name -eq 'echobridge' }).id
$linked = [System.Collections.Generic.HashSet[string]]::new()
$queue = [System.Collections.Generic.Queue[string]]::new()
$queue.Enqueue($app)
while ($queue.Count -gt 0) {
    $id = $queue.Dequeue()
    if (-not $linked.Add($id)) { continue }
    foreach ($dependency in $nodes[$id].deps) {
        if ($dependency.dep_kinds | Where-Object { $null -eq $_.kind }) { $queue.Enqueue($dependency.pkg) }
    }
}

function Get-NoticeFiles([string] $folder) {
    Get-ChildItem -LiteralPath $folder -File |
        Where-Object { $_.Name -match '^(licen[cs]e|copying|copyright|notice)' } |
        Sort-Object Name
}

$text = [System.Text.StringBuilder]::new()
[void] $text.AppendLine('EchoBridge third-party notices')
foreach ($package in ($linked | ForEach-Object { $packages[$_] } | Sort-Object { $_.name }, { $_.version })) {
    if ($null -eq $package.source) { continue } # EchoBridge's own crates
    [void] $text.AppendLine("`n===== $($package.name) $($package.version) =====")
    [void] $text.AppendLine("License: $(if ($package.license) { $package.license } else { 'see files' })")
    if ($package.repository) { [void] $text.AppendLine("Home-page: $($package.repository)") }
    $folder = Split-Path -Parent $package.manifest_path
    # Crates from a git workspace (DeepFilterNet) keep their licenses at the repository root.
    $files = @(Get-NoticeFiles $folder)
    if ($files.Count -eq 0) { $files = @(Get-NoticeFiles (Split-Path -Parent $folder)) }
    foreach ($file in $files) {
        [void] $text.AppendLine("`n$($file.Name)`n$(Get-Content -Raw -LiteralPath $file.FullName)")
    }
}

# WebRTC audio processing, compiled from the pywebrtc-audio sources by crates/aec3.
$webrtc = Get-ChildItem -Directory -Path (Join-Path $root 'target\release\build\echobridge-aec3-*\out\pywebrtc-audio-*') |
    Select-Object -First 1
if (-not $webrtc) { throw 'Build the app first: the WebRTC sources were not found.' }
[void] $text.AppendLine("`n===== WebRTC audio processing (pywebrtc-audio $($webrtc.Name.Split('-')[-1])) =====")
foreach ($file in Get-NoticeFiles $webrtc.FullName) {
    [void] $text.AppendLine("`n$($file.Name)`n$(Get-Content -Raw -LiteralPath $file.FullName)")
}

[System.IO.File]::WriteAllText($Output, $text.ToString().Replace("`r`n", "`n"))
Write-Host "Notices for $($linked.Count - 1) linked packages written to $Output"
