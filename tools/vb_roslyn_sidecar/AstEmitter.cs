using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.Text;
using Microsoft.CodeAnalysis.VisualBasic;
using Microsoft.CodeAnalysis.VisualBasic.Syntax;
using System.Text.Json.Serialization;
using System.Text.RegularExpressions;

internal sealed class AstEmitter
{
    private static readonly Dictionary<string, string> _progIdMap =
        new(StringComparer.OrdinalIgnoreCase)
        {
            ["Scripting.FileSystemObject"] = "System.IO",
            ["ADODB.Connection"] = "System.Data.SqlClient.SqlConnection",
            ["ADODB.Recordset"] = "System.Data.DataTable",
            ["WScript.Shell"] = "System.Diagnostics.Process",
        };

    private VisualBasicCompilation? _projectCompilation;
    private readonly Dictionary<string, SyntaxTree> _treesByPath =
        new(StringComparer.OrdinalIgnoreCase);

    public void BeginProject(string projectRoot)
    {
        _treesByPath.Clear();

        if (string.IsNullOrWhiteSpace(projectRoot) || !Directory.Exists(projectRoot))
        {
            _projectCompilation = null;
            return;
        }

        var trees = new List<SyntaxTree>();
        foreach (var vbPath in Directory.EnumerateFiles(projectRoot, "*.vb", SearchOption.AllDirectories))
        {
            try
            {
                var fileSource = File.ReadAllText(vbPath);
                var tree = VisualBasicSyntaxTree.ParseText(
                    SourceText.From(fileSource),
                    path: vbPath
                );
                trees.Add(tree);
                _treesByPath[vbPath] = tree;
            }
            catch
            {
                // Skip unreadable files and allow single-file fallback in Extract.
            }
        }

        _projectCompilation = VisualBasicCompilation.Create("sidecar_project")
            .AddSyntaxTrees(trees);
    }

    public (List<SymbolDto>, List<EdgeDto>) Extract(string path, string source)
    {
        var symbols = new List<SymbolDto>();
        var edges = new List<EdgeDto>();
        try
        {
            SyntaxTree tree;
            VisualBasicCompilation compilation;

            if (_projectCompilation is not null && _treesByPath.TryGetValue(path, out var existingTree))
            {
                tree = VisualBasicSyntaxTree.ParseText(SourceText.From(source), path: path);
                compilation = _projectCompilation.ReplaceSyntaxTree(existingTree, tree);
                _projectCompilation = compilation;
                _treesByPath[path] = tree;
            }
            else if (_projectCompilation is not null)
            {
                tree = VisualBasicSyntaxTree.ParseText(SourceText.From(source), path: path);
                compilation = _projectCompilation.AddSyntaxTrees(tree);
                _projectCompilation = compilation;
                _treesByPath[path] = tree;
            }
            else
            {
                tree = VisualBasicSyntaxTree.ParseText(SourceText.From(source), path: path);
                compilation = VisualBasicCompilation.Create("sidecar_single").AddSyntaxTrees(tree);
            }

            var model = compilation.GetSemanticModel(tree);
            var root = tree.GetCompilationUnitRoot();

        var namespaces = new Stack<string>();
        var types = new Stack<string>();
        var typeStartLines = new Stack<int>();
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
            var strict = root.Options.FirstOrDefault(o => o.ToString().StartsWith("Option Strict", StringComparison.OrdinalIgnoreCase));
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
            var typeStartLine = Line(tree, node);
            symbols.Add(new SymbolDto
            {
                Name = fqn,
                Kind = kind,
                StartLine = typeStartLine,
                EndLine = EndLine(tree, node),
            });
            if (types.Count > 0)
            {
                edges.Add(Contains(types.Peek(), fqn, typeStartLines.Peek(), typeStartLine, kind));
            }

            types.Push(fqn);
            typeStartLines.Push(typeStartLine);
            foreach (var child in node.ChildNodes()) Walk(child);
            types.Pop();
            typeStartLines.Pop();
        }

        void EmitMethod(MethodBlockSyntax node)
        {
            var stmt = node.SubOrFunctionStatement;
            var name = stmt.Identifier.Text;
            var fqn = ComposeName(name);
            var methodStartLine = Line(tree, node);
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
                StartLine = methodStartLine,
                EndLine = EndLine(tree, node),
                Metadata = null,
            });
            var methodSymbol = symbols[^1];
            if (types.Count > 0) edges.Add(Contains(types.Peek(), fqn, typeStartLines.Peek(), methodStartLine, "function"));

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

            // Single-pass traversal — replaces all DescendantNodes().OfType<T>() calls.
            var collector = new MethodNodeCollector();
            collector.Visit(node);

            // Populate dynamicControls BEFORE the Invocations loop which uses it.
            var dynamicControls = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            foreach (var decl in collector.LocalDeclarations)
            {
                if (!Patterns.ControlAsDecl().IsMatch(decl.ToString())) continue;
                foreach (var d in decl.Declarators)
                    foreach (var n in d.Names)
                        dynamicControls.Add(n.Identifier.Text);
            }

            foreach (var add in collector.AddRemoveHandlers)
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

            foreach (var inv in collector.Invocations)
            {
                var targetName = ResolveInvocationName(inv);
                var callSiteLine = Line(tree, inv);
                var invocationMetadata = ResolveInvocationMetadata(inv) ?? new Dictionary<string, string>();
                invocationMetadata["call_site_line"] = callSiteLine.ToString();
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = methodStartLine,
                    SourceLanguage = "vb",
                    TargetName = targetName,
                    TargetKind = "function",
                    Kind = "calls",
                    Metadata = invocationMetadata
                });

                if (IsSqlExecutionCall(targetName))
                {
                    sideEffects.Add("DB_Access");
                    edges.Add(new EdgeDto
                    {
                        SourceName = fqn,
                        SourceKind = "function",
                        SourceStartLine = methodStartLine,
                        SourceLanguage = "vb",
                        TargetName = "sql_execution",
                        TargetKind = "sql",
                        Kind = "sql_exec",
                        Metadata = new()
                        {
                            ["invocation"] = targetName,
                            ["call_site_line"] = callSiteLine.ToString()
                        }
                    });
                }

                if (TryExtractColumnName(inv, out var columnName))
                {
                    sideEffects.Add("DB_Access");
                    edges.Add(new EdgeDto
                    {
                        SourceName = fqn,
                        SourceKind = "function",
                        SourceStartLine = methodStartLine,
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
                        SourceStartLine = methodStartLine,
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
                            SourceStartLine = methodStartLine,
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

                // StringBuilder fragment check (merged from the old separate DescendantNodes loop).
                var exprText = inv.Expression.ToString();
                if (exprText.EndsWith(".Append", StringComparison.OrdinalIgnoreCase) ||
                    exprText.EndsWith(".AppendLine", StringComparison.OrdinalIgnoreCase))
                {
                    var frag = GetFirstStringArgument(inv);
                    if (!string.IsNullOrWhiteSpace(frag) && LooksLikeSql(frag))
                    {
                        sideEffects.Add("DB_Access");
                        edges.Add(new EdgeDto
                        {
                            SourceName = fqn,
                            SourceKind = "function",
                            SourceStartLine = methodStartLine,
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
                }

                // .Controls.Add check (merged from the old separate DescendantNodes loop).
                if (exprText.EndsWith(".Controls.Add", StringComparison.OrdinalIgnoreCase))
                {
                    var controlVar = SanitizeName(inv.ArgumentList?.Arguments.FirstOrDefault()?.ToString());
                    if (!string.IsNullOrWhiteSpace(controlVar) &&
                        (dynamicControls.Contains(controlVar) || knownControlNames.Contains(controlVar)))
                    {
                        var dynName = $"dynamic_control:{fqn}:{controlVar}";
                        symbols.Add(new SymbolDto
                        {
                            Name = dynName,
                            Kind = "dynamic_control",
                            StartLine = Line(tree, inv),
                            EndLine = Line(tree, inv)
                        });
                        sideEffects.Add("UI_Mutation");
                        edges.Add(new EdgeDto
                        {
                            SourceName = fqn,
                            SourceKind = "function",
                            SourceStartLine = methodStartLine,
                            SourceLanguage = "vb",
                            TargetName = dynName,
                            TargetKind = "dynamic_control",
                            Kind = "creates_dynamic_control"
                        });
                    }
                }
            }

            foreach (var create in collector.ObjectCreations)
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
                    SourceStartLine = methodStartLine,
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

            foreach (var assignment in collector.Assignments)
            {
                if (!assignment.Left.ToString().EndsWith(".CommandText", StringComparison.OrdinalIgnoreCase)) continue;
                var sql = TryExtractStringLiteral(assignment.Right);
                if (string.IsNullOrWhiteSpace(sql)) continue;
                sideEffects.Add("DB_Access");
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = methodStartLine,
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

            foreach (var local in collector.LocalDeclarations)
            {
                var txt = local.ToString();
                if (!Patterns.SqlQueryVariable().IsMatch(txt)) continue;
                var sql = TryExtractSqlFromExpressionText(txt);
                if (string.IsNullOrWhiteSpace(sql) || !LooksLikeSql(sql)) continue;
                sideEffects.Add("DB_Access");
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = methodStartLine,
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

            foreach (var withBlock in collector.WithBlocks)
            {
                var withTarget = SanitizeName(withBlock.WithStatement.Expression.ToString());
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
                            SourceStartLine = methodStartLine,
                            SourceLanguage = "vb",
                            TargetName = SanitizeName($"{withTarget}{stmtText}"),
                            TargetKind = "member",
                            Kind = kind
                        });
                    }
                }
            }

            foreach (var member in collector.MemberAccesses)
            {
                if (!member.ToString().StartsWith("My.", StringComparison.OrdinalIgnoreCase)) continue;
                sideEffects.Add("State_Access");
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = methodStartLine,
                    SourceLanguage = "vb",
                    TargetName = SanitizeName(member.ToString()),
                    TargetKind = "state",
                    Kind = "reads_state"
                });
            }

            foreach (var redim in collector.ReDims)
            {
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = methodStartLine,
                    SourceLanguage = "vb",
                    TargetName = "ReDim",
                    Kind = "anti_pattern"
                });
            }

            foreach (var onError in collector.OnErrors)
            {
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = methodStartLine,
                    SourceLanguage = "vb",
                    TargetName = SanitizeName(onError.ToString()),
                    Kind = "anti_pattern"
                });
            }

            objectVarCount += collector.VariableDeclarators
                .Count(v => v.AsClause?.ToString().Contains("As Object", StringComparison.OrdinalIgnoreCase) == true);

            foreach (var localType in collector.TopLevelTypeBlocks)
                Walk(localType);

            if (sideEffects.Count > 0)
                metadata["side_effects"] = string.Join(",", sideEffects.OrderBy(s => s));
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
            if (types.Count > 0) edges.Add(Contains(types.Peek(), fqn, typeStartLines.Peek(), Line(tree, node), "property"));
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
                    if (types.Count > 0) edges.Add(Contains(types.Peek(), fieldName, typeStartLines.Peek(), Line(tree, name.Identifier), kind));
                }
            }
        }

        Walk(root);

        string ComposeName(string terminal)
        {
            var parts = namespaces.Reverse()
                .Concat(types.Reverse())
                .Append(terminal)
                .Where(p => !string.IsNullOrWhiteSpace(p));
            return SanitizeName(string.Join('.', parts));
        }

        string ResolveInvocationName(InvocationExpressionSyntax invocation)
        {
            try
            {
                var info = model.GetSymbolInfo(invocation);
                var symbol = info.Symbol as IMethodSymbol;
                if (symbol is not null)
                {
                    return symbol.ToDisplayString();
                }

                // Fall back to raw text when no resolved symbol.
                return SanitizeName(invocation.Expression?.ToString() ?? "<unknown>");
            }
            catch
            {
                // Any Roslyn semantic lookup can throw on malformed trees.
                // Degrade gracefully to raw text.
                return SanitizeName(invocation.Expression?.ToString() ?? "<unknown>");
            }
        }

        Dictionary<string, string>? ResolveInvocationMetadata(InvocationExpressionSyntax invocation)
        {
            try
            {
                var info = model.GetSymbolInfo(invocation);
                if (info.Symbol is IMethodSymbol)
                {
                    return null;
                }
            }
            catch
            {
                // fall through
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

        static string SanitizeName(string raw)
        {
            if (string.IsNullOrEmpty(raw)) return raw;
            var collapsed = Patterns.Whitespace().Replace(raw, " ").Trim();
            const int maxLen = 256;
            if (collapsed.Length > maxLen)
                collapsed = collapsed.Substring(0, maxLen);
            return collapsed;
        }

        static (string source, string eventName) ParseEventExpression(ExpressionSyntax eventExpression)
        {
            if (eventExpression is MemberAccessExpressionSyntax member)
            {
                return (SanitizeName(member.Expression.ToString()),
                        SanitizeName(member.Name.Identifier.Text));
            }

            var raw = eventExpression.ToString();
            var parts = raw.Split('.', 2);
            return parts.Length == 2
                ? (SanitizeName(parts[0]), SanitizeName(parts[1]))
                : (SanitizeName(raw), SanitizeName(raw));
        }

        static string ParseDelegateExpression(ExpressionSyntax delegateExpression)
        {
            var raw = delegateExpression.ToString();
            const string prefix = "AddressOf ";
            if (raw.StartsWith(prefix, StringComparison.OrdinalIgnoreCase))
            {
                return SanitizeName(ExtractInvocationName(
                    SyntaxFactory.ParseExpression(raw[prefix.Length..])));
            }

            return SanitizeName(ExtractInvocationName(delegateExpression));
        }

        static bool IsSqlExecutionCall(string targetName) =>
            targetName.Contains("ExecuteReader", StringComparison.OrdinalIgnoreCase) ||
            targetName.Contains("ExecuteNonQuery", StringComparison.OrdinalIgnoreCase) ||
            targetName.Contains("ExecuteScalar", StringComparison.OrdinalIgnoreCase);

        static bool LooksLikeSql(string value) =>
            Patterns.SqlKeywords().IsMatch(value);

        static string ClassifySql(string value) =>
            Patterns.SqlExecPrefix().IsMatch(value) ? "stored_proc" : "inline";

        static string InferSqlTable(string value)
        {
            var matches = Patterns.SqlTableRef().Matches(value)
                .Cast<Match>()
                .Select(m => m.Groups[1].Value)
                .Where(v => !string.IsNullOrWhiteSpace(v))
                .Distinct(StringComparer.OrdinalIgnoreCase)
                .ToArray();
            return matches.Length == 0 ? string.Empty : string.Join(",", matches);
        }

        static string? TryExtractSqlFromExpressionText(string expressionText)
        {
            var fragments = Patterns.StringLiterals().Matches(expressionText)
                .Cast<Match>()
                .Select(m => m.Groups[1].Value.Trim())
                .Where(s => !string.IsNullOrWhiteSpace(s))
                .ToArray();
            if (fragments.Length == 0) return null;
            return string.Join(" ", fragments);
        }

        static string MapProgIdToModernEquivalent(string progId) =>
            _progIdMap.TryGetValue(progId, out var modern) ? modern : "unknown";

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
                return false;

            var exprText = invocation.Expression.ToString();
            if (Patterns.ColumnAccessExpr().IsMatch(exprText) ||
                Patterns.RowReaderPrefix().IsMatch(exprText))
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
            var typeText = declarator.AsClause?.ToString() ?? string.Empty;
            return Patterns.ControlTypeName().IsMatch(typeText);
        }
        }
        catch (Exception ex)
        {
            // Don't fail the whole response — return partial results plus an error marker.
            symbols.Add(new SymbolDto
            {
                Name = "file_parse_error",
                Kind = "file",
                StartLine = 1,
                EndLine = 1,
                Metadata = new()
                {
                    ["fqn"] = "file",
                    ["error"] = ex.GetType().Name,
                    ["error_message"] = ex.Message
                }
            });
        }

        return (symbols, edges);
    }

    static EdgeDto Contains(string src, string target, int sourceLine, int targetLine, string targetKind) => new()
    {
        SourceName = src,
        SourceKind = "class",
        SourceStartLine = sourceLine,
        SourceLanguage = "vb",
        TargetName = target,
        TargetKind = targetKind,
        TargetStartLine = targetLine,
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

    private sealed class MethodNodeCollector : VisualBasicSyntaxWalker
    {
        public readonly List<AddRemoveHandlerStatementSyntax> AddRemoveHandlers = [];
        public readonly List<InvocationExpressionSyntax> Invocations = [];
        public readonly List<ObjectCreationExpressionSyntax> ObjectCreations = [];
        public readonly List<AssignmentStatementSyntax> Assignments = [];
        public readonly List<LocalDeclarationStatementSyntax> LocalDeclarations = [];
        public readonly List<WithBlockSyntax> WithBlocks = [];
        public readonly List<MemberAccessExpressionSyntax> MemberAccesses = [];
        public readonly List<ReDimStatementSyntax> ReDims = [];
        public readonly List<SyntaxNode> OnErrors = [];
        public readonly List<VariableDeclaratorSyntax> VariableDeclarators = [];
        public readonly List<SyntaxNode> TopLevelTypeBlocks = [];

        public override void VisitAddRemoveHandlerStatement(AddRemoveHandlerStatementSyntax node)
        { AddRemoveHandlers.Add(node); base.VisitAddRemoveHandlerStatement(node); }

        public override void VisitInvocationExpression(InvocationExpressionSyntax node)
        { Invocations.Add(node); base.VisitInvocationExpression(node); }

        public override void VisitObjectCreationExpression(ObjectCreationExpressionSyntax node)
        { ObjectCreations.Add(node); base.VisitObjectCreationExpression(node); }

        public override void VisitAssignmentStatement(AssignmentStatementSyntax node)
        { Assignments.Add(node); base.VisitAssignmentStatement(node); }

        public override void VisitLocalDeclarationStatement(LocalDeclarationStatementSyntax node)
        { LocalDeclarations.Add(node); base.VisitLocalDeclarationStatement(node); }

        public override void VisitWithBlock(WithBlockSyntax node)
        { WithBlocks.Add(node); base.VisitWithBlock(node); }

        public override void VisitMemberAccessExpression(MemberAccessExpressionSyntax node)
        { MemberAccesses.Add(node); base.VisitMemberAccessExpression(node); }

        public override void VisitVariableDeclarator(VariableDeclaratorSyntax node)
        { VariableDeclarators.Add(node); base.VisitVariableDeclarator(node); }

        public override void VisitReDimStatement(ReDimStatementSyntax node)
        { ReDims.Add(node); base.VisitReDimStatement(node); }

        public override void VisitOnErrorGoToStatement(OnErrorGoToStatementSyntax node)
        { OnErrors.Add(node); base.VisitOnErrorGoToStatement(node); }

        public override void VisitOnErrorResumeNextStatement(OnErrorResumeNextStatementSyntax node)
        { OnErrors.Add(node); base.VisitOnErrorResumeNextStatement(node); }

        // Do NOT call base for type blocks — Walk() handles them separately.
        public override void VisitClassBlock(ClassBlockSyntax node) => TopLevelTypeBlocks.Add(node);
        public override void VisitModuleBlock(ModuleBlockSyntax node) => TopLevelTypeBlocks.Add(node);
        public override void VisitStructureBlock(StructureBlockSyntax node) => TopLevelTypeBlocks.Add(node);
        public override void VisitInterfaceBlock(InterfaceBlockSyntax node) => TopLevelTypeBlocks.Add(node);
        public override void VisitEnumBlock(EnumBlockSyntax node) => TopLevelTypeBlocks.Add(node);
    }
}

internal sealed class SymbolDto
{
    [JsonPropertyName("name")]
    public string Name { get; set; } = string.Empty;

    [JsonPropertyName("kind")]
    public string Kind { get; set; } = string.Empty;

    [JsonPropertyName("start_line")]
    public int StartLine { get; set; }

    [JsonPropertyName("end_line")]
    public int EndLine { get; set; }

    [JsonPropertyName("metadata")]
    public Dictionary<string, string>? Metadata { get; set; }
}

internal sealed class EdgeDto
{
    [JsonPropertyName("source_name")]
    public string SourceName { get; set; } = string.Empty;

    [JsonPropertyName("source_kind")]
    public string SourceKind { get; set; } = string.Empty;

    [JsonPropertyName("source_start_line")]
    public int SourceStartLine { get; set; }

    [JsonPropertyName("source_language")]
    public string SourceLanguage { get; set; } = "vb";

    [JsonPropertyName("target_name")]
    public string TargetName { get; set; } = string.Empty;

    [JsonPropertyName("target_kind")]
    public string? TargetKind { get; set; }

    [JsonPropertyName("target_start_line")]
    public int? TargetStartLine { get; set; }

    [JsonPropertyName("kind")]
    public string Kind { get; set; } = string.Empty;

    [JsonPropertyName("metadata")]
    public Dictionary<string, string>? Metadata { get; set; }
}

internal static partial class Patterns
{
    [GeneratedRegex(@"\b(select|insert|update|delete|exec(?:ute)?)\b", RegexOptions.IgnoreCase)]
    public static partial Regex SqlKeywords();

    [GeneratedRegex(@"^\s*exec(?:ute)?\b", RegexOptions.IgnoreCase)]
    public static partial Regex SqlExecPrefix();

    [GeneratedRegex(@"\b(?:from|join|into|update)\s+([a-zA-Z0-9_\.\[\]]+)", RegexOptions.IgnoreCase)]
    public static partial Regex SqlTableRef();

    [GeneratedRegex("\"([^\"]+)\"")]
    public static partial Regex StringLiterals();

    [GeneratedRegex(@"\s+")]
    public static partial Regex Whitespace();

    [GeneratedRegex(@"\.(Item|Fields|GetOrdinal)$", RegexOptions.IgnoreCase)]
    public static partial Regex ColumnAccessExpr();

    [GeneratedRegex(@"^(row|dr|reader|datarow|record)\b", RegexOptions.IgnoreCase)]
    public static partial Regex RowReaderPrefix();

    [GeneratedRegex(@"\b(Button|TextBox|DropDownList|GridView|Panel|Label|LinkButton)\b", RegexOptions.IgnoreCase)]
    public static partial Regex ControlTypeName();

    [GeneratedRegex(@"\b(sql|query)\b", RegexOptions.IgnoreCase)]
    public static partial Regex SqlQueryVariable();

    [GeneratedRegex(@"As\s+(Button|TextBox|DropDownList|GridView|Panel|Label|LinkButton)\b", RegexOptions.IgnoreCase)]
    public static partial Regex ControlAsDecl();
}
