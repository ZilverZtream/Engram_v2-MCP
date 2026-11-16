# Focused archive bootstrap checks; actual VB runtime proof uses a retained full ZIP.
[CmdletBinding()]
param([Parameter(Mandatory=$true)][string]$ScratchRoot)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ScratchRoot = (Resolve-Path -LiteralPath $ScratchRoot).Path.TrimEnd('\')
$owned = Join-Path $ScratchRoot ('b' + [Guid]::NewGuid().ToString('N').Substring(0,12))
$bootstrap = Join-Path $PSScriptRoot 'install_windows_release.ps1'
function Assert($Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function ExpectFailure($Arguments, [string]$Reason) {
    $failure = $null
    try { & $bootstrap @Arguments | Out-Null } catch { $failure = $_ }
    Assert ($null -ne $failure -and $failure.ToString().Contains($Reason)) "Expected refusal: $Reason"
}
try {
    [void][IO.Directory]::CreateDirectory($owned)
    $runtime = Join-Path $owned 'source\runtime'
    [void][IO.Directory]::CreateDirectory((Join-Path $runtime 'sidecar'))
    $project = Join-Path $owned 'project'
    [void][IO.Directory]::CreateDirectory($project)
    $files = @()
    foreach ($name in @('engram_server.exe','sidecar/vb_roslyn_sidecar-win-x64.exe','sidecar/vb_roslyn_sidecar-win-x64.dll','sidecar/vb_roslyn_sidecar-win-x64.runtimeconfig.json','sidecar/vb_roslyn_sidecar-win-x64.deps.json','sidecar/Microsoft.CodeAnalysis.VisualBasic.dll','sidecar/hostfxr.dll','sidecar/coreclr.dll')) {
        $path = Join-Path $runtime $name
        [IO.File]::WriteAllText($path, "Owned fake asset: $name")
        $files += @{path=$name; sha256=(Get-FileHash -LiteralPath $path).Hash}
    }
    @{runtime='win-x64';sidecar_only=$false;files=$files} | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $runtime 'windows-release-manifest.json') -Encoding UTF8
    # Ignored archive scripts must never be selected as the installer.
    [IO.File]::WriteAllText((Join-Path $owned 'source\install.ps1'), "throw 'Untrusted ZIP installer executed'")
    $script:downloadFixtureArchive = Join-Path $owned 'release.zip'
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $source = Join-Path $owned 'source'
    $zip = [IO.Compression.ZipFile]::Open($script:downloadFixtureArchive, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($file in Get-ChildItem -LiteralPath $source -Recurse -File) {
            $relative = $file.FullName.Substring($source.Length + 1).Replace('\','/')
            [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $file.FullName, $relative)
        }
    } finally { $zip.Dispose() }
    $hash = (Get-FileHash -LiteralPath $script:downloadFixtureArchive).Hash
    $clientPath = Join-Path $owned 'client.json'
    $original = '{"mcpServers":{"existing":{"command":"keep.exe"}},"preferences":{"theme":"dark"}}'
    [IO.File]::WriteAllText($clientPath, $original)
    $arguments = @{PackageArchive=$script:downloadFixtureArchive;PackageSha256=$hash;InstallRoot=(Join-Path $owned 'local-install');ProjectRoot=$project;McpConfigPath=$clientPath}
    & $bootstrap @arguments | Out-Null
    $client = Get-Content -LiteralPath $clientPath -Raw | ConvertFrom-Json
    Assert ($client.mcpServers.existing.command -eq 'keep.exe' -and $client.preferences.theme -eq 'dark') 'Unrelated client settings changed.'
    Assert ((Get-Content -LiteralPath (Join-Path $arguments.InstallRoot 'mcp-client.before.json') -Raw) -ceq $original) 'Client backup differs.'
    Assert ((Get-Content -LiteralPath (Join-Path $arguments.InstallRoot 'release-source.json') -Raw | ConvertFrom-Json).package_sha256 -eq $hash) 'Installed archive identity missing.'
    foreach ($asset in $files) { Assert ((Get-FileHash -LiteralPath (Join-Path $arguments.InstallRoot ('bin/' + $asset.path))).Hash -eq $asset.sha256) 'Installed asset differs.' }
    $clientHash = (Get-FileHash -LiteralPath $clientPath).Hash
    $arguments.InstallRoot = Join-Path $owned 'bad-hash'
    $arguments.PackageSha256 = '0' * 64
    ExpectFailure $arguments 'SHA-256'
    Assert (-not (Test-Path -LiteralPath $arguments.InstallRoot)) 'Bad ZIP hash created an installation.'
    Assert ((Get-FileHash -LiteralPath $clientPath).Hash -eq $clientHash) 'Bad ZIP hash changed client settings.'
    $arguments.PackageSha256 = $hash
    # Test the HTTPS branch with a deterministic owned transfer. No TLS/network
    # availability or external publisher is certified by this injected transport.
    $fixtureArchive = $script:downloadFixtureArchive
    ${function:Invoke-WebRequest} = {
        param([uri]$Uri, [string]$OutFile, [switch]$UseBasicParsing)
        if ($Uri.AbsoluteUri -ne 'https://releases.example.invalid/Engram.zip') { throw 'Wrong download URL.' }
        Copy-Item -LiteralPath $fixtureArchive -Destination $OutFile
    }.GetNewClosure()
    [void]$arguments.Remove('PackageArchive')
    $arguments.PackageUri = 'https://releases.example.invalid/Engram.zip'
    $arguments.InstallRoot = Join-Path $owned 'download-install'
    $arguments.McpConfigPath = Join-Path $owned 'download-client.json'
    & $bootstrap @arguments | Out-Null
    Assert ((Get-Content -LiteralPath (Join-Path $arguments.InstallRoot 'release-source.json') -Raw | ConvertFrom-Json).source_kind -eq 'https') 'HTTPS archive branch did not complete.'
    $arguments.PackageUri = 'http://releases.example.invalid/Engram.zip'
    $arguments.InstallRoot = Join-Path $owned 'insecure-url'
    ExpectFailure $arguments 'HTTPS'
    Assert (@(Get-ChildItem -LiteralPath $owned -Force | Where-Object Name -Like '.engram-*').Count -eq 0) 'Owned download/extraction staging remains.'
    Write-Output 'PASS: pinned local ZIP, injected HTTPS transfer, trusted installer selection, full manifest copy, client preservation, wrong-hash refusal and staging cleanup.'
} finally {
    $resolved = [IO.Path]::GetFullPath($owned)
    if ((Split-Path $resolved -Parent) -ne $ScratchRoot -or (Split-Path $resolved -Leaf) -notmatch '^b[a-f0-9]{12}$') { throw 'Refusing unexpected fixture cleanup path.' }
    if (Test-Path -LiteralPath $resolved) { Remove-Item -LiteralPath $resolved -Recurse -Force }
}
