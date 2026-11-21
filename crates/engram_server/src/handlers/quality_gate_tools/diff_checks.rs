//! Match concrete added code to conventions in the project's quality corpus.
use crate::services::pre_commit_review_service::DiffFile;
use engram_index::{HybridQuery, HybridSearchEngine};
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::LazyLock;

struct Check {
    key: &'static str,
    query: &'static str,
    // Each group must occur in a supporting document. Alternatives within a group are separated by |.
    terms: &'static [&'static str],
    message: &'static str,
}
const INLINE: Check = Check {
    key: "vb.inline-if",
    query: "single line If Then Return End If multiline",
    terms: &[
        "if",
        "single-line|one-line|single line|one line|multi-line|multiline",
    ],
    message: "A single-line If exits here. Expand it into an If block.",
};
const RESOURCE: Check = Check {
    key: "vb.context-lifetime",
    query: "Using DataContext IDisposable dispose",
    terms: &["context", "using"],
    message: "A new database context has no observed Using or explicit disposal in these added lines.",
};
const NULL: Check = Check {
    key: "vb.null-result",
    query: "FirstOrDefault Nothing dereference null guard",
    terms: &["firstordefault", "nothing|null"],
    message: "The lookup result is dereferenced before a null check.",
};
const COMMIT: Check = Check {
    key: "vb.borrowed-commit",
    query: "SubmitChanges caller supplied context owner",
    terms: &["submitchanges", "caller|owner|supplied"],
    message: "SubmitChanges is called directly on the database context parameter.",
};
const CATCH: Check = Check {
    key: "vb.empty-catch",
    query: "empty Catch exception swallow log",
    terms: &["catch", "empty|swallow|silent"],
    message: "The Catch block contains no executable statement.",
};
const AUDIT: Check = Check {
    key: "vb.mutation-audit",
    query: "audit log user change update event logging mutation",
    terms: &["audit|event log|eventlog", "log"],
    message: "This added method updates a property and commits without an observed audit call. Check caller-owned logging.",
};
const EVENT: Check = Check {
    key: "js.event-invocation",
    query: "preventDefault stopPropagation parentheses",
    terms: &["preventdefault", "invoke|parenthes|no-op|call"],
    message: "The event method is read without invoking it.",
};
const CONSOLE: Check = Check {
    key: "js.debug-console",
    query: "console.log debug logging production",
    terms: &["console.log", "remov|avoid|debug|never|production"],
    message: "A console.log debugging statement was added.",
};
const SPLICE: Check = Check {
    key: "js.splice-count",
    query: "splice idx delete count",
    terms: &["splice", "count|onward|single"],
    message: "splice has one argument and removes the rest of the array.",
};
const UI: Check = Check {
    key: "ui.literal",
    query: "hardcoded user facing UI strings localization resources",
    terms: &["hardcod|hard-cod|literal", "localiz|resourc|translat"],
    message: "A literal display string is passed directly to a UI operation.",
};
const AJAX: Check = Check {
    key: "js.ajax-error",
    query: "ajax error callback failure handler",
    terms: &["ajax", "error|failure", "callback|handler"],
    message: "This AJAX call has no observed failure handler.",
};

const MATERIALIZE: Check = Check {
    key: "linq.filter-after-materialization",
    query: "materialize whole table before filtering ToList Where database",
    terms: &["materializ|tolist", "filter|where"],
    message: "The predicate runs after materialization. Apply the filter to the query before reading the rows.",
};
const NOTHING: Check = Check {
    key: "vb.nothing-comparison",
    query: "Nothing reference equality Is Nothing value comparison VB",
    terms: &["nothing", "reference|is nothing"],
    message: "This reference is compared to Nothing by value. Use Is Nothing or IsNot Nothing.",
};
const RATIO: Check = Check {
    key: "vb.integer-ratio",
    query: "integer division ratio percentage Double Decimal operand truncation",
    terms: &["division|divid", "ratio|percent|truncat"],
    message: "Integer division truncates the ratio before scaling. Use floating-point or decimal division before rounding.",
};
const MODAL: Check = Check {
    key: "ui.modal-body",
    query: "modal-open document body modal dialog scroll lock lifecycle",
    terms: &["modal", "modal-open"],
    message: "The dialog is shown without an observed body modal-open update in this change. Verify the open lifecycle.",
};
const POPOVER: Check = Check {
    key: "ui.dynamic-popover-html",
    query: "popover html content untrusted escape sanitize XSS",
    terms: &["popover|tooltip", "html", "escap|sanit|xss|untrusted|encod"],
    message: "HTML mode inserts a composed value into popover content. Use text mode or explicitly encode the value.",
};
const TRANSPORT: Check = Check {
    key: "js.raw-ajax",
    query: "raw jQuery ajax wrapper api.ajax serviceajax requests",
    terms: &["ajax", "wrapper|serviceajax|raw jquery|api.ajax"],
    message: "This change uses the raw jQuery transport. Check the project's request wrapper.",
};
const RAW_ALERT: Check = Check {
    key: "js.raw-alert",
    query: "raw alert debug dialogs avoid alert()",
    terms: &["alert", "raw|debug|avoid|never"],
    message: "A raw browser alert was added.",
};

type Observation<'a> = (usize, &'a Check);
fn arguments(s: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let (mut depth, mut quote, mut escape, mut start) = (0, None, false, 0);
    for (i, c) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if let Some(q) = quote {
            if c == '\\' {
                escape = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' | '\x60' => quote = Some(c),
            '(' | '[' | '{' => depth += 1,
            ')' if depth == 0 => {
                parts.push(s[start..i].trim());
                return Some(parts);
            }
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    None
}
fn javascript(lines: &[(usize, String)], out: &mut Vec<Observation<'static>>) {
    static BARE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\.(?:preventDefault|stopPropagation)\s*;\s*$").unwrap());
    static LITERAL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\.(?:text|html|setText)\s*\(\s*["'][A-Za-z][^"']*\s+[A-Za-z][^"']*["']\s*\)"#)
            .unwrap()
    });
    for (i, (n, line)) in lines.iter().enumerate() {
        let code = line.trim();
        if ["//", "/*", "*"].iter().any(|p| code.starts_with(p)) {
            continue;
        }
        static SHOW: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?i)\b[\w.$]*(?:modal|dialog)[\w.$]*\.classList\.remove\(\s*["']hidden["']\s*\)"#).unwrap()
        });
        static ALERT: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"^(?:window\.)?alert\s*\(").unwrap());
        static LITERAL_ARG: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#"^\s*["'][A-Za-z][^"']*["']\s*\)"#).unwrap());
        static HTML: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#"(?i)\bhtml\s*:\s*true\b"#).unwrap());
        static DYNAMIC: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?is)\bcontent\s*:\s*(?:["'][^"']*["']\s*\+\s*[\w$(]|[\w$.]+\s*\+\s*["']|\x60[^\x60]*\$\{)"#).unwrap()
        });
        if SHOW.is_match(code)
            && !lines.iter().any(|(_, l)| {
                l.contains("modal-open") && (l.contains(".add(") || l.contains(".addClass("))
            })
        {
            out.push((*n, &MODAL));
        }
        if let Some(m) = ALERT.find(code) {
            out.push((*n, &RAW_ALERT));
            if LITERAL_ARG.is_match(&code[m.end()..]) {
                out.push((*n, &UI));
            }
        }
        if let Some((_, tail)) = code.split_once(".popover(") {
            let text = std::iter::once(tail)
                .chain(lines[i + 1..].iter().take(25).map(|(_, s)| s.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(args) = arguments(&text)
                && let Some(opts) = args.first()
                && HTML.is_match(opts)
                && DYNAMIC.is_match(opts)
            {
                out.push((*n, &POPOVER));
            }
        }
        if BARE.is_match(code) {
            out.push((*n, &EVENT));
        }
        if code.starts_with("console.log(") {
            out.push((*n, &CONSOLE));
        }
        if let Some((_, tail)) = code.split_once(".splice(")
            && arguments(tail).is_some_and(|v| v.len() == 1 && !v[0].is_empty())
        {
            out.push((*n, &SPLICE));
        }
        if LITERAL.is_match(code) {
            out.push((*n, &UI));
        }
        if let Some((_, tail)) = code.split_once(".ajax(") {
            let mut call = tail.to_owned();
            for (_, next) in lines.iter().skip(i + 1).take(18) {
                call.push('\n');
                call.push_str(next);
            }
            if code.contains("$.ajax(") || code.contains("jQuery.ajax(") {
                out.push((*n, &TRANSPORT));
                static ERROR_OPTION: LazyLock<Regex> =
                    LazyLock::new(|| Regex::new(r#"(?:^|[,{\s])["']?error["']?\s*:"#).unwrap());
                if let Some(args) = arguments(&call)
                    && let Some(options) = args.iter().find(|a| a.starts_with('{'))
                    && !ERROR_OPTION.is_match(options)
                {
                    // Only the continuation of this request can supply its failure handler.
                    let chain = call.split_once(*options).map_or("", |(_, rest)| {
                        rest.trim_start().trim_start_matches(')').trim_start()
                    });
                    let chain = chain.split(';').next().unwrap_or("");
                    if !chain.contains(".fail(") && !chain.contains(".catch(") {
                        out.push((*n, &AJAX));
                    }
                }
            }
            if arguments(&call).is_some_and(|args| {
                args.len() == 4 && (args[3].contains("=>") || args[3].contains("function"))
            }) {
                out.push((*n, &AJAX));
            }
        }
    }
}
fn vb(lines: &[(usize, String)], out: &mut Vec<Observation<'static>>) {
    static LOOKUP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^\s*dim\s+(\w+).*?=.*\.(?:firstordefault|singleordefault)\s*\(").unwrap()
    });
    static CTX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^\s*dim\s+(\w+)\b[^']*?\bnew\s+[\w.]*?(?:datacontext|dbcontext)\b")
            .unwrap()
    });
    static PARAM: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(\w+)\s+as\s+[\w.]*?(?:datacontext|dbcontext)\b").unwrap()
    });
    static SET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\w+\.\w+\s*=").unwrap());
    let code: Vec<String> = lines
        .iter()
        .map(|(_, s)| {
            s.split('\'')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .collect();
    for (i, (n, _)) in lines.iter().enumerate() {
        let s = code[i].as_str();
        static INLINE_EXIT: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"^if\b.+\bthen\s+(?:return\b|exit\s+(?:sub|function)\b)").unwrap()
        });
        static FILTER: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"\.(?:tolist|toarray)\s*\(\s*\)\s*\.\s*where\s*\(").unwrap()
        });
        static PERCENT: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\\\s*[\w.]+\s*\)?\s*\*\s*(?:100|1000)\b").unwrap());
        static VALUE_NULL: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"\b(?:if|elseif|orelse|andalso)\s+\(?\s*(\w+)\s*(?:=|<>)\s*nothing\b")
                .unwrap()
        });
        if FILTER.is_match(s) {
            out.push((*n, &MATERIALIZE));
        }
        if PERCENT.is_match(s) {
            out.push((*n, &RATIO));
        }
        if let Some(m) = VALUE_NULL.captures(s) {
            let var = &m[1];
            let binding = code[..i]
                .iter()
                .rev()
                .take_while(|l| !l.starts_with("end function") && !l.starts_with("end sub"))
                .find(|l| l.contains(&format!("dim {var} ")) || l.contains(&format!("{var} as ")));
            if binding.is_some_and(|l| {
                LOOKUP.is_match(l)
                    || [" as string", " as object", " as new ", " = new "]
                        .iter()
                        .any(|ty| l.contains(ty))
            }) {
                out.push((*n, &NOTHING));
            }
        }
        if INLINE_EXIT.is_match(s) {
            out.push((*n, &INLINE));
        }
        if let Some(m) = CTX.captures(s) {
            let name = &m[1];
            if !code[i + 1..].iter().any(|l| {
                l.contains(&format!("{name}.dispose(")) || l.starts_with(&format!("using {name}"))
            }) {
                out.push((*n, &RESOURCE));
            }
        }
        if let Some(m) = LOOKUP.captures(s) {
            let name = &m[1];
            for (j, l) in code.iter().enumerate().skip(i + 1) {
                if VALUE_NULL.captures(l).is_some_and(|m| &m[1] == name)
                    || l.contains(&format!("{name} is nothing"))
                    || l.contains(&format!("{name} isnot nothing"))
                    || l.contains(&format!("{name}?."))
                    || l.starts_with("end function")
                    || l.starts_with("end sub")
                    || l.starts_with(&format!("{name} ="))
                {
                    break;
                }
                if l.contains(&format!("{name}.")) {
                    out.push((lines[j].0, &NULL));
                    break;
                }
            }
        }
        if s.starts_with("catch")
            && code[i + 1..]
                .iter()
                .find(|l| !l.is_empty())
                .is_some_and(|l| l == "end try" || l.starts_with("catch ") || l == "finally")
        {
            out.push((*n, &CATCH));
        }
        if !(s.contains("function ") || s.contains("sub ")) {
            continue;
        }
        let Some(end) = code[i + 1..]
            .iter()
            .position(|l| l == "end function" || l == "end sub")
            .map(|j| i + 1 + j)
        else {
            continue;
        };
        let method = &code[i..=end];
        let decl = method
            .iter()
            .take_while(|l| !l.contains(')'))
            .map(String::as_str)
            .collect::<Vec<_>>();
        let signature = method[..(decl.len() + 1).min(method.len())].join(" ");
        for m in PARAM.captures_iter(&signature) {
            let name = &m[1];
            let commit = format!("{name}.submitchanges(");
            for j in i + 1..end {
                if code[j].starts_with(&commit) {
                    out.push((lines[j].0, &COMMIT));
                }
            }
        }
        if method.iter().any(|l| l.contains(".submitchanges("))
            && let Some(j) = method.iter().position(|l| SET.is_match(l))
            && !method
                .iter()
                .any(|l| (l.contains("log") || l.contains("audit")) && l.contains('('))
        {
            out.push((lines[i + j].0, &AUDIT));
        }
    }
}
pub(crate) struct LineFinding {
    pub path: String,
    pub line: usize,
    pub key: &'static str,
    pub message: &'static str,
    pub source: String,
}
impl LineFinding {
    pub(super) fn render(&self) -> String {
        format!(
            "- {}:{} [{}] {} Rule source: {}",
            self.path, self.line, self.key, self.message, self.source
        )
    }
}
pub(crate) fn query(project: &str, generation: u64, text: String, top_k: usize) -> HybridQuery {
    HybridQuery {
        project_id: project.to_owned(),
        namespace: "quality_gate".to_owned(),
        generation,
        text,
        top_k,
        fts_mode: "loose".into(),
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        include_path_suffixes: None,
        language_filters: None,
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: true,
    }
}

pub(crate) fn review(
    files: &[DiffFile],
    engine: &HybridSearchEngine,
    query: &HybridQuery,
    verifier: &mut crate::services::quality_source::SourceVerifier<'_>,
    root: &std::path::Path,
) -> Vec<LineFinding> {
    let mut results = Vec::new();
    let mut rules = BTreeMap::new();
    let guides: Vec<_> = ["CLAUDE.md", ".github/copilot-instructions.md", "AGENTS.md"]
        .into_iter()
        .filter_map(|p| {
            engram_core::safe_read_to_string(root, p)
                .ok()
                .map(|text| (p, text))
        })
        .collect();
    for file in files {
        let ext = file
            .path
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let mut found = Vec::new();
        // Never infer a missing guard or callback across omitted diff context.
        let mut start = 0;
        while start < file.added_lines.len() {
            let mut end = start + 1;
            while end < file.added_lines.len()
                && file.added_lines[end].0 == file.added_lines[end - 1].0 + 1
            {
                end += 1;
            }
            match ext.as_str() {
                "vb" => vb(&file.added_lines[start..end], &mut found),
                "ts" | "tsx" | "js" | "jsx" => {
                    javascript(&file.added_lines[start..end], &mut found)
                }
                _ => {}
            }
            start = end;
        }
        found.sort_by_key(|(n, check)| (*n, check.key));
        found.dedup_by_key(|(n, check)| (*n, check.key));
        for (n, check) in found {
            let evidence = rules.entry(check.key).or_insert_with(|| {
                let matches_policy = |text: &str| {
                    check
                        .terms
                        .iter()
                        .all(|group| group.split('|').any(|term| text.contains(term)))
                };
                for (path, text) in &guides {
                    if let Some((i, line)) = text.lines().enumerate().find(|(_, l)| {
                        let l = l.trim_start_matches([' ', '-']).to_ascii_lowercase();
                        !l.starts_with('"')
                            && matches_policy(&l)
                            && match check.key {
                                "vb.nothing-comparison" => {
                                    l.contains("= nothing") && l.contains("reference")
                                }
                                "ui.modal-body" => l.contains("modal-open") && l.contains("hidden"),
                                "js.raw-ajax" => l.contains("$.ajax") && l.contains("use"),
                                _ => false,
                            }
                    }) {
                        return Some(format!(
                            "{path}:{} - {}",
                            i + 1,
                            line.trim().chars().take(230).collect::<String>()
                        ));
                    }
                }
                let mut targeted = query.clone();
                targeted.text = check.query.into();
                targeted.top_k = 20;
                let hits = engine.lexical_search_with_content(&targeted).ok()?;
                hits.into_iter()
                    .filter_map(|(hit, text, _)| {
                        let status = verifier.document_status(&hit.doc_id, &text);
                        if status.starts_with("retired:") || status.starts_with("stale:") {
                            return None;
                        }
                        let policy = text
                            .lines()
                            .filter(|line| {
                                !line.starts_with("Quality-rule source:")
                                    && !line.starts_with("Source BLAKE3:")
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        let lower = policy.to_lowercase();
                        check
                            .terms
                            .iter()
                            .all(|group| group.split('|').any(|term| lower.contains(term)))
                            .then(|| {
                                (
                                    if check.key == AUDIT.key {
                                        policy.contains(".Create(") as u8
                                    } else {
                                        0
                                    },
                                    format!(
                                        "{} ({}) - {} [{}]",
                                        hit.path.as_str(),
                                        hit.doc_id,
                                        policy.trim().chars().take(230).collect::<String>(),
                                        status
                                    ),
                                )
                            })
                    })
                    .enumerate()
                    .max_by_key(|(rank, (specific, _))| (*specific, std::cmp::Reverse(*rank)))
                    .map(|(_, (_, rule))| rule)
            });
            if let Some(source) = evidence {
                results.push(LineFinding {
                    path: file.path.clone(),
                    line: n,
                    key: check.key,
                    message: check.message,
                    source: source.clone(),
                });
            }
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lines(s: &str) -> Vec<(usize, String)> {
        s.lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l.into()))
            .collect()
    }
    #[test]
    fn clean_owned_context_and_null_check_do_not_raise_findings() {
        let diff = lines(
            "Public Shared Function UpdateRecord(id As Integer, Optional db As StoreDataContext = Nothing) As Boolean\nTry\nUsing ctx As New StoreDataContextEx.ReuseOrCreate(db)\nDim row = ctx.Db.Items.FirstOrDefault(Function(r) r.Id = id)\nIf row Is Nothing Then\nReturn False\nEnd If\nrow.Note = notes\nctx.SubmitWhenOwned()\nAuditLog.Create(id)\nReturn True\nEnd Using\nCatch ex As Exception\nLogError(ex)\nReturn False\nEnd Try\nEnd Function",
        );
        let mut out = Vec::new();
        vb(&diff, &mut out);
        assert!(out.is_empty());
    }
    #[test]
    fn incorrect_guard_is_located_at_comparison() {
        let mut out = Vec::new();
        vb(
            &lines(
                "Dim record = rows.SingleOrDefault()\nIf record = Nothing Then Exit Sub\nrecord.Enabled = True",
            ),
            &mut out,
        );
        assert!(out.iter().any(|(n, c)| *n == 2 && c.key == NOTHING.key));
        assert!(!out.iter().any(|(_, c)| c.key == NULL.key));
    }
}
