use thiserror::Error;

pub type Result<T> = std::result::Result<T, EngramError>;

#[derive(Debug, Error)]
pub enum EngramError {
    #[error("config error: {0}")]
    Config(String),

    #[error("path not allowed: {0}")]
    PathNotAllowed(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl From<serde_yaml::Error> for EngramError {
    fn from(e: serde_yaml::Error) -> Self {
        EngramError::Serde(e.to_string())
    }
}

pub fn guess_language(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "go" => "go",
        "java" => "java",
        "cs" => "csharp",
        "vb" => "vbnet",
        "cpp" | "cc" | "cxx" | "hpp" | "h" => "cpp",
        "c" => "c",
        "md" => "markdown",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        // .NET WebForms / ASP.NET markup & service endpoints
        "aspx" | "ascx" | "master" | "asmx" | "ashx" | "svc" | "asax" => "aspx",
        "config" => "xml",
        "xml" => "xml",
        "sln" => "text",
        "csproj" | "vbproj" => "xml",
        "sql" => "sql",
        "rdlc" | "rdl" => "xml",
        "asp" => "asp_classic",
        "rpt" => "text",
        _ => "text",
    }
}

/// True for third-party/vendored asset paths whose CONTENTS should not feed
/// the code graph: package manager output, vendored libraries, and minified
/// bundles. These files stay searchable (they are still chunked/indexed)
/// but emit no symbols or edges - a 53k-line font-awesome bundle generating
/// bare-name `dependency` edges into app code poisons paths, blast radius,
/// and call graphs with name-collision phantoms.
///
/// Deliberately conservative: only universally vendor directory names and
/// minified-bundle filename shapes. App-owned dist/build dirs are NOT
/// matched - apps legitimately keep first-party code there.
pub fn is_vendor_path(rel_path: &str) -> bool {
    let norm = rel_path.replace('\\', "/").to_ascii_lowercase();

    // Directory segments that are package-manager or vendoring roots.
    const VENDOR_SEGMENTS: &[&str] = &[
        "node_modules",
        "bower_components",
        "jspm_packages",
        "vendor",
        "vendors",
        "packages", // NuGet solution-level packages dir
    ];
    for seg in norm.split('/') {
        if VENDOR_SEGMENTS.contains(&seg) {
            return true;
        }
    }
    // wwwroot/lib is the ASP.NET Core LibMan vendor root (lib alone is too
    // generic to match globally).
    if norm.contains("wwwroot/lib/") {
        return true;
    }

    // Minified/bundled artifacts - generated, never the source of truth.
    let file = norm.rsplit('/').next().unwrap_or(&norm);
    if file.ends_with(".min.js")
        || file.ends_with(".min.css")
        || file.ends_with(".min.map")
        || file.ends_with(".bundle.js")
        || file.ends_with(".umd.js")
    {
        return true;
    }
    // Versioned library cores like jquery-3.6.0.js / jquery-ui-1.13.2.js.
    if let Some(rest) = file.strip_prefix("jquery-")
        && rest
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || rest.starts_with("ui-"))
    {
        return true;
    }
    false
}

#[cfg(test)]
mod vendor_path_tests {
    use super::is_vendor_path;

    #[test]
    fn vendor_directories_match() {
        assert!(is_vendor_path("Site/bower_components/MeasureTool/x.js"));
        assert!(is_vendor_path("web/node_modules/leaflet/dist/leaflet.js"));
        assert!(is_vendor_path(
            "packages/Newtonsoft.Json.13.0.1/lib/net45/x.cs"
        ));
        assert!(is_vendor_path("src/wwwroot/lib/bootstrap/bootstrap.js"));
        assert!(
            is_vendor_path(r"Site\bower_components\x\y.js"),
            "backslashes"
        );
    }

    #[test]
    fn minified_and_bundles_match() {
        assert!(is_vendor_path(
            "Site/modules/map/~.js/markerclusterplus.min.js"
        ));
        assert!(is_vendor_path("Site/css/app.min.css"));
        assert!(is_vendor_path("Site/bower/gmaps-measuretool.umd.js"));
        assert!(is_vendor_path("Scripts/jquery-3.6.0.js"));
    }

    #[test]
    fn app_code_does_not_match() {
        assert!(!is_vendor_path(
            "Site/modules/dashboard/ts/map/maps.utils.ts"
        ));
        assert!(!is_vendor_path("App_Code/api-json/api-broker.vb"));
        assert!(!is_vendor_path("Site/modules/map/~.js/map.js"));
        // App-owned dist + a custom jquery plugin stay in the graph.
        assert!(!is_vendor_path("frontend/dist/app.js"));
        assert!(!is_vendor_path("Scripts/jquery.ociusGrid.js"));
        // 'package'/'libs' singular or different segments do not match.
        assert!(!is_vendor_path("src/package/manager.vb"));
    }
}
