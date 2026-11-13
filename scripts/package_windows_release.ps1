# Assemble an already built, trusted full runtime into an offline install bundle.
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$PackageRoot,
    [Parameter(Mandatory=$true)][string]$OutputPath
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$PackageRoot = (Resolve-Path -LiteralPath $PackageRoot).Path.TrimEnd('\')
$OutputPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputPath)
if ([IO.Path]::GetExtension($OutputPath) -ine '.zip') { throw 'OutputPath must end in .zip.' }
if (Test-Path -LiteralPath $OutputPath) { throw 'OutputPath already exists; preserve the existing bundle.' }
if (-not (Test-Path -LiteralPath (Split-Path $OutputPath -Parent) -PathType Container)) { throw 'Output parent must exist.' }
if (Test-Path -LiteralPath (Join-Path $PackageRoot 'windows-release-pending.json')) { throw 'Package build is incomplete.' }
$manifestPath = Join-Path $PackageRoot 'windows-release-manifest.json'
$manifestHash = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ($manifest.runtime -cne 'win-x64' -or $manifest.sidecar_only -isnot [bool] -or $manifest.sidecar_only) { throw 'A complete win-x64 runtime is required.' }
$inputs = [ordered]@{}
$expected = @{}
foreach ($asset in $manifest.files) {
    $relative = [string]$asset.path
    if ($relative -ne 'engram_server.exe' -and -not $relative.StartsWith('sidecar/')) { throw "Unexpected runtime asset: $relative" }
    foreach ($part in $relative.Split('/')) {
        if (-not $part -or $part -in @('.', '..') -or $part.IndexOfAny([IO.Path]::GetInvalidFileNameChars()) -ge 0 -or $part -ne $part.TrimEnd(' ', '.')) { throw "Invalid runtime asset: $relative" }
    }
    $entry = 'runtime/' + $relative
    if ($inputs.Contains($entry) -or [string]$asset.sha256 -notmatch '^[a-fA-F0-9]{64}$') { throw 'Invalid or duplicate manifest asset.' }
    $path = Join-Path $PackageRoot $relative.Replace('/', '\')
    if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ne $asset.sha256) { throw "Runtime checksum mismatch: $relative" }
    $inputs[$entry] = $path
    $expected[$entry] = [string]$asset.sha256
}
foreach ($name in @('engram_server.exe','sidecar/vb_roslyn_sidecar-win-x64.exe','sidecar/vb_roslyn_sidecar-win-x64.dll','sidecar/vb_roslyn_sidecar-win-x64.runtimeconfig.json','sidecar/vb_roslyn_sidecar-win-x64.deps.json','sidecar/Microsoft.CodeAnalysis.VisualBasic.dll','sidecar/hostfxr.dll','sidecar/coreclr.dll')) {
    if (-not $inputs.Contains('runtime/' + $name)) { throw "Required runtime asset missing: $name" }
}
$inputs['runtime/windows-release-manifest.json'] = $manifestPath
$expected['runtime/windows-release-manifest.json'] = $manifestHash
$inputs['install.ps1'] = Join-Path $PSScriptRoot 'install_windows.ps1'
$inputs['README.md'] = Join-Path (Split-Path $PSScriptRoot -Parent) 'docs/WINDOWS_BUNDLE.md'
foreach ($entry in @('install.ps1','README.md')) { $expected[$entry] = (Get-FileHash -LiteralPath $inputs[$entry] -Algorithm SHA256).Hash }
$partial = $OutputPath + '.partial-' + [Guid]::NewGuid().ToString('N')
$stream = $null
$archive = $null
try {
    $stream = [IO.File]::Open($partial, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $archive = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Create, $true)
    foreach ($entry in $inputs.Keys) {
        [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $inputs[$entry], $entry, [IO.Compression.CompressionLevel]::Optimal)
    }
    $archive.Dispose(); $archive = $null
    $stream.Position = 0
    $archive = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Read, $true)
    foreach ($entry in $archive.Entries) {
        $inputStream = $entry.Open()
        $hasher = [Security.Cryptography.SHA256]::Create()
        try { $hash = ([BitConverter]::ToString($hasher.ComputeHash($inputStream))).Replace('-', '') }
        finally { $hasher.Dispose(); $inputStream.Dispose() }
        if ($hash -ne $expected[$entry.FullName]) { throw "Archived asset differs: $($entry.FullName)" }
    }
    $archive.Dispose(); $archive = $null
    $stream.Dispose(); $stream = $null
    [IO.File]::Move($partial, $OutputPath)
    [pscustomobject]@{ archive=$OutputPath; sha256=(Get-FileHash -LiteralPath $OutputPath -Algorithm SHA256).Hash; entries=$inputs.Count; runtime_assets=$manifest.files.Count; runtime_manifest_sha256=$manifestHash }
} finally {
    if ($null -ne $archive) { $archive.Dispose() }
    if ($null -ne $stream) { $stream.Dispose() }
    # Only this invocation's unpublished archive, never an existing bundle.
    if (Test-Path -LiteralPath $partial) { Remove-Item -LiteralPath $partial -Force }
}
