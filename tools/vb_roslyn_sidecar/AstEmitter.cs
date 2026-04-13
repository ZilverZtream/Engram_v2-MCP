using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.Text;
using Microsoft.CodeAnalysis.VisualBasic;
using Microsoft.CodeAnalysis.VisualBasic.Syntax;

internal static class AstEmitter
{
    public static (List<SymbolDto>, List<EdgeDto>) Extract(string path, string source)
    {
        var tree = VisualBasicSyntaxTree.ParseText(SourceText.From(source), path: path);
        var root = tree.GetCompilationUnitRoot();

        var symbols = new List<SymbolDto>();
        var edges = new List<EdgeDto>();
        var namespaces = new Stack<string>();
        var types = new Stack<string>();
        string fileNode = string.IsNullOrWhiteSpace(path) ? "<memory>.vb" : path;

        if (root.Options.Any())
        {
            var strict = root.Options.FirstOrDefault(o => o.Name.ToString().Equals("Strict", StringComparison.OrdinalIgnoreCase));
            if (strict is not null)
            {
                symbols.Add(new SymbolDto
                {
                    Name = fileNode,
                    Kind = "file",
                    StartLine = 1,
                    EndLine = 1,
                    Metadata = new() { ["option_strict"] = strict.ValueKeyword.ToString() }
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

            symbols.Add(new SymbolDto
            {
                Name = fqn,
                Kind = "function",
                StartLine = Line(tree, node),
                EndLine = EndLine(tree, node),
                Metadata = metadata.Count == 0 ? null : metadata,
            });
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
                        SourceKind = "control",
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
                    edges.Add(new EdgeDto
                    {
                        SourceName = add.EventExpression.ToString(),
                        SourceKind = "control",
                        SourceStartLine = Line(tree, add),
                        SourceLanguage = "vb",
                        TargetName = add.DelegateExpression.ToString(),
                        Kind = "event_wiring",
                        Metadata = new() { ["wiring"] = "AddHandler", ["fqn"] = fqn }
                    });
                }
            }

            foreach (var inv in node.DescendantNodes().OfType<InvocationExpressionSyntax>())
            {
                edges.Add(new EdgeDto
                {
                    SourceName = fqn,
                    SourceKind = "function",
                    SourceStartLine = Line(tree, inv),
                    SourceLanguage = "vb",
                    TargetName = inv.Expression.ToString(),
                    TargetKind = "function",
                    Kind = "calls",
                    Metadata = new() { ["unresolved"] = "true" }
                });
            }

            foreach (var child in node.ChildNodes()) Walk(child);
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

        Walk(root);
        return (symbols, edges);

        string ComposeName(string terminal)
        {
            var parts = new List<string>();
            if (namespaces.Count > 0) parts.AddRange(namespaces.Reverse());
            if (types.Count > 0) parts.AddRange(types.Reverse());
            parts.Add(terminal);
            return string.Join('.', parts.Where(p => !string.IsNullOrWhiteSpace(p)));
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
