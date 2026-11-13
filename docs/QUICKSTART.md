# First verified query

This walkthrough connects an MCP client, indexes a small source sample, checks
freshness and retrieves a known function. It uses lexical search without model
downloads or provider credentials. Source builds and indexing a full application
are outside the five-minute small-sample target; their duration depends on the
machine and repository.

## If OciusX is already registered

Use the existing Engram connection. Call `list_projects` with `{}` and copy the
project ID for the correct OciusX directory. Call `get_index_freshness` with that
ID before relying on indexed results. Reuse the existing registration and runtime
configuration; the separate sample setup below is for a first connection.

For the first search, choose a function name you can see in the current checkout.
Use `search_memory` with that name, `semantic: false` and `include_content: true`.
Check the returned path and source, then use its `doc_id` with `get_chunk` if you
need the stored source. An empty search does not establish that a function is absent.

## Prepare a runtime

If you already have a working Engram installation, use its executable. A Windows
VB.NET installation needs its matching complete `sidecar/` directory alongside
the server. The current trusted parser contract is `vb-ingestion-parser-v6`.
Missing sidecar coverage is reported as fallback; an incompatible present
sidecar is refused. These are different outcomes.

For a fresh source checkout, follow [Install](../README.md#install). On Windows,
`scripts/build_windows_release.ps1` builds the server and self-contained VB
companion together; building requires the Rust toolchain and a stable .NET 9 SDK.
The shared target directory contains `release/engram_server.exe`, `release/sidecar/`
and `release/windows-release-manifest.json`. Use that complete package.

On Linux/macOS, `cargo build --release -p engram_server` produces
`target/release/engram_server` (or the configured `CARGO_TARGET_DIR`). The portable
client-side sample below does not require VB extraction. This guide does not
claim a matching VB sidecar package is published for those platforms.

## Install a Windows package and connect your project

If you have the complete trusted Windows package described above, this command
copies the server and all sidecar assets, creates a local Engram configuration,
and adds an `engram` entry to your chosen MCP client configuration:

```powershell
.\scripts\install_windows.ps1 `
    -PackageRoot 'C:\path\to\release' `
    -InstallRoot "$env:LOCALAPPDATA\engram" `
    -ProjectRoot 'C:\path\to\your-project' `
    -McpConfigPath 'C:\path\to\your-client\mcp.json'
```

Replace the paths. `InstallRoot` must be new; its parent and the client-config
parent must already exist. The client must support the **`mcpServers` JSON
format**. Existing unrelated servers and preferences are retained, with the
original client file backed up as `mcp-client.before.json` inside the install.
An existing installation or same-named MCP entry is refused; `-ServerName`
selects another entry name. This is a first-time installer, not an upgrade tool.

The installer verifies every manifest-listed asset before registration. Checksums
detect damaged or mismatched files; they do not authenticate an untrusted package.
It does not download a release, index your application or start a server. Public
prebuilt releases and macOS/Linux installation remain unfinished.

Restart the MCP client, then call:

```json
{"directory":"C:\\path\\to\\your-project","project_type":"dotnet_webforms_vb","wait":true}
```

Use `index_project` for that call. Keep its `project_id`, then follow the
freshness and query steps below. The installed configuration uses lexical
search without API keys. Set embedding and LLM providers in
`<InstallRoot>/engram.json` when those optional capabilities are needed.
The generated command uses shared multi-client mode without a detached daemon;
after the last client disconnects, the primary exits after its normal idle grace
period (300 seconds by default).

## Create a separate sample configuration

Choose a **new** directory for the sample. The commands refuse to overwrite it.
The sample configuration, copied sources, client snippet and index data all live
there. Keep them while using this connection; disconnect the client before any
later cleanup. This setup does not change the normal Engram configuration.

Run this in Windows PowerShell after replacing the three paths:

<!-- quickstart-windows -->
```powershell
$egRepo = 'C:\path\to\Engram-MCP_v2'
$egServer = 'C:\path\to\release\engram_server.exe'
$egQuickstart = 'C:\path\to\engram-quickstart'
$ErrorActionPreference = 'Stop'
if (-not (Test-Path -LiteralPath $egServer -PathType Leaf)) { throw 'Server executable not found' }
if (Test-Path -LiteralPath $egQuickstart) { throw 'Choose a new quickstart directory' }
[void][IO.Directory]::CreateDirectory($egQuickstart)
$egSample = Join-Path $egQuickstart 'sample'
Copy-Item -LiteralPath (Join-Path $egRepo 'tests/fixtures/webforms_retrieval/source/client') -Destination $egSample -Recurse
$egConfig = Join-Path $egQuickstart 'engram.json'
$egSettings = @{
    allowed_roots = @($egSample)
    data_dir = (Join-Path $egQuickstart 'data')
    embedding_backend = 'fts_only'
    llm_backend = 'none'
    advertise_all_tools = $true
    multi_client = $false
    multi_client_daemon = $false
}
$egUtf8 = New-Object Text.UTF8Encoding($false)
[IO.File]::WriteAllText($egConfig, ($egSettings | ConvertTo-Json -Depth 6), $egUtf8)
$egClient = @{ mcpServers = @{ engram_quickstart = @{
    command = $egServer
    args = @('--no-multi-client', '--no-daemon')
    env = @{ ENGRAM_CONFIG_PATH = $egConfig }
} } }
$egClientJson = $egClient | ConvertTo-Json -Depth 8
[IO.File]::WriteAllText((Join-Path $egQuickstart 'mcp-client.json'), $egClientJson, $egUtf8)
$egClientJson
```

On Linux/macOS, with Python 3 available, set the three paths and run:

```sh
export ENGRAM_SOURCE=/absolute/path/to/Engram-MCP_v2
export ENGRAM_SERVER=/absolute/path/to/target/release/engram_server
export ENGRAM_QUICKSTART=/absolute/path/to/engram-quickstart
python3 - <<'PY'
import json, os, shutil
from pathlib import Path
repo = Path(os.environ['ENGRAM_SOURCE']).resolve(strict=True)
server = Path(os.environ['ENGRAM_SERVER']).resolve(strict=True)
root = Path(os.environ['ENGRAM_QUICKSTART']).absolute()
if not server.is_file():
    raise SystemExit('Server executable not found')
root.mkdir(parents=True, exist_ok=False)
sample = root / 'sample'
shutil.copytree(repo / 'tests/fixtures/webforms_retrieval/source/client', sample)
config = root / 'engram.json'
config.write_text(json.dumps({
    'allowed_roots': [str(sample)], 'data_dir': str(root / 'data'),
    'embedding_backend': 'fts_only', 'llm_backend': 'none',
    'advertise_all_tools': True, 'multi_client': False, 'multi_client_daemon': False
}, indent=2), encoding='utf-8')
client = {'mcpServers': {'engram_quickstart': {
    'command': str(server), 'args': ['--no-multi-client', '--no-daemon'],
    'env': {'ENGRAM_CONFIG_PATH': str(config)}
}}}
text = json.dumps(client, indent=2)
(root / 'mcp-client.json').write_text(text, encoding='utf-8')
print(text)
PY
```

Add the printed server entry to your MCP client's configuration using the format
that client supports. Merge it with existing entries rather than replacing them.
The client starts Engram and owns its stdio connection; starting the executable
in a terminal alone does not index a project. Enable/reconnect the entry and
confirm `list_projects` appears in the client's tool list.

## Index, check, search

These are **tool names and argument objects**, not shell commands. Invoke them
through the MCP client. Substitute the absolute sample directory and the returned
project ID wherever the placeholders appear.

1. Call `index_project` with the following arguments. `wait: true` is convenient
   for this four-file sample. Copy the returned `project_id`.

<!-- quickstart-index_project -->
```json
{"directory":"<SAMPLE_DIRECTORY>","project_name":"Quickstart sample","project_type":"general","wait":true}
```

2. Call `get_index_freshness`:

<!-- quickstart-get_index_freshness -->
```json
{"project_id":"<PROJECT_ID>"}
```

For the untouched sample, expect a positive `active_generation`,
`generation_complete: true` and `disk_check: no_changes_detected`. If generation
is incomplete or disk/source checks report changes, resolve that condition
before treating the index as current. A project ID alone is not a freshness check.

3. Call `search_memory`:

<!-- quickstart-search_memory -->
```json
{"project_id":"<PROJECT_ID>","query":"validateImageUpload","semantic":false,"fts_mode":"loose","use_mmr":false,"include_content":true,"output_json":true}
```

Expect `upload.ts` among `results`, with the function `validateImageUpload` and
the `file.size > maxBytes` check. Compare it with `sample/upload.ts`. This verifies
one known source lookup; it does not establish complete project understanding.

4. Copy that hit's `doc_id`, then call `get_chunk`:

<!-- quickstart-get_chunk -->
```json
{"project_id":"<PROJECT_ID>","doc_id":"<DOC_ID>","output_json":true}
```

The returned `metadata.path` should be `upload.ts`, and `content` should contain
the same function. Stored source can normalize line endings; compare logical
lines. Keep the reported source hash with citations.

For a full application, allow its repository root in the normal configuration,
reuse any existing registration, and index with `wait: false` if a foreground
request would be too long. Poll the returned job using `get_job_status`, then
check freshness. Full OciusX indexing and optional semantic-provider setup have
separate costs; the small-sample timing is not an estimate for either.

## If a step fails

| Observation | Check |
| --- | --- |
| No tools appear | Executable path, client process stderr and `ENGRAM_CONFIG_PATH`; the configuration must exist and have an explicit allowed root. |
| Directory outside allowed roots | Use the absolute sample path written into `engram.json`; allow the intended repository explicitly for a different project. |
| Background job still running | Poll the same job ID; do not restart indexing because the client stopped waiting. |
| Empty search or wrong file | Confirm project ID, freshness, query spelling and the actual copied source; do not accept an empty result as success. |
| VB fallback or parser mismatch | Use the matching full Windows package, inspect the reported parser state and follow its reindex requirement. The client-side smoke test does not validate semantic VB extraction. |
