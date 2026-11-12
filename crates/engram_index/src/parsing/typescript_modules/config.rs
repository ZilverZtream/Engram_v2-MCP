//! JSONC config facts only. Keep every JSON file eligible as a relative
//! `extends` target; choosing a tsconfig belongs to the indexed graph pass.
use engram_core::typescript_modules::Config;

pub(crate) fn extract(source: &str) -> Config {
    let mut config = Config {
        version: 1,
        extends: None,
        base_url: None,
        paths: None,
        issues: vec![],
    };
    if source.len() > 2 * 1024 * 1024 {
        config.issues.push("config_size_budget".into());
        return config;
    }
    let parsed =
        jsonc(source).and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let Some(value) = parsed.filter(|v| v.is_object()) else {
        config.issues.push("invalid_jsonc_config".into());
        return config;
    };
    if let Some(extends) = value.get("extends") {
        if let Some(extends) = extends.as_str() {
            config.extends = Some(extends.into());
        } else {
            config.issues.push("unsupported_config_extends".into());
        }
    }
    if let Some(options) = value.get("compilerOptions") {
        if !options.is_object() {
            config.issues.push("invalid_compiler_options".into());
            return config;
        }
        if let Some(base) = options.get("baseUrl") {
            if let Some(base) = base.as_str() {
                config.base_url = Some(base.into());
            } else {
                config.issues.push("invalid_base_url".into());
            }
        }
        if let Some(paths) = options.get("paths") {
            if let Some(paths) = paths.as_object() {
                let mut result = std::collections::BTreeMap::new();
                for (pattern, targets) in paths {
                    let targets = targets.as_array().and_then(|a| {
                        a.iter()
                            .map(|v| v.as_str().map(str::to_string))
                            .collect::<Option<Vec<_>>>()
                    });
                    if pattern.matches('*').count() > 1
                        || targets.as_ref().is_none_or(|a| {
                            a.is_empty() || a.iter().any(|v| v.matches('*').count() > 1)
                        })
                    {
                        config.issues.push("unsupported_path_pattern".into());
                    } else {
                        result.insert(pattern.clone(), targets.expect("validated string targets"));
                    }
                }
                config.paths = Some(result);
            } else {
                config.issues.push("invalid_paths".into());
            }
        }
    }
    config
}

// Preserve string bytes, erase comments, then remove only structural trailing
// commas. A regex would corrupt URLs, quoted comment delimiters and escapes.
fn jsonc(source: &str) -> Option<String> {
    let mut bytes = source.trim_start_matches('\u{feff}').as_bytes().to_vec();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    bytes[i] = b' ';
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                bytes[i] = b' ';
                bytes[i + 1] = b' ';
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    bytes[i] = b' ';
                    i += 1;
                }
                if i + 1 >= bytes.len() {
                    return None;
                }
                bytes[i] = b' ';
                bytes[i + 1] = b' ';
                i += 2;
            }
            _ => i += 1,
        }
    }
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
        } else if bytes[i] == b',' {
            let mut next = i + 1;
            while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                next += 1;
            }
            if matches!(bytes.get(next), Some(b'}' | b']')) {
                bytes[i] = b' ';
            }
        }
        i += 1;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jsonc_keeps_strings_and_authored_alias_order() {
        let config = extract(
            r#"{ // project paths
            "extends": "./config/base.json",
            "compilerOptions": { "baseUrl": ".", "paths": {
                "@lib/*": ["generated/*", /* fallback */ "src/*",],
                "url": ["https://example.test/a/*"],
            }, },
        }"#,
        );
        assert!(config.issues.is_empty(), "{:?}", config.issues);
        assert_eq!(config.extends.as_deref(), Some("./config/base.json"));
        assert_eq!(
            config.paths.as_ref().unwrap()["@lib/*"],
            ["generated/*", "src/*"]
        );
        assert_eq!(
            config.paths.as_ref().unwrap()["url"],
            ["https://example.test/a/*"]
        );
    }
    #[test]
    fn malformed_configs_do_not_become_empty_valid_alias_tables() {
        for source in [
            "{/*",
            r#"{"compilerOptions":{"paths":{"x":42}}}"#,
            r#"{"extends":["a","b"]}"#,
        ] {
            assert!(!extract(source).issues.is_empty(), "{source}");
        }
    }
}
