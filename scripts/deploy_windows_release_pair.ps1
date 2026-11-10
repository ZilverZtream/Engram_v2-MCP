# PROPOSAL ONLY. Not invoked by preparation or by the build script.
# Operates on a quiescent, operator-managed private installation. Its file lock
# serializes cooperating installers; it is not an OS-wide filesystem CAS.
[CmdletBinding()]
param(
    [ValidateSet('ValidateUpgrade','Upgrade','ValidateRollback','Rollback')]
    [string]$Action = 'ValidateUpgrade',
    [string]$InstallRoot,
    [string]$CandidateRoot,
    [string]$EvidenceFile,
    [string]$EvidenceSha256,
    [string]$AuxiliaryInventory,
    [string]$AuxiliaryInventorySha256,
    [string]$TransactionRecord,
    [string]$TransactionRecordSha256
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $InstallRoot -or -not $EvidenceFile -or -not $EvidenceSha256 -or -not $AuxiliaryInventory -or -not $AuxiliaryInventorySha256) { throw 'Supply explicit install root, evidence/inventory paths and their exact SHA-256 pins; no interactive input is requested' }
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Windows only' }
$script:Held = [Collections.Generic.List[IDisposable]]::new()
$script:Pins = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::OrdinalIgnoreCase)
$maxStageBytes = 512MB
$maxArchiveBytes = 2GB # Hash/rename only; historical executables are never copied into stage.
$maxStageFiles = 1024
$maxInstallEntries = 4096 # Retained complete pairs and five journals per upgrade share this parent.
$lockStream = $null

function Sha([string]$Value) {
    if ($Value -notmatch '^[0-9a-fA-F]{64}$') { throw 'Expected an exact SHA-256 pin' }
    return $Value.ToLowerInvariant()
}
function Full([string]$Value) {
    if ($Value -notmatch '^[A-Za-z]:[\\/]') { throw 'Use an explicit local absolute drive path' }
    $result = [IO.Path]::GetFullPath($Value.Replace('/','\')).TrimEnd('\')
    if ($result.Length -le 3 -or $result.StartsWith('\\')) { throw 'Drive roots/UNC/verbatim paths are unsupported' }
    foreach ($piece in $result.Substring(3).Split('\')) {
        if (-not $piece -or $piece.Contains('~') -or $piece -match '[\x00-\x1f<>:"|?*]' -or $piece -ne $piece.TrimEnd(' ','.')) { throw 'Ambiguous/short-name Windows path component' }
    }
    return $result
}
function CheckPath([string]$Path) {
    $walk = Full $Path
    while ($walk.Length -gt 3) {
        if (Test-Path -LiteralPath $walk) {
            $entry = Get-Item -LiteralPath $walk -Force
            if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Reparse path preserved: $walk" }
        }
        $parent = [IO.Path]::GetDirectoryName($walk)
        if (-not $parent -or $parent -eq $walk) { break }
        $walk = $parent
    }
}
function Relative([string]$Value) {
    if (-not $Value -or $Value.Length -gt 512 -or $Value.Contains('\') -or $Value.StartsWith('/')) { throw 'Invalid manifest-relative path' }
    foreach ($part in $Value.Split('/')) {
        if (-not $part -or $part -in @('.','..') -or $part -ne $part.TrimEnd(' ','.') -or
            $part -match '[\x00-\x1f<>:"|?*]' -or $part -match '^(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)') { throw 'Unsupported Windows asset path' }
    }
    return $Value
}
function Child([string]$Root,[string]$Value) {
    [void](Relative $Value)
    $path = Full (Join-Path $Root $Value.Replace('/','\'))
    if (-not $path.StartsWith($Root+'\',[StringComparison]::OrdinalIgnoreCase)) { throw 'Asset escapes its recorded root' }
    CheckPath $path
    return $path
}
function Pin([string]$Path,[string]$Expected,[long]$MaxBytes) {
    $path = Full $Path
    $expected = Sha $Expected
    CheckPath $path
    if ($script:Pins.ContainsKey($path)) {
        $prior = $script:Pins[$path]
        if ($prior.Sha256 -ne $expected -or $prior.Bytes -gt $MaxBytes) { throw 'Conflicting pin/budget for the same file' }
        return $prior
    }
    $stream = [IO.File]::Open($path,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)
    $script:Held.Add($stream)
    if ($stream.Length -gt $MaxBytes) { throw "File exceeds declared verification budget: $path" }
    $hasher = [Security.Cryptography.SHA256]::Create()
    try { $actual = ([BitConverter]::ToString($hasher.ComputeHash($stream))).Replace('-','').ToLowerInvariant() }
    finally { $hasher.Dispose() }
    $stream.Position = 0
    if ($actual -ne $expected) { throw "Exact file hash differs; preserved: $path" }
    $result = [pscustomobject]@{ Path=$path; Sha256=$actual; Bytes=$stream.Length; Stream=$stream }
    $script:Pins.Add($path,$result)
    return $result
}
function JsonPin([string]$Path,[string]$Expected) {
    $pin = Pin $Path $Expected 1MB
    $buffer = [byte[]]::new([int]$pin.Bytes)
    $offset = 0
    while ($offset -lt $buffer.Length) {
        $count = $pin.Stream.Read($buffer,$offset,$buffer.Length-$offset)
        if ($count -le 0) { throw 'Truncated pinned JSON' }
        $offset += $count
    }
    $pin.Stream.Position = 0
    $text = [Text.UTF8Encoding]::new($false,$true).GetString($buffer).TrimStart([char]0xfeff)
    return ($text | ConvertFrom-Json)
}
function ClosePins {
    foreach ($handle in $script:Held) { $handle.Dispose() }
    $script:Held.Clear(); $script:Pins.Clear()
}
function Tree([string]$Root,[long]$Budget) {
    CheckPath $Root
    $map = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::OrdinalIgnoreCase)
    $pending = [Collections.Generic.Stack[string]]::new(); $pending.Push($Root)
    $bytes = 0L; $directories = 0; $entries = 0
    $directoryNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    while ($pending.Count) {
        $directory = $pending.Pop(); CheckPath $directory
        foreach ($name in [IO.Directory]::EnumerateFileSystemEntries($directory)) {
            $entries++; if ($entries -gt 4096) { throw 'Bounded installation inventory exceeded; preserved' }
            $entry = Get-Item -LiteralPath $name -Force
            if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Reparse entry preserved' }
            if ($entry.PSIsContainer) {
                $directories++; if ($directories -gt 256) { throw 'Installation directory bound exceeded' }
                [void]$directoryNames.Add($entry.FullName.Substring($Root.Length+1).Replace('\','/'))
                $pending.Push($entry.FullName)
            } else {
                $relative = $entry.FullName.Substring($Root.Length+1).Replace('\','/')
                [void](Relative $relative)
                $bytes += $entry.Length
                if ($bytes -gt $Budget -or $map.Count -ge 2048) { throw 'Installation inventory byte/count bound exceeded' }
                $map.Add($relative,[long]$entry.Length)
            }
        }
    }
    return [pscustomobject]@{ Files=$map; Bytes=$bytes; Directories=$directoryNames }
}
function ExactTree([string]$Root,$Expected,[long]$Budget) {
    $observed = Tree $Root $Budget
    $allowedDirectories = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($relative in $Expected.Keys) {
        $parts = $relative.Split('/')
        for ($index=1; $index -lt $parts.Length; $index++) { [void]$allowedDirectories.Add(($parts[0..($index-1)] -join '/')) }
    }
    foreach ($directory in $observed.Directories) { if (-not $allowedDirectories.Contains($directory)) { throw "Unknown directory preserved: $directory" } }
    if ($observed.Files.Count -ne $Expected.Count) { throw "Unknown/missing files preserved under $Root" }
    foreach ($path in $observed.Files.Keys) {
        if (-not $Expected.ContainsKey($path)) { throw "Unlisted file preserved: $path" }
        [void](Pin (Child $Root $path) $Expected[$path] $Budget)
    }
}
function Package([string]$Root,$Identity,[bool]$Candidate) {
    $root = Full $Root; CheckPath $root
    if (Test-Path -LiteralPath (Child $root 'windows-release-pending.json')) { throw 'Pending package inventory is not a final package' }
    $manifestSha = Sha ([string]$Identity.manifest_sha256)
    $binarySha = Sha ([string]$Identity.binary_sha256)
    $manifest = JsonPin (Child $root 'windows-release-manifest.json') $manifestSha
    if ($manifest.runtime -cne 'win-x64' -or $manifest.sidecar_only -isnot [bool] -or $manifest.sidecar_only -ne $false) { throw 'Require a final full win-x64 package manifest' }
    $files = @($manifest.files)
    if ($files.Count -lt 5 -or $files.Count+3 -gt $maxStageFiles) { throw 'Package asset inventory count is unsupported' }
    $expected = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::OrdinalIgnoreCase)
    $sidecar = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::OrdinalIgnoreCase)
    $bytes = 0L
    foreach ($file in $files) {
        $relative = Relative ([string]$file.path)
        if ($relative -ne 'engram_server.exe' -and -not $relative.StartsWith('sidecar/',[StringComparison]::Ordinal)) { throw 'Manifest contains a non-package path' }
        if ($file.PSObject.Properties['acceptable_sha256']) { throw 'Alternative/pending hashes are not a final package identity' }
        $hash = Sha ([string]$file.sha256)
        if ($expected.ContainsKey($relative)) { throw 'Duplicate/case-aliased package path' }
        $expected.Add($relative,$hash)
        $pin = Pin (Child $root $relative) $hash $maxStageBytes
        $bytes += $pin.Bytes
        if ($bytes -gt $maxStageBytes) { throw 'Package exceeds512MiB stage bound' }
        if ($relative.StartsWith('sidecar/',[StringComparison]::Ordinal)) { $sidecar.Add($relative.Substring(8),$hash) }
    }
    foreach ($required in @('engram_server.exe','sidecar/vb_roslyn_sidecar-win-x64.exe','sidecar/vb_roslyn_sidecar-win-x64.dll','sidecar/vb_roslyn_sidecar-win-x64.runtimeconfig.json','sidecar/vb_roslyn_sidecar-win-x64.deps.json')) {
        if (-not $expected.ContainsKey($required)) { throw "Required package asset missing: $required" }
    }
    if ($expected['engram_server.exe'] -ne $binarySha) { throw 'Manifest/server SHA pin differs' }
    ExactTree (Child $root 'sidecar') $sidecar $maxStageBytes
    $expected.Add('windows-release-manifest.json',$manifestSha)
    # Cargo's other release outputs are not package assets and are not copied.
    return [pscustomobject]@{ Root=$root; Expected=$expected; Bytes=$bytes; ManifestSha=$manifestSha; BinarySha=$binarySha }
}
function AddAux($Expected,$Aux) {
    foreach ($file in @($Aux.files)) { $Expected.Add([string]$file.path,(Sha ([string]$file.sha256))) }
}
function AddRetained($Expected,$Entries) {
    $entries = @($Entries)
    if ($entries.Count -gt 64) { throw 'Retained extra inventory exceeds bound' }
    foreach ($file in $entries) {
        $relative = Relative ([string]$file.path)
        if ($relative.Contains('/') -or (-not $relative.EndsWith('.exe',[StringComparison]::OrdinalIgnoreCase) -and $relative -notmatch '^engram_server\.exe\.pre_[A-Za-z0-9][A-Za-z0-9_.-]{0,95}$')) { throw 'Only explicitly reviewed, hash-pinned historical top-level executables or engram_server.exe.pre_ backups may be retained extras' }
        $Expected.Add($relative,(Sha ([string]$file.sha256)))
    }
}
function Quiescent($Gate) {
    if ($Gate.quiescence.maintained -isnot [bool] -or $Gate.quiescence.maintained -ne $true) { throw 'Reviewed maintained-quiescence evidence is required' }
    [void](Pin ([string]$Gate.quiescence.receipt.path) ([string]$Gate.quiescence.receipt.sha256) 1MB)
    $seenProcesses = 0
    Get-CimInstance Win32_Process -Property Name,ExecutablePath,ProcessId -Filter "Name='engram_server.exe' OR Name='vb_roslyn_sidecar-win-x64.exe'" | ForEach-Object {
        $entry = $_; $seenProcesses++
        if ($seenProcesses -gt 256) { throw 'Process inspection bound exceeded' }
        if (-not $entry.ExecutablePath) { throw 'Cannot establish an existing Engram process image; package paths are unchanged' }
        $image = Full ([string]$entry.ExecutablePath)
        if ($image.StartsWith($InstallRoot+'\',[StringComparison]::OrdinalIgnoreCase) -or
            ($CandidateRoot -and $image.StartsWith($CandidateRoot+'\',[StringComparison]::OrdinalIgnoreCase))) { throw 'Package process still running; operator must quiesce it before cutover' }
    }
    $primary = Full ([string]$Gate.quiescence.primary_identity_path); CheckPath $primary
    if (Test-Path -LiteralPath $primary) {
        $stream = [IO.File]::Open($primary,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)
        try {
            if ($stream.Length -gt 8192) { throw 'Primary identity exceeds inspection bound' }
            $buffer = [byte[]]::new(8193); $count = 0
            while ($count -lt $buffer.Length) {
                $read = $stream.Read($buffer,$count,$buffer.Length-$count)
                if ($read -eq 0) { break }; $count += $read
            }
            if ($count -gt 8192) { throw 'Primary identity grew beyond its inspection bound' }
            $identityText = [Text.UTF8Encoding]::new($false,$true).GetString($buffer,0,$count)
        } finally { $stream.Dispose() }
        $pidLines = @($identityText.Split("`n") | ForEach-Object { $_.TrimEnd("`r") } | Where-Object { $_ -match '^pid=[0-9]+$' })
        if ($pidLines.Count -ne 1) { throw 'Cannot validate the existing primary identity; preserved' }
        $appPrimaryPid = [int]$pidLines[0].Substring(4)
        if (Get-Process -Id $appPrimaryPid -ErrorAction SilentlyContinue) { throw 'Recorded primary PID is live; no process is stopped by this script' }
    }
}
function Gate([string]$Path,[string]$Hash,[bool]$Rollback) {
    $gate = JsonPin $Path $Hash
    if ($gate.schema_version -ne 1 -or $gate.release_ready -isnot [bool] -or $gate.release_ready -ne $true -or (Full ([string]$gate.install_root)) -ne $InstallRoot) { throw 'Release evidence is not final for this installation' }
    if ($gate.candidate.sidecar_contract -cnotin @('vb-ingestion-parser-v4', 'vb-ingestion-parser-v5', 'vb-ingestion-parser-v6')) { throw 'Candidate parser contract lacks final package evidence' }
    $ids = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($item in @($gate.gates)) {
        if ($item.id -cnotmatch '^S(0[1-9]|1[0-9])$' -or -not $ids.Add([string]$item.id) -or $item.status -cne 'passed' -or
            (Sha ([string]$item.package_manifest_sha256)) -ne (Sha ([string]$gate.candidate.manifest_sha256))) { throw 'Each of the19 gates needs explicit final-package-bound passing evidence' }
        [void](Pin ([string]$item.receipt.path) ([string]$item.receipt.sha256) 1MB)
    }
    if ($ids.Count -ne 19) { throw 'All19 storage release gates are required; partial/local tests do not authorize installation' }
    # Hashes/build dates do not establish whether a package move is an upgrade.
    # Every replacement needs reviewed current-data compatibility/materialization.
    $proof = $gate.storage_transition
    $expectedFrom = if ($Rollback) { $gate.candidate.manifest_sha256 } else { $gate.installed.manifest_sha256 }
    $expectedTo = if ($Rollback) { $gate.installed.manifest_sha256 } else { $gate.candidate.manifest_sha256 }
    if ($proof.status -cne 'passed' -or (Sha ([string]$proof.from_manifest_sha256)) -ne (Sha ([string]$expectedFrom)) -or
        (Sha ([string]$proof.to_manifest_sha256)) -ne (Sha ([string]$expectedTo))) { throw 'No exact-direction current-data transition compatibility/materialization proof; package replacement refused' }
    [void](Sha ([string]$proof.quiescent_data_state_identity))
    [void](Pin ([string]$proof.receipt.path) ([string]$proof.receipt.sha256) 1MB)
    return $gate
}
function AcquireLock {
    $path = Child $InstallRoot 'engram-pair-install.lock'
    $magic = [Text.Encoding]::ASCII.GetBytes("Engram full-pair installer lock v1`r`n")
    $created = $false
    try { $stream = [IO.File]::Open($path,[IO.FileMode]::CreateNew,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None); $created = $true }
    catch [IO.IOException] {
        $stream = [IO.File]::Open($path,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::None)
    }
    try {
        if ($created) { $stream.Write($magic,0,$magic.Length); $stream.Flush($true) }
        else {
            if ($stream.Length -ne $magic.Length) { throw 'Unknown coordination file preserved' }
            $buffer = [byte[]]::new($magic.Length); $count = 0
            while ($count -lt $buffer.Length) { $read = $stream.Read($buffer,$count,$buffer.Length-$count); if ($read -eq 0) { break }; $count += $read }
            if ($count -ne $magic.Length -or [Convert]::ToBase64String($buffer) -ne [Convert]::ToBase64String($magic)) { throw 'Unknown coordination file preserved' }
        }
    } catch { $stream.Dispose(); throw }
    return $stream # Persistent exact owner marker; no race-prone pathname deletion.
}
function Journal($Record,[int]$Step,[string]$Phase) {
    $journalValue = [ordered]@{}; foreach ($key in $Record.Keys) { $journalValue[$key] = $Record[$key] }
    $journalValue['phase'] = $Phase; $journalValue['at'] = [DateTime]::UtcNow.ToString('o')
    $path = Child $InstallRoot ('engram-pair-'+$Record.id+'.'+$Step.ToString('00')+'-'+$Phase+'.json')
    $bytes = [Text.UTF8Encoding]::new($false).GetBytes((ConvertTo-Json -InputObject $journalValue -Depth 12))
    if ($bytes.Length -gt 64KB) { throw 'Journal budget exceeded' }
    $stream = [IO.File]::Open($path,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
    try { $stream.Write($bytes,0,$bytes.Length); $stream.Flush($true) } finally { $stream.Dispose() }
    Write-Output ([pscustomobject]@{ journal=$path; sha256=(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant(); phase=$Phase })
}
function MoveNew([string]$From,[string]$To) {
    CheckPath $From; CheckPath $To
    foreach ($path in @($From,$To)) { if ([IO.Path]::GetDirectoryName($path) -ne $InstallRoot) { throw 'Rename target is not an immediate child of the recorded install root' } }
    if (Test-Path -LiteralPath $To) { throw "Destination winner preserved: $To" }
    [IO.Directory]::Move($From,$To) # No replacement. A later creator wins; retain both inputs on failure.
}

try {
    $InstallRoot = Full $InstallRoot; CheckPath $InstallRoot
    if (-not (Test-Path -LiteralPath $InstallRoot -PathType Container)) { throw 'Explicit install parent must already exist' }
    $installDrive = [IO.DriveInfo]::new([IO.Path]::GetPathRoot($InstallRoot))
    if ($installDrive.DriveType -ne [IO.DriveType]::Fixed -or $installDrive.DriveFormat -cne 'NTFS') { throw 'This private cutover protocol supports fixed local NTFS only' }
    $active = Child $InstallRoot 'bin'
    if ($CandidateRoot) {
        $CandidateRoot = Full $CandidateRoot; CheckPath $CandidateRoot
        if ($CandidateRoot.StartsWith($InstallRoot+'\',[StringComparison]::OrdinalIgnoreCase) -or
            $InstallRoot.StartsWith($CandidateRoot+'\',[StringComparison]::OrdinalIgnoreCase) -or $CandidateRoot -eq $InstallRoot) { throw 'Release and installation roots must be disjoint' }
    }
    $rollback = $Action -in @('ValidateRollback','Rollback')
    $gate = Gate $EvidenceFile $EvidenceSha256 $rollback
    $aux = JsonPin $AuxiliaryInventory $AuxiliaryInventorySha256
    if ((Full ([string]$aux.install_bin)) -ne $active -or @($aux.files).Count -ne 2) { throw 'Auxiliary inventory does not bind this bin and both wrappers' }
    $auxNames = @('engram_discover_shim.js','engram_logged.bat')
    foreach ($file in @($aux.files)) { if ($file.path -notin $auxNames -or $file.bytes -le 0 -or $file.bytes -gt 4096) { throw 'Only the two bounded, explicitly pinned live wrappers may be carried' }; [void](Sha ([string]$file.sha256)) }
    if (@($aux.files.path | Sort-Object -Unique).Count -ne 2) { throw 'Duplicate wrapper identity' }
    Quiescent $gate
    if ($Action -in @('Upgrade','Rollback')) { $lockStream = AcquireLock; Quiescent $gate }
    if (-not $rollback) {
        if (-not $CandidateRoot) { throw 'CandidateRoot is required for upgrade' }
        $old = Package $active $gate.installed $false
        AddAux $old.Expected $aux; AddRetained $old.Expected $gate.retained_extra_files
        ExactTree $active $old.Expected $maxArchiveBytes
        $candidate = Package $CandidateRoot $gate.candidate $true
        AddAux $candidate.Expected $aux
        foreach ($file in @($aux.files)) {
            $pin = Pin (Child $active $file.path) $file.sha256 4096
            if ($pin.Bytes -ne $file.bytes) { throw 'Wrapper length differs from its exact inventory' }
        }
        $stageTotal = $candidate.Bytes + (Pin (Child $CandidateRoot 'windows-release-manifest.json') $candidate.ManifestSha 1MB).Bytes
        foreach ($file in @($aux.files)) { $stageTotal += [long]$file.bytes }
        if ($stageTotal -gt $maxStageBytes -or $candidate.Expected.Count -gt $maxStageFiles) { throw 'Candidate plus exact wrappers exceeds declared stage budget' }
        if ($Action -eq 'ValidateUpgrade') { [pscustomobject]@{ status='validated-only'; old=$old.ManifestSha; candidate=$candidate.ManifestSha; wrappers=2; retained_extras=@($gate.retained_extra_files).Count }; return }
        if ($old.ManifestSha -eq $candidate.ManifestSha -and $old.BinarySha -eq $candidate.BinarySha) { [pscustomobject]@{ status='package-already-identical'; post_install_runtime_validation='not implied' }; return }
        $topEntries = 0
        foreach ($name in [IO.Directory]::EnumerateFileSystemEntries($InstallRoot)) {
            $topEntries++; if ($topEntries -gt $maxInstallEntries) { throw 'Install parent inventory bound exceeded; preserve entries' }
            if ([IO.Path]::GetFileName($name) -like 'engram-pair-stage-*') { throw 'Prior stage path exists; preserve/reconcile its journal before another stage' }
        }
        $id = [Guid]::NewGuid().ToString('N')
        $stage = Child $InstallRoot ('engram-pair-stage-'+$id); $backup = Child $InstallRoot ('engram-pair-backup-'+$id)
        $record = [ordered]@{ schema_version=1; id=$id; install_root=$InstallRoot; active=$active; stage=$stage; backup=$backup;
            gate_sha256=(Sha $EvidenceSha256); auxiliary_inventory_sha256=(Sha $AuxiliaryInventorySha256);
            old=$gate.installed; candidate=$gate.candidate; storage_transition=$gate.storage_transition; retained_extra_files=@($gate.retained_extra_files);
            coordination_lock=(Child $InstallRoot 'engram-pair-install.lock'); cleanup='Retain old complete bin, journals and failed stage for exact review; no automatic deletion'; started_daemon=$false }
        Journal $record 0 'prepared'
        [void](New-Item -ItemType Directory -Path $stage -ErrorAction Stop)
        $stageBytes = 0L
        foreach ($relative in $candidate.Expected.Keys) {
            $source = if ($relative -in $auxNames) { Child $active $relative } else { Child $CandidateRoot $relative }
            $pin = Pin $source $candidate.Expected[$relative] $maxStageBytes
            $stageBytes += $pin.Bytes
            if ($stageBytes -gt $maxStageBytes) { throw 'Candidate plus wrappers exceeds512MiB stage cap; stage retained' }
            $destination = Child $stage $relative
            [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($destination))
            $output = [IO.File]::Open($destination,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
            try { $pin.Stream.Position=0; $pin.Stream.CopyTo($output,65536); $output.Flush($true); $pin.Stream.Position=0 } finally { $output.Dispose() }
        }
        if ($candidate.Expected.Count -gt $maxStageFiles) { throw 'Stage file cap exceeded' }
        ExactTree $stage $candidate.Expected $maxStageBytes
        ExactTree $active $old.Expected $maxArchiveBytes
        Quiescent $gate
        Journal $record 1 'stage-verified'
        ClosePins
        MoveNew $active $backup
        Journal $record 2 'old-retired'
        ExactTree $backup $old.Expected $maxArchiveBytes
        ClosePins
        MoveNew $stage $active
        Journal $record 3 'candidate-active'
        ExactTree $active $candidate.Expected $maxStageBytes
        Journal $record 4 'committed-no-start'
        [pscustomobject]@{ status='complete-pair-swapped-not-started'; active=$active; retained_old_pair=$backup; wrappers_carried=2; historical_executables_preserved_in=$backup; post_install_runtime_validation='still required' }
    } else {
        $transaction = JsonPin $TransactionRecord $TransactionRecordSha256
        if ($transaction.schema_version -ne 1 -or $transaction.id -notmatch '^[0-9a-f]{32}$' -or (Full ([string]$transaction.install_root)) -ne $InstallRoot -or
            (Sha ([string]$transaction.old.manifest_sha256)) -ne (Sha ([string]$gate.installed.manifest_sha256)) -or
            (Sha ([string]$transaction.old.binary_sha256)) -ne (Sha ([string]$gate.installed.binary_sha256)) -or
            (Sha ([string]$transaction.candidate.manifest_sha256)) -ne (Sha ([string]$gate.candidate.manifest_sha256)) -or
            (Sha ([string]$transaction.candidate.binary_sha256)) -ne (Sha ([string]$gate.candidate.binary_sha256)) -or
            (Sha ([string]$transaction.auxiliary_inventory_sha256)) -ne (Sha $AuxiliaryInventorySha256)) { throw 'Transaction/old/new/auxiliary identities disagree; preserved' }
        if ($transaction.phase -cnotin @('prepared','stage-verified','old-retired','candidate-active','committed-no-start')) { throw 'Unsupported rollback source phase; preserved' }
        $backup = Child $InstallRoot ('engram-pair-backup-'+$transaction.id)
        $sourceStage = Child $InstallRoot ('engram-pair-stage-'+$transaction.id)
        if ((Full ([string]$transaction.backup)) -ne $backup -or (Full ([string]$transaction.active)) -ne $active -or (Full ([string]$transaction.stage)) -ne $sourceStage) { throw 'Unbound rollback paths' }
        $old = Package $backup $gate.installed $false
        AddAux $old.Expected $aux; AddRetained $old.Expected $transaction.retained_extra_files
        ExactTree $backup $old.Expected $maxArchiveBytes
        $current = $null
        if (Test-Path -LiteralPath $active) {
            $current = Package $active $gate.candidate $true; AddAux $current.Expected $aux
            ExactTree $active $current.Expected $maxStageBytes
        }
        if ($Action -eq 'ValidateRollback') { [pscustomobject]@{ status='validated-only'; restore=$backup; current_pair_will_be_retained=$true }; return }
        Quiescent $gate
        $id = [Guid]::NewGuid().ToString('N'); $retired = Child $InstallRoot ('engram-pair-retired-'+$id)
        $record = [ordered]@{ schema_version=1; id=$id; install_root=$InstallRoot; active=$active; restore=$backup; retired_current=$retired;
            preserved_source_stage_path=$sourceStage; storage_transition=$gate.storage_transition;
            source_transaction=(Full $TransactionRecord); source_transaction_sha256=(Sha $TransactionRecordSha256); gate_sha256=(Sha $EvidenceSha256);
            cleanup='Retain complete retired candidate and journals; no automatic deletion'; started_daemon=$false }
        Journal $record 0 'rollback-prepared'
        ClosePins
        if ($current) { MoveNew $active $retired; Journal $record 1 'candidate-retired' }
        MoveNew $backup $active
        ExactTree $active $old.Expected $maxArchiveBytes
        Journal $record 2 'rollback-committed-no-start'
        [pscustomobject]@{ status='original-complete-pair-restored-not-started'; active=$active; retained_candidate=$(if($current){$retired}else{$null}); preserved_source_stage_path=$(if(Test-Path -LiteralPath $sourceStage){$sourceStage}else{$null}); storage_compatibility='external reviewed exact-state proof required and supplied; not inferred by installer'; post_rollback_runtime_validation='still required' }
    }
} catch {
    Write-Warning 'Operation failed/refused. No automatic deletion, overwrite, service stop/start or data rollback was attempted. Preserve recorded old/candidate/stage/journal paths and reconcile actual identities before retry.'
    throw
} finally {
    ClosePins
    if ($lockStream) { $lockStream.Dispose() }
}
