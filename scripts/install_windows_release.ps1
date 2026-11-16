# Install a checksum-pinned Windows bundle without manually extracting it.
# Uses the adjacent trusted installer; executable scripts inside the ZIP are ignored.
[CmdletBinding(DefaultParameterSetName='Json')]
param(
    [uri]$PackageUri,
    [string]$PackageArchive,
    [Parameter(Mandatory=$true)][ValidatePattern('^[a-fA-F0-9]{64}$')][string]$PackageSha256,
    [Parameter(Mandatory=$true)][string]$InstallRoot,
    [Parameter(Mandatory=$true)][string]$ProjectRoot,
    [Parameter(Mandatory=$true,ParameterSetName='Json')][string]$McpConfigPath,
    [Parameter(Mandatory=$true,ParameterSetName='Codex')][switch]$Codex,
    [Parameter(ParameterSetName='Codex')][string]$CodexExecutable = 'codex',
    [ValidatePattern('^[A-Za-z0-9_-]+$')][string]$ServerName = 'engram'
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'This installer requires Windows.' }
if ([bool]$PackageUri -eq (-not [string]::IsNullOrWhiteSpace($PackageArchive))) {
    throw 'Supply exactly one PackageUri or PackageArchive.'
}
if ($PackageUri -and (-not $PackageUri.IsAbsoluteUri -or $PackageUri.Scheme -cne 'https' -or $PackageUri.UserInfo)) {
    throw 'PackageUri must be an absolute HTTPS URL without embedded credentials.'
}
$InstallRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallRoot)
$ProjectRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($ProjectRoot)
if (Test-Path -LiteralPath $InstallRoot) { throw 'InstallRoot already exists; use the managed upgrade procedure for an existing installation.' }
if (-not (Test-Path -LiteralPath $ProjectRoot -PathType Container)) { throw 'ProjectRoot must be an existing project directory.' }
$parent = Split-Path $InstallRoot -Parent
if (-not (Test-Path -LiteralPath $parent -PathType Container)) { throw 'The parent directory of InstallRoot must exist.' }
$installer = Join-Path $PSScriptRoot 'install_windows.ps1'
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) { $installer = Join-Path $PSScriptRoot 'install.ps1' }
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) { throw 'The adjacent trusted install_windows.ps1 is required.' }
if ($PackageArchive) { $PackageArchive = (Resolve-Path -LiteralPath $PackageArchive).Path }
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$owned = Join-Path $parent ('.engram-download-' + [Guid]::NewGuid().ToString('N'))
$archive = $null
try {
    [void][IO.Directory]::CreateDirectory($owned)
    if ($PackageUri) {
        $PackageArchive = Join-Path $owned 'release.zip'
        Invoke-WebRequest -Uri $PackageUri -OutFile $PackageArchive -UseBasicParsing | Out-Null
    }
    $actualHash = (Get-FileHash -LiteralPath $PackageArchive -Algorithm SHA256).Hash
    if ($actualHash -ine $PackageSha256) { throw 'Release ZIP SHA-256 does not match the trusted checksum; nothing was installed or registered.' }
    $runtime = Join-Path $owned 'runtime'
    $prefix = [IO.Path]::GetFullPath($runtime).TrimEnd('\') + '\'
    $archive = [IO.Compression.ZipFile]::OpenRead($PackageArchive)
    foreach ($entry in $archive.Entries) {
        if (-not $entry.FullName.StartsWith('runtime/', [StringComparison]::Ordinal)) { continue }
        $relative = $entry.FullName.Substring('runtime/'.Length)
        if (-not $relative -or $entry.FullName.EndsWith('/')) { continue }
        $destination = [IO.Path]::GetFullPath((Join-Path $runtime $relative.Replace('/', '\')))
        if (-not $destination.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Release ZIP contains an invalid runtime path.' }
        [void][IO.Directory]::CreateDirectory((Split-Path $destination -Parent))
        [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $destination, $false)
    }
    $archive.Dispose(); $archive = $null
    $arguments = @{
        PackageRoot=$runtime; InstallRoot=$InstallRoot; ProjectRoot=$ProjectRoot; ServerName=$ServerName
    }
    if ($Codex) { $arguments.Codex=$true; $arguments.CodexExecutable=$CodexExecutable }
    else { $arguments.McpConfigPath=$McpConfigPath }
    & $installer @arguments
    # Only add provenance after the existing installer has accepted and published
    # every manifest-listed runtime asset and registered the requested connection.
    $receipt = [ordered]@{
        package_sha256=$actualHash
        source_kind=$(if ($PackageUri) { 'https' } else { 'local_archive' })
        installation_receipt='installation.json'
    }
    $utf8 = New-Object Text.UTF8Encoding($false)
    [IO.File]::WriteAllText((Join-Path $InstallRoot 'release-source.json'), ($receipt | ConvertTo-Json), $utf8)
} finally {
    if ($null -ne $archive) { $archive.Dispose() }
    $resolved = [IO.Path]::GetFullPath($owned)
    $expectedParent = [IO.Path]::GetFullPath($parent).TrimEnd('\')
    if ((Split-Path $resolved -Parent) -ne $expectedParent -or (Split-Path $resolved -Leaf) -notmatch '^\.engram-download-[a-f0-9]{32}$') {
        throw 'Refusing unexpected download fixture cleanup path.'
    }
    if (Test-Path -LiteralPath $resolved) { Remove-Item -LiteralPath $resolved -Recurse -Force }
}
