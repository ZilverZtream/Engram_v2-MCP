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

    // Qualified name WITHOUT the parameter list. Method DEFINITIONS are
    // stored as bare dotted names ("_ata.Cls.CreateMany"), so CALL targets
    // must match that shape or cross-file resolution never binds — the
    // default ToDisplayString() appends "(Integer, List(Of Integer))" which
    // fails every resolver step. Arity travels separately in metadata["args"].
    private static readonly SymbolDisplayFormat BareQualifiedNameFormat = new(
        globalNamespaceStyle: SymbolDisplayGlobalNamespaceStyle.Omitted,
        typeQualificationStyle: SymbolDisplayTypeQualificationStyle.NameAndContainingTypesAndNamespaces,
        genericsOptions: SymbolDisplayGenericsOptions.None,
        memberOptions: SymbolDisplayMemberOptions.IncludeContainingType);

    private VisualBasicCompilation? _projectCompilation;
    private string? _projectRoot;
    private readonly Dictionary<string, SyntaxTree> _treesByPath =
        new(StringComparer.OrdinalIgnoreCase);
    private readonly Dictionary<string, List<SourcePropertyInfo>> _sourcePropertiesByName =
        new(StringComparer.OrdinalIgnoreCase);

    private sealed record SourcePropertyInfo(string Fqn, string ContainingType, string Path);

    /// <summary>
    /// Directory names that hold build output, VCS internals or IDE caches.
    /// Walking them costs real time on a large solution and the .vb files
    /// found there are generated copies, never the source of truth.
    /// </summary>
    private static readonly HashSet<string> NonSourceDirs =
        new(StringComparer.OrdinalIgnoreCase)
        {
            ".git", ".hg", ".svn", ".vs", ".vscode", ".idea",
            "bin", "obj", "packages", "node_modules", "TestResults",
            // Scratch trees. `.tmp` is not hypothetical: MiniLangCompiler
            // keeps 56,432 generated .vb files there against 917 real
            // sources in src/, and parsing them all is what made this
            // sidecar time out and crash.
            ".tmp", ".claude", "artifacts",
        };

    private static IEnumerable<string> EnumerateProjectVbFiles(string root)
    {
        var stack = new Stack<string>();
        stack.Push(root);
        while (stack.Count > 0)
        {
            var dir = stack.Pop();
            string[] subdirs;
            try
            {
                subdirs = Directory.GetDirectories(dir);
            }
            catch
            {
                continue;
            }

            foreach (var sub in subdirs)
            {
                if (!NonSourceDirs.Contains(Path.GetFileName(sub)))
                {
                    stack.Push(sub);
                }
            }

            string[] files;
            try
            {
                files = Directory.GetFiles(dir, "*.vb");
            }
            catch
            {
                continue;
            }

            foreach (var f in files)
            {
                yield return f;
            }
        }
    }

    // Only the decoded leading BOM is an encoding marker. Keep its character
    // position for syntax spans; embedded U+FEFF remains subject to parse errors.
    private static string SourceForParsing(string source) =>
        source.StartsWith("\uFEFF", StringComparison.Ordinal) ? " " + source.Substring(1) : source;

    /// <summary>
    /// Parse every .vb file under <paramref name="projectRoot"/> into one
    /// shared compilation. O(project) — call it when the project changes,
    /// NOT on every incremental update; use <see cref="InvalidateFiles"/>
    /// for that.
    /// </summary>
    /// <param name="files">
    /// The exact files to parse, as supplied by the indexer. When null or
    /// empty the sidecar falls back to walking <paramref name="projectRoot"/>.
    ///
    /// The walk is NOT equivalent to the list: the indexer applies ignore
    /// rules and extension presets that a bare directory walk knows nothing
    /// about. Preferring the list keeps the compilation to exactly what is
    /// being indexed.
    /// </param>
    public void BeginProject(string projectRoot, IReadOnlyList<string>? files = null)
    {
        _treesByPath.Clear();
        _sourcePropertiesByName.Clear();
        _projectRoot = null;

        if (string.IsNullOrWhiteSpace(projectRoot) || !Directory.Exists(projectRoot))
        {
            _projectCompilation = null;
            return;
        }

        IEnumerable<string> sources = files is { Count: > 0 }
            ? files
            : EnumerateProjectVbFiles(projectRoot);

        var trees = new List<SyntaxTree>();
        foreach (var vbPath in sources)
        {
            try
            {
                var fileSource = File.ReadAllText(vbPath);
                var tree = VisualBasicSyntaxTree.ParseText(
                    SourceText.From(SourceForParsing(fileSource)),
                    path: vbPath
                );
                trees.Add(tree);
                _treesByPath[vbPath] = tree;
                AddSourceProperties(tree);
            }
            catch
            {
                // Skip unreadable files and allow single-file fallback in Extract.
            }
        }

        _projectCompilation = VisualBasicCompilation.Create("sidecar_project")
            .AddSyntaxTrees(trees);
        _projectRoot = projectRoot;
    }

    /// <summary>
    /// Drop the cached syntax trees for the given paths, keeping the rest of
    /// the project compilation warm. Used for incremental updates: a changed
    /// file is then re-parsed from the source the caller sends with its next
    /// parse request, and a deleted file simply leaves the compilation.
    ///
    /// Cost is O(changed), not O(project) — the point of the command.
    /// </summary>
    /// <returns>How many cached trees were actually removed.</returns>
    public int InvalidateFiles(IEnumerable<string> paths)
    {
        if (_projectCompilation is null)
        {
            return 0;
        }

        var stale = new List<SyntaxTree>();
        foreach (var path in paths)
        {
            if (string.IsNullOrWhiteSpace(path))
            {
                continue;
            }
            if (_treesByPath.TryGetValue(path, out var tree))
            {
                stale.Add(tree);
                _treesByPath.Remove(path);
                RemoveSourceProperties(path);
            }
        }

        if (stale.Count > 0)
        {
            _projectCompilation = _projectCompilation.RemoveSyntaxTrees(stale);
        }
        return stale.Count;
    }

    private void AddSourceProperties(SyntaxTree tree)
    {
        foreach (var statement in tree.GetRoot().DescendantNodes().OfType<PropertyStatementSyntax>())
        {
            var terminal = statement.Identifier.Text;
            var namespaces = statement.Ancestors()
                .OfType<NamespaceBlockSyntax>()
                .Reverse()
                .Select(block => block.NamespaceStatement.Name.ToString());
            var types = statement.Ancestors()
                .Reverse()
                .Select(node => node switch
                {
                    ClassBlockSyntax block => block.ClassStatement.Identifier.Text,
                    ModuleBlockSyntax block => block.ModuleStatement.Identifier.Text,
                    StructureBlockSyntax block => block.StructureStatement.Identifier.Text,
                    InterfaceBlockSyntax block => block.InterfaceStatement.Identifier.Text,
                    _ => null
                })
                .Where(name => !string.IsNullOrWhiteSpace(name));
            var ownerParts = namespaces.Concat(types).ToList();
            if (ownerParts.Count == 0)
            {
                continue;
            }
            var owner = string.Join('.', ownerParts);
            var info = new SourcePropertyInfo(
                $"{owner}.{terminal}",
                owner,
                tree.FilePath);
            if (_sourcePropertiesByName.TryGetValue(terminal, out var existing))
            {
                existing.Add(info);
            }
            else
            {
                _sourcePropertiesByName[terminal] = [info];
            }
        }
    }

    private void RemoveSourceProperties(string path)
    {
        foreach (var terminal in _sourcePropertiesByName.Keys.ToList())
        {
            _sourcePropertiesByName[terminal].RemoveAll(property =>
                property.Path.Equals(path, StringComparison.OrdinalIgnoreCase));
            if (_sourcePropertiesByName[terminal].Count == 0)
            {
                _sourcePropertiesByName.Remove(terminal);
            }
        }
    }

    /// <summary>The root the current compilation was built from, if any.</summary>
    public string? ProjectRoot => _projectRoot;

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
                // Reuse the tree and compilation from BeginProject — no re-parse,
                // no compilation rebuild. This eliminates O(N) ParseText calls and
                // O(N²) ReplaceSyntaxTree allocations during the extraction loop.
                tree = existingTree;
                compilation = _projectCompilation;
            }
            else if (_projectCompilation is not null)
            {
                // File wasn't in the initial project scan (new file, or path
                // mismatch). Parse it and add to the compilation for this call,
                // but don't mutate _projectCompilation — avoids O(N²) rebuilds.
                tree = VisualBasicSyntaxTree.ParseText(SourceText.From(SourceForParsing(source)), path: path);
                compilation = _projectCompilation.AddSyntaxTrees(tree);
                RemoveSourceProperties(path);
                AddSourceProperties(tree);
            }
            else
            {
                tree = VisualBasicSyntaxTree.ParseText(SourceText.From(SourceForParsing(source)), path: path);
                compilation = VisualBasicCompilation.Create("sidecar_single").AddSyntaxTrees(tree);
                _sourcePropertiesByName.Clear();
                AddSourceProperties(tree);
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
                    EmitType(mod.ModuleStatement.Identifier.ToString(), "class", mod);
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
                    EmitMethod(m, m.SubOrFunctionStatement, m.SubOrFunctionStatement.Identifier.Text, m.SubOrFunctionStatement.HandlesClause);
                    return;
                case ConstructorBlockSyntax ctor:
                    EmitMethod(ctor, ctor.SubNewStatement, "New", null);
                    return;
                case PropertyBlockSyntax p:
                    EmitProperty(p);
                    return;
                case PropertyStatementSyntax p when p.Parent is not PropertyBlockSyntax:
                    EmitAutoProperty(p);
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

        void EmitMethod(SyntaxNode node, MethodBaseSyntax stmt, string name, HandlesClauseSyntax? handles)
        {
            var fqn = ComposeName(name);
            var methodStartLine = Line(tree, node);
            var metadata = new Dictionary<string, string>();
            metadata["signature"] = stmt.WithoutTrivia().NormalizeWhitespace().ToFullString();
            if (model.GetDeclaredSymbol(stmt) is IMethodSymbol declaredMethod)
            {
                metadata["access_level"] = declaredMethod.DeclaredAccessibility switch
                {
                    Accessibility.Public => "Public",
                    Accessibility.Private => "Private",
                    Accessibility.Protected => "Protected",
                    Accessibility.Internal => "Friend",
                    Accessibility.ProtectedOrInternal => "Protected Friend",
                    Accessibility.ProtectedAndInternal => "Private Protected",
                    _ => "unknown"
                };
                metadata["return_type"] = declaredMethod.ReturnsVoid ? "Void" : declaredMethod.ReturnType.ToDisplayString();
            }
            // TODO-13: parameter count enables arity-aware call resolution.
            metadata["arity"] = (stmt.ParameterList?.Parameters.Count ?? 0).ToString();
            var parameters = stmt.ParameterList?.Parameters ?? default;
            metadata["arity_min"] = parameters.Count(p => !p.Modifiers.Any(m => m.IsKind(SyntaxKind.OptionalKeyword) || m.IsKind(SyntaxKind.ParamArrayKeyword))).ToString();
            metadata["arity_variadic"] = parameters.Any(p => p.Modifiers.Any(m => m.IsKind(SyntaxKind.ParamArrayKeyword))).ToString().ToLowerInvariant();
            if (node is ConstructorBlockSyntax)
                metadata["constructor"] = "true";
            if (stmt.Modifiers.Any(m => m.IsKind(SyntaxKind.AsyncKeyword)))
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

            foreach (var hc in handles?.Events ?? new SeparatedSyntaxList<HandlesClauseItemSyntax>())
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
                // TODO-13: argument count lets the resolver prefer the
                // matching overload instead of the first name hit.
                invocationMetadata["args"] = (inv.ArgumentList?.Arguments.Count ?? 0).ToString();
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
                    var controlArgument = inv.ArgumentList?.Arguments.FirstOrDefault();
                    var controlVar = controlArgument is null
                        ? string.Empty
                        : SanitizeName(controlArgument.ToString());
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

            // AddressOf method references are calls-by-delegate: the target
            // IS invoked (bootstrap registrations like
            // GlobalConfiguration.Configure(AddressOf WebApiConfig.Register),
            // timer/thread callbacks). Without these edges the target shows
            // 0 callers and every unwired/dead-method surface false-flags it.
            foreach (var aref in collector.AddressOfRefs)
            {
                var delegateTarget = SanitizeName(ExtractInvocationName(aref.Operand));
                if (string.IsNullOrWhiteSpace(delegateTarget)) continue;
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = methodStartLine,
                    SourceLanguage = "vb",
                    TargetName = delegateTarget,
                    TargetKind = "function",
                    Kind = "calls",
                    Metadata = new()
                    {
                        ["call_site_line"] = Line(tree, aref).ToString(),
                        ["via"] = "addressof"
                    }
                });
            }

            foreach (var create in collector.ObjectCreations)
            {
                var constructor = model.GetSymbolInfo(create).Symbol as IMethodSymbol;
                var createdType = constructor?.ContainingType ?? model.GetTypeInfo(create).Type;
                var constructorOwner = createdType is not null && createdType.TypeKind != TypeKind.Error
                    ? createdType.ToDisplayString(BareQualifiedNameFormat)
                    : SanitizeName(create.Type.ReplaceNodes(
                        create.Type.DescendantNodesAndSelf().OfType<GenericNameSyntax>(),
                        (original, _) => SyntaxFactory.IdentifierName(original.Identifier)).ToString());
                edges.Add(new EdgeDto
                {
                    SourceName = fqn, SourceKind = "function",
                    SourceStartLine = methodStartLine, SourceLanguage = "vb",
                    TargetName = constructorOwner + ".New", TargetKind = "function",
                    Kind = "calls", Metadata = new()
                    {
                        ["call_site_line"] = Line(tree, create).ToString(),
                        ["args"] = (create.ArgumentList?.Arguments.Count ?? 0).ToString(),
                        ["via"] = "object_creation"
                    }
                });
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

            EmitPropertyReferences(collector, fqn, "function", methodStartLine);
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
            var propertyStartLine = Line(tree, node);
            symbols.Add(new SymbolDto
            {
                Name = fqn,
                Kind = "property",
                StartLine = propertyStartLine,
                EndLine = EndLine(tree, node),
            });
            if (types.Count > 0) edges.Add(Contains(types.Peek(), fqn, typeStartLines.Peek(), propertyStartLine, "property"));

            // Property accessor bodies are executable. Previously Walk()
            // indexed the declaration but silently skipped every call made by
            // Get/Set, disconnecting a common VB business-logic layer from its
            // downstream dependencies.
            var collector = new MethodNodeCollector();
            collector.Visit(node);
            foreach (var invocation in collector.Invocations)
            {
                var callSiteLine = Line(tree, invocation);
                var invocationMetadata = ResolveInvocationMetadata(invocation) ?? new Dictionary<string, string>();
                invocationMetadata["call_site_line"] = callSiteLine.ToString();
                invocationMetadata["args"] = (invocation.ArgumentList?.Arguments.Count ?? 0).ToString();
                invocationMetadata["via"] = "property_accessor";
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "property",
                    SourceStartLine = propertyStartLine,
                    SourceLanguage = "vb",
                    TargetName = ResolveInvocationName(invocation),
                    TargetKind = "function",
                    Kind = "calls",
                    Metadata = invocationMetadata
                });
            }

            EmitPropertyReferences(collector, fqn, "property", propertyStartLine, fqn);
            foreach (var child in node.ChildNodes()) Walk(child);
        }

        void EmitAutoProperty(PropertyStatementSyntax statement)
        {
            var fqn = ComposeName(statement.Identifier.Text);
            var propertyLine = Line(tree, statement);
            symbols.Add(new SymbolDto
            {
                Name = fqn,
                Kind = "property",
                StartLine = propertyLine,
                EndLine = EndLine(tree, statement),
            });
            if (types.Count > 0)
            {
                edges.Add(Contains(
                    types.Peek(),
                    fqn,
                    typeStartLines.Peek(),
                    propertyLine,
                    "property"));
            }
        }

        string? ResolveSourcePropertyName(
            SyntaxNode reference,
            string terminal,
            string receiver)
        {
            try
            {
                var symbol = model.GetSymbolInfo(reference).Symbol;
                var property = symbol as IPropertySymbol;
                if (property is null || !property.Locations.Any(location => location.IsInSource))
                {
                    // In incomplete legacy Web Site compilations the receiver
                    // can be an error type even while the target declaration
                    // is present in source. Resolve only a unique source
                    // property, or a unique candidate whose containing type
                    // matches the lexical receiver. Ambiguity stays omitted.
                    var candidates = _sourcePropertiesByName.GetValueOrDefault(terminal) ?? [];
                    if (candidates.Count == 1)
                    {
                        return SanitizeName(candidates[0].Fqn);
                    }
                    var matches = candidates.Where(candidate =>
                    {
                        var containing = candidate.ContainingType;
                        var typeName = containing.Split('.').Last();
                        return receiver.Equals(containing, StringComparison.OrdinalIgnoreCase) ||
                            receiver.EndsWith("." + containing, StringComparison.OrdinalIgnoreCase) ||
                            receiver.Equals(typeName, StringComparison.OrdinalIgnoreCase) ||
                            receiver.EndsWith("." + typeName, StringComparison.OrdinalIgnoreCase);
                    }).ToList();
                    if (matches.Count != 1)
                    {
                        return null;
                    }
                    return SanitizeName(matches[0].Fqn);
                }
                return SanitizeName(property.ToDisplayString(BareQualifiedNameFormat));
            }
            catch
            {
                // Malformed/incomplete source must degrade without inventing
                // a reference. The lexical fallback remains available to the
                // search layer, explicitly labelled as unverified evidence.
                return null;
            }
        }

        void EmitPropertyReferences(
            MethodNodeCollector collector,
            string sourceName,
            string sourceKind,
            int sourceStartLine,
            string? selfProperty = null)
        {
            // VB represents qualified type names and ordinary object members
            // with different syntax node families. Normalize both plus bare
            // same-type property reads, then deduplicate nested representations.
            var candidates = new List<(SyntaxNode Node, string Terminal, string Receiver)>();
            candidates.AddRange(collector.MemberAccesses.Select(member => (
                (SyntaxNode)member,
                member.Name?.Identifier.Text ?? string.Empty,
                member.Expression?.ToString() ?? string.Empty)));
            candidates.AddRange(collector.QualifiedNames.Select(qualified => (
                (SyntaxNode)qualified,
                qualified.Right?.Identifier.Text ?? string.Empty,
                qualified.Left?.ToString() ?? string.Empty)));
            candidates.AddRange(collector.IdentifierNames
                .Where(identifier => identifier.Parent is not MemberAccessExpressionSyntax and not QualifiedNameSyntax)
                .Select(identifier => ((SyntaxNode)identifier, identifier.Identifier.Text, string.Empty)));

            var emitted = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            foreach (var (reference, terminal, receiver) in candidates)
            {
                if (string.IsNullOrWhiteSpace(terminal) ||
                    ResolveSourcePropertyName(reference, terminal, receiver) is not { } target ||
                    selfProperty?.Equals(target, StringComparison.OrdinalIgnoreCase) == true)
                {
                    continue;
                }
                var assignment = reference.Ancestors()
                    .OfType<AssignmentStatementSyntax>()
                    .FirstOrDefault();
                var isWrite = assignment is not null && assignment.Left.Span.Contains(reference.Span);
                var line = Line(tree, reference);
                var via = isWrite ? "property_set" : "property_get";
                if (!emitted.Add($"{target}\0{via}\0{line}"))
                {
                    continue;
                }
                edges.Add(new EdgeDto
                {
                    SourceName = sourceName,
                    SourceKind = sourceKind,
                    SourceStartLine = sourceStartLine,
                    SourceLanguage = "vb",
                    TargetName = target,
                    TargetKind = "property",
                    Kind = "calls",
                    Metadata = new()
                    {
                        ["call_site_line"] = line.ToString(),
                        ["via"] = via
                    }
                });
            }
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
            // Entries on the `types` stack are already fully qualified (they
            // were produced by this method). Re-concatenating namespaces plus
            // the whole type chain duplicated every ancestor segment, e.g.
            // `_api2._api2.Logger.LogError` — and worse for deeper nesting.
            if (types.Count > 0)
            {
                return SanitizeName($"{types.Peek()}.{terminal}");
            }
            var parts = namespaces.Reverse()
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
                    return SanitizeName(symbol.ToDisplayString(BareQualifiedNameFormat));
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
        public readonly List<UnaryExpressionSyntax> AddressOfRefs = [];
        public readonly List<ObjectCreationExpressionSyntax> ObjectCreations = [];
        public readonly List<AssignmentStatementSyntax> Assignments = [];
        public readonly List<LocalDeclarationStatementSyntax> LocalDeclarations = [];
        public readonly List<WithBlockSyntax> WithBlocks = [];
        public readonly List<MemberAccessExpressionSyntax> MemberAccesses = [];
        public readonly List<QualifiedNameSyntax> QualifiedNames = [];
        public readonly List<IdentifierNameSyntax> IdentifierNames = [];
        public readonly List<ReDimStatementSyntax> ReDims = [];
        public readonly List<SyntaxNode> OnErrors = [];
        public readonly List<VariableDeclaratorSyntax> VariableDeclarators = [];
        public readonly List<SyntaxNode> TopLevelTypeBlocks = [];

        public override void VisitAddRemoveHandlerStatement(AddRemoveHandlerStatementSyntax node)
        { AddRemoveHandlers.Add(node); base.VisitAddRemoveHandlerStatement(node); }

        public override void VisitInvocationExpression(InvocationExpressionSyntax node)
        { Invocations.Add(node); base.VisitInvocationExpression(node); }

        public override void VisitUnaryExpression(UnaryExpressionSyntax node)
        {
            if (node.IsKind(SyntaxKind.AddressOfExpression)) AddressOfRefs.Add(node);
            base.VisitUnaryExpression(node);
        }

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

        public override void VisitQualifiedName(QualifiedNameSyntax node)
        { QualifiedNames.Add(node); base.VisitQualifiedName(node); }

        public override void VisitIdentifierName(IdentifierNameSyntax node)
        { IdentifierNames.Add(node); base.VisitIdentifierName(node); }

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
