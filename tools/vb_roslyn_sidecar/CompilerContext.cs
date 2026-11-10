using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.VisualBasic;
using System.Reflection.Metadata;
using System.Reflection.PortableExecutable;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.RegularExpressions;
using System.Xml;
using System.Xml.Linq;

// A deliberately partial ASP.NET Website context. Besides installed framework
// metadata, one explicitly configured Web API pair can be admitted. Other
// application/bin/package references remain unassessed.
internal sealed record CompilerContextDto
{
    [JsonPropertyName("root")] public string Root { get; init; } = "";
    [JsonPropertyName("fingerprint")] public string Fingerprint { get; init; } = "";
    [JsonPropertyName("mode")] public string Mode { get; init; } = "source_only";
    [JsonPropertyName("reference_bytes")] public long ReferenceBytes { get; init; }
    [JsonPropertyName("reason")] public string? Reason { get; init; }
}

internal sealed class CompilerContextSnapshot
{
    internal const long MaxReferenceBytes = 64L * 1024 * 1024;
    private const int MaxConfigBytes = 1024 * 1024;
    private const int MaxConfigTotal = 8 * 1024 * 1024;
    private const string WebsiteGuid = "E24C65DC-7377-472B-9ABA-BC803B73C61A";
    internal CompilerContextDto Identity { get; private set; } = new();
    internal string? WebsiteRoot { get; private set; }
    private readonly List<(string Path, long Bytes, string Hash)> references = [];
    private readonly List<string> observations = [];
    private readonly List<string> imports = [];
    private readonly Dictionary<string, object> defines = new(StringComparer.OrdinalIgnoreCase);
    private readonly Dictionary<string, string> assemblies = new(StringComparer.OrdinalIgnoreCase);
    private readonly Dictionary<string, XElement> httpRedirects = new(StringComparer.OrdinalIgnoreCase);
    private string? httpConfigurationIssue, optionalReason;
    private bool httpEnabled;
    private int configBytes;
    private bool infer = true, explicitly = true, autoVb = true, debug;
    private OptionStrict strict = OptionStrict.Off;
    private string? targetFramework, provider;
    private bool aspNetSettings = true;

    private sealed class Unsupported(string reason) : Exception(reason);
    private void Observe(string name, string value) { observations.Add(name); observations.Add(value); }
    private static bool Under(string root, string path) => string.Equals(root, path, StringComparison.OrdinalIgnoreCase)
        || path.StartsWith(root + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase);
    private static void OrdinaryFile(string path)
    {
        if ((File.GetAttributes(path) & (FileAttributes.ReparsePoint | FileAttributes.Directory)) != 0)
            throw new Unsupported("unsupported_metadata_path");
        for (var dir = Path.GetDirectoryName(path); dir is not null; dir = Path.GetDirectoryName(dir))
            if ((File.GetAttributes(dir) & FileAttributes.ReparsePoint) != 0)
                throw new Unsupported("unsupported_metadata_path");
    }
    private byte[]? ReadConfig(string path)
    {
        path = Path.GetFullPath(path);
        if (!File.Exists(path)) { Observe(path, "absent"); return null; }
        OrdinaryFile(path);
        using var input = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
        var length = input.Length;
        if (length > MaxConfigBytes || length > MaxConfigTotal - configBytes)
            throw new Unsupported("configuration_byte_limit");
        var bytes = new byte[checked((int)length)];
        input.ReadExactly(bytes);
        if (input.ReadByte() != -1 || input.Length != length) throw new Unsupported("configuration_changed_during_read");
        configBytes += bytes.Length;
        Observe(path, Convert.ToHexString(SHA256.HashData(bytes)).ToLowerInvariant());
        return bytes;
    }
    private static XDocument Xml(byte[] bytes)
    {
        using var input = new MemoryStream(bytes, writable: false);
        using var reader = XmlReader.Create(input, new XmlReaderSettings {
            DtdProcessing = DtdProcessing.Prohibit, XmlResolver = null, MaxCharactersInDocument = MaxConfigBytes });
        return XDocument.Load(reader);
    }
    private static bool Bool(string value) => bool.TryParse(value, out var result) ? result
        : throw new Unsupported("unsupported_boolean_setting");
    private void ApplyConfig(byte[]? bytes)
    {
        if (bytes is null) return;
        var root = Xml(bytes).Root;
        if (root?.Name != "configuration") throw new Unsupported("unsupported_configuration_document");
        ObserveHttpConfiguration(root);
        if (root.Elements("location").Any(location => location.Descendants().Any(e =>
            e.Name.LocalName is "compilation" or "pages" or "system.codedom")))
            throw new Unsupported("scoped_compiler_configuration");
        var web = root.Element("system.web");
        var compilation = web?.Element("compilation");
        var pages = web?.Element("pages");
        var codedom = root.Element("system.codedom");
        var assemblySettings = compilation?.Element("assemblies");
        if (new[] { web, compilation, pages, codedom, assemblySettings, pages?.Element("namespaces"), codedom?.Element("compilers") }.Where(e => e is not null)
            .Any(e => e!.Attribute("configSource") is not null || e.Attribute("file") is not null))
            throw new Unsupported("external_compiler_configuration");
        if (assemblySettings is not null)
        {
            foreach (var item in assemblySettings.Elements())
            {
                if (item.Name == "clear") { assemblies.Clear(); continue; }
                var full = (string?)item.Attribute("assembly");
                if (full is null || full.Length > 1024) throw new Unsupported("unsupported_assembly_setting");
                var name = full.Split(',')[0].Trim();
                if (item.Name == "remove") assemblies.Remove(name);
                else if (item.Name == "add")
                {
                    if (assemblies.Count >= 256) throw new Unsupported("assembly_setting_limit");
                    assemblies[name] = full;
                }
                else throw new Unsupported("unsupported_assembly_setting");
            }
        }
        if (compilation?.Attribute("targetFramework") is { } target) targetFramework = target.Value;
        if (compilation?.Attribute("debug") is { } debugOption) debug = Bool(debugOption.Value);
        if (compilation?.Attribute("explicit") is { } explicitOption) explicitly = Bool(explicitOption.Value);
        if (compilation?.Attribute("strict") is { } strictOption) strict = Bool(strictOption.Value) ? OptionStrict.On : OptionStrict.Off;
        var namespaces = pages?.Element("namespaces");
        if (namespaces?.Attribute("autoImportVBNamespace") is { } automatic) autoVb = Bool(automatic.Value);
        if (namespaces is not null)
        {
            foreach (var item in namespaces.Elements())
            {
                if (item.Name == "clear") imports.Clear();
                else if (item.Name == "remove") imports.RemoveAll(n => n.Equals((string?)item.Attribute("namespace"), StringComparison.Ordinal));
                else if (item.Name == "add" && item.Attribute("namespace") is { } name)
                {
                    if (name.Value.Length > 256 || imports.Count >= 128) throw new Unsupported("namespace_limit");
                    _ = GlobalImport.Parse(name.Value);
                    if (!imports.Contains(name.Value, StringComparer.Ordinal)) imports.Add(name.Value);
                }
                else throw new Unsupported("unsupported_namespace_setting");
            }
        }
        foreach (var compiler in codedom?.Element("compilers")?.Elements("compiler") ?? [])
        {
            var languages = ((string?)compiler.Attribute("language") ?? "").Split(';');
            if (!languages.Any(l => l.Equals("vb", StringComparison.OrdinalIgnoreCase) || l.Equals("visualbasic", StringComparison.OrdinalIgnoreCase))) continue;
            provider = (string?)compiler.Attribute("type");
            foreach (var option in compiler.Elements("providerOption"))
            {
                var name = (string?)option.Attribute("name");
                var value = (string?)option.Attribute("value") ?? "";
                if (name == "UseAspNetSettings") aspNetSettings = Bool(value);
                else if (name == "OptionInfer") infer = Bool(value);
                else if (name is not ("CompilerVersion" or "WarnAsError")) throw new Unsupported("unsupported_provider_option");
            }
            var options = (string?)compiler.Attribute("compilerOptions") ?? "";
            foreach (Match option in Regex.Matches(options, @"(?:^|\s)[/-](?<name>optioninfer|optionexplicit|optionstrict)(?<value>[+-]|:[^\s]+)?", RegexOptions.IgnoreCase))
            {
                var value = option.Groups["value"].Value.ToLowerInvariant();
                var enabled = value is "" or "+" or ":+" or ":on" or ":true";
                if (!enabled && value is not ("-" or ":-" or ":off" or ":false")) throw new Unsupported("unsupported_vb_option");
                switch (option.Groups["name"].Value.ToLowerInvariant())
                {
                    case "optioninfer": infer = enabled; break;
                    case "optionexplicit": explicitly = enabled; break;
                    case "optionstrict": strict = enabled ? OptionStrict.On : OptionStrict.Off; break;
                }
            }
            foreach (var token in options.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries))
            {
                if (Regex.IsMatch(token, @"^[/\-]option(infer|explicit|strict)([+-]|:(on|off|true|false|[+-]))?$", RegexOptions.IgnoreCase)
                    || Regex.IsMatch(token, @"^[/\-]nowarn:[0-9,]+$", RegexOptions.IgnoreCase)
                    || token.Equals("/langversion:default", StringComparison.OrdinalIgnoreCase)) continue;
                if (token.StartsWith("/define:", StringComparison.OrdinalIgnoreCase))
                {
                    foreach (var item in token[8..].Split(','))
                    {
                        var split = item.Split('=', 2); var key = split[0];
                        if (defines.Count >= 32 || !Regex.IsMatch(key, @"^[A-Za-z_][A-Za-z0-9_]{0,63}$"))
                            throw new Unsupported("unsupported_preprocessor_setting");
                        var value = split.Length == 1 ? "True" : split[1].Replace("\\\"", "\"", StringComparison.Ordinal);
                        object parsed;
                        if (bool.TryParse(value, out var boolean)) parsed = boolean;
                        else if (long.TryParse(value, System.Globalization.NumberStyles.Integer, System.Globalization.CultureInfo.InvariantCulture, out var integer)) parsed = integer;
                        else if (value.Length is >= 2 and <= 258 && value.StartsWith('"') && value.EndsWith('"') && !value[1..^1].Contains('"')) parsed = value[1..^1];
                        else throw new Unsupported("unsupported_preprocessor_setting");
                        defines[key] = parsed;
                    }
                    continue;
                }
                throw new Unsupported("unsupported_compiler_option");
            }
            // Reference/import/root-namespace overrides need their own context
            // admission. Never silently combine them with this narrow profile.
            if (Regex.IsMatch(options, @"(?:^|\s)[/-](?:imports|r|reference|rootnamespace|noconfig|nostdlib|vbruntime)(?::|\s|$)", RegexOptions.IgnoreCase))
                throw new Unsupported("unsupported_compiler_override");
        }
    }
    private static bool HttpAssembly(string? name) => name is not null
        && (name.Equals("System.Web.Http", StringComparison.OrdinalIgnoreCase)
            || name.Equals("System.Net.Http", StringComparison.OrdinalIgnoreCase));
    private static bool HasHttpRuntimePolicy(XContainer container)
    {
        XNamespace ns = "urn:schemas-microsoft-com:asm.v1";
        return container.Descendants("runtime").Any(runtime =>
            runtime.Attribute("configSource") is not null || runtime.Attribute("file") is not null
            || runtime.Descendants(ns + "assemblyIdentity").Any(identity => HttpAssembly((string?)identity.Attribute("name")))
            || runtime.Elements(ns + "assemblyBinding").Any(binding =>
                binding.Attribute("configSource") is not null || binding.Attribute("file") is not null
                || binding.Element(ns + "probing") is not null));
    }
    private void ObserveHttpConfiguration(XElement root)
    {
        XNamespace ns = "urn:schemas-microsoft-com:asm.v1";
        if (root.Elements("location").Any(HasHttpRuntimePolicy))
            httpConfigurationIssue = "website_http_scoped_configuration";
        var runtime = root.Element("runtime");
        if (runtime is null) return;
        if (root.Elements("runtime").Skip(1).Any()
            || runtime.Attribute("configSource") is not null || runtime.Attribute("file") is not null)
            httpConfigurationIssue = "website_http_external_configuration";
        foreach (var binding in runtime.Elements(ns + "assemblyBinding"))
        {
            if (binding.Element(ns + "probing") is not null)
                httpConfigurationIssue = "website_http_private_probing";
            foreach (var dependency in binding.Elements(ns + "dependentAssembly"))
            {
                var name = dependency.Elements(ns + "assemblyIdentity").Select(identity => (string?)identity.Attribute("name"))
                    .FirstOrDefault(HttpAssembly);
                if (name is null) continue;
                if (binding.Attributes().Any(a => !a.IsNamespaceDeclaration)
                    || dependency.Attributes().Any() || dependency.Elements(ns + "assemblyIdentity").Count() != 1
                    || dependency.Elements(ns + "bindingRedirect").Count() != 1
                    || dependency.Elements().Any(e => e.Name != ns + "assemblyIdentity" && e.Name != ns + "bindingRedirect")
                    || !httpRedirects.TryAdd(name, new XElement(dependency)))
                    httpConfigurationIssue = "website_http_unsupported_binding_policy";
            }
        }
    }
    private void RequireHttpRedirect(string name, string token, Version version)
    {
        XNamespace ns = "urn:schemas-microsoft-com:asm.v1";
        if (!httpRedirects.TryGetValue(name, out var dependency)) throw new Unsupported("website_http_redirect_missing");
        var identity = dependency.Element(ns + "assemblyIdentity")!;
        var redirect = dependency.Element(ns + "bindingRedirect")!;
        if (identity.Attributes().Any(a => a.Name != "name" && a.Name != "publicKeyToken" && a.Name != "culture")
            || redirect.Attributes().Any(a => a.Name != "oldVersion" && a.Name != "newVersion")
            || !string.Equals((string?)identity.Attribute("publicKeyToken"), token, StringComparison.OrdinalIgnoreCase)
            || (string?)identity.Attribute("culture") != "neutral"
            || !Version.TryParse((string?)redirect.Attribute("newVersion"), out var target) || target != version)
            throw new Unsupported("website_http_redirect_identity");
        var range = ((string?)redirect.Attribute("oldVersion") ?? "").Split('-');
        if (range.Length is < 1 or > 2 || !Version.TryParse(range[0], out var first)
            || !Version.TryParse(range[^1], out var last) || first > version || last < version || first > last)
            throw new Unsupported("website_http_redirect_range");
        Observe("http_redirect:" + name, dependency.ToString(SaveOptions.DisableFormatting));
    }
    private (string Path, long Bytes, string Hash) ReadHttpReference(string path, string name, Version version, string token, long remaining)
    {
        Observe("http_path", path);
        if (!File.Exists(path)) { Observe(path, "absent"); throw new Unsupported("website_http_reference_missing"); }
        OrdinaryFile(path);
        using var input = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read);
        var length = input.Length;
        if (length <= 0 || length > 16 * 1024 * 1024 || length > remaining)
            throw new Unsupported("website_http_reference_byte_limit");
        var hash = HashReference(input, length);
        Observe(path, hash);
        input.Position = 0;
        using var image = new PEReader(input, PEStreamOptions.LeaveOpen);
        var metadataBytes = image.PEHeaders.CorHeader?.MetadataDirectory.Size ?? 0;
        if (metadataBytes <= 0) throw new Unsupported("website_http_metadata_invalid");
        if (metadataBytes > MaxConfigBytes) throw new Unsupported("website_http_metadata_byte_limit");
        var metadata = image.GetMetadataReader();
        if (!metadata.IsAssembly) throw new Unsupported("website_http_metadata_invalid");
        var assembly = metadata.GetAssemblyDefinition();
        var publicKey = metadata.GetBlobBytes(assembly.PublicKey);
        var actualToken = publicKey.Length == 0 ? "" : Convert.ToHexString(SHA1.HashData(publicKey)[^8..].Reverse().ToArray()).ToLowerInvariant();
        if ((assembly.Flags & System.Reflection.AssemblyFlags.PublicKey) == 0
            || !metadata.GetString(assembly.Name).Equals(name, StringComparison.OrdinalIgnoreCase)
            || assembly.Version != version || metadata.GetString(assembly.Culture) != "" || actualToken != token)
            throw new Unsupported("website_http_reference_identity");
        // This is a metadata identity check, not assembly execution or a claim
        // that all dependencies are resolved. Admit only the demonstrated pair.
        if (name == "System.Web.Http")
        {
            var dependencies = metadata.AssemblyReferences.Select(metadata.GetAssemblyReference)
                .Where(reference => metadata.GetString(reference.Name).Equals("System.Net.Http", StringComparison.OrdinalIgnoreCase)).ToArray();
            if (dependencies.Length != 1 || dependencies[0].Version != new Version(4, 0, 0, 0)
                || metadata.GetString(dependencies[0].Culture) != ""
                || (dependencies[0].Flags & System.Reflection.AssemblyFlags.PublicKey) != 0
                || Convert.ToHexString(metadata.GetBlobBytes(dependencies[0].PublicKeyOrToken)).ToLowerInvariant() != "b03f5f7f11d50a3a")
                throw new Unsupported("website_http_dependency_identity");
        }
        Observe("http_identity", name + "," + version + ",neutral," + actualToken);
        return (path, length, hash);
    }
    private void TryHttpProfile()
    {
        var web = Path.Combine(WebsiteRoot!, "bin", "System.Web.Http.dll");
        if (!assemblies.ContainsKey("System.Net.Http") && httpRedirects.Count == 0
            && httpConfigurationIssue is null && !File.Exists(web)) return;
        try
        {
            if (httpConfigurationIssue is not null) throw new Unsupported(httpConfigurationIssue);
            if (!assemblies.TryGetValue("System.Net.Http", out var declared)) throw new Unsupported("website_http_reference_not_enabled");
            var parts = declared.Split(',').Select(part => part.Trim()).ToArray();
            if (parts.Length != 4 || new[] { "Version", "Culture", "PublicKeyToken" }.Any(key =>
                parts.Skip(1).Count(part => part.StartsWith(key + "=", StringComparison.OrdinalIgnoreCase)) != 1))
                throw new Unsupported("website_http_declaration_identity");
            var identity = new System.Reflection.AssemblyName(declared);
            if (!string.Equals(identity.Name, "System.Net.Http", StringComparison.OrdinalIgnoreCase)
                || identity.Version != new Version(4, 0, 0, 0) || !string.IsNullOrEmpty(identity.CultureName)
                || Convert.ToHexString(identity.GetPublicKeyToken() ?? []).ToLowerInvariant() != "b03f5f7f11d50a3a")
                throw new Unsupported("website_http_declaration_identity");
            RequireHttpRedirect("System.Web.Http", "31bf3856ad364e35", new Version(5, 3, 0, 0));
            RequireHttpRedirect("System.Net.Http", "b03f5f7f11d50a3a", new Version(4, 0, 0, 0));
            var privateNet = Path.Combine(WebsiteRoot!, "bin", "System.Net.Http.dll");
            var overridden = true;
            try { _ = File.GetAttributes(privateNet); }
            catch (FileNotFoundException) { overridden = false; }
            catch (DirectoryNotFoundException) { overridden = false; }
            Observe(privateNet, overridden ? "present_unsupported_override" : "absent");
            if (overridden) throw new Unsupported("website_http_private_net_override");
            var net = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.Windows), "Microsoft.NET", "Framework64", "v4.0.30319", "System.Net.Http.dll");
            var remaining = MaxReferenceBytes - references.Sum(r => r.Bytes);
            var webReference = ReadHttpReference(web, "System.Web.Http", new Version(5, 3, 0, 0), "31bf3856ad364e35", remaining);
            var netReference = ReadHttpReference(net, "System.Net.Http", new Version(4, 0, 0, 0), "b03f5f7f11d50a3a", remaining - webReference.Bytes);
            references.Add(webReference); references.Add(netReference); httpEnabled = true;
        }
        catch (Unsupported error) { optionalReason = error.Message; }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or ArgumentException
            or InvalidOperationException or BadImageFormatException or System.Security.SecurityException)
        { optionalReason = "website_http_metadata_unavailable"; }
        Observe("http_profile", optionalReason ?? "admitted");
    }
    private static bool Skipped(string name) => name.StartsWith(".audit", StringComparison.OrdinalIgnoreCase)
        || new[] { ".git", ".hg", ".svn", ".vs", ".vscode", "bin", "obj", "packages", "node_modules", "vendor" }
            .Contains(name, StringComparer.OrdinalIgnoreCase);
    private static bool HasScopedCompilerSettings(XDocument document) => document.Descendants().Any(e =>
        e.Name.LocalName is "compilation" or "system.codedom"
        || (e.Name.LocalName == "pages" && (e.Element("namespaces") is not null || e.Attribute("configSource") is not null))
        || (e.Name.LocalName == "system.web" && e.Attribute("configSource") is not null));
    private void CheckDescendantConfigurations()
    {
        var pending = new Queue<string>(); pending.Enqueue(WebsiteRoot!);
        var directories = 0; var configs = 0;
        while (pending.Count != 0)
        {
            var directory = pending.Dequeue();
            if (++directories > 8192) throw new Unsupported("configuration_directory_limit");
            if ((File.GetAttributes(directory) & FileAttributes.ReparsePoint) != 0)
                throw new Unsupported("unsupported_metadata_path");
            var config = Path.Combine(directory, "web.config");
            if (File.Exists(config))
            {
                if (++configs > 256) throw new Unsupported("configuration_count_limit");
                if (!directory.Equals(WebsiteRoot, StringComparison.OrdinalIgnoreCase))
                {
                    var bytes = ReadConfig(config)!;
                    var document = Xml(bytes);
                    if (HasScopedCompilerSettings(document)) throw new Unsupported("descendant_compiler_configuration");
                    if (HasHttpRuntimePolicy(document)) httpConfigurationIssue = "website_http_scoped_configuration";
                }
            }
            // Bound enumeration before sorting; never recurse into source files
            // or standard output/vendor folders. No source body is opened.
            var children = Directory.EnumerateDirectories(directory).Take(8193).ToArray();
            if (children.Length + pending.Count + directories > 8192)
                throw new Unsupported("configuration_directory_limit");
            foreach (var child in children.Where(p => !Skipped(Path.GetFileName(p))).OrderBy(p => p, StringComparer.OrdinalIgnoreCase)) pending.Enqueue(child);
        }
    }
    private static string HashReference(Stream input, long length)
    {
        using var hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
        var buffer = new byte[32768]; long remaining = length;
        while (remaining != 0)
        {
            var count = input.Read(buffer, 0, (int)Math.Min(buffer.Length, remaining));
            if (count == 0) throw new Unsupported("framework_reference_changed");
            hash.AppendData(buffer, 0, count); remaining -= count;
        }
        if (input.ReadByte() != -1 || input.Length != length) throw new Unsupported("framework_reference_changed");
        return Convert.ToHexString(hash.GetHashAndReset()).ToLowerInvariant();
    }
    private void Discover(string root)
    {
        if (!OperatingSystem.IsWindows() || !Environment.Is64BitProcess) throw new Unsupported("unsupported_platform");
        if (!Directory.Exists(root)) throw new Unsupported("root_unavailable");
        var direct = ReadConfig(Path.Combine(root, "web.config"));
        string? solutionTarget = null;
        if (direct is not null) WebsiteRoot = root;
        else
        {
            // Top-level solutions only; never enumerate source trees.
            var solutions = Directory.EnumerateFiles(root, "*.sln", SearchOption.TopDirectoryOnly).Take(17).OrderBy(p => p, StringComparer.OrdinalIgnoreCase).ToArray();
            if (solutions.Length > 16) throw new Unsupported("solution_limit");
            Observe("solutions", JsonSerializer.Serialize(solutions));
            if (solutions.Length != 1) throw new Unsupported("website_solution_not_unique");
            var text = Encoding.UTF8.GetString(ReadConfig(solutions[0])!);
            var websites = Regex.Matches(text, "Project\\(\"\\{" + WebsiteGuid + "\\}\\\"\\)\\s*=\\s*\"[^\"]*\",\\s*\"(?<path>[^\"]+)\"[^\\r\\n]*\\r?\\n(?<body>.*?)EndProject", RegexOptions.IgnoreCase | RegexOptions.Singleline | RegexOptions.NonBacktracking);
            if (websites.Count != 1) throw new Unsupported("website_project_not_unique");
            var relative = websites[0].Groups["path"].Value.Replace('\\', Path.DirectorySeparatorChar);
            if (Path.IsPathRooted(relative) || relative.Contains(':')) throw new Unsupported("unsupported_website_path");
            WebsiteRoot = Path.TrimEndingDirectorySeparator(Path.GetFullPath(Path.Combine(root, relative)));
            if (!Under(root, WebsiteRoot)) throw new Unsupported("website_outside_root");
            var target = Regex.Match(websites[0].Groups["body"].Value, "TargetFrameworkMoniker\\s*=\\s*\"(?<value>[^\"]+)\"", RegexOptions.IgnoreCase);
            if (!target.Success) throw new Unsupported("website_framework_unspecified");
            solutionTarget = target.Groups["value"].Value.Replace("%3D", "=", StringComparison.OrdinalIgnoreCase);
        }
        var framework = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.Windows), "Microsoft.NET", "Framework64", "v4.0.30319", "Config");
        var machine = ReadConfig(Path.Combine(framework, "machine.config"));
        var inherited = ReadConfig(Path.Combine(framework, "web.config"));
        if (machine is null || inherited is null) throw new Unsupported("framework_configuration_missing");
        ApplyConfig(machine); ApplyConfig(inherited);
        if (direct is not null) ApplyConfig(direct);
        else
        {
            var relative = Path.GetRelativePath(root, WebsiteRoot!);
            var current = root;
            var parts = relative.Split(Path.DirectorySeparatorChar);
            if (parts.Length > 16) throw new Unsupported("website_path_depth_limit");
            foreach (var part in parts) { current = Path.Combine(current, part); ApplyConfig(ReadConfig(Path.Combine(current, "web.config"))); }
        }
        if (targetFramework != "4.8" || (solutionTarget is not null && !solutionTarget.Equals(".NETFramework,Version=v4.8", StringComparison.OrdinalIgnoreCase)))
            throw new Unsupported("unsupported_target_framework");
        if (provider is null || !provider.StartsWith("Microsoft.CodeDom.Providers.DotNetCompilerPlatform.VBCodeProvider,", StringComparison.Ordinal)
            || !aspNetSettings) throw new Unsupported("unsupported_vb_provider");
        foreach (var name in new[] { "System", "System.Core", "System.Data", "System.Data.Linq", "System.Web" })
        {
            if (!assemblies.TryGetValue(name, out var declared)) throw new Unsupported("required_framework_reference_not_enabled");
            var version = Regex.Match(declared, @"(?:^|,)\s*Version=(?<value>[^,]+)", RegexOptions.IgnoreCase);
            if (version.Success && version.Groups["value"].Value.Trim() != "4.0.0.0")
                throw new Unsupported("unsupported_framework_reference_version");
        }
        // ASP.NET CodeDom emits these before the explicit compiler options;
        // user /define values therefore retain precedence over the defaults.
        if (!defines.ContainsKey("_MYTYPE")) defines["_MYTYPE"] = "Web";
        if (debug && !defines.ContainsKey("DEBUG")) defines["DEBUG"] = 1;
        CheckDescendantConfigurations();
        if (autoVb && !imports.Contains("Microsoft.VisualBasic", StringComparer.Ordinal)) imports.Add("Microsoft.VisualBasic");
        Observe("effective", JsonSerializer.Serialize(new { WebsiteRoot, targetFramework, infer, explicitly, strict, debug, imports, defines, assemblies }));
        var refRoot = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ProgramFilesX86), "Reference Assemblies", "Microsoft", "Framework", ".NETFramework", "v4.8");
        long total = 0;
        foreach (var name in new[] { "mscorlib", "System", "System.Core", "Microsoft.VisualBasic", "System.Data", "System.Data.Linq", "System.Web" })
        {
            var path = Path.Combine(refRoot, name + ".dll");
            if (!File.Exists(path)) { Observe(path, "absent"); throw new Unsupported("framework_reference_missing"); }
            OrdinaryFile(path);
            using var input = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
            var bytes = input.Length;
            if (bytes <= 0 || bytes > 16 * 1024 * 1024 || bytes > MaxReferenceBytes - total) throw new Unsupported("framework_reference_byte_limit");
            var hash = HashReference(input, bytes);
            if (input.Position != bytes || input.Length != bytes) throw new Unsupported("framework_reference_changed");
            total += bytes; references.Add((path, bytes, hash)); Observe(path, hash);
        }
        TryHttpProfile();
    }
    internal static CompilerContextSnapshot Read(string projectRoot)
    {
        var snapshot = new CompilerContextSnapshot();
        var root = Path.TrimEndingDirectorySeparator(Path.GetFullPath(projectRoot));
        snapshot.Observe("root", root);
        string? reason = null;
        try { snapshot.Discover(root); }
        catch (Unsupported error) { reason = error.Message; }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or XmlException or ArgumentException or InvalidOperationException or System.Security.SecurityException)
        { reason = "compiler_context_unavailable"; }
        snapshot.Observe("result", reason ?? (snapshot.httpEnabled ? "framework_plus_web_http" : "framework_only"));
        snapshot.Identity = new CompilerContextDto { Root = root,
            Fingerprint = Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(JsonSerializer.Serialize(snapshot.observations)))).ToLowerInvariant(),
            Mode = reason is null ? (snapshot.httpEnabled ? "framework_plus_web_http" : "framework_only") : "source_only", Reason = reason ?? snapshot.optionalReason,
            ReferenceBytes = reason is null ? snapshot.references.Sum(r => r.Bytes) : 0 };
        return snapshot;
    }
    private bool HasMetadata => Identity.Mode is "framework_only" or "framework_plus_web_http";
    internal bool AdmitsSource(string path) => !HasMetadata || (WebsiteRoot is not null && Under(WebsiteRoot, path)
        && !Path.GetRelativePath(WebsiteRoot, path).Split(Path.DirectorySeparatorChar).SkipLast(1).Any(Skipped));
    internal VisualBasicParseOptions ParseOptions => HasMetadata
        ? VisualBasicParseOptions.Default.WithPreprocessorSymbols(defines)
        : VisualBasicParseOptions.Default;
    internal VisualBasicCompilation Apply(VisualBasicCompilation compilation)
    {
        if (!HasMetadata) return compilation;
        var loaded = new List<MetadataReference>(references.Count);
        foreach (var reference in references)
        {
            OrdinaryFile(reference.Path);
            using var input = new FileStream(reference.Path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
            if (input.Length != reference.Bytes) throw new IOException("Compiler metadata changed after admission");
            var bytes = new byte[checked((int)reference.Bytes)]; input.ReadExactly(bytes);
            if (input.ReadByte() != -1 || input.Length != reference.Bytes ||
                !Convert.ToHexString(SHA256.HashData(bytes)).Equals(reference.Hash, StringComparison.OrdinalIgnoreCase))
                throw new IOException("Compiler metadata changed after admission");
            loaded.Add(MetadataReference.CreateFromImage(bytes, filePath: reference.Path));
        }
        return compilation.WithOptions(compilation.Options.WithOptionInfer(infer).WithOptionExplicit(explicitly)
            .WithOptionStrict(strict).WithGlobalImports(imports.Select(GlobalImport.Parse))).WithReferences(loaded);
    }
}
