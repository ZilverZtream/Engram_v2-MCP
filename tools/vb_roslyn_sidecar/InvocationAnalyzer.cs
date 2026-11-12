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
            dto.Repetition = Repetition(invocation, semanticModel);
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
                    var argumentSyntax = arguments.Value[ordinal];
                    var argumentOperation = operation?.Arguments.FirstOrDefault(candidate =>
                        candidate.Syntax.Span == argumentSyntax.Span);
                    if (argumentSyntax is OmittedArgumentSyntax omitted)
                    {
                        dto.Arguments.Add(new InvocationArgumentDto
                        {
                            SourceIndex = 0,
                            SpanStart = omitted.Span.Start,
                            SpanLength = omitted.Span.Length,
                            StartLine = StartLine(omitted),
                            EndLine = EndLine(omitted),
                            Classification = "omitted",
                            SyntaxOrdinal = ordinal,
                            ParameterOrdinal = argumentOperation?.Parameter?.Ordinal
                        });
                        continue;
                    }

                    if (argumentSyntax is not SimpleArgumentSyntax simple)
                    {
                        report.OmittedArgumentCount++;
                        report.Notes.Add($"unsupported argument syntax at line {StartLine(argumentSyntax)}");
                        continue;
                    }
                    var expression = simple.Expression;
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

    private static InvocationRepetitionDto? Repetition(InvocationExpressionSyntax invocation, SemanticModel model)
    {
        var ancestors = invocation.Ancestors().ToList();
        // A lambda is deferred code, not an execution of the containing loop or handler.
        // Do not manufacture an outer-method database path through it either.
        if (ancestors.Any(node => node is LambdaExpressionSyntax)) return null;
        var method = ancestors.OfType<MethodBlockSyntax>().FirstOrDefault();
        if (method is null) return null;
        var result = new InvocationRepetitionDto
        {
            MethodName = method.SubOrFunctionStatement.Identifier.ValueText,
            MethodStartLine = StartLine(method), MethodEndLine = EndLine(method),
            MethodId = Bounded(model.GetDeclaredSymbol(method.SubOrFunctionStatement)?.GetDocumentationCommentId())
        };
        foreach (var ancestor in ancestors.TakeWhile(node => node != method))
        {
            SyntaxNode? header = null;
            string? kind = null;
            switch (ancestor)
            {
                case ForEachBlockSyntax loop when loop.Statements.Any(statement => statement.Span.Contains(invocation.Span)):
                    header = loop.ForEachStatement; kind = "for_each"; break;
                case ForBlockSyntax loop when loop.Statements.Any(statement => statement.Span.Contains(invocation.Span)):
                    header = loop.ForStatement; kind = "for"; break;
                case WhileBlockSyntax loop:
                    header = loop.WhileStatement; kind = "while"; break;
                case DoLoopBlockSyntax loop:
                    header = loop.DoStatement; kind = "do_loop"; break;
            }
            if (header is null) continue;
            if (result.Contexts.Count == 8) { result.ContextsTruncated = true; break; }
            result.Contexts.Add(new RepetitionContextDto
            {
                Kind = kind!, StartLine = StartLine(header), EndLine = EndLine(header),
                SpanStart = ancestor.Span.Start, SpanLength = ancestor.Span.Length
            });
        }
        foreach (var binding in method.SubOrFunctionStatement.HandlesClause?.Events ?? default)
        {
            var eventName = binding.EventMember.ToString().Trim('[', ']');
            if (!eventName.Equals("RowDataBound", StringComparison.OrdinalIgnoreCase)
                && !eventName.Equals("ItemDataBound", StringComparison.OrdinalIgnoreCase)) continue;
            if (result.Contexts.Count == 8) { result.ContextsTruncated = true; break; }
            result.Contexts.Add(new RepetitionContextDto
            {
                Kind = eventName.Equals("RowDataBound", StringComparison.OrdinalIgnoreCase) ? "row_data_bound" : "item_data_bound",
                StartLine = StartLine(binding), EndLine = EndLine(binding),
                SpanStart = method.Span.Start, SpanLength = method.Span.Length,
                Binding = Bounded(binding.ToString())
            });
        }
        result.DatabaseCall = DatabaseCall(invocation, model);
        return result;
    }

    private static DatabaseCallDto? DatabaseCall(InvocationExpressionSyntax invocation, SemanticModel model)
    {
        if (invocation.Expression is not MemberAccessExpressionSyntax member) return null;
        var methodName = member.Name.Identifier.ValueText;
        var execute = new[] { "ExecuteReader", "ExecuteScalar", "ExecuteNonQuery", "ExecuteXmlReader", "ExecuteReaderAsync", "ExecuteScalarAsync", "ExecuteNonQueryAsync", "ExecuteXmlReaderAsync" };
        var adapter = new[] { "Fill", "FillSchema", "Update" };
        var commandCall = execute.Contains(methodName, StringComparer.OrdinalIgnoreCase);
        if (!commandCall && !adapter.Contains(methodName, StringComparer.OrdinalIgnoreCase))
            return ContextCall(invocation, member, model);
        // Resolve the receiver declaration, not a substring such as GetData in a helper name.
        var receiver = member.Expression;
        if (receiver is null) return null;
        var symbol = model.GetSymbolInfo(receiver).Symbol;
        var declarations = symbol?.DeclaringSyntaxReferences;
        if (declarations is null || declarations.Value.Length != 1) return null;
        var syntax = declarations.Value[0].GetSyntax();
        var declaration = syntax.AncestorsAndSelf().OfType<VariableDeclaratorSyntax>().FirstOrDefault();
        if (declaration is null) return null;
        var creation = (declaration.AsClause as AsNewClauseSyntax)?.NewExpression as ObjectCreationExpressionSyntax
            ?? declaration.Initializer?.Value as ObjectCreationExpressionSyntax;
        var type = creation?.Type ?? (declaration.AsClause as SimpleAsClauseSyntax)?.Type;
        if (type is null) return null;
        var typeName = type.ToString().Replace("Global.", "", StringComparison.OrdinalIgnoreCase);
        var suffix = typeName.Split('.').Last().Trim('[', ']');
        var allowed = commandCall ? new[] { "SqlCommand", "OleDbCommand", "OdbcCommand", "DbCommand" }
            : new[] { "SqlDataAdapter", "OleDbDataAdapter", "OdbcDataAdapter", "DbDataAdapter" };
        if (!allowed.Contains(suffix, StringComparer.OrdinalIgnoreCase)) return null;
        var resolvedType = model.GetTypeInfo(type).Type;
        // A source-defined class named SqlCommand is not framework database evidence.
        if (resolvedType?.Locations.Any(location => location.IsInSource) == true) return null;
        if (resolvedType is not null && resolvedType.TypeKind != TypeKind.Error
            && !resolvedType.ContainingNamespace.ToDisplayString().StartsWith("System.Data", StringComparison.Ordinal)
            && !resolvedType.ContainingNamespace.ToDisplayString().Equals("Microsoft.Data.SqlClient", StringComparison.Ordinal)) return null;
        if (typeName.Contains('.') && !typeName.StartsWith("System.Data.", StringComparison.OrdinalIgnoreCase)
            && !typeName.StartsWith("Data.", StringComparison.OrdinalIgnoreCase)
            && !typeName.StartsWith("Microsoft.Data.SqlClient.", StringComparison.OrdinalIgnoreCase)) return null;
        string? sql = null;
        if (creation?.ArgumentList?.Arguments.FirstOrDefault() is SimpleArgumentSyntax argument
            && argument.Expression is LiteralExpressionSyntax literal && literal.IsKind(SyntaxKind.StringLiteralExpression))
            sql = literal.Token.ValueText.Length <= 4096 ? literal.Token.ValueText : null;
        return new DatabaseCallDto
        {
            Method = methodName, ReceiverType = typeName, DeclarationLine = StartLine(declaration),
            LiteralCommand = sql, CommandTextOrigin = "constructor_literal_only; later CommandText and parameter assignments are not evaluated",
            Evidence = resolvedType?.TypeKind == TypeKind.Error ? "declared_database_type_syntax" : "declared_database_type_symbol"
        };
    }

    private static readonly HashSet<string> Materializers = new(StringComparer.OrdinalIgnoreCase)
        { "ToList", "ToArray", "First", "FirstOrDefault", "Single", "SingleOrDefault", "Count", "LongCount", "Any", "Sum", "Min", "Max", "Average" };
    private static readonly HashSet<string> QueryCombinators = new(StringComparer.OrdinalIgnoreCase)
        { "Where", "Select", "SelectMany", "OrderBy", "OrderByDescending", "ThenBy", "ThenByDescending", "Take", "Skip", "Distinct", "AsQueryable", "Join", "GroupJoin", "DefaultIfEmpty", "GroupBy" };

    private static DatabaseCallDto? ContextCall(InvocationExpressionSyntax invocation, MemberAccessExpressionSyntax member, SemanticModel model)
    {
        var name = member.Name.Identifier.ValueText;
        var direct = new[] { "SubmitChanges", "SaveChanges", "SaveChangesAsync", "ExecuteQuery", "ExecuteCommand" }
            .Contains(name, StringComparer.OrdinalIgnoreCase);
        if ((!Materializers.Contains(name) && !direct) || member.Expression is null) return null;
        var origin = ContextOrigin(member.Expression, invocation, model, new HashSet<ISymbol>(SymbolEqualityComparer.Default), 0);
        if (origin is null) return null;
        return new DatabaseCallDto
        {
            Method = name, ReceiverType = origin.Value.Type, DeclarationLine = origin.Value.Line,
            Evidence = direct ? "declared_context_execution_candidate" : "query_materialization_from_declared_context",
            CommandTextOrigin = "LINQ/provider SQL is not translated or executed; declared DataContext/DbContext source syntax is candidate evidence"
        };
    }

    private static (string Type, int Line)? ContextOrigin(ExpressionSyntax expression, InvocationExpressionSyntax call, SemanticModel model, HashSet<ISymbol> visited, int depth)
    {
        if (depth >= 8) return null;
        if (expression is ParenthesizedExpressionSyntax paren) return ContextOrigin(paren.Expression, call, model, visited, depth + 1);
        if (expression is QueryExpressionSyntax query)
        {
            var origins = query.Clauses.OfType<FromClauseSyntax>().SelectMany(clause => clause.Variables)
                .Select(variable => ContextOrigin(variable.Expression, call, model, new HashSet<ISymbol>(visited, SymbolEqualityComparer.Default), depth + 1))
                .Where(origin => origin is not null).Distinct().ToList();
            return origins.Count == 1 ? origins[0] : null;
        }
        if (expression is InvocationExpressionSyntax nested)
        {
            // Stop at a materialization/AsEnumerable boundary: operations on its
            // in-memory result are not additional database executions.
            if (nested.Expression is not MemberAccessExpressionSyntax access || access.Expression is null
                || !QueryCombinators.Contains(access.Name.Identifier.ValueText)) return null;
            return ContextOrigin(access.Expression, call, model, visited, depth + 1);
        }
        var symbol = model.GetSymbolInfo(expression).Symbol;
        if (symbol is not null && visited.Add(symbol) && symbol.DeclaringSyntaxReferences.Length == 1)
        {
            var syntax = symbol.DeclaringSyntaxReferences[0].GetSyntax();
            var declaration = syntax.AncestorsAndSelf().OfType<VariableDeclaratorSyntax>().FirstOrDefault();
            if (declaration is not null)
            {
                var creation = (declaration.AsClause as AsNewClauseSyntax)?.NewExpression as ObjectCreationExpressionSyntax
                    ?? declaration.Initializer?.Value as ObjectCreationExpressionSyntax;
                var type = creation?.Type ?? (declaration.AsClause as SimpleAsClauseSyntax)?.Type;
                var typeName = type?.ToString();
                if (typeName is not null && (typeName.EndsWith("DataContext", StringComparison.OrdinalIgnoreCase) || typeName.EndsWith("DbContext", StringComparison.OrdinalIgnoreCase)))
                {
                    var resolved = model.GetTypeInfo(type!).Type;
                    // Source-defined lookalikes must actually declare a context base.
                    if (resolved?.Locations.Any(location => location.IsInSource) == true
                        && !HasContextBase(resolved)) return null;
                    return (typeName, StartLine(declaration));
                }
                if (symbol is ILocalSymbol && declaration.Initializer is not null)
                {
                    var method = call.Ancestors().OfType<MethodBlockSyntax>().FirstOrDefault();
                    if (method is null || method.DescendantNodes().OfType<AssignmentStatementSyntax>().Any(assignment =>
                        assignment.SpanStart > declaration.Span.End && assignment.SpanStart < call.SpanStart
                        && SymbolEqualityComparer.Default.Equals(model.GetSymbolInfo(assignment.Left).Symbol, symbol))) return null;
                    return ContextOrigin(declaration.Initializer.Value, call, model, visited, depth + 1);
                }
            }
        }
        if (expression is MemberAccessExpressionSyntax member && member.Expression is not null)
            return ContextOrigin(member.Expression, call, model, visited, depth + 1);
        return null;
    }

    private static bool HasContextBase(ITypeSymbol type)
    {
        var parent = type.BaseType;
        for (var i = 0; parent is not null && i < 8; i++, parent = parent.BaseType)
            if (parent.Name is "DataContext" or "DbContext") return true;
        return false;
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
    [JsonPropertyName("repetition_version")]
    public string RepetitionVersion { get; set; } = "vb-repetition-contexts-v1";
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
    [JsonPropertyName("repetition")]
    public InvocationRepetitionDto? Repetition { get; set; }
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

internal sealed class InvocationRepetitionDto
{
    [JsonPropertyName("method_name")] public string MethodName { get; set; } = string.Empty;
    [JsonPropertyName("method_id")] public string? MethodId { get; set; }
    [JsonPropertyName("method_start_line")] public int MethodStartLine { get; set; }
    [JsonPropertyName("method_end_line")] public int MethodEndLine { get; set; }
    [JsonPropertyName("contexts")] public List<RepetitionContextDto> Contexts { get; set; } = new();
    [JsonPropertyName("contexts_truncated")] public bool ContextsTruncated { get; set; }
    [JsonPropertyName("database_call")] public DatabaseCallDto? DatabaseCall { get; set; }
}

internal sealed class RepetitionContextDto
{
    [JsonPropertyName("kind")] public string Kind { get; set; } = string.Empty;
    [JsonPropertyName("start_line")] public int StartLine { get; set; }
    [JsonPropertyName("end_line")] public int EndLine { get; set; }
    [JsonPropertyName("span_start")] public int SpanStart { get; set; }
    [JsonPropertyName("span_length")] public int SpanLength { get; set; }
    [JsonPropertyName("binding")] public string? Binding { get; set; }
}

internal sealed class DatabaseCallDto
{
    [JsonPropertyName("method")] public string Method { get; set; } = string.Empty;
    [JsonPropertyName("receiver_type")] public string ReceiverType { get; set; } = string.Empty;
    [JsonPropertyName("declaration_line")] public int DeclarationLine { get; set; }
    [JsonPropertyName("literal_command")] public string? LiteralCommand { get; set; }
    [JsonPropertyName("command_text_origin")] public string? CommandTextOrigin { get; set; }
    [JsonPropertyName("evidence")] public string Evidence { get; set; } = string.Empty;
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
