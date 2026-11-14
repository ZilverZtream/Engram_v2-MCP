# Connect Engram to your AI client on Windows

This archive includes the Windows x64 server, its complete self-contained VB.NET
parser sidecar, and a first-time installer. Rust and .NET SDKs are not required.
Use a trusted archive; checksums detect damaged files, not an untrusted publisher.

Download a completed build from the repository's **Actions → Windows package**
workflow. Choose a successful run for the commit you intend to install, then
download its `Engram-win-x64-<commit>` artifact. GitHub access to the repository is
required. These builds are retained for 30 days and are not permanent releases.
The workflow runs for relevant changes on `master` and can also be started manually.

Extract the downloaded Actions artifact. It contains `Engram-win-x64.zip`, its
SHA256 file, and build/source identity JSON. Before extracting the inner ZIP:

```powershell
$expected = ((Get-Content .\Engram-win-x64.zip.sha256 -Raw).Trim() -split '\s+')[0]
if ((Get-FileHash .\Engram-win-x64.zip -Algorithm SHA256).Hash -ine $expected) {
    throw 'Archive checksum mismatch'
}
Expand-Archive .\Engram-win-x64.zip .\Engram
Set-Location .\Engram
```

`source.json` identifies the source commit and workflow run. Retain it with the
download when reporting an installation problem. The inner archive contains the
same complete runtime and installer described below; no local build is needed.

Extract the trusted archive. For **Codex**, with its CLI installed, run from the
extracted directory:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 `
    -Codex `
    -InstallRoot "$env:LOCALAPPDATA\engram" `
    -ProjectRoot 'C:\path\to\your-project'
```

This registers Engram through `codex mcp add` using the CLI's configuration.
An existing same-named server is refused. `-ServerName` chooses another name;
`-CodexExecutable` selects a CLI executable or wrapper outside `PATH`.
The existing Codex settings and other server entries are preserved. Restart the
client after registration. See [Codex MCP setup](https://developers.openai.com/codex/mcp).
The execution-policy option applies only to this PowerShell process.

For an **`mcpServers` JSON client**, run:

```powershell
.\install.ps1 `
    -InstallRoot "$env:LOCALAPPDATA\engram" `
    -ProjectRoot 'C:\path\to\your-project' `
    -McpConfigPath 'C:\path\to\your-client\mcp.json'
```

Replace the paths. Use `-McpConfigPath` for JSON clients or `-Codex` for Codex.
The installation directory must be new; its parent and the client configuration's
parent must exist. An existing installation or `engram` client entry is preserved
and refused. Use `-ServerName` for a different MCP entry name. This is not an upgrade
installer. Unrelated client entries are retained, and an existing JSON client file is
backed up inside the new installation as `mcp-client.before.json`.

The installer copies every manifest-listed runtime asset, creates `engram.json`
inside the installation, and registers the server. It does not start a server or
index the project. Keep the server and its sidecar together.

Restart your AI client and use its Engram connection. Ask the AI to call
`index_project` with:

```json
{"directory":"C:\\path\\to\\your-project","project_type":"dotnet_webforms_vb","wait":true}
```

For another stack, choose its corresponding project type. Keep the returned
`project_id`. Then call `get_index_freshness` with:

```json
{"project_id":"YOUR_PROJECT_ID"}
```

Choose a function name from the project and call `search_memory`:

```json
{"project_id":"YOUR_PROJECT_ID","query":"YourFunctionName","semantic":false,"include_content":true}
```

Check the returned path and source. If a result is truncated, use its returned
`doc_id` with `get_chunk` to retrieve the stored source. Empty results do not prove
the function is absent; check indexing coverage and the spelling of the name.

The installed configuration uses lexical search and needs no model download or
provider credentials. Your AI client supplies the conversational model. Optional
Engram embedding and text-generation providers can be configured later in
`<InstallRoot>\engram.json`. The installer grants Engram access only to the project
directory you supplied.
