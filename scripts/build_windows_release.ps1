[CmdletBinding()]
param(
    [string]$TargetDirectory = $env:CARGO_TARGET_DIR,
    [switch]$SidecarOnly
)

$ErrorActionPreference = 'Stop'
function Write-AtomicJson($Value, [string]$Path) {
    $temporary = $Path + '.write-' + [Guid]::NewGuid().ToString('N')
    try {
        ConvertTo-Json -InputObject $Value -Depth 8 | Set-Content -LiteralPath $temporary -Encoding UTF8
        if (Test-Path -LiteralPath $Path) { [IO.File]::Replace($temporary, $Path, [System.Management.Automation.Language.NullString]::Value) }
        else { [IO.File]::Move($temporary, $Path) }
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Force }
    }
}
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'This script builds the Windows x64 release. Run it on Windows.'
}
if (-not $TargetDirectory) {
    $common = & git -C $repoRoot rev-parse --path-format=absolute --git-common-dir
    if ($LASTEXITCODE -ne 0) { throw 'Cannot locate the shared Git/build directory' }
    $TargetDirectory = Join-Path (Split-Path $common.Trim() -Parent) 'target'
}
$TargetDirectory = [IO.Path]::GetFullPath($TargetDirectory)
$sidecarOutput = Join-Path $TargetDirectory 'release\sidecar'
# Forward slashes avoid a trailing backslash escaping a native argument's
# closing quote when the selected target directory contains spaces.
$sidecarBuild = (Join-Path $TargetDirectory 'sidecar-build\win-x64').Replace('\', '/') + '/'
$sidecarIntermediate = (Join-Path $TargetDirectory 'sidecar-obj\win-x64').Replace('\', '/') + '/'
$publishStage = $null
$previousCargoTarget = $env:CARGO_TARGET_DIR
$previousCertificateSetting = $env:DOTNET_GENERATE_ASPNET_CERTIFICATE
$env:CARGO_TARGET_DIR = $TargetDirectory
$env:DOTNET_GENERATE_ASPNET_CERTIFICATE = 'false'
try {
    Push-Location (Join-Path $repoRoot 'tools\vb_roslyn_sidecar')
    try {
        & dotnet --version
        if ($LASTEXITCODE -ne 0) { throw 'Install a stable .NET 9 SDK before building the release' }
    } finally { Pop-Location }
    if (-not $SidecarOnly) {
        & cargo build --manifest-path (Join-Path $repoRoot 'Cargo.toml') -p engram_server --release -j1
        if ($LASTEXITCODE -ne 0) { throw 'Rust release build failed' }
    }

    # Publish into one owned, fresh directory so obsolete SDK/native files cannot
    # silently enter a later package. Teardown runs on success and failure.
    $publishStage = Join-Path $TargetDirectory ('sidecar-publish-' + [Guid]::NewGuid().ToString('N'))
    [void][IO.Directory]::CreateDirectory($publishStage)
    # SDK selection is anchored beside global.json. Every build output stays
    # under the shared target; no project checkout or scratch drive is created.
    Push-Location (Join-Path $repoRoot 'tools\vb_roslyn_sidecar')
    try {
        & dotnet publish vb_roslyn_sidecar.csproj --configuration Release --runtime win-x64 `
            --self-contained true --output $publishStage --nologo `
            -p:AssemblyName=vb_roslyn_sidecar-win-x64 -p:PublishSingleFile=false `
            -p:PublishAot=false -p:UseSharedCompilation=false -p:DebugType=None -p:DebugSymbols=false `
            "-p:BaseOutputPath=$sidecarBuild" "-p:BaseIntermediateOutputPath=$sidecarIntermediate" `
            "-p:MSBuildProjectExtensionsPath=$sidecarIntermediate"
        if ($LASTEXITCODE -ne 0) { throw 'Roslyn sidecar publish failed' }
    } finally {
        Pop-Location
    }

    $sidecarExecutable = Join-Path $publishStage 'vb_roslyn_sidecar-win-x64.exe'
    if (-not (Test-Path -LiteralPath $sidecarExecutable -PathType Leaf)) {
        throw 'Publish did not produce the companion executable expected by Engram'
    }
    $assets = @(Get-ChildItem -LiteralPath $publishStage -File -Recurse | ForEach-Object {
        $relative = $_.FullName.Substring($publishStage.TrimEnd('\').Length + 1).Replace('\', '/')
        [ordered]@{ path = 'sidecar/' + $relative; sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash }
    })
    $manifestPath = Join-Path $TargetDirectory 'release\windows-release-manifest.json'
    [void][IO.Directory]::CreateDirectory((Split-Path $manifestPath -Parent))
    $pendingManifestPath = Join-Path $TargetDirectory 'release\windows-release-pending.json'
    $previousAssets = @{}
    foreach ($inventoryPath in @($manifestPath, $pendingManifestPath)) {
        if (Test-Path -LiteralPath $inventoryPath -PathType Leaf) {
            $previousManifest = Get-Content -LiteralPath $inventoryPath -Raw | ConvertFrom-Json
            foreach ($asset in $previousManifest.files) {
                $key = [string]$asset.path
                $previousAssets[$key] = @($previousAssets[$key]) + @($asset.sha256) + @($asset.acceptable_sha256) |
                    Where-Object { $_ } | Select-Object -Unique
            }
        }
    }
    # A manifest only establishes ownership when the existing bytes match it.
    # Never delete unknown or manually modified files from a reused target.
    $obsolete = @()
    if (Test-Path -LiteralPath $sidecarOutput) {
        foreach ($existing in Get-ChildItem -LiteralPath $sidecarOutput -File -Recurse) {
            $relative = $existing.FullName.Substring($sidecarOutput.TrimEnd('\').Length + 1).Replace('\', '/')
            $key = 'sidecar/' + $relative
            if (-not $previousAssets.ContainsKey($key) -or
                (Get-FileHash -LiteralPath $existing.FullName -Algorithm SHA256).Hash -notin $previousAssets[$key]) {
                throw "Unowned or modified release asset preserved: $($existing.FullName). Reconcile it before packaging."
            }
            if ($key -notin $assets.path) { $obsolete += $existing.FullName }
        }
    }
    # Recovery accepts either the pre-copy or intended bytes. Publish this
    # inventory atomically BEFORE changing files; interrupted builds can retry.
    $pendingAssets = @{}
    foreach ($key in $previousAssets.Keys) { $pendingAssets[$key] = @($previousAssets[$key]) }
    foreach ($asset in $assets) { $pendingAssets[$asset.path] = @($pendingAssets[$asset.path]) + @($asset.sha256) }
    $pendingFiles = @($pendingAssets.Keys | ForEach-Object {
        [ordered]@{ path = $_; acceptable_sha256 = @($pendingAssets[$_] | Where-Object { $_ } | Select-Object -Unique) }
    })
    Write-AtomicJson ([ordered]@{ runtime = 'win-x64'; pending = $true; files = $pendingFiles }) $pendingManifestPath
    foreach ($asset in $assets) {
        $relative = $asset.path.Substring('sidecar/'.Length)
        $destination = Join-Path $sidecarOutput $relative
        [void][IO.Directory]::CreateDirectory((Split-Path $destination -Parent))
        Copy-Item -LiteralPath (Join-Path $publishStage $relative) -Destination $destination -Force
    }
    foreach ($path in $obsolete) { Remove-Item -LiteralPath $path -Force }
    if (-not $SidecarOnly) {
        $serverExecutable = Join-Path $TargetDirectory 'release\engram_server.exe'
        if (-not (Test-Path -LiteralPath $serverExecutable -PathType Leaf)) { throw 'Server executable missing' }
        $assets += [ordered]@{ path = 'engram_server.exe'; sha256 = (Get-FileHash -LiteralPath $serverExecutable -Algorithm SHA256).Hash }
    }
    $runtimeConfig = Get-Content -LiteralPath (Join-Path $sidecarOutput 'vb_roslyn_sidecar-win-x64.runtimeconfig.json') -Raw | ConvertFrom-Json
    $framework = @($runtimeConfig.runtimeOptions.includedFrameworks | Where-Object name -eq 'Microsoft.NETCore.App')[0]
    $manifest = [ordered]@{ runtime = 'win-x64'; framework = $framework.version; sidecar_only = [bool]$SidecarOnly; files = $assets }
    Write-AtomicJson $manifest $manifestPath
    Remove-Item -LiteralPath $pendingManifestPath -Force
    Write-Output "Release assets: $(Join-Path $TargetDirectory 'release')"
    Write-Output 'Deploy the complete sidecar directory with engram_server.exe.'
} finally {
    [Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', $previousCargoTarget, 'Process')
    [Environment]::SetEnvironmentVariable('DOTNET_GENERATE_ASPNET_CERTIFICATE', $previousCertificateSetting, 'Process')
    if ($publishStage -and (Test-Path -LiteralPath $publishStage)) {
        $resolvedStage = [IO.Path]::GetFullPath($publishStage)
        $targetPrefix = $TargetDirectory.TrimEnd('\') + '\'
        if (-not $resolvedStage.StartsWith($targetPrefix, [StringComparison]::OrdinalIgnoreCase) -or
            (Split-Path $resolvedStage -Leaf) -notmatch '^sidecar-publish-[0-9a-f]{32}$') {
            throw 'Refusing to remove a publish stage outside the recorded build target'
        }
        Remove-Item -LiteralPath $resolvedStage -Recurse -Force
    }
}
