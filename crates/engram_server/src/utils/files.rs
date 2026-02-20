fn default_exts() -> Vec<&'static str> {
    vec![
        "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "cs", "vb", "c", "cpp", "cc", "cxx",
        "h", "hpp", "md", "toml", "yaml", "yml", "json", "aspx", "ascx", "master", "asmx", "ashx",
        "svc", "asax", "config", "xml",
    ]
}

/// Return the file extensions to index for a given project_type.
///
/// WebForms presets include the full set of legacy ASP.NET file types:
///   - `.aspx` / `.ascx` / `.master` — WebForms pages, user controls, master pages
///   - `.asmx`  — ASMX Web Service endpoints
///   - `.ashx`  — Generic HTTP Handlers
///   - `.svc`   — WCF Service Host endpoints
///   - `.asax`  — Global Application File (Global.asax)
///   - `.config` / `.xml` / `.rdlc` — configuration and report definitions
///   - `.sql`   — stored procedures and DDL scripts
pub fn exts_for_project_type(project_type: &str) -> Vec<&'static str> {
    match project_type.to_lowercase().as_str() {
        "dotnetwebformscs" | "dotnet_webforms_cs" | "webforms_cs" | "webformscs" => vec![
            "cs", "aspx", "ascx", "master", "asmx", "ashx", "svc", "asax", "config", "xml", "sln",
            "csproj", "sql", "rdlc", "md", "json",
        ],
        "dotnetwebformsvb" | "dotnet_webforms_vb" | "webforms_vb" | "webformsvb" => vec![
            "vb", "aspx", "ascx", "master", "asmx", "ashx", "svc", "asax", "config", "xml", "sln",
            "vbproj", "sql", "rdlc", "md", "json",
        ],
        _ => default_exts(),
    }
}

pub fn pattern_match(file_path: &str, pattern: &str) -> bool {
    // Glob-style wildcard matcher supporting:
    // - `*` for 0+ characters
    // - `?` for exactly one character
    // - `\` to escape a literal `*`, `?`, or `\`
    //
    // Matching is done against normalized path separators.
    if pattern.trim().is_empty() {
        return false;
    }

    let text = file_path.replace('\\', "/");
    let pat = pattern.trim().replace('\\', "/");

    let text_chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut escaped = false;
    for c in pat.chars() {
        if escaped {
            tokens.push((c, false));
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
        } else {
            tokens.push((c, true));
        }
    }
    if escaped {
        tokens.push(('\\', false));
    }

    let mut dp = vec![vec![false; text_chars.len() + 1]; tokens.len() + 1];
    dp[0][0] = true;

    for i in 1..=tokens.len() {
        let (pc, is_meta) = tokens[i - 1];
        if is_meta && pc == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }

    for i in 1..=tokens.len() {
        let (pc, is_meta) = tokens[i - 1];
        for j in 1..=text_chars.len() {
            let tc = text_chars[j - 1];
            dp[i][j] = if is_meta && pc == '*' {
                dp[i - 1][j] || dp[i][j - 1]
            } else if is_meta && pc == '?' {
                dp[i - 1][j - 1]
            } else {
                dp[i - 1][j - 1] && pc == tc
            };
        }
    }

    dp[tokens.len()][text_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::pattern_match;

    #[test]
    fn matches_glob_wildcards() {
        assert!(pattern_match("src/foo/bar.rs", "src/*/*.rs"));
        assert!(pattern_match("src/foo/bar.rs", "src/*/ba?.rs"));
        assert!(!pattern_match("src/foo/bar.rs", "src/*/ba??.rs"));
    }

    #[test]
    fn supports_path_separator_normalization() {
        assert!(pattern_match(r"src\\foo\\bar.rs", "src/*/*.rs"));
    }

    #[test]
    fn escaped_meta_chars_are_literal() {
        assert!(pattern_match("foo*bar.txt", r"foo\*bar.txt"));
        assert!(!pattern_match("fooxbar.txt", r"foo\*bar.txt"));
    }
}
