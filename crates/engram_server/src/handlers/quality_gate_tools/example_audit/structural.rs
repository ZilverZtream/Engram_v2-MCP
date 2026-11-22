//! Source-backed contracts whose evidence spans statements. Inspect the new
//! side of each hunk, including unchanged context, but report added lines only.
use super::*;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Contract {
    Redirect,
    Postback,
    MasterCast,
    Filter { accessor: String, predicate: String },
    Html,
    Poll,
    Listener,
    Selector,
    PublicMemberCase(bool),
}

pub(super) fn contracts(policy: &str) -> Vec<Contract> {
    let p = policy.to_ascii_lowercase();
    let mut out = Vec::new();
    if p.contains("redirect")
        && p.contains("return")
        && (p.contains("mandatory") || p.contains("after") || p.contains("immediately"))
    {
        out.push(Contract::Redirect);
    }
    if p.contains("postback")
        && (p.contains("permission check") || p.contains("authorization/role gate"))
    {
        out.push(Contract::Postback);
    }
    if p.contains("cast") && p.contains("master") && p.contains("concrete master") {
        out.push(Contract::MasterCast);
    }
    // A project can name its own unsafe accessor and its required filter.
    static RAW: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)`(?:[\w.]+\.)?(\w+)\(\)`\s+does not filter").unwrap());
    static FILTER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)filter at query level using\s+`(?:[\w.]+\.)?(\w+)\(").unwrap()
    });
    if let (Some(a), Some(b)) = (RAW.captures(policy), FILTER.captures(policy)) {
        out.push(Contract::Filter {
            accessor: a[1].to_lowercase(),
            predicate: b[1].to_lowercase(),
        });
    }
    if p.contains("innerhtml")
        && (p.contains("untrusted") || p.contains("unencoded"))
        && p.contains("encode")
    {
        out.push(Contract::Html);
    }
    if p.contains("setinterval") && p.contains("overlap") && p.contains("request") {
        out.push(Contract::Poll);
    }
    if p.contains("removeeventlistener") && p.contains("listener") && p.contains("null") {
        out.push(Contract::Listener);
    }
    if p.contains("document.queryselector") && p.contains("component") && p.contains("scope dom") {
        out.push(Contract::Selector);
    }
    out
}

impl Contract {
    pub(super) fn description(&self) -> String {
        match self {
            Self::PublicMemberCase(true) => {
                "Public TypeScript method violates the repository PascalCase rule"
            }
            Self::PublicMemberCase(false) => {
                "Public TypeScript method violates the repository camelCase rule"
            }
            Self::Redirect => "Redirect continues without a visible immediate Return",
            Self::Postback => {
                "Permission check is nested in the initial-load branch and skipped on postback"
            }
            Self::MasterCast => "Master is cast to a concrete type",
            Self::Filter {
                accessor,
                predicate,
            } => {
                return format!(
                    "{accessor} result reaches output without a visible {predicate} filter"
                );
            }
            Self::Html => {
                "A dynamic value is concatenated into HTML markup without visible encoding"
            }
            Self::Poll => "Interval starts an asynchronous request without a visible overlap guard",
            Self::Listener => "Listener reference is cleared in cleanup without visible detachment",
            Self::Selector => "Component uses a document-wide selector instead of its root",
        }
        .to_owned()
    }
}

pub(super) struct Hunk {
    vb: bool,
    js: bool,
    public_methods: Vec<(usize, String)>,
    rows: Vec<(usize, bool, Vec<String>)>,
}
impl Hunk {
    pub(super) fn new(path: &str, rows: impl IntoIterator<Item = (usize, bool, String)>) -> Self {
        let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        let vb = ext == "vb";
        let mut block = false;
        let rows: Vec<_> = rows.into_iter().collect();
        let mut public_methods = Vec::new();
        if matches!(ext.as_str(), "ts" | "tsx" | "cts" | "mts") {
            let src = rows
                .iter()
                .map(|(_, _, s)| s.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let mut members = engram_index::js_method_facts::members(Path::new(path), &src);
            if members.is_empty()
                && rows
                    .iter()
                    .any(|(_, _, s)| s.trim_start().starts_with("public "))
            {
                members = engram_index::js_method_facts::members(
                    Path::new(path),
                    &format!("class Scope {{\n{src}\n}}"),
                );
                for m in &mut members {
                    m.line = m.line.saturating_sub(1);
                }
            }
            for m in members.into_iter().filter(|m| m.public) {
                if let Some((n, true, _)) = m.line.checked_sub(1).and_then(|i| rows.get(i)) {
                    public_methods.push((*n, m.name));
                }
            }
        }
        Self {
            vb,
            public_methods,
            js: matches!(ext.as_str(), "js" | "ts" | "tsx" | "jsx" | "mjs"),
            rows: rows
                .into_iter()
                .map(|(n, a, s)| (n, a, syntax::source_tokens(&s, vb, &ext, &mut block)))
                .collect(),
        }
    }
    pub(super) fn file(file: &DiffFile) -> Vec<Self> {
        if file.hunks.is_empty() {
            return vec![Self::new(
                &file.path,
                file.added_lines.iter().map(|(n, s)| (*n, true, s.clone())),
            )];
        }
        file.hunks
            .iter()
            .map(|h| {
                let mut n = h.new_start;
                Self::new(
                    &file.path,
                    h.body.iter().filter_map(|s| {
                        let added = s.starts_with('+');
                        if !added && !s.starts_with(' ') {
                            return None;
                        }
                        let line = n;
                        n += 1;
                        Some((line, added, s[1..].to_owned()))
                    }),
                )
            })
            .collect()
    }
    pub(super) fn hits(&self, c: &Contract) -> Vec<usize> {
        if let Contract::PublicMemberCase(upper) = c {
            return self
                .public_methods
                .iter()
                .filter(|(_, s)| crate::services::written_casing::violates(s, *upper))
                .map(|(n, _)| *n)
                .collect();
        }
        let mut hits = Vec::new();
        let mut initial = Vec::new();
        for (i, (n, added, t)) in self.rows.iter().enumerate() {
            let w: Vec<_> = t.iter().map(String::as_str).collect();
            if w.is_empty() {
                continue;
            }
            if self.vb {
                if w.starts_with(&["end", "sub"]) || w.starts_with(&["end", "function"]) {
                    initial.clear();
                }
                if w.starts_with(&["end", "if"]) {
                    initial.pop();
                }
                if matches!(w[0], "else" | "elseif") {
                    if let Some(v) = initial.last_mut() {
                        *v = false;
                    }
                }
            }
            let finding = if !added {
                false
            } else {
                match c {
                    Contract::Redirect if self.vb => {
                        let call = w
                            .windows(2)
                            .position(|a| a == ["saferedirect", "("])
                            .or_else(|| {
                                w.windows(4)
                                    .position(|a| a == ["response", ".", "redirect", "("])
                                    .filter(|_| w.windows(2).any(|a| a == [",", "false"]))
                            });
                        call.is_some()
                            && !w.windows(2).any(|a| a == [":", "return"])
                            && self.rows[i + 1..]
                                .iter()
                                .find(|(_, _, t)| !t.is_empty())
                                .is_some_and(|(_, _, next)| {
                                    next[0] != "return"
                                        && !next.starts_with(&["exit".into(), "sub".into()])
                                        && !next.starts_with(&["exit".into(), "function".into()])
                                })
                    }
                    Contract::Postback if self.vb => {
                        initial.contains(&true)
                            && w.contains(&"if")
                            && w.iter().any(|s| {
                                s.ends_with("access")
                                    || s.contains("permission")
                                    || s.contains("superuser")
                                    || s.ends_with("isinrole")
                                    || s.starts_with("checkread")
                                    || s.starts_with("checkwrite")
                            })
                    }
                    Contract::MasterCast if self.vb => w.windows(5).any(|a| {
                        matches!(a[0], "ctype" | "directcast")
                            && a[1..4] == ["(", "master", ","]
                            && !matches!(a[4], "object" | "masterpage")
                    }),
                    Contract::Filter {
                        accessor,
                        predicate,
                    } if self.vb && w.windows(2).any(|a| a == [accessor.as_str(), "("]) => {
                        let tail = self.rows[i..]
                            .iter()
                            .take_while(|(_, _, v)| {
                                !v.starts_with(&["end".into(), "sub".into()])
                                    && !v.starts_with(&["end".into(), "function".into()])
                            })
                            .flat_map(|(_, _, v)| v.iter().map(String::as_str))
                            .collect::<Vec<_>>();
                        let variable = w
                            .iter()
                            .position(|s| *s == "=")
                            .and_then(|j| j.checked_sub(1))
                            .map(|j| w[j]);
                        let output = tail.windows(3).any(|a| {
                            a[0] == "datasource"
                                && a[1] == "="
                                && (variable == Some(a[2]) || a[2] == accessor)
                        }) || w[0] == "return"
                            || w.contains(&"datasource");
                        let before_bind = &tail[..tail
                            .iter()
                            .position(|s| *s == "databind")
                            .unwrap_or(tail.len())];
                        let filtered =
                            before_bind
                                .iter()
                                .position(|s| *s == "where")
                                .is_some_and(|j| {
                                    let condition = &before_bind[j + 1..];
                                    condition
                                        .iter()
                                        .position(|s| *s == predicate)
                                        .is_some_and(|k| condition[..k].contains(&"not"))
                                });
                        output && !filtered
                    }
                    Contract::Html if self.js => {
                        w.windows(3).any(|a| {
                            a[0] == "." && matches!(a[1], "innerHTML" | "outerHTML") && a[2] == "="
                        }) && dynamic_markup(&w)
                    }
                    Contract::Selector if self.js => {
                        w.windows(4).any(|a| {
                            a == ["document", ".", "querySelector", "("]
                                || a == ["document", ".", "querySelectorAll", "("]
                        }) && self.component(i)
                    }
                    Contract::Listener if self.js => w.windows(5).any(|a| {
                        a[0] == "this"
                            && a[1] == "."
                            && a[3] == "="
                            && a[4] == "null"
                            && self.cleanup(i, a[2])
                    }),
                    Contract::Poll if self.js => {
                        w.windows(2).any(|a| a == ["setInterval", "("]) && self.poll(i)
                    }
                    _ => false,
                }
            };
            if finding {
                hits.push(*n);
            }
            if self.vb && w.first() == Some(&"if") && w.last() == Some(&"then") {
                initial.push(
                    w.windows(2).any(|a| a == ["not", "ispostback"])
                        || w.windows(4).any(|a| {
                            a == ["not", "page", ".", "ispostback"]
                                || a == ["not", "me", ".", "ispostback"]
                        }),
                );
            }
        }
        hits
    }
    fn component(&self, i: usize) -> bool {
        self.rows[..=i]
            .iter()
            .rev()
            .take_while(|(_, _, v)| !v.starts_with(&["}".into()]))
            .any(|(_, _, v)| {
                v.iter().any(|s| {
                    matches!(
                        s.as_str(),
                        "this" | "public" | "private" | "protected" | "class"
                    )
                }) || v.last().is_some_and(|s| s == "{")
                    && v.first()
                        .is_some_and(|s| !matches!(s.as_str(), "function" | "if" | "for" | "while"))
                    && v.iter().any(|s| s == ")")
            })
    }
    fn cleanup(&self, i: usize, field: &str) -> bool {
        let Some(start) = (0..=i).rev().find(|&j| {
            self.rows[j].2.windows(2).any(|a| {
                matches!(
                    a[0].as_str(),
                    "dispose" | "destroy" | "cleanup" | "disconnectedCallback"
                ) && a[1] == "("
            })
        }) else {
            return false;
        };
        let body = block_tokens(&self.rows[start..], "{", "}");
        if !body
            .windows(5)
            .any(|a| a == ["this", ".", field, "=", "null"])
        {
            return false;
        }
        let registered = self.rows.iter().any(|(_, _, v)| {
            v.iter().any(|s| s == "addEventListener") && v.iter().any(|s| s == field)
        });
        (registered || field.to_lowercase().contains("listener"))
            && !body
                .split(|s| *s == ";")
                .any(|stmt| stmt.contains(&"removeEventListener") && stmt.contains(&field))
    }
    fn poll(&self, i: usize) -> bool {
        let first = self.rows[i]
            .2
            .iter()
            .position(|s| s == "setInterval")
            .unwrap();
        let all = self.rows[i..]
            .iter()
            .flat_map(|(_, _, v)| v.iter().map(String::as_str))
            .skip(first + 1);
        let body = balanced(all, "(", ")");
        let request = body
            .windows(2)
            .any(|a| matches!(a[0], "ajax" | "fetch" | "serviceajax") && a[1] == "(");
        let guarded = body.contains(&"abort")
            || body.split(|s| *s == ";").any(|s| {
                s.contains(&"if")
                    && s.iter().any(|v| {
                        let s = v.to_lowercase();
                        [
                            "pending",
                            "busy",
                            "inflight",
                            "inprogress",
                            "loading",
                            "running",
                        ]
                        .iter()
                        .any(|term| s.contains(term))
                    })
            });
        request && !guarded
    }
}
fn balanced<'a>(tokens: impl Iterator<Item = &'a str>, open: &str, close: &str) -> Vec<&'a str> {
    let mut depth = 0;
    let mut out = Vec::new();
    for s in tokens.skip_while(|s| *s != open) {
        out.push(s);
        if s == open {
            depth += 1;
        } else if s == close {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
    }
    out
}
fn block_tokens<'a>(
    rows: &'a [(usize, bool, Vec<String>)],
    open: &str,
    close: &str,
) -> Vec<&'a str> {
    balanced(
        rows.iter()
            .flat_map(|(_, _, v)| v.iter().map(String::as_str)),
        open,
        close,
    )
}
fn dynamic_markup(w: &[&str]) -> bool {
    let Some(at) = w.iter().position(|s| *s == "=") else {
        return false;
    };
    let rhs = &w[at + 1..];
    rhs.iter().any(|s| s.starts_with('`') && s.contains("${"))
        || rhs.contains(&"+")
            && rhs.split(|s| *s == "+").any(|part| {
                let mut names = part
                    .iter()
                    .filter(|s| s.starts_with(|c: char| c.is_alphabetic() || c == '_' || c == '$'));
                names.next().is_some()
                    && !part.windows(2).any(|a| {
                        a[1] == "("
                            && matches!(
                                a[0].to_ascii_lowercase().as_str(),
                                "htmlencode"
                                    | "escapehtml"
                                    | "encodehtml"
                                    | "sanitize"
                                    | "sanitizehtml"
                            )
                    })
            })
}
