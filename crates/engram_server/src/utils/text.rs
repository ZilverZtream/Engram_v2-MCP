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

/// Structured frame extracted from a stacktrace line.
#[derive(Debug, Clone)]
pub struct StackFrame {
    /// File path (may be relative or absolute).
    pub file: Option<String>,
    /// Line number in the source file.
    pub line: Option<u32>,
    /// Function/method name.
    pub function: Option<String>,
    /// FQN class.method or namespace.class.method.
    pub fqn: Option<String>,
}

/// Parse a stacktrace into structured frames, then build a search query.
/// Handles: Python, .NET/C#, Java, JavaScript/Node.js, Rust, Go, VB.NET,
/// PHP, Ruby, and generic formats.
pub fn parse_stack_frames(stack: &str) -> Vec<StackFrame> {
    // Compile all patterns once via OnceLock
    static PATTERNS: std::sync::OnceLock<StackPatterns> = std::sync::OnceLock::new();
    let p = PATTERNS.get_or_init(StackPatterns::new);

    let mut frames = Vec::new();
    for line in stack.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Python: File "path/file.py", line N, in func_name
        if let Some(caps) = p.python.captures(trimmed) {
            frames.push(StackFrame {
                file: caps.get(1).map(|m| m.as_str().to_string()),
                line: caps.get(2).and_then(|m| m.as_str().parse().ok()),
                function: caps.get(3).map(|m| m.as_str().to_string()),
                fqn: None,
            });
            continue;
        }

        // Java: at com.example.Class.method(File.java:123)
        // Must be checked BEFORE .NET because .NET's greedy `\([^)]*\)` would
        // swallow Java's `(File.java:42)` without extracting the file/line.
        if let Some(caps) = p.java.captures(trimmed) {
            let fqn_raw = caps.get(1).map(|m| m.as_str().to_string());
            let func = fqn_raw
                .as_deref()
                .and_then(|f| f.rsplit('.').next())
                .map(|s| s.to_string());
            frames.push(StackFrame {
                file: caps.get(2).map(|m| m.as_str().to_string()),
                line: caps.get(3).and_then(|m| m.as_str().parse().ok()),
                function: func,
                fqn: fqn_raw,
            });
            continue;
        }

        // .NET/C#/VB: at Namespace.Class.Method(args) in path\file.cs:line N
        // or:  at Namespace.Class.Method(args)
        if let Some(caps) = p.dotnet.captures(trimmed) {
            let fqn_raw = caps.get(1).map(|m| m.as_str().to_string());
            let func = fqn_raw
                .as_deref()
                .and_then(|f| f.rsplit('.').next())
                .map(|s| s.to_string());
            frames.push(StackFrame {
                file: caps.get(2).map(|m| m.as_str().to_string()),
                line: caps.get(3).and_then(|m| m.as_str().parse().ok()),
                function: func,
                fqn: fqn_raw,
            });
            continue;
        }

        // Node.js: at Object.fn (/abs/path/file.js:N:M)  or  at fn (path:N:M)
        if let Some(caps) = p.nodejs.captures(trimmed) {
            frames.push(StackFrame {
                file: caps.get(2).map(|m| m.as_str().to_string()),
                line: caps.get(3).and_then(|m| m.as_str().parse().ok()),
                function: caps.get(1).map(|m| {
                    let s = m.as_str();
                    // Strip "Object." or "Module." prefix
                    s.rsplit('.').next().unwrap_or(s).to_string()
                }),
                fqn: None,
            });
            continue;
        }

        // Rust: thread 'main' panicked at 'msg', src/main.rs:N:M
        // or:    N: func_name  at path/file.rs:L:C
        if let Some(caps) = p.rust_panic.captures(trimmed) {
            frames.push(StackFrame {
                file: caps.get(1).map(|m| m.as_str().to_string()),
                line: caps.get(2).and_then(|m| m.as_str().parse().ok()),
                function: None,
                fqn: None,
            });
            continue;
        }
        if let Some(caps) = p.rust_bt.captures(trimmed) {
            frames.push(StackFrame {
                file: caps.get(2).map(|m| m.as_str().to_string()),
                line: caps.get(3).and_then(|m| m.as_str().parse().ok()),
                function: caps.get(1).map(|m| m.as_str().to_string()),
                fqn: None,
            });
            continue;
        }

        // Go: goroutine N [running]:  or  path/file.go:N +0xABC
        if let Some(caps) = p.go.captures(trimmed) {
            frames.push(StackFrame {
                file: caps.get(1).map(|m| m.as_str().to_string()),
                line: caps.get(2).and_then(|m| m.as_str().parse().ok()),
                function: None,
                fqn: None,
            });
            continue;
        }

        // PHP: #N path/file.php(L): Class->method()
        if let Some(caps) = p.php.captures(trimmed) {
            frames.push(StackFrame {
                file: caps.get(1).map(|m| m.as_str().to_string()),
                line: caps.get(2).and_then(|m| m.as_str().parse().ok()),
                function: caps.get(3).map(|m| m.as_str().to_string()),
                fqn: None,
            });
            continue;
        }

        // Ruby: from path/file.rb:N:in `method_name'
        if let Some(caps) = p.ruby.captures(trimmed) {
            frames.push(StackFrame {
                file: caps.get(1).map(|m| m.as_str().to_string()),
                line: caps.get(2).and_then(|m| m.as_str().parse().ok()),
                function: caps.get(3).map(|m| m.as_str().to_string()),
                fqn: None,
            });
            continue;
        }

        // Generic: any path-like token with a recognized extension
        if let Some(caps) = p.generic_file.captures(trimmed) {
            frames.push(StackFrame {
                file: caps.get(1).map(|m| m.as_str().to_string()),
                line: caps.get(2).and_then(|m| m.as_str().parse().ok()),
                function: None,
                fqn: None,
            });
        }
    }
    frames
}

/// Build a search query from a stacktrace, using structured parsing + token extraction.
pub fn stacktrace_to_query(stack: &str) -> String {
    let frames = parse_stack_frames(stack);

    let mut seen = std::collections::HashSet::new();
    let mut terms: Vec<String> = Vec::new();

    // Extract high-value tokens from structured frames
    for frame in &frames {
        if let Some(ref file) = frame.file {
            // Normalize: take filename only (strip directory path)
            let basename = file.rsplit(['/', '\\']).next().unwrap_or(file);
            if basename.len() >= 3 && basename.len() <= 80 && seen.insert(basename.to_string()) {
                terms.push(basename.to_string());
            }
        }
        if let Some(ref func) = frame.function
            && func.len() >= 3
            && func.len() <= 80
            && seen.insert(func.to_string())
        {
            terms.push(func.to_string());
        }
        if let Some(ref fqn) = frame.fqn {
            // Also add class name (second-to-last segment)
            let parts: Vec<&str> = fqn.split('.').collect();
            if parts.len() >= 2 {
                let class = parts[parts.len() - 2];
                if class.len() >= 3 && seen.insert(class.to_string()) {
                    terms.push(class.to_string());
                }
            }
        }
    }

    // Fallback: generic identifier extraction for any remaining tokens
    static IDENT_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = IDENT_RE
        .get_or_init(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]{2,}").expect("valid regex"));
    for m in re.find_iter(stack).take(80) {
        let t = m.as_str();
        if t.len() > 80 {
            continue;
        }
        // Skip common noise tokens
        if matches!(
            t,
            "the"
                | "this"
                | "self"
                | "None"
                | "null"
                | "true"
                | "false"
                | "return"
                | "throw"
                | "catch"
                | "try"
                | "new"
                | "var"
                | "let"
                | "const"
                | "class"
                | "void"
                | "int"
                | "string"
                | "bool"
                | "object"
                | "static"
                | "public"
                | "private"
                | "protected"
                | "internal"
                | "async"
                | "await"
                | "from"
                | "import"
                | "using"
                | "namespace"
                | "System"
                | "Object"
                | "String"
                | "Exception"
        ) {
            continue;
        }
        if seen.insert(t.to_string()) {
            terms.push(t.to_string());
        }
        if terms.len() >= 60 {
            break;
        }
    }

    terms.join(" ")
}

struct StackPatterns {
    python: regex::Regex,
    dotnet: regex::Regex,
    java: regex::Regex,
    nodejs: regex::Regex,
    rust_panic: regex::Regex,
    rust_bt: regex::Regex,
    go: regex::Regex,
    php: regex::Regex,
    ruby: regex::Regex,
    generic_file: regex::Regex,
}

impl StackPatterns {
    fn new() -> Self {
        Self {
            // Python: File "path/file.py", line 42, in function_name
            python: regex::Regex::new(
                r#"File "([^"]+\.py[cow]?)", line (\d+)(?:, in (\w+))?"#
            ).expect("valid regex"),

            // .NET: at Namespace.Class.Method(args) in C:\path\file.cs:line 42
            // also: at Namespace.Class.Method(args)
            dotnet: regex::Regex::new(
                r"^\s*at\s+([\w.<>+`\[\],]+)\([^)]*\)(?:\s+in\s+(.+?):(?:line\s+)?(\d+))?"
            ).expect("valid regex"),

            // Java: at com.example.Class.method(File.java:123)
            java: regex::Regex::new(
                r"^\s*at\s+([\w.$]+)\(([^:)]+):(\d+)\)"
            ).expect("valid regex"),

            // Node.js: at funcName (path/file.js:N:M)  or  at path/file.js:N:M
            nodejs: regex::Regex::new(
                r"^\s*at\s+(?:([\w.<>\[\]$]+)\s+)?\(?([^\s()]+?):(\d+):\d+\)?"
            ).expect("valid regex"),

            // Rust panic: thread 'name' panicked at 'msg', src/file.rs:N:M
            rust_panic: regex::Regex::new(
                r"panicked at .+,\s*(.+\.rs):(\d+):\d+"
            ).expect("valid regex"),

            // Rust backtrace: N: symbol at path/file.rs:L:C
            rust_bt: regex::Regex::new(
                r"^\s*\d+:\s+([\w:<>]+)\s+at\s+(.+?):(\d+)(?::\d+)?"
            ).expect("valid regex"),

            // Go: path/file.go:N +0xABC  or  path/file.go:N
            go: regex::Regex::new(
                r"([\w./\\-]+\.go):(\d+)"
            ).expect("valid regex"),

            // PHP: #N /path/file.php(42): Class->method()
            php: regex::Regex::new(
                r"#\d+\s+(.+\.php)\((\d+)\):\s*([\w\\:->]+)"
            ).expect("valid regex"),

            // Ruby: from path/file.rb:N:in `method'
            ruby: regex::Regex::new(
                r"from\s+(.+\.rb):(\d+):in\s+`(\w+)'"
            ).expect("valid regex"),

            // Generic: any file path with recognized extension, optionally followed by :line
            generic_file: regex::Regex::new(
                r"([\w./\\-]+\.(?:rs|py|js|ts|tsx|jsx|go|java|cs|vb|aspx|ascx|master|config|php|rb|cpp|c|h|swift|kt|scala|lua|sql|cshtml|vbhtml|fs|fsx))[:\(](\d+)"
            ).expect("valid regex"),
        }
    }
}

pub fn code_to_query(code: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = if let Some(re) = RE.get() {
        re
    } else {
        match regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]{2,}") {
            Ok(compiled) => RE.get_or_init(|| compiled),
            Err(err) => {
                tracing::error!("failed to compile code token regex: {err}");
                return String::new();
            }
        }
    };
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_python_traceback() {
        let stack = r#"Traceback (most recent call last):
  File "app/views.py", line 42, in handle_request
    result = compute(data)
  File "app/utils.py", line 15, in compute
    return process(data)
ValueError: invalid literal"#;
        let frames = parse_stack_frames(stack);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].file.as_deref(), Some("app/views.py"));
        assert_eq!(frames[0].line, Some(42));
        assert_eq!(frames[0].function.as_deref(), Some("handle_request"));
        assert_eq!(frames[1].file.as_deref(), Some("app/utils.py"));
        assert_eq!(frames[1].line, Some(15));
    }

    #[test]
    fn parse_dotnet_stacktrace() {
        let stack = r#"System.NullReferenceException: Object reference not set
   at MyApp.Services.UserService.GetUser(Int32 id) in C:\src\Services\UserService.cs:line 45
   at MyApp.Controllers.UserController.Index() in C:\src\Controllers\UserController.cs:line 23"#;
        let frames = parse_stack_frames(stack);
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0].fqn.as_deref(),
            Some("MyApp.Services.UserService.GetUser")
        );
        assert_eq!(frames[0].function.as_deref(), Some("GetUser"));
        assert_eq!(frames[0].line, Some(45));
        assert!(
            frames[0]
                .file
                .as_deref()
                .unwrap()
                .ends_with("UserService.cs")
        );
    }

    #[test]
    fn parse_java_stacktrace() {
        let stack = r#"Exception in thread "main" java.lang.NullPointerException
    at com.example.MyClass.doStuff(MyClass.java:42)
    at com.example.Main.main(Main.java:10)"#;
        let frames = parse_stack_frames(stack);
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames[0].fqn.as_deref(),
            Some("com.example.MyClass.doStuff")
        );
        assert_eq!(frames[0].function.as_deref(), Some("doStuff"));
        assert_eq!(frames[0].file.as_deref(), Some("MyClass.java"));
        assert_eq!(frames[0].line, Some(42));
    }

    #[test]
    fn parse_nodejs_stacktrace() {
        let stack = r#"Error: Cannot find module 'foo'
    at Module._resolveFilename (internal/modules/cjs/loader.js:636:15)
    at processTicksAndRejections (internal/process/task_queues.js:95:5)"#;
        let frames = parse_stack_frames(stack);
        assert!(frames.len() >= 2);
        assert_eq!(frames[0].function.as_deref(), Some("_resolveFilename"));
        assert_eq!(frames[0].line, Some(636));
    }

    #[test]
    fn parse_rust_panic() {
        let stack = "thread 'main' panicked at 'index out of bounds', src/main.rs:42:5";
        let frames = parse_stack_frames(stack);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].file.as_deref(), Some("src/main.rs"));
        assert_eq!(frames[0].line, Some(42));
    }

    #[test]
    fn parse_go_stacktrace() {
        let stack = r#"goroutine 1 [running]:
main.main()
    /home/user/project/main.go:15 +0x1a3"#;
        let frames = parse_stack_frames(stack);
        assert!(!frames.is_empty());
        let go_frame = frames.iter().find(|f| {
            f.file
                .as_deref()
                .map(|s| s.ends_with(".go"))
                .unwrap_or(false)
        });
        assert!(go_frame.is_some());
        assert_eq!(go_frame.unwrap().line, Some(15));
    }

    #[test]
    fn parse_aspx_generic_path() {
        let stack = "Error at Controls/Menu.ascx:12 - unhandled exception";
        let frames = parse_stack_frames(stack);
        assert!(!frames.is_empty());
        assert!(frames[0].file.as_deref().unwrap().contains("Menu.ascx"));
        assert_eq!(frames[0].line, Some(12));
    }

    #[test]
    fn stacktrace_query_deduplicates() {
        let stack = r#"  at MyClass.DoStuff() in C:\src\MyClass.cs:line 10
  at MyClass.DoStuff() in C:\src\MyClass.cs:line 10
  at MyClass.DoStuff() in C:\src\MyClass.cs:line 10"#;
        let query = stacktrace_to_query(stack);
        // "MyClass.cs" and "DoStuff" should each appear only once
        let count = query.matches("MyClass.cs").count();
        assert_eq!(count, 1, "MyClass.cs should appear once, got {}", count);
    }

    #[test]
    fn stacktrace_query_filters_noise() {
        let stack = "at System.Object.ToString() in void class string";
        let query = stacktrace_to_query(stack);
        // Common noise tokens should be filtered
        assert!(!query.contains(" class ") || !query.split_whitespace().any(|t| t == "class"));
        assert!(!query.split_whitespace().any(|t| t == "void"));
    }
}
