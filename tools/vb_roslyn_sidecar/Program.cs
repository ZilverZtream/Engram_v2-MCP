using System.Text.Json;

while (Console.In.ReadLine() is { } line)
{
    if (string.IsNullOrWhiteSpace(line))
    {
        continue;
    }

    SidecarRequest? request;
    try
    {
        request = JsonSerializer.Deserialize<SidecarRequest>(line);
    }
    catch (Exception ex)
    {
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Error = ex.Message }));
        Console.Out.Flush();
        continue;
    }

    if (request is null)
    {
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Error = "invalid request" }));
        Console.Out.Flush();
        continue;
    }

    if (request.Cmd == "shutdown")
    {
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse()));
        Console.Out.Flush();
        return;
    }

    if (request.Cmd != "parse")
    {
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Path = request.Path, Error = $"unknown command {request.Cmd}" }));
        Console.Out.Flush();
        continue;
    }

    try
    {
        var (symbols, edges) = AstEmitter.Extract(request.Path ?? string.Empty, request.Source ?? string.Empty);
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse
        {
            Path = request.Path,
            Symbols = symbols,
            Edges = edges,
        }));
    }
    catch (Exception ex)
    {
        Console.WriteLine(JsonSerializer.Serialize(new SidecarResponse { Path = request.Path, Error = ex.ToString() }));
    }

    Console.Out.Flush();
}

internal sealed class SidecarRequest
{
    public string? Cmd { get; set; }
    public string? Path { get; set; }
    public string? Source { get; set; }
}

internal sealed class SidecarResponse
{
    public string? Path { get; set; }
    public List<SymbolDto> Symbols { get; set; } = new();
    public List<EdgeDto> Edges { get; set; } = new();
    public string? Error { get; set; }
}
