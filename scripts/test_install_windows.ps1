# Installer behavior checks with owned fake assets. Runtime/sidecar execution is
# checked separately against an actual complete release package.
[CmdletBinding()]
param([Parameter(Mandatory=$true)][string]$ScratchRoot)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$ScratchRoot = [IO.Path]::GetFullPath($ScratchRoot)
if (-not (Test-Path -LiteralPath $ScratchRoot -PathType Container)) { throw 'ScratchRoot must exist.' }
$owned = Join-Path $ScratchRoot ('installer-test-' + [Guid]::NewGuid().ToString('N'))
$installer = Join-Path $PSScriptRoot 'install_windows.ps1'
function Assert($Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function ExpectFailure($Arguments) {
    $failed = $false
    try { & $installer @Arguments | Out-Null } catch { $failed = $true }
    Assert $failed 'Expected installation refusal.'
}
try {
    [void][IO.Directory]::CreateDirectory($owned)
    $package = Join-Path $owned 'release'
    $project = Join-Path $owned 'project'
    [void][IO.Directory]::CreateDirectory((Join-Path $package 'sidecar'))
    [void][IO.Directory]::CreateDirectory($project)
    $files = @()
    foreach ($name in @('engram_server.exe', 'sidecar/vb_roslyn_sidecar-win-x64.exe', 'sidecar/vb_roslyn_sidecar-win-x64.dll', 'sidecar/vb_roslyn_sidecar-win-x64.runtimeconfig.json', 'sidecar/vb_roslyn_sidecar-win-x64.deps.json', 'sidecar/Microsoft.CodeAnalysis.VisualBasic.dll', 'sidecar/hostfxr.dll', 'sidecar/coreclr.dll')) {
        $path = Join-Path $package $name
        [IO.File]::WriteAllText($path, "Owned fake asset: $name")
        $files += @{ path=$name; sha256=(Get-FileHash -LiteralPath $path).Hash }
    }
    $manifest = @{ runtime='win-x64'; sidecar_only=$false; files=$files }
    $manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $package 'windows-release-manifest.json') -Encoding UTF8
    $clientPath = Join-Path $owned 'client.json'
    $original = '{"mcpServers":{"existing":{"command":"existing.exe","env":{"EXISTING":"keep"}}},"preferences":{"theme":"dark","flags":[true,false]}}'
    [IO.File]::WriteAllText($clientPath, $original)
    $arguments = @{ PackageRoot=$package; InstallRoot=(Join-Path $owned 'installed'); ProjectRoot=$project; McpConfigPath=$clientPath }
    & $installer @arguments | Out-Null
    $client = Get-Content -LiteralPath $clientPath -Raw | ConvertFrom-Json
    Assert ($client.mcpServers.existing.env.EXISTING -eq 'keep' -and $client.preferences.theme -eq 'dark' -and $client.preferences.flags.Count -eq 2) 'Unrelated client settings changed.'
    Assert ($client.mcpServers.engram.command -eq (Join-Path $arguments.InstallRoot 'bin\engram_server.exe')) 'Registered command is not the installed server.'
    Assert ((Get-Content -LiteralPath (Join-Path $arguments.InstallRoot 'mcp-client.before.json') -Raw) -ceq $original) 'Original client backup was not preserved.'
    foreach ($asset in $files) {
        Assert ((Get-FileHash -LiteralPath (Join-Path $arguments.InstallRoot ('bin/' + $asset.path))).Hash -eq $asset.sha256) 'Installed package differs.'
    }
    $clientHash = (Get-FileHash -LiteralPath $clientPath).Hash
    ExpectFailure $arguments
    Assert ((Get-FileHash -LiteralPath $clientPath).Hash -eq $clientHash) 'Existing installation refusal changed client config.'
    $arguments.InstallRoot = Join-Path $owned 'duplicate-name'
    ExpectFailure $arguments
    Assert (-not (Test-Path -LiteralPath $arguments.InstallRoot)) 'Duplicate server name created an installation.'
    Assert ((Get-FileHash -LiteralPath $clientPath).Hash -eq $clientHash) 'Duplicate server name changed client config.'

    $arguments.InstallRoot = Join-Path $owned 'fresh-install'
    $arguments.McpConfigPath = Join-Path $owned 'new-client.json'
    & $installer @arguments | Out-Null
    Assert ((Get-Content -LiteralPath $arguments.McpConfigPath -Raw | ConvertFrom-Json).mcpServers.engram.env.ENGRAM_CONFIG_PATH -eq (Join-Path $arguments.InstallRoot 'engram.json')) 'New client configuration is not connected to the installed config.'

    [IO.File]::WriteAllText((Join-Path $package 'sidecar/coreclr.dll'), 'Damaged package')
    $arguments.InstallRoot = Join-Path $owned 'corrupt-install'
    $arguments.McpConfigPath = Join-Path $owned 'corrupt-client.json'
    ExpectFailure $arguments
    Assert (-not (Test-Path -LiteralPath $arguments.InstallRoot) -and -not (Test-Path -LiteralPath $arguments.McpConfigPath)) 'Corrupt package was installed or registered.'
    Assert (@(Get-ChildItem -LiteralPath $owned -Force | Where-Object Name -Like '.engram-*').Count -eq 0) 'Owned staging files remain.'
    Write-Output 'PASS: new/existing client registration, complete asset copying, existing-install/name preservation and corrupt-package refusal.'
} finally {
    $resolved = [IO.Path]::GetFullPath($owned)
    if ((Split-Path $resolved -Parent) -ne $ScratchRoot.TrimEnd('\') -or (Split-Path $resolved -Leaf) -notmatch '^installer-test-[a-f0-9]{32}$') { throw 'Refusing unexpected fixture cleanup path.' }
    if (Test-Path -LiteralPath $resolved) { Remove-Item -LiteralPath $resolved -Recurse -Force }
}
