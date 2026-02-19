/// Check if `haystack` contains `needle` as a whole word (not surrounded by alphanumeric/underscore).
pub fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let hay = haystack.as_bytes();
    for (pos, _) in haystack.match_indices(needle) {
        let before_ok = pos == 0 || {
            let b = hay[pos - 1];
            !b.is_ascii_alphanumeric() && b != b'_'
        };
        let after_pos = pos + needle.len();
        let after_ok = after_pos >= hay.len() || {
            let b = hay[after_pos];
            !b.is_ascii_alphanumeric() && b != b'_'
        };
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

pub fn stacktrace_to_query(stack: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]{2,}|[A-Za-z0-9_\-/\\]+\.(rs|py|js|ts|go|java|cs)")
            .expect("Invalid regex")
    });
    let mut terms: Vec<String> = Vec::new();
    for m in re.find_iter(stack).take(60) {
        let t = m.as_str();
        if t.len() > 80 {
            continue;
        }
        terms.push(t.to_string());
    }
    terms.join(" ")
}

pub fn code_to_query(code: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re =
        RE.get_or_init(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]{2,}").expect("Invalid regex"));
    let mut terms: Vec<String> = Vec::new();
    for m in re.find_iter(code).take(30) {
        let t = m.as_str();
        if t.len() > 30 {
            continue;
        }
        if matches!(t, "self" | "this" | "that" | "Some" | "None" | "Result") {
            continue;
        }
        terms.push(t.to_string());
    }
    terms.join(" ")
}
