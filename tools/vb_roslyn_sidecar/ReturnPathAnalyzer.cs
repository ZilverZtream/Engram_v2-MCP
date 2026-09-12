using System.Security.Cryptography;
using System.Text;
using System.Text.Json.Serialization;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.VisualBasic;
using Microsoft.CodeAnalysis.VisualBasic.Syntax;

internal sealed class ReturnPathReport
{
    [JsonPropertyName("request_id")] public string RequestId { get; set; } = "";
    [JsonPropertyName("version")] public string Version { get; set; } = "vb-return-paths-v1";
    [JsonPropertyName("status")] public string Status { get; set; } = "unavailable";
    [JsonPropertyName("source_sha256")] public string SourceSha256 { get; set; } = "";
    [JsonPropertyName("method_start_line")] public int MethodStartLine { get; set; }
    [JsonPropertyName("method_end_line")] public int MethodEndLine { get; set; }
    [JsonPropertyName("structural_complete")] public bool StructuralComplete { get; set; }
    [JsonPropertyName("scope")] public string Scope { get; set; } = "Syntactic evaluation history for supported VB structured statements only. Every path assumes preceding statements and return expressions complete normally. Not compilation/name binding, path feasibility, current variable state, helper effects, exception-flow or semantic proof. Unavailable results contain no inferred fallthrough paths.";
    [JsonPropertyName("paths")] public List<ReturnPathDto> Paths { get; set; } = new();
    [JsonPropertyName("normal_fallthrough_paths")] public int NormalFallthroughPaths { get; set; }
    [JsonPropertyName("unsupported")] public List<ReturnPathIssue> Unsupported { get; set; } = new();
    [JsonPropertyName("unsupported_omitted")] public int UnsupportedOmitted { get; set; }
    [JsonPropertyName("caps")] public ReturnPathCaps Caps { get; set; } = new();
    [JsonPropertyName("parameter_occurrences")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public ParameterOccurrenceReport? ParameterOccurrences { get; set; }
    [JsonPropertyName("branch_contexts")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public BranchContextReport? BranchContexts { get; set; }
}
// Optional source ownership evidence, independent of return-path support.
internal sealed class BranchContextReport
{
    [JsonPropertyName("version")] public string Version { get; set; } = "vb-branch-contexts-v1";
    [JsonPropertyName("status")] public string Status { get; set; } = "unavailable";
    [JsonPropertyName("reason")] public string? Reason { get; set; }
    [JsonPropertyName("source_sha256")] public string SourceSha256 { get; set; } = "";
    [JsonPropertyName("method_start_line")] public int MethodStartLine { get; set; }
    [JsonPropertyName("method_end_line")] public int MethodEndLine { get; set; }
    [JsonPropertyName("entries")] public List<BranchContextEntry> Entries { get; set; } = new();
    [JsonPropertyName("unavailable_lines")] public List<int> UnavailableLines { get; set; } = new();
    [JsonPropertyName("scope")] public string Scope { get; set; } = "Exact supplied-file statement/header spans and enclosing VB If syntax only. Header evaluation excludes its own branch outcome. No binding, dominance, reachability, feasibility, variable-state, helper-effect or semantic-equivalence claim. Other control constructs are not predicates here. Unlisted and unavailable lines have no inferred ownership. Nested lambda lines and overlapping line ownership are excluded. Caps: 256 entries, 16 predicates per entry, 1024 UTF-8 bytes per condition, 1024 unavailable lines, 24 KiB inventory.";
}
internal sealed class BranchContextEntry
{
    [JsonPropertyName("statement_kind")] public string StatementKind { get; set; } = "";
    [JsonPropertyName("start_line")] public int StartLine { get; set; }
    [JsonPropertyName("end_line")] public int EndLine { get; set; }
    [JsonPropertyName("conditions")] public List<BranchContextCondition> Conditions { get; set; } = new();
}
internal sealed class BranchContextCondition
{
    [JsonPropertyName("expression")] public string Expression { get; set; } = "";
    [JsonPropertyName("line")] public int Line { get; set; }
    [JsonPropertyName("end_line")] public int EndLine { get; set; }
    [JsonPropertyName("outcome")] public string Outcome { get; set; } = "";
}
// Independent optional evidence: unavailable control-flow analysis need not prevent
// bounded lexical occurrence accounting. No semantic model or read/write inference.
internal sealed class ParameterOccurrenceReport
{
    [JsonPropertyName("version")] public string Version { get; set; } = "vb-parameter-occurrences-v1";
    [JsonPropertyName("status")] public string Status { get; set; } = "unknown";
    [JsonPropertyName("reason")] public string? Reason { get; set; }
    [JsonPropertyName("scanned_body_tokens")] public int ScannedBodyTokens { get; set; }
    [JsonPropertyName("parameters")] public List<ParameterOccurrenceDto> Parameters { get; set; } = new();
    [JsonPropertyName("scope")] public string Scope { get; set; } = "VB body IdentifierToken ValueText matches, ordinal case-insensitive; declaration/signature/defaults, comments and literal text excluded. Captures, writes, ByRef arguments, NameOf and member-name collisions count conservatively. A positive count is not a bound parameter reference or read; zero is not runtime-unused proof. No compiler binding, overload or execution claim.";
}
internal sealed class ParameterOccurrenceDto
{
    [JsonPropertyName("identifier")] public string Identifier { get; set; } = "";
    [JsonPropertyName("declaration_line")] public int DeclarationLine { get; set; }
    [JsonPropertyName("body_identifier_occurrences")] public int BodyIdentifierOccurrences { get; set; }
}
internal sealed class ReturnPathCaps
{
    [JsonPropertyName("max_source_bytes")] public int MaxSourceBytes { get; set; } = 2 * 1024 * 1024;
    [JsonPropertyName("max_expression_bytes")] public int MaxExpressionBytes { get; set; } = 1024;
    [JsonPropertyName("max_response_bytes")] public int MaxResponseBytes { get; set; } = 64 * 1024;
    [JsonPropertyName("max_paths")] public int MaxPaths { get; set; } = 64;
    [JsonPropertyName("max_returns")] public int MaxReturns { get; set; } = 32;
    [JsonPropertyName("max_nesting")] public int MaxNesting { get; set; } = 16;
}
internal sealed class ReturnPathIssue
{
    [JsonPropertyName("kind")] public string Kind { get; set; } = "";
    [JsonPropertyName("line")] public int Line { get; set; }
    [JsonPropertyName("reason")] public string Reason { get; set; } = "";
}
internal sealed class ReturnConditionDto
{
    [JsonPropertyName("kind")] public string Kind { get; set; } = "";
    [JsonPropertyName("expression")] public string Expression { get; set; } = "";
    [JsonPropertyName("line")] public int Line { get; set; }
    [JsonPropertyName("outcome")] public string Outcome { get; set; } = "";
    [JsonPropertyName("case_expression")] public string? CaseExpression { get; set; }
    [JsonPropertyName("case_line")] public int? CaseLine { get; set; }
}
internal sealed class ReturnPathDto
{
    [JsonPropertyName("conditions")] public List<ReturnConditionDto> Conditions { get; set; } = new();
    [JsonPropertyName("return_line")] public int ReturnLine { get; set; }
    [JsonPropertyName("return_expression")] public string ReturnExpression { get; set; } = "";
    [JsonPropertyName("normal_completion_required")] public bool NormalCompletionRequired { get; set; } = true;
}

/// <summary>Fresh-source structural enumeration. It never consults cached trees or semantic models.</summary>
internal sealed class ReturnPathAnalyzer
{
    private readonly string source;
    private readonly ReturnPathReport report;
    private readonly HashSet<int> returns = new();
    private ReturnPathAnalyzer(string source, int start, int end, string requestId)
    {
        this.source = source;
        report = new ReturnPathReport
        {
            RequestId = requestId,
            SourceSha256 = Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(source))).ToLowerInvariant(),
            MethodStartLine = start,
            MethodEndLine = end
        };
    }
    public static ReturnPathReport Analyze(string source, int start, int end, string? requestId)
    {
        if (string.IsNullOrEmpty(requestId) || Encoding.UTF8.GetByteCount(requestId) > 256) throw new ArgumentException("request_id must contain 1..256 UTF-8 bytes");
        var analyzer = new ReturnPathAnalyzer(source, start, end, requestId);
        var result = analyzer.Run(start, end);
        // Optional evidence cannot consume the existing return-path response budget.
        // Missing metadata is explicitly unknown to consumers, never an empty inventory.
        if (result.BranchContexts != null && System.Text.Json.JsonSerializer.SerializeToUtf8Bytes(result, AppJsonContext.Default.ReturnPathReport).Length > 64 * 1024)
            result.BranchContexts = null;
        if (result.ParameterOccurrences != null && System.Text.Json.JsonSerializer.SerializeToUtf8Bytes(result, AppJsonContext.Default.ReturnPathReport).Length > 64 * 1024)
            result.ParameterOccurrences = null;
        return result;
    }
    private int Line(SyntaxNode node) => node.GetLocation().GetLineSpan().StartLinePosition.Line + 1;
    private string Exact(SyntaxNode node) => source.Substring(node.Span.Start, node.Span.Length);
    private void Issue(string kind, int line, string reason)
    {
        if (report.Unsupported.Count < 16) report.Unsupported.Add(new() { Kind = kind, Line = line, Reason = reason });
        else report.UnsupportedOmitted++;
    }
    private ReturnPathReport Run(int start, int end)
    {
        if (Encoding.UTF8.GetByteCount(source) > 2 * 1024 * 1024)
        {
            Issue("source_budget", start, "Source exceeds 2 MiB parse budget.");
            return report;
        }
        if (start < 1 || end < start)
        {
            Issue("method_range", start, "Exact positive declaration/end line range required.");
            return report;
        }
        // A UTF-8 BOM decoded into a string is not VB syntax. Replace only the
        // leading marker for parsing, preserving every character offset and line.
        // Fingerprints and Exact() continue to use the original supplied source.
        var parseSource = source.StartsWith("\uFEFF", StringComparison.Ordinal)
            ? " " + source.Substring(1) : source;
        var tree = VisualBasicSyntaxTree.ParseText(parseSource);
        var root = tree.GetCompilationUnitRoot();
        // Reject parse errors, even outside the requested method: parser recovery
        // can change declaration ownership. This is intentionally conservative.
        if (tree.GetDiagnostics().Any(d => d.Severity == DiagnosticSeverity.Error))
        {
            Issue("parse_error", start, "Supplied file has syntax errors; recovered method boundaries are not trusted.");
            return report;
        }
        var candidates = root.DescendantNodes().OfType<MethodBlockSyntax>()
            .Where(m => Line(m.SubOrFunctionStatement) == start && m.GetLocation().GetLineSpan().EndLinePosition.Line + 1 == end)
            .ToList();
        if (candidates.Count != 1)
        {
            Issue("method_range", start, "Range must select exactly one complete Sub/Function declaration, not a nested expression or partial member.");
            return report;
        }
        var method = candidates[0];
        foreach (var trivia in root.DescendantTrivia(descendIntoTrivia: true))
        {
            var kind = trivia.Kind().ToString();
            if (kind is "IfDirectiveTrivia" or "ElseIfDirectiveTrivia" or "ElseDirectiveTrivia" or "EndIfDirectiveTrivia")
                Issue("conditional_compilation", start, "Conditional compilation in supplied file is outside this syntax-only slice.");
        }
        report.ParameterOccurrences = !root.DescendantTrivia(descendIntoTrivia: true).Any(t => t.GetStructure() is DirectiveTriviaSyntax)
            ? InventoryParameters(method)
            : new ParameterOccurrenceReport { Reason = "file_directives" };
        report.BranchContexts = InventoryBranches(method,
            root.DescendantTrivia(descendIntoTrivia: true).Any(t => t.GetStructure() is DirectiveTriviaSyntax));
        if (method.SubOrFunctionStatement.Modifiers.Any(t => t.IsKind(SyntaxKind.AsyncKeyword) || t.IsKind(SyntaxKind.IteratorKeyword)))
            Issue("deferred_member", start, "Async/iterator method outcomes are unsupported.");
        foreach (var node in method.DescendantNodes())
        {
            if (node.Kind().ToString().Contains("LambdaExpression", StringComparison.Ordinal) || node is AwaitExpressionSyntax)
                Issue("deferred_expression", Line(node), "Lambda/await expressions make the whole selected method unavailable; nested returns are never caller returns.");
        }
        Validate(method.Statements, 0);
        if (report.Unsupported.Count != 0) return report;
        try
        {
            var remaining = Walk(method.Statements, new() { new() }, 0);
            CheckCount(remaining.Count);
            report.NormalFallthroughPaths = remaining.Count;
            report.StructuralComplete = true;
            report.Status = "available";
            if (report.BranchContexts != null && System.Text.Json.JsonSerializer.SerializeToUtf8Bytes(report, AppJsonContext.Default.ReturnPathReport).Length > 64 * 1024)
                report.BranchContexts = null;
            if (report.ParameterOccurrences != null && System.Text.Json.JsonSerializer.SerializeToUtf8Bytes(report, AppJsonContext.Default.ReturnPathReport).Length > 64 * 1024)
                report.ParameterOccurrences = null;
            if (System.Text.Json.JsonSerializer.SerializeToUtf8Bytes(report, AppJsonContext.Default.ReturnPathReport).Length > 64 * 1024)
            {
                report.Paths.Clear(); report.NormalFallthroughPaths = 0;
                report.Status = "unavailable"; report.StructuralComplete = false;
                Issue("response_budget", start, "Serialized return_paths exceeds 64 KiB; all paths discarded.");
            }
        }
        catch (PathBudgetException)
        {
            report.Paths.Clear();
            report.NormalFallthroughPaths = 0;
            Issue("path_budget", start, "Exceeded 64 structural paths; all partial paths discarded.");
        }
        return report;
    }
    private BranchContextReport InventoryBranches(MethodBlockSyntax method, bool directives)
    {
        BranchContextReport Empty(string reason) => new() { Reason = reason,
            SourceSha256 = report.SourceSha256, MethodStartLine = report.MethodStartLine,
            MethodEndLine = report.MethodEndLine };
        var inventory = Empty("");
        if (directives) return Empty("file_directives");
        int EndLine(SyntaxNode n) => n.GetLocation().GetLineSpan().EndLinePosition.Line + 1;
        bool Lambda(SyntaxNode n) => n.Kind().ToString().Contains("LambdaExpression", StringComparison.Ordinal);
        var excluded = new HashSet<int>();
        foreach (var lambda in method.DescendantNodes().Where(Lambda))
        {
            if (EndLine(lambda) - Line(lambda) > 1024) return Empty("unavailable_line_budget");
            for (int line = Line(lambda); line <= EndLine(lambda); line++) excluded.Add(line);
        }
        foreach (var node in method.DescendantNodes().OfType<StatementSyntax>())
        {
            if (node is MethodStatementSyntax || node is EndBlockStatementSyntax ||
                node.Kind().ToString().EndsWith("Block", StringComparison.Ordinal) ||
                node.Ancestors().TakeWhile(a => a != method).Any(Lambda)) continue;
            // A single-line If is a header plus separate child statements; the
            // shared source line is excluded below rather than bound arbitrarily.
            var spanNode = node is SingleLineIfStatementSyntax si ? (SyntaxNode)si.Condition : node;
            var entry = new BranchContextEntry { StatementKind = node.Kind().ToString(),
                StartLine = Line(spanNode), EndLine = EndLine(spanNode) };
            foreach (var ancestor in node.Ancestors().TakeWhile(a => a != method).Reverse())
            {
                void Add(ExpressionSyntax expression, bool outcome)
                {
                    entry.Conditions.Add(new() { Expression = Exact(expression), Line = Line(expression),
                        EndLine = EndLine(expression), Outcome = outcome ? "true" : "false" });
                }
                if (ancestor is MultiLineIfBlockSyntax block)
                {
                    var child = node.AncestorsAndSelf().First(n => n.Parent == block);
                    if (child == block.IfStatement || child == block.EndIfStatement) continue;
                    if (child is ElseIfBlockSyntax elseif)
                    {
                        Add(block.IfStatement.Condition, false);
                        foreach (var previous in block.ElseIfBlocks)
                        {
                            if (previous == elseif) break;
                            Add(previous.ElseIfStatement.Condition, false);
                        }
                        if (node != elseif.ElseIfStatement) Add(elseif.ElseIfStatement.Condition, true);
                    }
                    else if (child is ElseBlockSyntax)
                    {
                        Add(block.IfStatement.Condition, false);
                        foreach (var previous in block.ElseIfBlocks) Add(previous.ElseIfStatement.Condition, false);
                    }
                    else Add(block.IfStatement.Condition, true);
                }
                else if (ancestor is SingleLineIfStatementSyntax single)
                {
                    bool inElse = single.ElseClause != null && node.AncestorsAndSelf().Contains(single.ElseClause);
                    Add(single.Condition, !inElse);
                }
            }
            if (entry.Conditions.Count > 16 || entry.Conditions.Any(c => Encoding.UTF8.GetByteCount(c.Expression) > 1024))
                return Empty("condition_budget");
            if (entry.EndLine - entry.StartLine > 1024) return Empty("statement_line_budget");
            inventory.Entries.Add(entry);
            if (inventory.Entries.Count > 256) return Empty("entry_budget");
        }
        var owners = new Dictionary<int, int>();
        foreach (var entry in inventory.Entries)
            for (int line = entry.StartLine; line <= entry.EndLine; line++)
                owners[line] = owners.GetValueOrDefault(line) + 1;
        foreach (var pair in owners.Where(p => p.Value > 1)) excluded.Add(pair.Key);
        if (excluded.Count > 1024) return Empty("unavailable_line_budget");
        inventory.Entries.RemoveAll(e => Enumerable.Range(e.StartLine, e.EndLine - e.StartLine + 1).Any(excluded.Contains));
        inventory.UnavailableLines = excluded.OrderBy(n => n).ToList();
        inventory.Status = "available"; inventory.Reason = null;
        // Serialize through the already registered report graph, without adding
        // another source-generation root or modifying the protocol transport.
        var wrapper = new ReturnPathReport { BranchContexts = inventory };
        if (System.Text.Json.JsonSerializer.SerializeToUtf8Bytes(wrapper, AppJsonContext.Default.ReturnPathReport).Length > 24 * 1024)
            return Empty("inventory_response_budget");
        return inventory;
    }
    private ParameterOccurrenceReport InventoryParameters(MethodBlockSyntax method)
    {
        var inventory = new ParameterOccurrenceReport();
        var declarations = method.SubOrFunctionStatement.ParameterList?.Parameters;
        if (declarations is null) { inventory.Status = "available"; return inventory; }
        if (declarations.Value.Count > 64) { inventory.Reason = "parameter_budget_64"; return inventory; }
        var byName = new Dictionary<string, ParameterOccurrenceDto>(StringComparer.OrdinalIgnoreCase);
        foreach (var declaration in declarations.Value)
        {
            var token = declaration.Identifier.Identifier;
            var name = token.ValueText;
            if (name.Length == 0 || Encoding.UTF8.GetByteCount(name) > 128 || byName.ContainsKey(name))
                return new ParameterOccurrenceReport { Reason = "ambiguous_or_oversized_parameter_identifier" };
            var item = new ParameterOccurrenceDto { Identifier = name,
                DeclarationLine = token.GetLocation().GetLineSpan().StartLinePosition.Line + 1 };
            byName.Add(name, item);
            inventory.Parameters.Add(item);
        }
        // Walk the complete body, including deferred nested expressions. Treating a
        // capture as absent would be unsound; treating a member collision as a read
        // would also be unsound. Only literal IdentifierToken occurrences are counted.
        foreach (var statement in method.Statements)
        foreach (var token in statement.DescendantTokens(descendIntoTrivia: false))
        {
            if (++inventory.ScannedBodyTokens > 100_000)
                return new ParameterOccurrenceReport { Reason = "body_token_budget_100000" };
            if (token.IsKind(SyntaxKind.IdentifierToken) && byName.TryGetValue(token.ValueText, out var item))
                item.BodyIdentifierOccurrences++;
        }
        inventory.Status = "available";
        return inventory;
    }
    private void ExpressionBudget(SyntaxNode node)
    {
        if (Encoding.UTF8.GetByteCount(Exact(node)) > 1024)
            Issue("expression_budget", Line(node), "Exact expression exceeds 1024 UTF-8 bytes; no clipped evidence supplied.");
    }
    private void Validate(SyntaxList<StatementSyntax> statements, int depth)
    {
        if (depth > 16) { Issue("nesting_budget", statements.Count > 0 ? Line(statements[0]) : report.MethodStartLine, "Exceeded 16 nested control blocks."); return; }
        foreach (var statement in statements)
        {
            switch (statement)
            {
                case SingleLineIfStatementSyntax single:
                    ExpressionBudget(single.Condition);
                    Validate(single.Statements, depth + 1);
                    if (single.ElseClause is { } singleElse) Validate(singleElse.Statements, depth + 1);
                    break;
                case MultiLineIfBlockSyntax conditional:
                    ExpressionBudget(conditional.IfStatement.Condition);
                    foreach (var branch in conditional.ElseIfBlocks) ExpressionBudget(branch.ElseIfStatement.Condition);
                    Validate(conditional.Statements, depth + 1);
                    foreach (var branch in conditional.ElseIfBlocks) Validate(branch.Statements, depth + 1);
                    if (conditional.ElseBlock is { } otherwise) Validate(otherwise.Statements, depth + 1);
                    break;
                case SelectBlockSyntax select:
                    ExpressionBudget(select.SelectStatement.Expression);
                    foreach (var branch in select.CaseBlocks)
                    {
                        ExpressionBudget(branch.CaseStatement);
                        foreach (var clause in branch.CaseStatement.Cases)
                            if (clause is not ElseCaseClauseSyntax && (clause is not SimpleCaseClauseSyntax simple || !SimpleLabel(simple.Value)))
                                Issue("case_pattern", Line(clause), "Only exact simple literal/identifier/member case labels are supported; ranges and relational patterns are unavailable.");
                        Validate(branch.Statements, depth + 1);
                    }
                    break;
                case ReturnStatementSyntax ret:
                    if (ret.Expression is { } expression) ExpressionBudget(expression);
                    returns.Add(ret.SpanStart);
                    if (returns.Count > 32) Issue("return_budget", Line(ret), "Exceeded 32 distinct explicit return sites.");
                    break;
                case LocalDeclarationStatementSyntax:
                case AssignmentStatementSyntax:
                case ExpressionStatementSyntax:
                case CallStatementSyntax:
                case EmptyStatementSyntax:
                    break;
                default:
                    Issue("unsupported_statement", Line(statement), statement.Kind() + " is outside supported straight-line/If/Select/Return syntax.");
                    break;
            }
        }
    }
    private static bool SimpleLabel(ExpressionSyntax value) => value is LiteralExpressionSyntax or IdentifierNameSyntax
        || value is MemberAccessExpressionSyntax member && member.Expression is not null && SimpleLabel(member.Expression);
    private void CheckCount(int active)
    {
        if (active + report.Paths.Count > 64) throw new PathBudgetException();
    }
    private static List<List<ReturnConditionDto>> Add(List<List<ReturnConditionDto>> paths, ReturnConditionDto condition)
        => paths.Select(path => { var copy = new List<ReturnConditionDto>(path); copy.Add(condition); return copy; }).ToList();
    private List<List<ReturnConditionDto>> Walk(SyntaxList<StatementSyntax> statements, List<List<ReturnConditionDto>> active, int depth)
    {
        foreach (var statement in statements)
        {
            if (active.Count == 0) break;
            switch (statement)
            {
                case ReturnStatementSyntax ret:
                    foreach (var path in active) report.Paths.Add(new() { Conditions = path, ReturnLine = Line(ret), ReturnExpression = ret.Expression is null ? "" : Exact(ret.Expression) });
                    active = new();
                    break;
                case SingleLineIfStatementSyntax single:
                    var yesSingle = Add(active, new() { Kind = "if", Expression = Exact(single.Condition), Line = Line(single.Condition), Outcome = "true" });
                    var noSingle = Add(active, new() { Kind = "if", Expression = Exact(single.Condition), Line = Line(single.Condition), Outcome = "false" });
                    var joinedSingle = Walk(single.Statements, yesSingle, depth + 1);
                    joinedSingle.AddRange(single.ElseClause is { } singleElse
                        ? Walk(singleElse.Statements, noSingle, depth + 1) : noSingle);
                    active = joinedSingle;
                    break;
                case MultiLineIfBlockSyntax conditional:
                    var left = active;
                    var joined = new List<List<ReturnConditionDto>>();
                    void Branch(ExpressionSyntax expression, SyntaxList<StatementSyntax> body)
                    {
                        var yes = Add(left, new() { Kind = "if", Expression = Exact(expression), Line = Line(expression), Outcome = "true" });
                        joined.AddRange(Walk(body, yes, depth + 1));
                        left = Add(left, new() { Kind = "if", Expression = Exact(expression), Line = Line(expression), Outcome = "false" });
                        CheckCount(joined.Count + left.Count);
                    }
                    Branch(conditional.IfStatement.Condition, conditional.Statements);
                    foreach (var branch in conditional.ElseIfBlocks) Branch(branch.ElseIfStatement.Condition, branch.Statements);
                    joined.AddRange(conditional.ElseBlock is { } otherwise ? Walk(otherwise.Statements, left, depth + 1) : left);
                    active = joined;
                    break;
                case SelectBlockSyntax select:
                    var unmatched = active;
                    var alternatives = new List<List<ReturnConditionDto>>();
                    var selector = select.SelectStatement.Expression;
                    foreach (var branch in select.CaseBlocks)
                    {
                        if (branch.CaseStatement.Cases.Any(c => c is ElseCaseClauseSyntax))
                        {
                            alternatives.AddRange(Walk(branch.Statements, unmatched, depth + 1));
                            unmatched = new();
                        }
                        else
                        {
                            // A list means one case-group match, not independent
                            // sequential evaluations. Preserve the exact label list.
                            var labels = source.Substring(branch.CaseStatement.Cases.Span.Start, branch.CaseStatement.Cases.Span.Length);
                            ReturnConditionDto Condition(string outcome) => new() { Kind = "case", Expression = Exact(selector), Line = Line(selector), Outcome = outcome, CaseExpression = labels, CaseLine = Line(branch.CaseStatement) };
                            alternatives.AddRange(Walk(branch.Statements, Add(unmatched, Condition("match")), depth + 1));
                            unmatched = Add(unmatched, Condition("no_match"));
                        }
                        CheckCount(alternatives.Count + unmatched.Count);
                    }
                    alternatives.AddRange(unmatched);
                    active = alternatives;
                    break;
            }
            CheckCount(active.Count);
        }
        return active;
    }
    private sealed class PathBudgetException : Exception { }
}
