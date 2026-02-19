fn default_exts() -> Vec<&'static str> {
    vec![
        "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "cs", "vb", "c", "cpp", "cc", "cxx",
        "h", "hpp", "md", "toml", "yaml", "yml", "json", "aspx", "ascx", "master", "config", "xml",
    ]
}

/// Return the file extensions to index for a given project_type.
/// WebForms presets add .aspx/.ascx/.master/.vb/.config/.xml/.csproj/.vbproj/.sln/.sql/.rdlc.
pub fn exts_for_project_type(project_type: &str) -> Vec<&'static str> {
    match project_type.to_lowercase().as_str() {
        "dotnetwebformscs" | "dotnet_webforms_cs" | "webforms_cs" | "webformscs" => vec![
            "cs", "aspx", "ascx", "master", "config", "xml", "sln", "csproj", "sql", "rdlc", "md",
            "json",
        ],
        "dotnetwebformsvb" | "dotnet_webforms_vb" | "webforms_vb" | "webformsvb" => vec![
            "vb", "aspx", "ascx", "master", "config", "xml", "sln", "vbproj", "sql", "rdlc", "md",
            "json",
        ],
        _ => default_exts(),
    }
}

pub fn pattern_match(file_path: &str, pattern: &str) -> bool {
    // Very small glob-like matcher:
    // - if pattern contains '*' we treat it like a suffix/prefix/contains check
    // - if pattern starts with '.' treat as suffix
    if pattern.trim().is_empty() {
        return false;
    }
    if pattern == "*" {
        return true;
    }
    if pattern.starts_with('.') {
        return file_path.ends_with(pattern);
    }
    if pattern.contains('*') {
        let parts: Vec<&str> = pattern.split('*').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return true;
        }
        let must_anchor_start = !pattern.starts_with('*');
        let must_anchor_end = !pattern.ends_with('*');
        let mut idx = 0usize;
        for (i, p) in parts.iter().enumerate() {
            if let Some(pos) = file_path[idx..].find(p) {
                if i == 0 && must_anchor_start && pos != 0 {
                    return false;
                }
                idx += pos + p.len();
            } else {
                return false;
            }
        }
        if must_anchor_end && idx != file_path.len() {
            return false;
        }
        return true;
    }
    file_path.contains(pattern)
}
