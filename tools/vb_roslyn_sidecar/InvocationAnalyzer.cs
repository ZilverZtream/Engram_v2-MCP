using System.Security.Cryptography;
using System.Text;
using System.Text.Json.Serialization;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.Operations;
using Microsoft.CodeAnalysis.VisualBasic;
using Microsoft.CodeAnalysis.VisualBasic.Syntax;

internal static class InvocationAnalyzer
{
    private const int MaxInvocations = 512;
    private const int MaxArgumentsPerInvocation = 64;
    private const int MaxIdentityChars = 512;

    internal static InvocationReport Analyze(string source, string? requestId)
    {
        // Always parse the request text. This query never consults AstEmitter's cached trees.
        // A decoded leading BOM is an encoding marker, not VB syntax. Replace
        // it with one space so every UTF-16 span remains aligned to `source`.
        var parseSource = source.StartsWith("\uFEFF", StringComparison.Ordinal)
            ? " " + source[1..]
            : source;
        var tree = VisualBasicSyntaxTree.ParseText(parseSource);
        var diagnostics = tree.GetDiagnostics().ToList();
        var compilation = VisualBasicCompilation.Create("invocation_query").AddSyntaxTrees(tree);
        var semanticModel = compilation.GetSemanticModel(tree, ignoreAccessibility: true);
        var report = new InvocationReport
        {
            RequestId = requestId,
            SourceSha256 = Convert.ToHexString(
                SHA256.HashData(Encoding.UTF8.GetBytes(source))).ToLowerInvariant(),
            ParseErrorCount = diagnostics.Count(
                diagnostic => diagnostic.Severity == DiagnosticSeverity.Error)
        };
        report.ParseStatus = report.ParseErrorCount == 0 ? "complete" : "syntax_errors";

        foreach (var invocation in tree.GetRoot().DescendantNodes().OfType<InvocationExpressionSyntax>())
        {
            if (report.Invocations.Count >= MaxInvocations)
            {
                report.Truncated = true;
                report.OmittedInvocationCount++;
                continue;
            }

            IInvocationOperation? operation = null;
            try
            {
                operation = semanticModel.GetOperation(invocation) as IInvocationOperation;
            }
            catch
            {
                // Syntax evidence remains available and is labelled unresolved below.
            }
            var dto = new InvocationDto
            {
                SourceIndex = 0,
                SpanStart = invocation.Span.Start,
                SpanLength = invocation.Span.Length,
                CalleeSpanStart = invocation.Expression.Span.Start,
                CalleeSpanLength = invocation.Expression.Span.Length,
                StartLine = StartLine(invocation),
                EndLine = EndLine(invocation),
                TargetMethodId = Bounded(operation?.TargetMethod.GetDocumentationCommentId()),
                Resolution = operation?.TargetMethod is null ? "lexical" : "symbol"
            };
            var arguments = invocation.ArgumentList?.Arguments;
            if (arguments is not null)
            {
                for (var ordinal = 0; ordinal < arguments.Value.Count; ordinal++)
                {
                    if (ordinal >= MaxArgumentsPerInvocation)
                    {
                        report.Truncated = true;
                        report.OmittedArgumentCount++;
                        continue;
                    }
                    if (arguments.Value[ordinal] is not SimpleArgumentSyntax simple)
                    {
                        report.OmittedArgumentCount++;
                        report.Notes.Add($"unsupported argument syntax at line {StartLine(arguments.Value[ordinal])}");
                        continue;
                    }
                    var expression = simple.Expression;
                    var argumentOperation = operation?.Arguments.FirstOrDefault(candidate =>
                        candidate.Syntax.Span == simple.Span);
                    var classification = expression is MemberAccessExpressionSyntax
                        ? "member_access"
                        : expression.IsKind(SyntaxKind.StringLiteralExpression)
                            ? "string_literal"
                            : "other";
                    dto.Arguments.Add(new InvocationArgumentDto
                    {
                        SourceIndex = 0,
                        SpanStart = expression.Span.Start,
                        SpanLength = expression.Span.Length,
                        StartLine = StartLine(expression),
                        EndLine = EndLine(expression),
                        Name = Bounded(simple.NameColonEquals?.Name.Identifier.ValueText),
                        Classification = classification,
                        SyntaxOrdinal = ordinal,
                        ParameterOrdinal = argumentOperation?.Parameter?.Ordinal
                    });
                }
            }
            report.Invocations.Add(dto);
        }
        if (report.Notes.Count > 32)
        {
            report.Notes = report.Notes.Take(32).ToList();
            report.Truncated = true;
        }
        return report;
    }

    private static int StartLine(SyntaxNode node) =>
        node.GetLocation().GetLineSpan().StartLinePosition.Line + 1;

    private static int EndLine(SyntaxNode node) =>
        node.GetLocation().GetLineSpan().EndLinePosition.Line + 1;

    private static string? Bounded(string? value) => value is null
        ? null
        : value.Length <= MaxIdentityChars ? value : value[..MaxIdentityChars];
}

internal sealed class InvocationReport
{
    [JsonPropertyName("version")]
    public string Version { get; set; } = "vb-invocations-v1";

    [JsonPropertyName("request_id")]
    public string? RequestId { get; set; }

    [JsonPropertyName("source_sha256")]
    public string SourceSha256 { get; set; } = string.Empty;

    [JsonPropertyName("source_count")]
    public int SourceCount { get; set; } = 1;

    [JsonPropertyName("scope")]
    public string Scope { get; set; } = "full_source";

    [JsonPropertyName("parse_status")]
    public string ParseStatus { get; set; } = "complete";

    [JsonPropertyName("parse_error_count")]
    public int ParseErrorCount { get; set; }

    [JsonPropertyName("invocations")]
    public List<InvocationDto> Invocations { get; set; } = new();

    [JsonPropertyName("truncated")]
    public bool Truncated { get; set; }

    [JsonPropertyName("omitted_invocation_count")]
    public int OmittedInvocationCount { get; set; }

    [JsonPropertyName("omitted_argument_count")]
    public int OmittedArgumentCount { get; set; }

    [JsonPropertyName("notes")]
    public List<string> Notes { get; set; } = new();
}

internal sealed class InvocationDto
{
    [JsonPropertyName("source_index")]
    public int SourceIndex { get; set; }

    [JsonPropertyName("span_start")]
    public int SpanStart { get; set; }

    [JsonPropertyName("span_length")]
    public int SpanLength { get; set; }

    [JsonPropertyName("callee_span_start")]
    public int CalleeSpanStart { get; set; }

    [JsonPropertyName("callee_span_length")]
    public int CalleeSpanLength { get; set; }

    [JsonPropertyName("start_line")]
    public int StartLine { get; set; }

    [JsonPropertyName("end_line")]
    public int EndLine { get; set; }

    [JsonPropertyName("target_method_id")]
    public string? TargetMethodId { get; set; }

    [JsonPropertyName("resolution")]
    public string Resolution { get; set; } = "lexical";

    [JsonPropertyName("arguments")]
    public List<InvocationArgumentDto> Arguments { get; set; } = new();
}

internal sealed class InvocationArgumentDto
{
    [JsonPropertyName("source_index")]
    public int SourceIndex { get; set; }

    [JsonPropertyName("span_start")]
    public int SpanStart { get; set; }

    [JsonPropertyName("span_length")]
    public int SpanLength { get; set; }

    [JsonPropertyName("start_line")]
    public int StartLine { get; set; }

    [JsonPropertyName("end_line")]
    public int EndLine { get; set; }

    [JsonPropertyName("name")]
    public string? Name { get; set; }

    [JsonPropertyName("classification")]
    public string Classification { get; set; } = "other";

    [JsonPropertyName("syntax_ordinal")]
    public int SyntaxOrdinal { get; set; }

    [JsonPropertyName("parameter_ordinal")]
    public int? ParameterOrdinal { get; set; }
}
