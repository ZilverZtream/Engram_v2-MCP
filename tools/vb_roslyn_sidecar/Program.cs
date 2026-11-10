using System.Text.Json;
using System.Text.Json.Serialization;

// The Rust JSONL transport writes UTF-8 bytes regardless of the Windows console code page.
Console.InputEncoding = new System.Text.UTF8Encoding(false, true);
Console.OutputEncoding = new System.Text.UTF8Encoding(false, true);

var emitter = new AstEmitter();
// These identify the modules this process actually loaded, including the
// entry assembly containing the extraction passes. Disk deployment inventory
// cannot identify an already-running child.
var parserIdentity = ParserIdentity.Create();
while (Console.In.ReadLine() is { } line)
{
    if (string.IsNullOrWhiteSpace(line))
    {
        continue;
    }

    SidecarRequest? request;
    try
    {
        request = JsonSerializer.Deserialize(line, AppJsonContext.Default.SidecarRequest);
    }
    catch (Exception ex)
    {
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Error = ex.Message }, AppJsonContext.Default.SidecarResponse));
        Console.Out.Flush();
        continue;
    }

    if (request is null)
    {
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Error = "invalid request" }, AppJsonContext.Default.SidecarResponse));
        Console.Out.Flush();
        continue;
    }

    if (request.Cmd == "identity")
    {
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { ParserIdentity = parserIdentity }, AppJsonContext.Default.SidecarResponse));
        Console.Out.Flush();
        continue;
    }

    if (request.Cmd == "shutdown")
    {
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse(), AppJsonContext.Default.SidecarResponse));
        Console.Out.Flush();
        return;
    }

    if (request.Cmd == "compiler_context")
    {
        try
        {
            var context = CompilerContextSnapshot.Read(request.ProjectRoot ?? string.Empty);
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
            { CompilerContext = context.Identity, ParserIdentity = parserIdentity }, AppJsonContext.Default.SidecarResponse));
        }
        catch (Exception ex)
        {
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
            { Error = ex.Message, CompilationError = true }, AppJsonContext.Default.SidecarResponse));
        }
        Console.Out.Flush();
        continue;
    }

    if (request.Cmd == "begin_project")
    {
        try
        {
            emitter.BeginProject(request.ProjectRoot ?? string.Empty, request.Files);
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { ProjectRoot = emitter.ProjectRoot }, AppJsonContext.Default.SidecarResponse));
        }
        catch (Exception ex)
        {
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Error = ex.Message }, AppJsonContext.Default.SidecarResponse));
        }

        Console.Out.Flush();
        continue;
    }

    if (request.Cmd == "plan_refresh" || request.Cmd == "refresh_project")
    {
        try
        {
            CompilationPlanDto? plan = null;
            if (request.Cmd == "plan_refresh")
                plan = emitter.PlanRefresh(request.ProjectRoot ?? string.Empty, request.Files, request.Paths ?? [],
                    request.CompilationPolicy ?? throw new InvalidOperationException("Missing compilation policy"), request.CompilerContext);
            else
                emitter.RefreshProject(request.PlanId ?? throw new InvalidOperationException("Missing compilation plan token"));
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
            {
                ProjectRoot = emitter.ProjectRoot,
                ParserIdentity = parserIdentity,
                CompilationPlan = plan,
                CompilerContext = plan is not null ? plan.CompilerContext : emitter.PreparedCompilerContext
            }, AppJsonContext.Default.SidecarResponse));
        }
        catch (Exception ex)
        {
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
            {
                Error = ex.Message,
                CompilationError = true
            }, AppJsonContext.Default.SidecarResponse));
        }
        Console.Out.Flush();
        continue;
    }

    if (request.Cmd == "invalidate")
    {
        try
        {
            var removed = emitter.InvalidateFiles(request.Paths ?? new List<string>());
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Invalidated = removed }, AppJsonContext.Default.SidecarResponse));
        }
        catch (Exception ex)
        {
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Error = ex.Message }, AppJsonContext.Default.SidecarResponse));
        }

        Console.Out.Flush();
        continue;
    }

    if (request.Cmd == "return_paths")
    {
        try
        {
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
            {
                Path = request.Path,
                ReturnPaths = ReturnPathAnalyzer.Analyze(request.Source ?? string.Empty,
                    request.MethodStartLine ?? 0, request.MethodEndLine ?? 0, request.RequestId)
            }, AppJsonContext.Default.SidecarResponse));
        }
        catch (Exception ex)
        {
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Path = request.Path, Error = ex.Message }, AppJsonContext.Default.SidecarResponse));
        }
        Console.Out.Flush();
        continue;
    }

    if (request.Cmd == "invocations")
    {
        try
        {
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
            {
                Path = request.Path,
                InvocationReport = InvocationAnalyzer.Analyze(
                    request.Source ?? string.Empty, request.RequestId)
            }, AppJsonContext.Default.SidecarResponse));
        }
        catch (Exception ex)
        {
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
            {
                Path = request.Path,
                Error = ex.Message
            }, AppJsonContext.Default.SidecarResponse));
        }
        Console.Out.Flush();
        continue;
    }

    if (request.Cmd != "parse")
    {
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Path = request.Path, Error = $"unknown command {request.Cmd}" }, AppJsonContext.Default.SidecarResponse));
        Console.Out.Flush();
        continue;
    }

    try
    {
        // New ingestion callers bind every parse to the warmed root. Omitted
        // fields retain the legacy protocol. Standalone queries never mutate
        // the singleton's compilation or property cache.
        var selected = request.Standalone ? new AstEmitter() : emitter;
        if (!request.Standalone && request.ProjectRoot is not null &&
            !string.Equals(selected.ProjectRoot, request.ProjectRoot,
                OperatingSystem.IsWindows() ? StringComparison.OrdinalIgnoreCase : StringComparison.Ordinal))
        {
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
            {
                Error = "The intended project compilation is not prepared; retry the source batch",
                CompilationError = true
            }, AppJsonContext.Default.SidecarResponse));
            Console.Out.Flush();
            continue;
        }
        if (!request.Standalone && request.ProjectRoot is not null &&
            !selected.IsPreparedSource(request.Path ?? string.Empty, request.Source ?? string.Empty))
        {
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
            {
                Error = "Source bytes changed after compilation preparation; retry the source batch",
                CompilationError = true
            }, AppJsonContext.Default.SidecarResponse));
            Console.Out.Flush();
            continue;
        }
        if (!request.Standalone && request.ProjectRoot is not null && request.CompilerContext != selected.PreparedCompilerContext)
        {
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
            { Error = "Compiler context differs from prepared compilation", CompilationError = true }, AppJsonContext.Default.SidecarResponse));
            Console.Out.Flush();
            continue;
        }
        if (!request.Standalone && request.ProjectRoot is null && selected.PreparedCompilerContext is not null
            && !selected.IsPreparedSource(request.Path ?? string.Empty, request.Source ?? string.Empty))
        {
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
            { Error = "Changed source requires standalone extraction or a new admitted compilation", CompilationError = true }, AppJsonContext.Default.SidecarResponse));
            Console.Out.Flush();
            continue;
        }
        var (symbols, edges) = selected.Extract(request.Path ?? string.Empty, request.Source ?? string.Empty);
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
        {
            Path = request.Path,
            Symbols = symbols,
            Edges = edges,
            ParserIdentity = parserIdentity,
            ProjectRoot = selected.ProjectRoot,
            Standalone = request.Standalone,
            CompilerContext = request.Standalone ? null : selected.PreparedCompilerContext
        }, AppJsonContext.Default.SidecarResponse));
    }
    catch (Exception ex)
    {
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Path = request.Path, Error = ex.ToString() }, AppJsonContext.Default.SidecarResponse));
    }

    Console.Out.Flush();
}

internal sealed class SidecarRequest
{
    [JsonPropertyName("compiler_context")]
    public CompilerContextDto? CompilerContext { get; set; }
    [JsonPropertyName("cmd")]
    public string? Cmd { get; set; }

    [JsonPropertyName("path")]
    public string? Path { get; set; }

    [JsonPropertyName("source")]
    public string? Source { get; set; }

    [JsonPropertyName("request_id")]
    public string? RequestId { get; set; }

    [JsonPropertyName("method_start_line")]
    public int? MethodStartLine { get; set; }

    [JsonPropertyName("method_end_line")]
    public int? MethodEndLine { get; set; }

    [JsonPropertyName("project_root")]
    public string? ProjectRoot { get; set; }

    [JsonPropertyName("standalone")]
    public bool Standalone { get; set; }

    [JsonPropertyName("plan_id")]
    public ulong? PlanId { get; set; }

    [JsonPropertyName("compilation_policy")]
    public CompilationPolicyDto? CompilationPolicy { get; set; }

    /// <summary>Absolute paths for the `invalidate` command.</summary>
    [JsonPropertyName("paths")]
    public List<string>? Paths { get; set; }

    /// <summary>
    /// The exact files `begin_project` should parse, from the indexer.
    /// Omitted by older callers, which fall back to a directory walk.
    /// </summary>
    [JsonPropertyName("files")]
    public List<string>? Files { get; set; }
}

internal sealed class SidecarResponse
{
    [JsonPropertyName("compiler_context")]
    public CompilerContextDto? CompilerContext { get; set; }
    [JsonPropertyName("compilation_plan")]
    public CompilationPlanDto? CompilationPlan { get; set; }
    [JsonPropertyName("project_root")]
    public string? ProjectRoot { get; set; }

    [JsonPropertyName("standalone")]
    public bool Standalone { get; set; }

    [JsonPropertyName("compilation_error")]
    public bool CompilationError { get; set; }
    [JsonPropertyName("parser_identity")]
    public ParserIdentity? ParserIdentity { get; set; }

    [JsonPropertyName("path")]
    public string? Path { get; set; }

    [JsonPropertyName("symbols")]
    public List<SymbolDto> Symbols { get; set; } = new();

    [JsonPropertyName("edges")]
    public List<EdgeDto> Edges { get; set; } = new();

    [JsonPropertyName("error")]
    public string? Error { get; set; }

    /// <summary>How many cached trees the `invalidate` command dropped.</summary>
    [JsonPropertyName("return_paths")]
    public ReturnPathReport? ReturnPaths { get; set; }

    [JsonPropertyName("invocation_report")]
    public InvocationReport? InvocationReport { get; set; }

    [JsonPropertyName("invalidated")]
    public int? Invalidated { get; set; }
}

internal sealed record CompilationPolicyDto
{
    [JsonPropertyName("max_total_source_bytes")]
    public long MaxTotalSourceBytes { get; init; }
    [JsonPropertyName("max_file_source_bytes")]
    public long MaxFileSourceBytes { get; init; }
    [JsonPropertyName("max_trees")]
    public long MaxTrees { get; init; }
    [JsonPropertyName("allocation_multiplier")]
    public long AllocationMultiplier { get; init; }
    [JsonPropertyName("tree_overhead_bytes")]
    public long TreeOverheadBytes { get; init; }
    public void Validate()
    {
        if (MaxTotalSourceBytes <= 0 || MaxTotalSourceBytes > 32 * 1024 * 1024 ||
            MaxFileSourceBytes <= 0 || MaxFileSourceBytes > 10 * 1024 * 1024 ||
            MaxTrees <= 0 || MaxTrees > 250_000 || AllocationMultiplier != 32 || TreeOverheadBytes != 8192)
            throw new InvalidOperationException("Unsupported VB compilation admission policy");
    }
}

internal sealed class CompilationPlanDto
{
    [JsonPropertyName("compiler_context")]
    public CompilerContextDto? CompilerContext { get; set; }
    [JsonPropertyName("reference_bytes")]
    public long ReferenceBytes { get; set; }
    [JsonPropertyName("id")]
    public ulong Id { get; set; }
    [JsonPropertyName("total_source_bytes")]
    public long TotalSourceBytes { get; set; }
    [JsonPropertyName("tree_count")]
    public long TreeCount { get; set; }
    [JsonPropertyName("changed_source_bytes")]
    public long ChangedSourceBytes { get; set; }
    [JsonPropertyName("changed_tree_count")]
    public long ChangedTreeCount { get; set; }
    [JsonPropertyName("max_changed_file_bytes")]
    public long MaxChangedFileBytes { get; set; }
    [JsonPropertyName("full")]
    public bool Full { get; set; }
    [JsonPropertyName("policy")]
    public CompilationPolicyDto Policy { get; set; } = new();
}

internal sealed class ParserIdentity
{
    [JsonPropertyName("contract")]
    public string Contract { get; set; } = "vb-ingestion-parser-v6";
    [JsonPropertyName("entry_mvid")]
    public string EntryMvid { get; set; } = string.Empty;
    [JsonPropertyName("roslyn_core_mvid")]
    public string RoslynCoreMvid { get; set; } = string.Empty;
    [JsonPropertyName("roslyn_vb_mvid")]
    public string RoslynVbMvid { get; set; } = string.Empty;
    public static ParserIdentity Create() => new()
    {
        EntryMvid = typeof(AstEmitter).Assembly.ManifestModule.ModuleVersionId.ToString("D"),
        RoslynCoreMvid = typeof(Microsoft.CodeAnalysis.Compilation).Assembly.ManifestModule.ModuleVersionId.ToString("D"),
        RoslynVbMvid = typeof(Microsoft.CodeAnalysis.VisualBasic.VisualBasicCompilation).Assembly.ManifestModule.ModuleVersionId.ToString("D"),
    };
}

// Additive source locator, carried inside metadata until Rust validates it
// against the admitted declaration and its exact source revision.
internal sealed class SourceMemberLocatorDto
{
    [JsonPropertyName("version")] public int Version { get; set; } = 1;
    [JsonPropertyName("name")] public string Name { get; set; } = "";
    [JsonPropertyName("semantic_id")] public string SemanticId { get; set; } = "";
    [JsonPropertyName("file_path")] public string FilePath { get; set; } = "";
    [JsonPropertyName("start_line")] public int StartLine { get; set; }
    [JsonPropertyName("span_start")] public int SpanStart { get; set; }
    [JsonPropertyName("span_length")] public int SpanLength { get; set; }
    [JsonPropertyName("syntax_source_sha256")] public string SyntaxSourceSha256 { get; set; } = "";
}

internal sealed class InterfaceMemberLocatorDto
{
    [JsonPropertyName("version")] public int Version { get; set; } = 1;
    [JsonPropertyName("name")] public string Name { get; set; } = "";
    [JsonPropertyName("semantic_id")] public string SemanticId { get; set; } = "";
    [JsonPropertyName("file_path")] public string FilePath { get; set; } = "";
    [JsonPropertyName("start_line")] public int StartLine { get; set; }
    [JsonPropertyName("span_start")] public int SpanStart { get; set; }
    [JsonPropertyName("span_length")] public int SpanLength { get; set; }
    [JsonPropertyName("syntax_source_sha256")] public string SyntaxSourceSha256 { get; set; } = "";
    [JsonPropertyName("contract_type_key")] public string ContractTypeKey { get; set; } = "";
    [JsonPropertyName("contract_type")] public string ContractType { get; set; } = "";
}

[JsonSourceGenerationOptions(DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull)]
[JsonSerializable(typeof(SidecarRequest))]
[JsonSerializable(typeof(SidecarResponse))]
[JsonSerializable(typeof(InterfaceMemberLocatorDto))]
[JsonSerializable(typeof(SourceMemberLocatorDto))]
internal partial class AppJsonContext : JsonSerializerContext { }
