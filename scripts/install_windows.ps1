# Install a trusted, unpacked Windows release into a NEW directory and register
# it with an MCP client using the mcpServers JSON format. No downloads or builds.
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$PackageRoot,
    [Parameter(Mandatory=$true)][string]$InstallRoot,
    [Parameter(Mandatory=$true)][string]$ProjectRoot,
    [Parameter(Mandatory=$true)][string]$McpConfigPath,
    [ValidatePattern('^[A-Za-z0-9_-]+$')][string]$ServerName = 'engram'
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'This installer requires Windows.' }
$utf8 = New-Object Text.UTF8Encoding($false)
function Absolute([string]$Path) {
    return $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)
}
function WriteJson($Value, [string]$Path) {
    [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 100), $utf8)
}
function Hash([string]$Path) { return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash }
function PackagePath([string]$Root, [string]$Relative) {
    if ($Relative -ne 'engram_server.exe' -and -not $Relative.StartsWith('sidecar/')) { throw "Unexpected package asset: $Relative" }
    foreach ($part in $Relative.Split('/')) {
        if (-not $part -or $part -in @('.', '..') -or $part.IndexOfAny([IO.Path]::GetInvalidFileNameChars()) -ge 0 -or $part -ne $part.TrimEnd(' ', '.')) {
            throw "Invalid package path: $Relative"
        }
    }
    return Join-Path $Root $Relative.Replace('/', '\')
}
$PackageRoot = Absolute $PackageRoot
$InstallRoot = (Absolute $InstallRoot).TrimEnd('\')
$ProjectRoot = Absolute $ProjectRoot
$McpConfigPath = Absolute $McpConfigPath
if (Test-Path -LiteralPath $InstallRoot) { throw 'InstallRoot already exists. This installer never upgrades or overwrites an existing installation.' }
if (-not (Test-Path -LiteralPath $ProjectRoot -PathType Container)) { throw 'ProjectRoot must be an existing project directory.' }
$installParent = Split-Path $InstallRoot -Parent
$clientParent = Split-Path $McpConfigPath -Parent
if (-not (Test-Path -LiteralPath $installParent -PathType Container) -or -not (Test-Path -LiteralPath $clientParent -PathType Container)) {
    throw 'The parent directories of InstallRoot and McpConfigPath must already exist.'
}

# Read and validate client settings before writing any installation files.
$clientExisted = Test-Path -LiteralPath $McpConfigPath
$clientHash = $null
if ($clientExisted) {
    $clientHash = Hash $McpConfigPath
    $client = Get-Content -LiteralPath $McpConfigPath -Raw | ConvertFrom-Json
} else { $client = [pscustomobject]@{} }
if ($client -isnot [pscustomobject]) { throw 'MCP configuration must be a JSON object.' }
if (-not $client.PSObject.Properties['mcpServers']) {
    $client | Add-Member -NotePropertyName mcpServers -NotePropertyValue ([pscustomobject]@{})
}
if ($client.mcpServers -isnot [pscustomobject]) { throw 'mcpServers must be a JSON object.' }
if ($client.mcpServers.PSObject.Properties[$ServerName]) { throw "MCP server '$ServerName' already exists; its configuration was preserved. Choose a different ServerName." }

$manifestPath = Join-Path $PackageRoot 'windows-release-manifest.json'
if (Test-Path -LiteralPath (Join-Path $PackageRoot 'windows-release-pending.json')) { throw 'Package build is incomplete.' }
$manifestHash = Hash $manifestPath
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ($manifest.runtime -cne 'win-x64' -or $manifest.sidecar_only -isnot [bool] -or $manifest.sidecar_only) { throw 'A full win-x64 release package is required.' }
$assets = @{}
foreach ($asset in $manifest.files) {
    $relative = [string]$asset.path
    $source = PackagePath $PackageRoot $relative
    if ($assets.ContainsKey($relative) -or [string]$asset.sha256 -notmatch '^[A-Fa-f0-9]{64}$') { throw 'Invalid or duplicate manifest asset.' }
    if ((Hash $source) -ne $asset.sha256) { throw "Package checksum mismatch: $relative" }
    $assets[$relative] = [string]$asset.sha256
}
foreach ($required in @('engram_server.exe', 'sidecar/vb_roslyn_sidecar-win-x64.exe', 'sidecar/vb_roslyn_sidecar-win-x64.dll', 'sidecar/vb_roslyn_sidecar-win-x64.runtimeconfig.json', 'sidecar/vb_roslyn_sidecar-win-x64.deps.json', 'sidecar/Microsoft.CodeAnalysis.VisualBasic.dll', 'sidecar/hostfxr.dll', 'sidecar/coreclr.dll')) {
    if (-not $assets.ContainsKey($required)) { throw "Full VB sidecar package is missing: $required" }
}

$stage = Join-Path $installParent ('.engram-install-' + [Guid]::NewGuid().ToString('N'))
$clientStage = Join-Path $clientParent ('.engram-client-' + [Guid]::NewGuid().ToString('N') + '.json')
$published = $false
$registered = $false
try {
    [void][IO.Directory]::CreateDirectory($stage)
    $stageBin = Join-Path $stage 'bin'
    foreach ($relative in $assets.Keys) {
        $destination = PackagePath $stageBin $relative
        [void][IO.Directory]::CreateDirectory((Split-Path $destination -Parent))
        Copy-Item -LiteralPath (PackagePath $PackageRoot $relative) -Destination $destination
        if ((Hash $destination) -ne $assets[$relative]) { throw "Copied package checksum mismatch: $relative" }
    }
    Copy-Item -LiteralPath $manifestPath -Destination (Join-Path $stageBin 'windows-release-manifest.json')
    if ((Hash (Join-Path $stageBin 'windows-release-manifest.json')) -ne $manifestHash) { throw 'Package manifest changed during installation.' }
    $configPath = Join-Path $InstallRoot 'engram.json'
    $config = [ordered]@{
        allowed_roots = @($ProjectRoot)
        data_dir = (Join-Path $InstallRoot 'data')
        embedding_backend = 'fts_only'
        llm_backend = 'none'
        advertise_all_tools = $true
        multi_client = $true
        multi_client_daemon = $false
    }
    WriteJson $config (Join-Path $stage 'engram.json')
    $entry = [ordered]@{
        command = (Join-Path $InstallRoot 'bin\engram_server.exe')
        args = @('--no-daemon')
        env = @{ ENGRAM_CONFIG_PATH = $configPath }
    }
    $client.mcpServers | Add-Member -NotePropertyName $ServerName -NotePropertyValue $entry
    WriteJson $client $clientStage
    WriteJson ([ordered]@{
        package_manifest_sha256 = $manifestHash
        assets = $assets.Count
        project_root = $ProjectRoot
        mcp_config_path = $McpConfigPath
        server_name = $ServerName
    }) (Join-Path $stage 'installation.json')

    # Publish the complete package before making it discoverable by the client.
    [IO.Directory]::Move($stage, $InstallRoot)
    $published = $true
    if ($clientExisted) {
        if ((Hash $McpConfigPath) -ne $clientHash) { throw 'Client configuration changed during installation; it was preserved.' }
        [IO.File]::Replace($clientStage, $McpConfigPath, (Join-Path $InstallRoot 'mcp-client.before.json'))
    } else { [IO.File]::Move($clientStage, $McpConfigPath) }
    $registered = $true
    Write-Output "Installed Engram and $($assets.Count - 1) sidecar assets in $InstallRoot"
    Write-Output "Registered '$ServerName' in $McpConfigPath. Restart the MCP client to connect."
    Write-Output "Next: index_project(directory='$ProjectRoot', project_type='dotnet_webforms_vb', wait=true)."
    Write-Output 'Source search is ready without API keys. Configure embedding/LLM providers in engram.json when needed.'
} finally {
    # Only paths newly owned by this invocation may be removed. Never touch a
    # preexisting install, project, client file, or package during teardown.
    if (Test-Path -LiteralPath $clientStage) { Remove-Item -LiteralPath $clientStage -Force }
    if (Test-Path -LiteralPath $stage) {
        $resolved = [IO.Path]::GetFullPath($stage)
        if ((Split-Path $resolved -Parent) -ne $installParent -or (Split-Path $resolved -Leaf) -notmatch '^\.engram-install-[a-f0-9]{32}$') { throw 'Unexpected owned staging path.' }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
    if ($published -and -not $registered) {
        # A complete unpublished installation is retained for inspection; client
        # registration failed, so no process was started by this installer.
        Write-Warning "Client registration failed. Complete new installation retained at $InstallRoot; resolve the client configuration before using it."
    }
}
