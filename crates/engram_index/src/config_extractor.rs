/// ASP.NET web.config / app.config XML parser.
///
/// Extracts middleware registrations and application settings:
///   - `<httpModules>` / `<system.webServer><modules>` → `http_module` symbols + `registers_module` edges
///   - `<httpHandlers>` / `<system.webServer><handlers>` → `route_handler` symbols + `registers_handler` edges
///   - `<appSettings>` → `app_setting` symbols
///   - `<connectionStrings>` → `connection_string` symbols (name only, no secrets)
///
/// Edge source is the config file itself; target is the FQN of the module/handler class.
use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use engram_core::RelPath;
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::collections::HashMap;

/// Extract symbols and edges from a web.config or app.config file.
pub fn extract_web_config(
    rel_path: &RelPath,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut edges: Vec<ExtractedEdge> = Vec::new();

    let mut reader = Reader::from_str(source);

    // XML path stack for context awareness.
    let mut path_stack: Vec<String> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let tag = local_name_str(e);
                path_stack.push(tag);
            }
            Ok(Event::Empty(ref e)) => {
                let tag = local_name_str(e);
                // Process self-closing <add .../> elements in context.
                let context = xml_path(&path_stack, &tag);
                process_element(&context, e, rel_path, &mut symbols, &mut edges);
            }
            Ok(Event::End(_)) => {
                path_stack.pop();
            }
            Ok(Event::Eof) => break,
            Err(err) => {
                tracing::warn!(
                    "web.config XML parse error in {}: {}",
                    rel_path.as_str(),
                    err
                );
                break;
            }
            _ => {}
        }
    }

    (symbols, edges)
}

/// Build a `/`-separated XML context path for matching.
fn xml_path(stack: &[String], current_tag: &str) -> String {
    let mut path = String::new();
    for s in stack {
        path.push_str(&s.to_lowercase());
        path.push('/');
    }
    path.push_str(&current_tag.to_lowercase());
    path
}

/// Get the local tag name as a lowercase String.
fn local_name_str(e: &BytesStart) -> String {
    String::from_utf8_lossy(e.local_name().as_ref()).to_lowercase()
}

/// Read a UTF-8 attribute value by key from an XML element.
fn attr_value(e: &BytesStart, key: &[u8]) -> Option<String> {
    for attr in e.attributes().flatten() {
        if attr.key.as_ref() == key {
            return String::from_utf8(attr.value.to_vec()).ok();
        }
    }
    None
}

/// Process a self-closing `<add .../>` (or similar) element based on its XML context.
fn process_element(
    context: &str,
    e: &BytesStart,
    rel_path: &RelPath,
    symbols: &mut Vec<ExtractedSymbol>,
    edges: &mut Vec<ExtractedEdge>,
) {
    // ── httpModules / system.webServer modules ────────────────────────────
    if (context.contains("httpmodules/add") || context.contains("modules/add"))
        && let Some(type_fqn) = attr_value(e, b"type")
    {
        let name = attr_value(e, b"name").unwrap_or_else(|| type_fqn.clone());
        let class_name = extract_class_name(&type_fqn);

        let mut meta = HashMap::new();
        meta.insert("type".into(), type_fqn.clone());
        if let Some(asm) = extract_assembly(&type_fqn) {
            meta.insert("assembly".into(), asm);
        }

        symbols.push(ExtractedSymbol {
            name: name.clone(),
            kind: "http_module".to_string(),
            start_line: 0,
            end_line: 0,
            metadata: Some(meta),
        });

        edges.push(ExtractedEdge {
            source_name: rel_path.as_str().to_string(),
            source_kind: "file".to_string(),
            source_start_line: 0,
            source_language: "xml".to_string(),
            target_name: class_name,
            target_kind: Some("class".to_string()),
            target_start_line: None,
            kind: "registers_module".to_string(),
            metadata: Some(HashMap::from([("module_name".into(), name)])),
        });
    }

    // ── httpHandlers / system.webServer handlers ──────────────────────────
    if (context.contains("httphandlers/add") || context.contains("handlers/add"))
        && let Some(type_fqn) = attr_value(e, b"type")
    {
        let verb = attr_value(e, b"verb").unwrap_or_default();
        let path_pattern = attr_value(e, b"path").unwrap_or_default();
        let name = attr_value(e, b"name").unwrap_or_else(|| format!("{} {}", verb, path_pattern));
        let class_name = extract_class_name(&type_fqn);

        let mut meta = HashMap::new();
        meta.insert("type".into(), type_fqn.clone());
        if !verb.is_empty() {
            meta.insert("verb".into(), verb);
        }
        if !path_pattern.is_empty() {
            meta.insert("path".into(), path_pattern);
        }
        if let Some(asm) = extract_assembly(&type_fqn) {
            meta.insert("assembly".into(), asm);
        }

        symbols.push(ExtractedSymbol {
            name: name.clone(),
            kind: "route_handler".to_string(),
            start_line: 0,
            end_line: 0,
            metadata: Some(meta),
        });

        edges.push(ExtractedEdge {
            source_name: rel_path.as_str().to_string(),
            source_kind: "file".to_string(),
            source_start_line: 0,
            source_language: "xml".to_string(),
            target_name: class_name,
            target_kind: Some("class".to_string()),
            target_start_line: None,
            kind: "registers_handler".to_string(),
            metadata: Some(HashMap::from([("handler_name".into(), name)])),
        });
    }

    // ── appSettings ──────────────────────────────────────────────────────
    if context.contains("appsettings/add")
        && let Some(key) = attr_value(e, b"key")
    {
        let value = attr_value(e, b"value").unwrap_or_default();

        let mut meta = HashMap::new();
        meta.insert("key".into(), key.clone());
        meta.insert("value".into(), value);

        symbols.push(ExtractedSymbol {
            name: key,
            kind: "app_setting".to_string(),
            start_line: 0,
            end_line: 0,
            metadata: Some(meta),
        });
    }

    // ── connectionStrings ────────────────────────────────────────────────
    if context.contains("connectionstrings/add")
        && let Some(name) = attr_value(e, b"name")
    {
        let provider = attr_value(e, b"providerName").unwrap_or_default();

        let mut meta = HashMap::new();
        meta.insert("name".into(), name.clone());
        if !provider.is_empty() {
            meta.insert("provider".into(), provider);
        }
        // Intentionally do NOT store connectionString value (secrets).

        symbols.push(ExtractedSymbol {
            name,
            kind: "connection_string".to_string(),
            start_line: 0,
            end_line: 0,
            metadata: Some(meta),
        });
    }
}

/// Extract the class name (FQN without assembly) from a .NET type string.
///
/// Input: `"Namespace.ClassName, AssemblyName"` → `"Namespace.ClassName"`
/// Input: `"Namespace.ClassName"` → `"Namespace.ClassName"`
fn extract_class_name(type_str: &str) -> String {
    type_str
        .split(',')
        .next()
        .unwrap_or(type_str)
        .trim()
        .to_string()
}

/// Extract the assembly name from a .NET type string.
///
/// Input: `"Namespace.ClassName, AssemblyName"` → `Some("AssemblyName")`
/// Input: `"Namespace.ClassName"` → `None`
fn extract_assembly(type_str: &str) -> Option<String> {
    let parts: Vec<&str> = type_str.splitn(2, ',').collect();
    if parts.len() > 1 {
        Some(parts[1].trim().to_string())
    } else {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_httpmodules_extraction() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.web>
    <httpModules>
      <add name="AuthModule" type="MyApp.Security.AuthModule, MyApp" />
      <add name="LoggingModule" type="MyApp.Logging.RequestLogger, MyApp" />
    </httpModules>
  </system.web>
</configuration>"#;

        let rel = RelPath::new("web.config");
        let (syms, edges) = extract_web_config(&rel, xml);

        assert_eq!(syms.len(), 2, "Should find 2 httpModules");
        assert_eq!(edges.len(), 2, "Should find 2 registers_module edges");

        let auth = syms.iter().find(|s| s.name == "AuthModule").unwrap();
        assert_eq!(auth.kind, "http_module");
        let meta = auth.metadata.as_ref().unwrap();
        assert_eq!(meta["type"], "MyApp.Security.AuthModule, MyApp");

        let auth_edge = edges
            .iter()
            .find(|e| e.target_name == "MyApp.Security.AuthModule")
            .unwrap();
        assert_eq!(auth_edge.kind, "registers_module");
    }

    #[test]
    fn test_system_webserver_modules() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.webServer>
    <modules>
      <add name="UrlRewrite" type="MyApp.Routing.UrlRewriteModule, MyApp" />
    </modules>
  </system.webServer>
</configuration>"#;

        let rel = RelPath::new("web.config");
        let (syms, edges) = extract_web_config(&rel, xml);

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].kind, "http_module");
        assert_eq!(edges[0].kind, "registers_module");
        assert_eq!(edges[0].target_name, "MyApp.Routing.UrlRewriteModule");
    }

    #[test]
    fn test_httphandlers_extraction() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.web>
    <httpHandlers>
      <add verb="*" path="*.report" type="MyApp.Handlers.ReportHandler, MyApp" />
    </httpHandlers>
  </system.web>
</configuration>"#;

        let rel = RelPath::new("web.config");
        let (syms, edges) = extract_web_config(&rel, xml);

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].kind, "route_handler");
        let meta = syms[0].metadata.as_ref().unwrap();
        assert_eq!(meta.get("verb").unwrap(), "*");
        assert_eq!(meta.get("path").unwrap(), "*.report");

        assert_eq!(edges[0].kind, "registers_handler");
        assert_eq!(edges[0].target_name, "MyApp.Handlers.ReportHandler");
    }

    #[test]
    fn test_app_settings() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <appSettings>
    <add key="SiteName" value="My Application" />
    <add key="MaxRetries" value="3" />
  </appSettings>
</configuration>"#;

        let rel = RelPath::new("web.config");
        let (syms, edges) = extract_web_config(&rel, xml);

        assert_eq!(syms.len(), 2, "Should find 2 app settings");
        assert_eq!(edges.len(), 0, "App settings don't produce edges");

        let site = syms.iter().find(|s| s.name == "SiteName").unwrap();
        assert_eq!(site.kind, "app_setting");
        let meta = site.metadata.as_ref().unwrap();
        assert_eq!(meta["value"], "My Application");
    }

    #[test]
    fn test_connection_strings() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <connectionStrings>
    <add name="MainDb" connectionString="Server=...;Database=MyDB" providerName="System.Data.SqlClient" />
  </connectionStrings>
</configuration>"#;

        let rel = RelPath::new("web.config");
        let (syms, _) = extract_web_config(&rel, xml);

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "MainDb");
        assert_eq!(syms[0].kind, "connection_string");
        let meta = syms[0].metadata.as_ref().unwrap();
        assert_eq!(meta["provider"], "System.Data.SqlClient");
        // Connection string value should NOT be stored.
        assert!(
            meta.get("connectionString").is_none(),
            "Should not store sensitive connection string value"
        );
    }

    #[test]
    fn test_comments_ignored() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <!-- This is a comment -->
  <appSettings>
    <!-- <add key="Disabled" value="should not appear" /> -->
    <add key="Active" value="yes" />
  </appSettings>
</configuration>"#;

        let rel = RelPath::new("web.config");
        let (syms, _) = extract_web_config(&rel, xml);

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "Active");
    }

    #[test]
    fn test_combined_config() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <appSettings>
    <add key="Debug" value="true" />
  </appSettings>
  <connectionStrings>
    <add name="Db" connectionString="..." />
  </connectionStrings>
  <system.web>
    <httpModules>
      <add name="Auth" type="App.AuthModule, App" />
    </httpModules>
    <httpHandlers>
      <add verb="GET" path="api/*" type="App.ApiHandler, App" />
    </httpHandlers>
  </system.web>
  <system.webServer>
    <modules>
      <add name="Cors" type="App.CorsModule, App" />
    </modules>
  </system.webServer>
</configuration>"#;

        let rel = RelPath::new("web.config");
        let (syms, edges) = extract_web_config(&rel, xml);

        // 1 app_setting + 1 connection_string + 2 http_module + 1 route_handler = 5
        assert_eq!(syms.len(), 5, "Should find all 5 symbols");
        // 2 registers_module + 1 registers_handler = 3
        assert_eq!(edges.len(), 3, "Should find 3 registration edges");

        let modules: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "registers_module")
            .collect();
        let handlers: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "registers_handler")
            .collect();
        assert_eq!(modules.len(), 2);
        assert_eq!(handlers.len(), 1);
    }

    // ── New tests ──────────────────────────────────────────────────────────────

    #[test]
    fn extract_app_setting_key_value() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <appSettings>
    <add key="MaxPageSize" value="100" />
  </appSettings>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, _) = extract_web_config(&rel, xml);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "MaxPageSize");
        assert_eq!(syms[0].kind, "app_setting");
        let meta = syms[0].metadata.as_ref().unwrap();
        assert_eq!(meta["key"], "MaxPageSize");
        assert_eq!(meta["value"], "100");
    }

    #[test]
    fn extract_multiple_app_settings() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <appSettings>
    <add key="Key1" value="Val1" />
    <add key="Key2" value="Val2" />
    <add key="Key3" value="Val3" />
  </appSettings>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, _) = extract_web_config(&rel, xml);
        assert_eq!(syms.len(), 3);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Key1"));
        assert!(names.contains(&"Key2"));
        assert!(names.contains(&"Key3"));
        assert!(syms.iter().all(|s| s.kind == "app_setting"));
    }

    #[test]
    fn extract_connection_string_name() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <connectionStrings>
    <add name="AppDatabase" connectionString="Server=.;Database=AppDb" />
  </connectionStrings>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, _) = extract_web_config(&rel, xml);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "AppDatabase");
        assert_eq!(syms[0].kind, "connection_string");
    }

    #[test]
    fn extract_connection_string_provider() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <connectionStrings>
    <add name="OracleDb" connectionString="Data Source=..." providerName="Oracle.ManagedDataAccess.Client" />
  </connectionStrings>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, _) = extract_web_config(&rel, xml);
        assert_eq!(syms.len(), 1);
        let meta = syms[0].metadata.as_ref().unwrap();
        assert_eq!(meta["provider"], "Oracle.ManagedDataAccess.Client");
    }

    #[test]
    fn connection_string_value_not_stored() {
        // Sensitive connection string value must never be persisted
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <connectionStrings>
    <add name="Secure" connectionString="Password=SuperSecret123;Server=prod;" providerName="System.Data.SqlClient" />
  </connectionStrings>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, _) = extract_web_config(&rel, xml);
        assert_eq!(syms.len(), 1);
        let meta = syms[0].metadata.as_ref().unwrap();
        assert!(
            meta.get("connectionString").is_none(),
            "connection string value must not be stored"
        );
        // provider is stored, name is stored, but the raw value is not
        assert!(meta.get("provider").is_some());
    }

    #[test]
    fn connection_string_without_provider_has_no_provider_key() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <connectionStrings>
    <add name="MinimalDb" connectionString="Server=.;Database=X" />
  </connectionStrings>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, _) = extract_web_config(&rel, xml);
        assert_eq!(syms.len(), 1);
        let meta = syms[0].metadata.as_ref().unwrap();
        // No providerName attribute → no "provider" key in metadata
        assert!(
            meta.get("provider").is_none(),
            "provider key should be absent when providerName is missing"
        );
    }

    #[test]
    fn extract_http_module_type_and_assembly() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.web>
    <httpModules>
      <add name="TraceModule" type="MyApp.Diagnostics.TraceModule, MyApp.Core" />
    </httpModules>
  </system.web>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, edges) = extract_web_config(&rel, xml);

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].kind, "http_module");
        assert_eq!(syms[0].name, "TraceModule");
        let meta = syms[0].metadata.as_ref().unwrap();
        assert_eq!(meta["type"], "MyApp.Diagnostics.TraceModule, MyApp.Core");
        assert_eq!(meta["assembly"], "MyApp.Core");

        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "registers_module");
        assert_eq!(edges[0].target_name, "MyApp.Diagnostics.TraceModule");
        assert_eq!(edges[0].source_kind, "file");
        assert_eq!(edges[0].target_kind.as_deref(), Some("class"));
    }

    #[test]
    fn extract_http_handler_verb_path_type() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.web>
    <httpHandlers>
      <add verb="POST" path="upload.axd" type="MyApp.Upload.UploadHandler, MyApp" />
    </httpHandlers>
  </system.web>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, edges) = extract_web_config(&rel, xml);

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].kind, "route_handler");
        let meta = syms[0].metadata.as_ref().unwrap();
        assert_eq!(meta["verb"], "POST");
        assert_eq!(meta["path"], "upload.axd");
        assert_eq!(meta["type"], "MyApp.Upload.UploadHandler, MyApp");

        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "registers_handler");
        assert_eq!(edges[0].target_name, "MyApp.Upload.UploadHandler");
    }

    #[test]
    fn handler_name_attribute_used_when_present() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.webServer>
    <handlers>
      <add name="ApiHandler" verb="*" path="api/*" type="MyApp.ApiHandler, MyApp" />
    </handlers>
  </system.webServer>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, _) = extract_web_config(&rel, xml);

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "ApiHandler");
    }

    #[test]
    fn handler_name_falls_back_to_verb_path_when_no_name_attr() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.web>
    <httpHandlers>
      <add verb="GET" path="*.rpt" type="MyApp.ReportHandler, MyApp" />
    </httpHandlers>
  </system.web>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, _) = extract_web_config(&rel, xml);

        assert_eq!(syms.len(), 1);
        // Name falls back to "VERB PATH"
        assert_eq!(syms[0].name, "GET *.rpt");
    }

    #[test]
    fn missing_config_keys_handled_gracefully() {
        // An <add /> element with no attributes should not crash
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <appSettings>
    <add />
  </appSettings>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, edges) = extract_web_config(&rel, xml);
        // No key attribute → no symbols produced
        assert!(
            syms.is_empty(),
            "Empty <add/> should produce no symbols, got: {syms:?}"
        );
        assert!(edges.is_empty());
    }

    #[test]
    fn empty_config_returns_empty() {
        let xml = r#"<?xml version="1.0"?><configuration></configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, edges) = extract_web_config(&rel, xml);
        assert!(syms.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn malformed_xml_handled_gracefully() {
        let xml = "<configuration><appSettings><add key=\"Broken\" value=\"x\"";
        let rel = RelPath::new("web.config");
        // Should not panic — extractor breaks on error
        let (_syms, _edges) = extract_web_config(&rel, xml);
        // We don't assert counts here since partial parse behavior may vary;
        // the critical requirement is: no panic.
    }

    #[test]
    fn registers_module_edge_source_is_file_path() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.web>
    <httpModules>
      <add name="Foo" type="App.FooModule, App" />
    </httpModules>
  </system.web>
</configuration>"#;
        let rel = RelPath::new("src/web.config");
        let (_, edges) = extract_web_config(&rel, xml);

        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].source_name, "src/web.config");
        assert_eq!(edges[0].source_kind, "file");
        assert_eq!(edges[0].source_language, "xml");
    }

    #[test]
    fn module_without_name_uses_type_as_name() {
        // When no name attribute is given, the symbol name falls back to the full type string
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.web>
    <httpModules>
      <add type="MyApp.AnonymousModule, MyApp" />
    </httpModules>
  </system.web>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, _) = extract_web_config(&rel, xml);

        assert_eq!(syms.len(), 1);
        // Name should equal the full type string when name attr is absent
        assert_eq!(syms[0].name, "MyApp.AnonymousModule, MyApp");
    }

    #[test]
    fn extract_system_webserver_handler() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.webServer>
    <handlers>
      <add name="ScriptResource" verb="GET,HEAD" path="ScriptResource.axd" type="System.Web.Handlers.ScriptResourceHandler, System.Web.Extensions, Version=4.0.0.0, Culture=neutral, PublicKeyToken=31bf3856ad364e35" />
    </handlers>
  </system.webServer>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, edges) = extract_web_config(&rel, xml);

        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].kind, "route_handler");
        assert_eq!(syms[0].name, "ScriptResource");

        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "registers_handler");
        // Class name is extracted before the first comma
        assert_eq!(
            edges[0].target_name,
            "System.Web.Handlers.ScriptResourceHandler"
        );
    }

    #[test]
    fn multiple_connection_strings() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <connectionStrings>
    <add name="Primary" connectionString="Server=primary;" providerName="System.Data.SqlClient" />
    <add name="Replica" connectionString="Server=replica;" providerName="System.Data.SqlClient" />
    <add name="Archive" connectionString="Server=archive;" />
  </connectionStrings>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, edges) = extract_web_config(&rel, xml);

        assert_eq!(syms.len(), 3, "Three connection strings expected");
        assert!(edges.is_empty(), "Connection strings produce no edges");
        assert!(syms.iter().all(|s| s.kind == "connection_string"));
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Primary"));
        assert!(names.contains(&"Replica"));
        assert!(names.contains(&"Archive"));
    }

    #[test]
    fn app_setting_empty_value_allowed() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <appSettings>
    <add key="EmptyFlag" value="" />
  </appSettings>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (syms, _) = extract_web_config(&rel, xml);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "EmptyFlag");
        let meta = syms[0].metadata.as_ref().unwrap();
        assert_eq!(meta["value"], "");
    }

    #[test]
    fn extract_class_name_strips_assembly() {
        // Unit test for the private helper via public behaviour
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.web>
    <httpModules>
      <add name="M" type="Company.Product.MyModule, Company.Product, Version=1.0.0.0" />
    </httpModules>
  </system.web>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (_, edges) = extract_web_config(&rel, xml);
        assert_eq!(edges.len(), 1);
        // Only the FQN class name — no assembly / version info
        assert_eq!(edges[0].target_name, "Company.Product.MyModule");
    }

    #[test]
    fn registers_module_edge_metadata_contains_module_name() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.web>
    <httpModules>
      <add name="SessionExpiry" type="MyApp.SessionExpiryModule, MyApp" />
    </httpModules>
  </system.web>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (_, edges) = extract_web_config(&rel, xml);
        assert_eq!(edges.len(), 1);
        let meta = edges[0].metadata.as_ref().unwrap();
        assert_eq!(meta["module_name"], "SessionExpiry");
    }

    #[test]
    fn registers_handler_edge_metadata_contains_handler_name() {
        let xml = r#"<?xml version="1.0"?>
<configuration>
  <system.web>
    <httpHandlers>
      <add name="ImageResize" verb="GET" path="resize.axd" type="MyApp.ImageHandler, MyApp" />
    </httpHandlers>
  </system.web>
</configuration>"#;
        let rel = RelPath::new("web.config");
        let (_, edges) = extract_web_config(&rel, xml);
        assert_eq!(edges.len(), 1);
        let meta = edges[0].metadata.as_ref().unwrap();
        assert_eq!(meta["handler_name"], "ImageResize");
    }
}
