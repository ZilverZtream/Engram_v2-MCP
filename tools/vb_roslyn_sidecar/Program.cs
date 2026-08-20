using System.Text.Json;
using System.Text.Json.Serialization;

var emitter = new AstEmitter();
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

    if (request.Cmd == "shutdown")
    {
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse(), AppJsonContext.Default.SidecarResponse));
        Console.Out.Flush();
        return;
    }

    if (request.Cmd == "begin_project")
    {
        try
        {
            emitter.BeginProject(request.ProjectRoot ?? string.Empty);
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse(), AppJsonContext.Default.SidecarResponse));
        }
        catch (Exception ex)
        {
            Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Error = ex.Message }, AppJsonContext.Default.SidecarResponse));
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

    if (request.Cmd != "parse")
    {
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Path = request.Path, Error = $"unknown command {request.Cmd}" }, AppJsonContext.Default.SidecarResponse));
        Console.Out.Flush();
        continue;
    }

    try
    {
        var (symbols, edges) = emitter.Extract(request.Path ?? string.Empty, request.Source ?? string.Empty);
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
        {
            Path = request.Path,
            Symbols = symbols,
            Edges = edges
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
    [JsonPropertyName("cmd")]
    public string? Cmd { get; set; }

    [JsonPropertyName("path")]
    public string? Path { get; set; }

    [JsonPropertyName("source")]
    public string? Source { get; set; }

    [JsonPropertyName("project_root")]
    public string? ProjectRoot { get; set; }

    /// <summary>Absolute paths for the `invalidate` command.</summary>
    [JsonPropertyName("paths")]
    public List<string>? Paths { get; set; }
}

internal sealed class SidecarResponse
{
    [JsonPropertyName("path")]
    public string? Path { get; set; }

    [JsonPropertyName("symbols")]
    public List<SymbolDto> Symbols { get; set; } = new();

    [JsonPropertyName("edges")]
    public List<EdgeDto> Edges { get; set; } = new();

    [JsonPropertyName("error")]
    public string? Error { get; set; }

    /// <summary>How many cached trees the `invalidate` command dropped.</summary>
    [JsonPropertyName("invalidated")]
    public int? Invalidated { get; set; }
}

[JsonSourceGenerationOptions(DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull)]
[JsonSerializable(typeof(SidecarRequest))]
[JsonSerializable(typeof(SidecarResponse))]
internal partial class AppJsonContext : JsonSerializerContext { }
