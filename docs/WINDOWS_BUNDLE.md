# Connect Engram to your AI client on Windows

This archive includes the Windows x64 server, its complete self-contained VB.NET
parser sidecar, and a first-time installer. Rust and .NET SDKs are not required.
Use a trusted archive; checksums detect damaged files, not an untrusted publisher.

Extract the archive. From its extracted directory, run:

```powershell
.\install.ps1 `
    -InstallRoot "$env:LOCALAPPDATA\engram" `
    -ProjectRoot 'C:\path\to\your-project' `
    -McpConfigPath 'C:\path\to\your-client\mcp.json'
```

Replace the paths. The AI client must support the `mcpServers` JSON format.
The installation directory must be new; its parent and the client configuration's
parent must exist. An existing installation or `engram` client entry is preserved
and refused. Use `-ServerName` for a different MCP entry name. This is not an upgrade
installer. Unrelated client entries are retained, and an existing client file is
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
