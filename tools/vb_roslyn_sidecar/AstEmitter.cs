using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.Text;
using Microsoft.CodeAnalysis.VisualBasic;
using Microsoft.CodeAnalysis.VisualBasic.Syntax;
using System.Text.RegularExpressions;

internal static class AstEmitter
{
    public static (List<SymbolDto>, List<EdgeDto>) Extract(string path, string source)
    {
        var tree = VisualBasicSyntaxTree.ParseText(SourceText.From(source), path: path);
        var compilation = VisualBasicCompilation.Create("sidecar").AddSyntaxTrees(tree);
        var model = compilation.GetSemanticModel(tree);
        var root = tree.GetCompilationUnitRoot();

        var symbols = new List<SymbolDto>();
        var edges = new List<EdgeDto>();
        var namespaces = new Stack<string>();
        var types = new Stack<string>();
        var knownControlNames = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        const string fileNode = "file";
        var isDesigner = path.EndsWith(".designer.vb", StringComparison.OrdinalIgnoreCase);
        var parseErrorCount = tree.GetDiagnostics().Count(d => d.Severity == DiagnosticSeverity.Error);
        symbols.Add(new SymbolDto
        {
            Name = "file_parse",
            Kind = "file",
            StartLine = 1,
            EndLine = 1,
            Metadata = new()
            {
                ["fqn"] = "file",
                ["parse_success"] = (parseErrorCount == 0).ToString().ToLowerInvariant(),
                ["parse_error_count"] = parseErrorCount.ToString(),
                ["is_designer"] = isDesigner.ToString().ToLowerInvariant()
            }
        });

        if (root.Options.Any())
        {
            var strict = root.Options.FirstOrDefault(o => o.Name.ToString().Equals("Strict", StringComparison.OrdinalIgnoreCase));
            if (strict is not null)
            {
                symbols.Add(new SymbolDto
                {
                    Name = "file_directives",
                    Kind = "file",
                    StartLine = 1,
                    EndLine = 1,
                    Metadata = new()
                    {
                        ["fqn"] = "file",
                        ["option_strict"] = strict.ValueKeyword.ToString(),
                        ["path"] = path ?? string.Empty,
                        ["is_designer"] = isDesigner.ToString().ToLowerInvariant()
                    }
                });
            }
        }

        foreach (var imp in root.Imports)
        {
            foreach (var clause in imp.ImportsClauses)
            {
                edges.Add(new EdgeDto
                {
                    SourceName = fileNode,
                    SourceKind = "file",
                    SourceStartLine = Line(tree, clause),
                    SourceLanguage = "vb",
                    TargetName = clause.ToString(),
                    TargetKind = "namespace",
                    Kind = "imports"
                });
            }
        }

        void Walk(SyntaxNode node)
        {
            switch (node)
            {
                case NamespaceBlockSyntax ns:
                {
                    var nsName = ns.NamespaceStatement.Name.ToString();
                    namespaces.Push(nsName);
                    foreach (var child in ns.Members) Walk(child);
                    namespaces.Pop();
                    return;
                }
                case ClassBlockSyntax cls:
                    EmitType(cls.ClassStatement.Identifier.ToString(), "class", cls);
                    return;
                case ModuleBlockSyntax mod:
                    EmitType(mod.ModuleStatement.Identifier.ToString(), "module", mod);
                    return;
                case StructureBlockSyntax st:
                    EmitType(st.StructureStatement.Identifier.ToString(), "struct", st);
                    return;
                case InterfaceBlockSyntax iface:
                    EmitType(iface.InterfaceStatement.Identifier.ToString(), "interface", iface);
                    return;
                case EnumBlockSyntax en:
                    EmitType(en.EnumStatement.Identifier.ToString(), "enum", en);
                    return;
                case MethodBlockSyntax m:
                    EmitMethod(m);
                    return;
                case PropertyBlockSyntax p:
                    EmitProperty(p);
                    return;
                case FieldDeclarationSyntax f:
                    EmitField(f);
                    return;
            }

            foreach (var child in node.ChildNodes()) Walk(child);
        }

        void EmitType(string name, string kind, SyntaxNode node)
        {
            var fqn = ComposeName(name);
            symbols.Add(new SymbolDto
            {
                Name = fqn,
                Kind = kind,
                StartLine = Line(tree, node),
                EndLine = EndLine(tree, node),
            });
            if (types.Count > 0)
            {
                edges.Add(Contains(types.Peek(), fqn, Line(tree, node), kind));
            }

            types.Push(fqn);
            foreach (var child in node.ChildNodes()) Walk(child);
            types.Pop();
        }

        void EmitMethod(MethodBlockSyntax node)
        {
            var stmt = node.SubOrFunctionStatement;
            var name = stmt.Identifier.Text;
            var fqn = ComposeName(name);
            var metadata = new Dictionary<string, string>();
            if (stmt.Modifiers.Any(m => m.Kind() == SyntaxKind.AsyncKeyword))
                metadata["async"] = "true";
            if (Lifecycle(name) is { } life)
            {
                metadata["lifecycle_stage"] = life.stage;
                metadata["lifecycle_sequence"] = life.seq;
            }
            var lateBindingCallCount = 0;
            var callByNameCount = 0;
            var objectVarCount = 0;
            var sideEffects = new HashSet<string>();

            symbols.Add(new SymbolDto
            {
                Name = fqn,
                Kind = "function",
                StartLine = Line(tree, node),
                EndLine = EndLine(tree, node),
                Metadata = null,
            });
            var methodSymbol = symbols[^1];
            if (types.Count > 0) edges.Add(Contains(types.Peek(), fqn, Line(tree, node), "function"));

            foreach (var hc in stmt.HandlesClause?.Events ?? new SeparatedSyntaxList<HandlesClauseItemSyntax>())
            {
                var txt = hc.ToString();
                var parts = txt.Split('.', 2);
                if (parts.Length == 2)
                {
                    edges.Add(new EdgeDto
                    {
                        SourceName = parts[0],
                        SourceKind = parts[0] is "Me" or "MyBase" ? "self" : "control",
                        SourceStartLine = Line(tree, hc),
                        SourceLanguage = "vb",
                        TargetName = name,
                        TargetKind = "function",
                        TargetStartLine = Line(tree, node),
                        Kind = "event_wiring",
                        Metadata = new() { ["fqn"] = fqn }
                    });
                }
            }

            foreach (var add in node.DescendantNodes().OfType<AddRemoveHandlerStatementSyntax>())
            {
                if (add.Kind() == SyntaxKind.AddHandlerStatement)
                {
                    var (eventSourceName, eventName) = ParseEventExpression(add.EventExpression);
                    var delegateName = ParseDelegateExpression(add.DelegateExpression);
                    edges.Add(new EdgeDto
                    {
                        SourceName = eventSourceName,
                        SourceKind = eventSourceName is "Me" or "MyBase" ? "self" : "control",
                        SourceStartLine = Line(tree, add),
                        SourceLanguage = "vb",
                        TargetName = delegateName,
                        Kind = "event_wiring",
                        Metadata = new()
                        {
                            ["wiring"] = "AddHandler",
                            ["fqn"] = fqn,
                            ["event"] = eventName
                        }
                    });
                }
            }

            foreach (var inv in node.DescendantNodes().OfType<InvocationExpressionSyntax>())
            {
                var targetName = ResolveInvocationName(inv);
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = Line(tree, inv),
                    SourceLanguage = "vb",
                    TargetName = targetName,
                    TargetKind = "function",
                    Kind = "calls",
                    Metadata = ResolveInvocationMetadata(inv)
                });

                if (IsSqlExecutionCall(targetName))
                {
                    sideEffects.Add("DB_Access");
                    edges.Add(new EdgeDto
                    {
                        SourceName = fqn,
                        SourceKind = "function",
                        SourceStartLine = Line(tree, inv),
                        SourceLanguage = "vb",
                        TargetName = "sql_execution",
                        TargetKind = "sql",
                        Kind = "sql_exec",
                        Metadata = new() { ["invocation"] = targetName }
                    });
                }

                if (TryExtractColumnName(inv, out var columnName))
                {
                    sideEffects.Add("DB_Access");
                    edges.Add(new EdgeDto
                    {
                        SourceName = fqn,
                        SourceKind = "function",
                        SourceStartLine = Line(tree, inv),
                        SourceLanguage = "vb",
                        TargetName = $"binding_field:{columnName}",
                        TargetKind = "binding_field",
                        Kind = "reads_column"
                    });
                }
                if (targetName.Contains("RegisterStartupScript", StringComparison.OrdinalIgnoreCase) ||
                    targetName.Contains("RegisterClientScriptBlock", StringComparison.OrdinalIgnoreCase))
                {
                    sideEffects.Add("UI_Mutation");
                    edges.Add(new EdgeDto
                    {
                        SourceName = fqn,
                        SourceKind = "function",
                        SourceStartLine = Line(tree, inv),
                        SourceLanguage = "vb",
                        TargetName = "script_runtime",
                        TargetKind = "script",
                        Kind = "injects_script"
                    });
                }

                if (targetName.Contains("CreateObject", StringComparison.OrdinalIgnoreCase) ||
                    targetName.Contains("GetObject", StringComparison.OrdinalIgnoreCase))
                {
                    lateBindingCallCount++;
                    var progId = GetFirstStringArgument(inv);
                    if (!string.IsNullOrWhiteSpace(progId))
                    {
                        var modernEquivalent = MapProgIdToModernEquivalent(progId);
                        edges.Add(new EdgeDto
                        {
                            SourceName = fqn,
                            SourceKind = "function",
                            SourceStartLine = Line(tree, inv),
                            SourceLanguage = "vb",
                            TargetName = progId,
                            TargetKind = "com_component",
                            Kind = "depends_on",
                            Metadata = new()
                            {
                                ["late_binding"] = "true",
                                ["modern_equivalent"] = modernEquivalent
                            }
                        });
                    }
                }
                if (targetName.Contains("CallByName", StringComparison.OrdinalIgnoreCase))
                {
                    lateBindingCallCount++;
                    callByNameCount++;
                }
            }

            foreach (var create in node.DescendantNodes().OfType<ObjectCreationExpressionSyntax>())
            {
                var typeText = create.Type.ToString();
                if (!typeText.Contains("Command", StringComparison.OrdinalIgnoreCase)) continue;
                var sqlArg = create.ArgumentList?.Arguments
                    .Select(GetArgumentExpression)
                    .Where(e => e is not null)
                    .Select(e => TryExtractStringLiteral(e!))
                    .FirstOrDefault(v => !string.IsNullOrWhiteSpace(v));
                if (string.IsNullOrWhiteSpace(sqlArg)) continue;
                if (!LooksLikeSql(sqlArg)) continue;
                sideEffects.Add("DB_Access");

                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = Line(tree, create),
                    SourceLanguage = "vb",
                    TargetName = "sql_query",
                    TargetKind = "sql",
                    Kind = "sql_calls",
                    Metadata = new()
                    {
                        ["sql_text"] = sqlArg,
                        ["classification"] = ClassifySql(sqlArg),
                        ["table"] = InferSqlTable(sqlArg)
                    }
                });
            }

            foreach (var assignment in node.DescendantNodes().OfType<AssignmentStatementSyntax>())
            {
                if (!assignment.Left.ToString().EndsWith(".CommandText", StringComparison.OrdinalIgnoreCase)) continue;
                var sql = TryExtractStringLiteral(assignment.Right);
                if (string.IsNullOrWhiteSpace(sql)) continue;
                sideEffects.Add("DB_Access");
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = Line(tree, assignment),
                    SourceLanguage = "vb",
                    TargetName = "sql_query",
                    TargetKind = "sql",
                    Kind = "sql_calls",
                    Metadata = new()
                    {
                        ["sql_text"] = sql,
                        ["classification"] = ClassifySql(sql),
                        ["table"] = InferSqlTable(sql)
                    }
                });
            }

            foreach (var local in node.DescendantNodes().OfType<LocalDeclarationStatementSyntax>())
            {
                var txt = local.ToString();
                if (!Regex.IsMatch(txt, @"\b(sql|query)\b", RegexOptions.IgnoreCase)) continue;
                var sql = TryExtractSqlFromExpressionText(txt);
                if (string.IsNullOrWhiteSpace(sql) || !LooksLikeSql(sql)) continue;
                sideEffects.Add("DB_Access");
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = Line(tree, local),
                    SourceLanguage = "vb",
                    TargetName = "sql_query",
                    TargetKind = "sql",
                    Kind = "sql_calls",
                    Metadata = new()
                    {
                        ["sql_text"] = sql,
                        ["classification"] = ClassifySql(sql),
                        ["table"] = InferSqlTable(sql),
                        ["source"] = "local_concat"
                    }
                });
            }

            foreach (var append in node.DescendantNodes().OfType<InvocationExpressionSyntax>())
            {
                var exprText = append.Expression.ToString();
                if (!exprText.EndsWith(".Append", StringComparison.OrdinalIgnoreCase) &&
                    !exprText.EndsWith(".AppendLine", StringComparison.OrdinalIgnoreCase)) continue;
                var frag = GetFirstStringArgument(append);
                if (string.IsNullOrWhiteSpace(frag) || !LooksLikeSql(frag)) continue;
                sideEffects.Add("DB_Access");
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = Line(tree, append),
                    SourceLanguage = "vb",
                    TargetName = "sql_query",
                    TargetKind = "sql",
                    Kind = "sql_calls",
                    Metadata = new()
                    {
                        ["sql_text"] = frag,
                        ["classification"] = ClassifySql(frag),
                        ["table"] = InferSqlTable(frag),
                        ["source"] = "stringbuilder_fragment"
                    }
                });
            }

            foreach (var localType in node.DescendantNodes().OfType<TypeBlockSyntax>()
                         .Where(t => !t.Ancestors().OfType<TypeBlockSyntax>().Any()))
            {
                Walk(localType);
            }

            foreach (var withBlock in node.DescendantNodes().OfType<WithBlockSyntax>())
            {
                var withTarget = withBlock.WithStatement.Expression.ToString();
                foreach (var statement in withBlock.Statements)
                {
                    var lines = statement.ToString().Split('\n');
                    foreach (var rawLine in lines)
                    {
                        var stmtText = rawLine.Trim();
                        if (!stmtText.StartsWith(".", StringComparison.Ordinal)) continue;
                        sideEffects.Add("State_Access");
                        var kind = stmtText.Contains("=", StringComparison.Ordinal) ? "writes_state" : "reads_state";
                        edges.Add(new EdgeDto
                        {
                            SourceName = fqn,
                            SourceKind = "function",
                            SourceStartLine = Line(tree, statement),
                            SourceLanguage = "vb",
                            TargetName = $"{withTarget}{stmtText}",
                            TargetKind = "member",
                            Kind = kind
                        });
                    }
                }
            }

            foreach (var member in node.DescendantNodes().OfType<MemberAccessExpressionSyntax>()
                         .Where(m => m.ToString().StartsWith("My.", StringComparison.OrdinalIgnoreCase)))
            {
                sideEffects.Add("State_Access");
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = Line(tree, member),
                    SourceLanguage = "vb",
                    TargetName = member.ToString(),
                    TargetKind = "state",
                    Kind = "reads_state"
                });
            }

            foreach (var redim in node.DescendantNodes().Where(n => n.Kind() == SyntaxKind.ReDimStatement))
            {
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = Line(tree, redim),
                    SourceLanguage = "vb",
                    TargetName = "ReDim",
                    Kind = "anti_pattern"
                });
            }

            foreach (var onError in node.DescendantNodes().Where(n => n.ToString().StartsWith("On Error", StringComparison.OrdinalIgnoreCase)))
            {
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = Line(tree, onError),
                    SourceLanguage = "vb",
                    TargetName = onError.ToString(),
                    Kind = "anti_pattern"
                });
            }

            objectVarCount += node.DescendantNodes().OfType<VariableDeclaratorSyntax>()
                .Count(v => v.AsClause?.ToString().Contains("As Object", StringComparison.OrdinalIgnoreCase) == true);
            var dynamicControls = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            foreach (var decl in node.DescendantNodes().OfType<LocalDeclarationStatementSyntax>())
            {
                if (!Regex.IsMatch(decl.ToString(), @"As\s+(Button|TextBox|DropDownList|GridView|Panel|Label|LinkButton)\b", RegexOptions.IgnoreCase))
                    continue;
                foreach (var d in decl.Declarators)
                {
                    foreach (var n in d.Names)
                    {
                        dynamicControls.Add(n.Identifier.Text);
                    }
                }
            }
            foreach (var addCall in node.DescendantNodes().OfType<InvocationExpressionSyntax>()
                         .Where(i => i.Expression.ToString().EndsWith(".Controls.Add", StringComparison.OrdinalIgnoreCase)))
            {
                var controlVar = addCall.ArgumentList?.Arguments.FirstOrDefault()?.ToString();
                if (string.IsNullOrWhiteSpace(controlVar) ||
                    (!dynamicControls.Contains(controlVar) && !knownControlNames.Contains(controlVar)))
                    continue;
                var dynName = $"dynamic_control:{fqn}:{controlVar}";
                symbols.Add(new SymbolDto
                {
                    Name = dynName,
                    Kind = "dynamic_control",
                    StartLine = Line(tree, addCall),
                    EndLine = Line(tree, addCall)
                });
                sideEffects.Add("UI_Mutation");
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = Line(tree, addCall),
                    SourceLanguage = "vb",
                    TargetName = dynName,
                    TargetKind = "dynamic_control",
                    Kind = "creates_dynamic_control"
                });
            }

            if (sideEffects.Count > 0)
            {
                metadata["side_effects"] = string.Join(",", sideEffects.OrderBy(s => s));
            }
            if (lateBindingCallCount > 0) metadata["late_binding_call_count"] = lateBindingCallCount.ToString();
            if (callByNameCount > 0) metadata["callbyname_count"] = callByNameCount.ToString();
            if (objectVarCount > 0) metadata["object_var_count"] = objectVarCount.ToString();
            methodSymbol.Metadata = metadata.Count == 0 ? null : metadata;
        }

        void EmitProperty(PropertyBlockSyntax node)
        {
            var name = node.PropertyStatement.Identifier.Text;
            var fqn = ComposeName(name);
            symbols.Add(new SymbolDto
            {
                Name = fqn,
                Kind = "property",
                StartLine = Line(tree, node),
                EndLine = EndLine(tree, node),
            });
            if (types.Count > 0) edges.Add(Contains(types.Peek(), fqn, Line(tree, node), "property"));
            foreach (var child in node.ChildNodes()) Walk(child);
        }

        void EmitField(FieldDeclarationSyntax node)
        {
            foreach (var declarator in node.Declarators)
            {
                foreach (var name in declarator.Names)
                {
                    var fieldName = ComposeName(name.Identifier.Text);
                    var isWithEvents = node.Modifiers.Any(m => m.IsKind(SyntaxKind.WithEventsKeyword));
                    var kind = isDesigner && isWithEvents ? "control_ref" : "field";
                    if (LooksLikeControlField(node, declarator))
                    {
                        knownControlNames.Add(name.Identifier.Text);
                    }
                    symbols.Add(new SymbolDto
                    {
                        Name = fieldName,
                        Kind = kind,
                        StartLine = Line(tree, name.Identifier),
                        EndLine = Line(tree, name.Identifier)
                    });
                    if (types.Count > 0) edges.Add(Contains(types.Peek(), fieldName, Line(tree, name.Identifier), kind));
                }
            }
        }

        Walk(root);
        return (symbols, edges);

        string ComposeName(string terminal)
        {
            var parts = new List<string>();
            if (namespaces.Count > 0) parts.AddRange(namespaces.Reverse().ToArray());
            if (types.Count > 0) parts.AddRange(types.Reverse().ToArray());
            parts.Add(terminal);
            return string.Join('.', parts.Where(p => !string.IsNullOrWhiteSpace(p)));
        }

        string ResolveInvocationName(InvocationExpressionSyntax invocation)
        {
            var symbol = model.GetSymbolInfo(invocation).Symbol as IMethodSymbol;
            if (symbol is not null)
            {
                return symbol.ToDisplayString();
            }

            return invocation.Expression.ToString();
        }

        Dictionary<string, string>? ResolveInvocationMetadata(InvocationExpressionSyntax invocation)
        {
            var symbol = model.GetSymbolInfo(invocation).Symbol as IMethodSymbol;
            if (symbol is not null)
            {
                return null;
            }

            return new Dictionary<string, string> { ["unresolved"] = "true" };
        }

        static string ExtractInvocationName(ExpressionSyntax expression) => expression switch
        {
            MemberAccessExpressionSyntax member => member.Name.Identifier.Text,
            IdentifierNameSyntax id => id.Identifier.Text,
            GenericNameSyntax generic => generic.Identifier.Text,
            InvocationExpressionSyntax inner => ExtractInvocationName(inner.Expression),
            _ => expression.ToString()
        };

        static (string source, string eventName) ParseEventExpression(ExpressionSyntax eventExpression)
        {
            if (eventExpression is MemberAccessExpressionSyntax member)
            {
                return (member.Expression.ToString(), member.Name.Identifier.Text);
            }

            var raw = eventExpression.ToString();
            var parts = raw.Split('.', 2);
            return parts.Length == 2 ? (parts[0], parts[1]) : (raw, raw);
        }

        static string ParseDelegateExpression(ExpressionSyntax delegateExpression)
        {
            if (delegateExpression is AddressOfExpressionSyntax addressOfExpression)
            {
                return ExtractInvocationName(addressOfExpression.Expression);
            }

            var raw = delegateExpression.ToString();
            const string prefix = "AddressOf ";
            if (raw.StartsWith(prefix, StringComparison.OrdinalIgnoreCase))
            {
                return ExtractInvocationName(
                    SyntaxFactory.ParseExpression(raw[prefix.Length..]));
            }

            return ExtractInvocationName(delegateExpression);
        }

        static bool IsSqlExecutionCall(string targetName) =>
            targetName.Contains("ExecuteReader", StringComparison.OrdinalIgnoreCase) ||
            targetName.Contains("ExecuteNonQuery", StringComparison.OrdinalIgnoreCase) ||
            targetName.Contains("ExecuteScalar", StringComparison.OrdinalIgnoreCase);

        static bool LooksLikeSql(string value) =>
            Regex.IsMatch(value, @"\b(select|insert|update|delete|exec(?:ute)?)\b", RegexOptions.IgnoreCase);

        static string ClassifySql(string value) =>
            Regex.IsMatch(value, @"^\s*exec(?:ute)?\b", RegexOptions.IgnoreCase) ? "stored_proc" : "inline";

        static string InferSqlTable(string value)
        {
            var matches = Regex.Matches(value, @"\b(?:from|join|into|update)\s+([a-zA-Z0-9_\.\[\]]+)", RegexOptions.IgnoreCase)
                .Cast<Match>()
                .Select(m => m.Groups[1].Value)
                .Where(v => !string.IsNullOrWhiteSpace(v))
                .Distinct(StringComparer.OrdinalIgnoreCase)
                .ToArray();
            return matches.Length == 0 ? string.Empty : string.Join(",", matches);
        }

        static string? TryExtractSqlFromExpressionText(string expressionText)
        {
            var fragments = Regex.Matches(expressionText, "\"([^\"]+)\"")
                .Cast<Match>()
                .Select(m => m.Groups[1].Value.Trim())
                .Where(s => !string.IsNullOrWhiteSpace(s))
                .ToArray();
            if (fragments.Length == 0) return null;
            return string.Join(" ", fragments);
        }

        static string MapProgIdToModernEquivalent(string progId)
        {
            var map = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase)
            {
                ["Scripting.FileSystemObject"] = "System.IO",
                ["ADODB.Connection"] = "System.Data.SqlClient.SqlConnection",
                ["ADODB.Recordset"] = "System.Data.DataTable",
                ["WScript.Shell"] = "System.Diagnostics.Process",
            };
            return map.TryGetValue(progId, out var modern) ? modern : "unknown";
        }

        static string? TryExtractStringLiteral(ExpressionSyntax expression)
        {
            if (expression is LiteralExpressionSyntax literal && literal.IsKind(SyntaxKind.StringLiteralExpression))
            {
                return literal.Token.ValueText;
            }

            var raw = expression.ToString().Trim();
            if (raw.Length >= 2 && raw.StartsWith("\"", StringComparison.Ordinal) && raw.EndsWith("\"", StringComparison.Ordinal))
            {
                return raw[1..^1];
            }

            return null;
        }

        static bool TryExtractColumnName(InvocationExpressionSyntax invocation, out string columnName)
        {
            columnName = string.Empty;
            var stringArg = GetFirstStringArgument(invocation);
            if (string.IsNullOrWhiteSpace(stringArg))
            {
                return false;
            }

            var exprText = invocation.Expression.ToString();
            if (Regex.IsMatch(exprText, @"\.(Item|Fields|GetOrdinal)$", RegexOptions.IgnoreCase) ||
                Regex.IsMatch(exprText, @"^(row|dr|reader|datarow|record)\b", RegexOptions.IgnoreCase))
            {
                columnName = stringArg;
                return true;
            }

            return false;
        }

        static string? GetFirstStringArgument(InvocationExpressionSyntax invocation)
        {
            var arg = invocation.ArgumentList?.Arguments.FirstOrDefault();
            var expression = arg is null ? null : GetArgumentExpression(arg);
            return expression is null ? null : TryExtractStringLiteral(expression);
        }

        static ExpressionSyntax? GetArgumentExpression(ArgumentSyntax argument) => argument switch
        {
            SimpleArgumentSyntax simple => simple.Expression,
            _ => null
        };

        static bool LooksLikeControlField(FieldDeclarationSyntax field, VariableDeclaratorSyntax declarator)
        {
            var typeText = declarator.AsClause?.ToString() ?? field.AsClause?.ToString() ?? string.Empty;
            return Regex.IsMatch(typeText, @"\b(Button|TextBox|DropDownList|GridView|Panel|Label|LinkButton)\b", RegexOptions.IgnoreCase);
        }
    }

    static EdgeDto Contains(string src, string target, int line, string targetKind) => new()
    {
        SourceName = src,
        SourceKind = "class",
        SourceStartLine = line,
        SourceLanguage = "vb",
        TargetName = target,
        TargetKind = targetKind,
        TargetStartLine = line,
        Kind = "contains"
    };

    static int Line(SyntaxTree tree, SyntaxNode node) => tree.GetLineSpan(node.Span).StartLinePosition.Line + 1;
    static int Line(SyntaxTree tree, SyntaxToken token) => tree.GetLineSpan(token.Span).StartLinePosition.Line + 1;
    static int EndLine(SyntaxTree tree, SyntaxNode node) => tree.GetLineSpan(node.Span).EndLinePosition.Line + 1;

    static (string stage, string seq)? Lifecycle(string name) => name.ToLowerInvariant() switch
    {
        "page_preinit" => ("PreInit", "1"),
        "page_init" => ("Init", "2"),
        "page_initcomplete" => ("InitComplete", "3"),
        "page_preload" => ("PreLoad", "4"),
        "page_load" => ("Load", "5"),
        "page_loadcomplete" => ("LoadComplete", "6"),
        "page_prerender" => ("PreRender", "7"),
        "page_prerendercomplete" => ("PreRenderComplete", "8"),
        "page_savestatecomplete" => ("SaveStateComplete", "9"),
        "page_render" or "render" => ("Render", "10"),
        "page_unload" => ("Unload", "11"),
        "oninit" => ("Init", "2"),
        "onload" => ("Load", "5"),
        "onprerender" => ("PreRender", "7"),
        "onunload" => ("Unload", "11"),
        _ => null
    };
}

internal sealed class SymbolDto
{
    public string Name { get; set; } = string.Empty;
    public string Kind { get; set; } = string.Empty;
    public int StartLine { get; set; }
    public int EndLine { get; set; }
    public Dictionary<string, string>? Metadata { get; set; }
}

internal sealed class EdgeDto
{
    public string SourceName { get; set; } = string.Empty;
    public string SourceKind { get; set; } = string.Empty;
    public int SourceStartLine { get; set; }
    public string SourceLanguage { get; set; } = "vb";
    public string TargetName { get; set; } = string.Empty;
    public string? TargetKind { get; set; }
    public int? TargetStartLine { get; set; }
    public string Kind { get; set; } = string.Empty;
    public Dictionary<string, string>? Metadata { get; set; }
}
