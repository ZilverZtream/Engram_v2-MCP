use std::path::{Path, PathBuf};

fn default_exts() -> Vec<&'static str> {
    vec![
        "rs", "py", "js", "ts", "tsx", "jsx", "mjs", "cjs", "go", "java", "cs", "vb", "c", "cpp",
        "cc", "cxx", "h", "hpp", "md", "toml", "yaml", "yml", "json", "aspx", "ascx", "master",
        "asmx", "ashx", "svc", "asax", "config", "xml", "html", "htm", "css", "scss", "less",
    ]
}

/// Return the file extensions to index for a given project_type string.
///
/// This function accepts the canonical strings produced by `ProjectType::as_str()`
/// (stored in the registry) as well as the legacy alias spellings that existed
/// before the `ProjectType` enum was introduced, so registry reads remain valid.
/// New code receiving user input should use `ProjectType` and call
/// `exts_for_project_type_enum` to get exhaustive, compile-time-verified dispatch.
pub fn exts_for_project_type(project_type: &str) -> Vec<&'static str> {
    if let Some(pt) = crate::models::ProjectType::from_registry_str(project_type) {
        exts_for_project_type_enum(pt)
    } else {
        // ENG-AUD-2026-S17-001: unrecognized project_type falls through to the
        // broad default extension set.  This is intentional for backward compat
        // with persisted records that pre-date the ProjectType enum, but widens
        // the ingest surface beyond what was intended for type-specific projects.
        // Log so operators can identify stale or malformed type strings.
        if !project_type.is_empty() {
            tracing::warn!(
                project_type = %project_type,
                "ENG-AUD-2026-S17-001: unrecognized project_type {:?} — falling back to \
                 default extension set; update persisted record to a known type",
                project_type
            );
        }
        default_exts()
    }
}

fn dotnet_webforms_cs_exts() -> Vec<&'static str> {
    vec![
        "cs", "aspx", "ascx", "master", "asmx", "ashx", "svc", "asax", "config", "xml", "sln",
        "csproj", "sql", "rdlc", "rdl", "asp", "rpt", "md", "json", "js", "ts", "jsx", "tsx",
        "mjs", "cjs", "html", "htm", "css", "scss", "less", "resx",
    ]
}

fn dotnet_webforms_vb_exts() -> Vec<&'static str> {
    vec![
        "vb", "aspx", "ascx", "master", "asmx", "ashx", "svc", "asax", "config", "xml", "sln",
        "vbproj", "sql", "rdlc", "rdl", "asp", "rpt", "md", "json", "js", "ts", "jsx", "tsx",
        "mjs", "cjs", "html", "htm", "css", "scss", "less", "resx",
    ]
}

fn rust_exts() -> Vec<&'static str> {
    vec!["rs", "toml", "md", "json", "yaml", "yml"]
}

fn csharp_exts() -> Vec<&'static str> {
    vec![
        "cs", "csproj", "sln", "props", "targets", "config", "json", "xml", "resx", "xaml",
        "razor", "cshtml", "sql", "md",
    ]
}

fn cpp_exts() -> Vec<&'static str> {
    vec![
        "cpp", "cc", "cxx", "c++", "hpp", "hh", "hxx", "h++", "h", "inl", "ipp", "tpp", "ixx",
        "cppm", "c", "cmake", "txt", "md", "json", "yaml", "yml",
    ]
}

fn c_exts() -> Vec<&'static str> {
    vec![
        "c", "h", "inc", "s", "S", "asm", "lds", "ld", "mak", "mk", "cmake", "txt", "md", "json",
        "yaml", "yml",
    ]
}

/// Exhaustive, enum-dispatched variant for use with validated `ProjectType` input.
///
/// ENG-AUD-2026-EXH-P1-0001: new indexing paths receive a `ProjectType` enum
/// (fail-closed at serde boundary) and call this function, so the compiler
/// catches any unhandled variant at compile time.
pub fn exts_for_project_type_enum(pt: crate::models::ProjectType) -> Vec<&'static str> {
    use crate::models::ProjectType;
    match pt {
        ProjectType::DotnetWebformsCs => dotnet_webforms_cs_exts(),
        ProjectType::DotnetWebformsVb => dotnet_webforms_vb_exts(),
        ProjectType::General => default_exts(),
        ProjectType::Rust => rust_exts(),
        ProjectType::CSharp => csharp_exts(),
        ProjectType::Cpp => cpp_exts(),
        ProjectType::C => c_exts(),
    }
}

/// Normalize path separators `\` to `/` in a pattern, but preserve escape
/// sequences `\*`, `\?`, and `\\` which are glob metachar escapes.
fn normalize_pattern_separators(p: &str) -> String {
    let mut result = String::with_capacity(p.len());
    let chars: Vec<char> = p.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            if next == '*' || next == '?' {
                // Escape sequence — keep the backslash as escape
                result.push('\\');
                result.push(next);
                i += 2;
                continue;
            }
        }
        if chars[i] == '\\' {
            // Path separator — normalize to forward slash
            result.push('/');
        } else {
            result.push(chars[i]);
        }
        i += 1;
    }
    result
}

pub fn pattern_match(file_path: &str, pattern: &str) -> bool {
    if pattern.trim().is_empty() {
        return false;
    }

    let text = file_path.replace('\\', "/");
    let pat = normalize_pattern_separators(pattern.trim());

    const MAX_PATTERN_CHARS: usize = 2_048;
    const MAX_TEXT_CHARS: usize = 8_192;
    if pat.chars().count() > MAX_PATTERN_CHARS || text.chars().count() > MAX_TEXT_CHARS {
        return false;
    }

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
            if c == '*' && tokens.last().copied() == Some(('*', true)) {
                continue;
            }
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

/// Recursively discover files with specific extensions under a directory.
pub async fn discover_files_recursive(
    dir: &Path,
    extensions: &[&str],
    max_files: usize,
) -> Vec<String> {
    use std::collections::VecDeque;

    let skip_dirs: std::collections::HashSet<&str> = [
        "bin",
        "obj",
        "node_modules",
        ".git",
        "packages",
        ".vs",
        ".svn",
        "debug",
        "release",
    ]
    .into_iter()
    .collect();

    let normalized_exts: std::collections::HashSet<String> = extensions
        .iter()
        .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
        .collect();

    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(dir.to_path_buf());

    let mut results = Vec::new();

    while let Some(current_dir) = queue.pop_front() {
        if results.len() >= max_files {
            break;
        }
        let mut entries = match tokio::fs::read_dir(&current_dir).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if results.len() >= max_files {
                break;
            }
            let path = entry.path();
            let Ok(ft) = entry.file_type().await else {
                continue;
            };

            if ft.is_symlink() {
                // Security: never follow symlinks during recursive discovery.
                // This prevents escaping the project root and symlink-loop DoS.
                continue;
            }

            if ft.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && !skip_dirs.contains(name.to_lowercase().as_str())
                {
                    queue.push_back(path);
                }
            } else if ft.is_file()
                && let Some(ext) = path.extension().and_then(|e| e.to_str())
            {
                let ext_normalized = ext.to_ascii_lowercase();
                if !normalized_exts.contains(&ext_normalized) {
                    continue;
                }
                if let Some(rel) = path.strip_prefix(dir).ok().and_then(|r| r.to_str()) {
                    results.push(rel.replace('\\', "/"));
                }
            }
        }
    }

    results
}

/// Find the code-behind file for an ASPX file.
pub fn find_codebehind_path(aspx_path: &Path) -> Option<PathBuf> {
    let s = aspx_path.to_string_lossy();
    for ext in &[".vb", ".cs"] {
        let cb = PathBuf::from(format!("{s}{ext}"));
        if cb.exists() {
            return Some(cb);
        }
    }
    if let Some(stem) = aspx_path.to_str() {
        for base_ext in &[".aspx", ".ascx", ".master"] {
            if let Some(stripped) = stem.strip_suffix(base_ext) {
                for ext in &[".aspx.vb", ".aspx.cs", ".ascx.vb", ".ascx.cs"] {
                    let cb = PathBuf::from(format!("{stripped}{ext}"));
                    if cb.exists() {
                        return Some(cb);
                    }
                }
            }
        }
    }
    None
}

/// Find the ASPX file for a code-behind file.
pub fn find_aspx_for_codebehind(cb_path: &Path) -> Option<PathBuf> {
    let s = cb_path.to_string_lossy();
    for ext in &[".vb", ".cs"] {
        if let Some(stripped) = s.strip_suffix(ext) {
            let aspx = PathBuf::from(stripped);
            if aspx.exists() {
                return Some(aspx);
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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

    #[test]
    fn project_type_match_is_case_insensitive() {
        let exts = exts_for_project_type("DotNet_WebForms_CS");
        assert!(exts.contains(&"csproj"));
        assert!(exts.contains(&"aspx"));
    }

    #[test]
    fn webforms_exts_include_frontend_assets() {
        let cs_exts = exts_for_project_type("dotnet_webforms_cs");
        let vb_exts = exts_for_project_type("dotnet_webforms_vb");
        let default = exts_for_project_type("general");

        for ext in [
            "js", "ts", "jsx", "tsx", "mjs", "cjs", "html", "htm", "css", "scss", "less",
        ] {
            assert!(cs_exts.contains(&ext));
            assert!(vb_exts.contains(&ext));
            assert!(default.contains(&ext));
        }
    }

    #[test]
    fn typed_project_modes_have_curated_file_sets() {
        let rust_exts = exts_for_project_type_enum(crate::models::ProjectType::Rust);
        assert!(rust_exts.contains(&"rs"));
        assert!(rust_exts.contains(&"toml"));
        assert!(!rust_exts.contains(&"cs"));

        let csharp_exts = exts_for_project_type_enum(crate::models::ProjectType::CSharp);
        assert!(csharp_exts.contains(&"cs"));
        assert!(csharp_exts.contains(&"csproj"));
        assert!(!csharp_exts.contains(&"vb"));
        assert!(!csharp_exts.contains(&"cpp"));

        let cpp_exts = exts_for_project_type_enum(crate::models::ProjectType::Cpp);
        assert!(cpp_exts.contains(&"cpp"));
        assert!(cpp_exts.contains(&"hpp"));
        assert!(!cpp_exts.contains(&"vb"));
        assert!(!cpp_exts.contains(&"csproj"));

        let c_exts = exts_for_project_type_enum(crate::models::ProjectType::C);
        assert!(c_exts.contains(&"c"));
        assert!(c_exts.contains(&"h"));
        assert!(!c_exts.contains(&"rs"));
        assert!(!c_exts.contains(&"cs"));
    }

    #[test]
    fn registry_aliases_resolve_for_new_typed_modes() {
        assert!(exts_for_project_type("rustlang").contains(&"rs"));
        assert!(exts_for_project_type("c#").contains(&"cs"));
        assert!(exts_for_project_type("c++").contains(&"cpp"));
        assert!(exts_for_project_type("ansi_c").contains(&"c"));
    }

    #[test]
    fn webforms_indexes_resx_localization_files() {
        // .resx is central to WebForms localization (a story that touches a
        // user-facing string edits the whole language family). It must be
        // indexed so resx files get graph nodes and the change-set family
        // expansion can backfill the full language set.
        assert!(exts_for_project_type("dotnet_webforms_vb").contains(&"resx"));
        assert!(exts_for_project_type("dotnet_webforms_cs").contains(&"resx"));
    }

    #[test]
    fn rejects_pathological_pattern_lengths() {
        let huge_pat = "*".repeat(4_096);
        assert!(!pattern_match("src/file.rs", &huge_pat));
    }
}
