using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.Operations;
using Microsoft.CodeAnalysis.VisualBasic;
using Microsoft.CodeAnalysis.VisualBasic.Syntax;

// Same authored-syntax contract as engram_core::value_flow. The Rust graph
// post-pass owns reaching definitions and cross-method admission.
internal sealed class FlowSiteDto
{
    [JsonPropertyName("offset")] public int Offset { get; set; }
    [JsonPropertyName("end_offset")] public int EndOffset { get; set; }
    [JsonPropertyName("line")] public int Line { get; set; }
    [JsonPropertyName("column")] public int Column { get; set; }
    [JsonPropertyName("text")] public string Text { get; set; } = "";
    [JsonPropertyName("text_truncated")] public bool TextTruncated { get; set; }
}
internal sealed class FlowParameterDto
{
    [JsonPropertyName("name")] public string Name { get; set; } = "";
    [JsonPropertyName("index")] public int Index { get; set; }
    [JsonPropertyName("site")] public FlowSiteDto Site { get; set; } = new();
    [JsonPropertyName("by_ref")] public bool ByRef { get; set; }
    [JsonPropertyName("variadic")] public bool Variadic { get; set; }
    [JsonPropertyName("optional")] public bool Optional { get; set; }
}
internal sealed class FlowArgumentDto
{
    [JsonPropertyName("index")] public int Index { get; set; }
    [JsonPropertyName("parameter_index")] public int? ParameterIndex { get; set; }
    [JsonPropertyName("name")] public string? Name { get; set; }
    [JsonPropertyName("by_ref")] public bool ByRef { get; set; }
    [JsonPropertyName("site")] public FlowSiteDto Site { get; set; } = new();
    [JsonPropertyName("value")] public FlowExpressionDto Value { get; set; } = new();
}
internal sealed class FlowExpressionDto
{
    [JsonPropertyName("kind")] public string Kind { get; set; } = "unknown";
    [JsonPropertyName("site")] public FlowSiteDto Site { get; set; } = new();
    [JsonPropertyName("name")] public string? Name { get; set; }
    [JsonPropertyName("operator")] public string? Operator { get; set; }
    [JsonPropertyName("children")] public List<FlowExpressionDto> Children { get; set; } = new();
    [JsonPropertyName("arguments")] public List<FlowArgumentDto> Arguments { get; set; } = new();
    [JsonPropertyName("target_semantic_id")] public string? TargetSemanticId { get; set; }
}
internal sealed class FlowStatementDto
{
    [JsonPropertyName("kind")] public string Kind { get; set; } = "unknown";
    [JsonPropertyName("site")] public FlowSiteDto Site { get; set; } = new();
    [JsonPropertyName("name")] public string? Name { get; set; }
    [JsonPropertyName("expression")] public FlowExpressionDto? Expression { get; set; }
    [JsonPropertyName("body")] public List<FlowStatementDto> Body { get; set; } = new();
    [JsonPropertyName("otherwise")] public List<FlowStatementDto> Otherwise { get; set; } = new();
}
internal sealed class FlowProgramDto
{
    [JsonPropertyName("version")] public int Version { get; set; } = 1;
    [JsonPropertyName("language")] public string Language { get; set; } = "vb";
    [JsonPropertyName("offset_unit")] public string OffsetUnit { get; set; } = "utf16_code_units";
    [JsonPropertyName("parameters")] public List<FlowParameterDto> Parameters { get; set; } = new();
    [JsonPropertyName("body")] public FlowStatementDto Body { get; set; } = new();
    [JsonPropertyName("issues")] public List<string> Issues { get; set; } = new();
}

internal sealed class ValueFlowEmitter
{
    private const int MaxNodes = 4096, MaxDepth = 32, MaxText = 512;
    private static readonly AppJsonContext FlowJson = new(new JsonSerializerOptions
    { MaxDepth = 128, DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull });
    private int remaining = MaxNodes;
    private readonly SemanticModel model;
    private readonly Func<ISymbol, string> semanticId;
    private readonly List<string> issues = new();
    private ValueFlowEmitter(SemanticModel model, Func<ISymbol, string> semanticId)
    { this.model = model; this.semanticId = semanticId; }

    private void Issue(string message)
    { if (issues.Count < 32 && !issues.Contains(message)) issues.Add(message); }
    private bool Admit(int depth)
    {
        if (remaining == 0 || depth >= MaxDepth)
        { Issue("Authored value syntax exceeds the depth/node budget; lineage is incomplete."); return false; }
        remaining--; return true;
    }
    private static FlowSiteDto Site(SyntaxNode node)
    {
        var position = node.SyntaxTree.GetLineSpan(node.Span).StartLinePosition;
        var text = node.ToString();
        return new() { Offset = node.SpanStart, EndOffset = node.Span.End,
            Line = position.Line + 1, Column = position.Character + 1,
            Text = string.Concat(text.EnumerateRunes().Take(MaxText).Select(r => r.ToString())),
            TextTruncated = text.EnumerateRunes().Count() > MaxText };
    }
    private FlowStatementDto Block(SyntaxNode owner, IEnumerable<StatementSyntax> statements, int depth)
    {
        var block = new FlowStatementDto { Kind = "block", Site = Site(owner) };
        foreach (var statement in statements)
        {
            block.Body.Add(Statement(statement, depth + 1));
            if (remaining == 0) { Issue("Authored value syntax reaches the node budget; remaining statements may be omitted."); break; }
        }
        return block;
    }
    private FlowStatementDto Statement(StatementSyntax statement, int depth)
    {
        var result = new FlowStatementDto { Site = Site(statement) };
        if (!Admit(depth)) return result;
        switch (statement)
        {
            case LocalDeclarationStatementSyntax local:
                result.Kind = "sequence";
                foreach (var declarator in local.Declarators)
                    foreach (var name in declarator.Names)
                    {
                        if (!Admit(depth + 1)) { result.Kind = "unknown"; return result; }
                        var initializer = declarator.Initializer?.Value;
                        if (declarator.AsClause is AsNewClauseSyntax asNew) initializer = asNew.NewExpression;
                        result.Body.Add(new() { Kind = "declare", Site = Site(name), Name = name.Identifier.ValueText,
                            Expression = initializer is null ? null : Expression(initializer, depth + 2) });
                    }
                break;
            case AssignmentStatementSyntax assignment:
                result.Kind = "expression";
                result.Expression = new() { Kind = "assign", Site = Site(assignment), Operator = assignment.OperatorToken.Text,
                    Children = new() { Expression(assignment.Left, depth + 1), Expression(assignment.Right, depth + 1) } };
                break;
            case ReturnStatementSyntax returned:
                result.Kind = "return";
                if (returned.Expression is { } value) result.Expression = Expression(value, depth + 1);
                break;
            case ThrowStatementSyntax thrown:
                result.Kind = "throw";
                if (thrown.Expression is { } thrownValue) result.Expression = Expression(thrownValue, depth + 1);
                break;
            case ExpressionStatementSyntax expression:
                result.Kind = "expression"; result.Expression = Expression(expression.Expression, depth + 1); break;
            case CallStatementSyntax call:
                result.Kind = "expression"; result.Expression = Expression(call.Invocation, depth + 1); break;
            case MultiLineIfBlockSyntax conditional:
                result.Kind = "if"; result.Expression = Expression(conditional.IfStatement.Condition, depth + 1);
                result.Body.Add(Block(conditional, conditional.Statements, depth + 1));
                var tail = result;
                var branchDepth = depth;
                foreach (var branch in conditional.ElseIfBlocks)
                {
                    if (!Admit(++branchDepth)) { tail.Otherwise.Add(new() { Site = Site(branch) }); return result; }
                    var next = new FlowStatementDto { Kind = "if", Site = Site(branch), Expression = Expression(branch.ElseIfStatement.Condition, branchDepth + 1) };
                    next.Body.Add(Block(branch, branch.Statements, branchDepth + 1));
                    tail.Otherwise.Add(next); tail = next;
                }
                if (conditional.ElseBlock is { } other) tail.Otherwise.Add(Block(other, other.Statements, depth + 1));
                break;
            case SingleLineIfStatementSyntax conditional:
                result.Kind = "if"; result.Expression = Expression(conditional.Condition, depth + 1);
                result.Body.Add(Block(conditional, conditional.Statements, depth + 1));
                if (conditional.ElseClause is { } singleElse) result.Otherwise.Add(Block(singleElse, singleElse.Statements, depth + 1));
                break;
            case ExitStatementSyntax exit when exit.IsKind(SyntaxKind.ExitFunctionStatement) || exit.IsKind(SyntaxKind.ExitSubStatement):
                result.Kind = "return"; break;
            case EmptyStatementSyntax:
                result.Kind = "empty"; break;
            default:
                Issue($"Statement {statement.Kind()} is not modeled; reaching values after it are incomplete."); break;
        }
        return result;
    }
    private FlowExpressionDto Expression(ExpressionSyntax expression, int depth)
    {
        var result = new FlowExpressionDto { Site = Site(expression) };
        if (!Admit(depth)) return result;
        switch (expression)
        {
            case IdentifierNameSyntax name:
                result.Kind = "name"; result.Name = name.Identifier.ValueText; break;
            case LiteralExpressionSyntax:
                result.Kind = "literal"; break;
            case ParenthesizedExpressionSyntax parens:
                result.Kind = "operation"; result.Operator = "parentheses";
                result.Children.Add(Expression(parens.Expression, depth + 1)); break;
            case AwaitExpressionSyntax awaited:
                result.Kind = "operation"; result.Operator = "await";
                result.Children.Add(Expression(awaited.Expression, depth + 1)); break;
            case CTypeExpressionSyntax cast:
                result.Kind = "operation"; result.Operator = "cast";
                result.Children.Add(Expression(cast.Expression, depth + 1)); break;
            case DirectCastExpressionSyntax cast:
                result.Kind = "operation"; result.Operator = "cast";
                result.Children.Add(Expression(cast.Expression, depth + 1)); break;
            case TryCastExpressionSyntax cast:
                result.Kind = "operation"; result.Operator = "cast";
                result.Children.Add(Expression(cast.Expression, depth + 1)); break;
            case PredefinedCastExpressionSyntax cast:
                result.Kind = "operation"; result.Operator = "cast";
                result.Children.Add(Expression(cast.Expression, depth + 1)); break;
            case BinaryExpressionSyntax binary:
                result.Kind = "operation"; result.Operator = binary.OperatorToken.Text;
                result.Children.Add(Expression(binary.Left, depth + 1)); result.Children.Add(Expression(binary.Right, depth + 1)); break;
            case UnaryExpressionSyntax unary:
                result.Kind = "operation"; result.Operator = unary.OperatorToken.Text;
                result.Children.Add(Expression(unary.Operand, depth + 1)); break;
            case TernaryConditionalExpressionSyntax conditional:
                result.Kind = "conditional";
                result.Children.Add(Expression(conditional.Condition, depth + 1));
                result.Children.Add(Expression(conditional.WhenTrue, depth + 1));
                result.Children.Add(Expression(conditional.WhenFalse, depth + 1)); break;
            case BinaryConditionalExpressionSyntax conditional:
                result.Kind = "operation"; result.Operator = "??";
                result.Children.Add(Expression(conditional.FirstExpression, depth + 1));
                result.Children.Add(Expression(conditional.SecondExpression, depth + 1)); break;
            case MemberAccessExpressionSyntax member:
                result.Kind = "member"; result.Name = member.Name.ToString();
                if (member.Expression is { } receiver) result.Children.Add(Expression(receiver, depth + 1));
                else Issue("Implicit With receiver is not modeled in value lineage.");
                break;
            case MeExpressionSyntax or MyBaseExpressionSyntax or MyClassExpressionSyntax:
                result.Kind = "receiver"; result.Name = expression.ToString(); break;
            case InvocationExpressionSyntax invocation:
                result.Kind = "call"; result.Name = invocation.Expression.ToString();
                var selected = model.GetSymbolInfo(invocation).Symbol as IMethodSymbol;
                if (selected is not null && selected.ReducedFrom is null)
                    result.TargetSemanticId = semanticId(selected.OriginalDefinition);
                else if (selected?.ReducedFrom is not null)
                    Issue("Reduced extension-method parameter mapping is not modeled.");
                if (invocation.Expression is MemberAccessExpressionSyntax access && access.Expression is { } callReceiver)
                    result.Children.Add(Expression(callReceiver, depth + 1));
                var operation = model.GetOperation(invocation) as IInvocationOperation;
                var index = 0;
                foreach (var argument in invocation.ArgumentList?.Arguments ?? default)
                {
                    if (!Admit(depth + 1)) break;
                    var mapping = operation?.Arguments.FirstOrDefault(a => !a.IsImplicit && a.Syntax.Span == argument.Span)?.Parameter;
                    var simple = argument as SimpleArgumentSyntax;
                    result.Arguments.Add(new() { Index = index++, ParameterIndex = mapping?.Ordinal,
                        Name = simple?.NameColonEquals?.Name.Identifier.ValueText,
                        ByRef = mapping is not null && mapping.RefKind != RefKind.None, Site = Site(argument),
                        Value = simple is null ? new() { Site = Site(argument) } : Expression(simple.Expression, depth + 2) });
                    if (simple is null) Issue("Omitted/unsupported call argument has no authored value expression.");
                }
                break;
            default:
                Issue($"Expression {expression.Kind()} is not modeled; value lineage is incomplete."); break;
        }
        return result;
    }

    public static string Extract(SyntaxNode node, MethodBaseSyntax declaration, SemanticModel model, Func<ISymbol, string> semanticId)
    {
        var emitter = new ValueFlowEmitter(model, semanticId);
        var program = new FlowProgramDto();
        foreach (var parameter in declaration.ParameterList?.Parameters ?? default)
        {
            if (!emitter.Admit(0)) break;
            program.Parameters.Add(new() { Name = parameter.Identifier.Identifier.ValueText, Index = program.Parameters.Count,
                Site = Site(parameter.Identifier), ByRef = parameter.Modifiers.Any(m => m.IsKind(SyntaxKind.ByRefKeyword)),
                Variadic = parameter.Modifiers.Any(m => m.IsKind(SyntaxKind.ParamArrayKeyword)), Optional = parameter.Default is not null });
        }
        if (node.GetDiagnostics().Any(d => d.Severity == DiagnosticSeverity.Error))
        { emitter.Issue("Method contains syntax errors; value lineage is unavailable."); program.Body.Site = Site(node); }
        else if (node is MethodBlockBaseSyntax block) program.Body = emitter.Block(node, block.Statements, 0);
        else
        { emitter.Issue("Callable has no implementation body; only parameter definitions are available."); program.Body = new() { Kind = "empty", Site = Site(node) }; }
        program.Issues = emitter.issues;
        return JsonSerializer.Serialize(program, FlowJson.FlowProgramDto);
    }
}
