//! Bounded source evidence for repeated database calls, not runtime query counts.
use engram_graph::{EdgeKind, GraphStore, Node};
use engram_index::vb_extractor::{
    VbInvocation, VbInvocationReport, vb_invocations, vb_utf16_span_text,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::Read,
    path::Path,
};

struct Source {
    text: String,
    hash: String,
    report: VbInvocationReport,
    nodes: Vec<Node>,
}

pub struct Analyzer<'a> {
    graph: &'a GraphStore,
    project: &'a str,
    directory: &'a Path,
    selected: &'a BTreeMap<String, u64>,
    sources: BTreeMap<String, Source>,
    failed: BTreeSet<String>,
    bytes: usize,
    steps: usize,
    pub findings: Vec<Value>,
    pub gaps: Vec<String>,
    pub truncated: bool,
    pub unresolved_calls: usize,
}

impl<'a> Analyzer<'a> {
    pub fn new(
        graph: &'a GraphStore,
        project: &'a str,
        directory: &'a Path,
        selected: &'a BTreeMap<String, u64>,
    ) -> Self {
        Self {
            graph,
            project,
            directory,
            selected,
            sources: BTreeMap::new(),
            failed: BTreeSet::new(),
            bytes: 0,
            steps: 0,
            findings: vec![],
            gaps: vec![],
            truncated: false,
            unresolved_calls: 0,
        }
    }
    fn gap(&mut self, message: String) {
        if self.gaps.len() < 64 && !self.gaps.contains(&message) {
            self.gaps.push(message);
        } else if self.gaps.len() >= 64 {
            self.truncated = true;
        }
    }
    fn read(&mut self, path: &str) -> anyhow::Result<(String, String)> {
        let revision = self
            .selected
            .get(path)
            .ok_or_else(|| anyhow::anyhow!("Source is not selected"))?;
        let node = self
            .graph
            .get_node(self.project, &format!("file:{path}"))?
            .ok_or_else(|| anyhow::anyhow!("File fingerprint unavailable"))?;
        anyhow::ensure!(
            node.generation == *revision,
            "Selected file revision differs from graph file revision"
        );
        let target = engram_core::safe_join(self.directory, path)?;
        let mut source = String::new();
        std::fs::File::open(target)?
            .take(2_000_001)
            .read_to_string(&mut source)?;
        anyhow::ensure!(
            source.len() <= 2_000_000,
            "Source exceeds 2000000-byte sidecar limit"
        );
        self.bytes += source.len();
        anyhow::ensure!(self.bytes <= 64 * 1024 * 1024, "Source reads exceed 64 MiB");
        let hash = blake3::hash(source.as_bytes()).to_hex().to_string();
        anyhow::ensure!(
            node.metadata
                .as_ref()
                .and_then(|m| m.get("file_hash"))
                .and_then(|v| v.as_str())
                == Some(hash.as_str()),
            "Source changed since indexing; update_project before analysis"
        );
        Ok((source, hash))
    }
    fn load(&mut self, path: &str) -> bool {
        if self.sources.contains_key(path) {
            return true;
        }
        if self.failed.contains(path) {
            return false;
        }
        let loaded = (|| -> anyhow::Result<Source> {
            anyhow::ensure!(
                path.to_ascii_lowercase().ends_with(".vb"),
                "Only VB.NET invocation ownership is supported"
            );
            anyhow::ensure!(
                self.sources.len() < 32,
                "Helper/source expansion exceeds 32 VB files"
            );
            let (text, hash) = self.read(path)?;
            let absolute = engram_core::safe_join(self.directory, path)?;
            let report = vb_invocations(&absolute, &text).map_err(anyhow::Error::msg)?;
            validate_report(&text, &report)?;
            let nodes =
                self.graph
                    .query_nodes_in_file(self.project, Some("function"), path, 1025)?;
            anyhow::ensure!(
                nodes.len() <= 1024,
                "Function inventory exceeds 1024 declarations in a file"
            );
            Ok(Source {
                text,
                hash,
                report,
                nodes,
            })
        })();
        match loaded {
            Ok(source) => {
                if source.report.truncated {
                    self.truncated = true;
                    self.gap(format!("{path}: Roslyn invocation inventory truncated"));
                }
                self.sources.insert(path.into(), source);
                true
            }
            Err(e) => {
                self.failed.insert(path.into());
                self.gap(format!("{path}: {e}"));
                false
            }
        }
    }
    fn site(&self, path: &str, inv: &VbInvocation) -> Value {
        let s = &self.sources[path];
        json!({"file":path,"start_line":inv.start_line,"end_line":inv.end_line,"source_hash":s.hash,
            "callee":vb_utf16_span_text(&s.text,inv.callee_span_start,inv.callee_span_length),
            "method":inv.repetition.as_ref().map(|r|&r.method_name)})
    }
    fn markup_bindings(&mut self, path: &str) -> BTreeMap<String, Vec<Value>> {
        let mut result = BTreeMap::<String, Vec<Value>>::new();
        let Some(markup) = path.strip_suffix(".vb") else {
            return result;
        };
        if ![".aspx", ".ascx", ".master"]
            .iter()
            .any(|e| markup.to_ascii_lowercase().ends_with(e))
            || !self.selected.contains_key(markup)
        {
            return result;
        }
        let (text, hash) = match self.read(markup) {
            Ok(v) => v,
            Err(e) => {
                self.gap(format!("{markup}: {e}"));
                return result;
            }
        };
        // Preserve line positions while excluding both server and HTML comments
        // before reusing the existing markup extractor.
        let comments = regex::Regex::new(r"(?s)<%--.*?--%>|<!--.*?-->")
            .expect("literal markup comment pattern");
        let masked = comments.replace_all(&text, |cap: &regex::Captures<'_>| {
            cap[0]
                .chars()
                .map(|c| if c == '\n' || c == '\r' { c } else { ' ' })
                .collect::<String>()
        });
        let (_, edges) = engram_index::webforms::extract_webforms(
            self.directory,
            &engram_core::RelPath::new(markup),
            &masked,
        );
        if !edges
            .iter()
            .any(|e| e.kind == "codebehind_file" && e.target_name == path)
        {
            return result;
        }
        // The legacy event inventory omits OnRowDataBound. Inspect literal
        // attributes on server data controls without changing graph extraction.
        let tags=regex::Regex::new(r#"(?is)<asp:(?:GridView|DataGrid|Repeater|ListView|DataList)\b(?:[^>"']|"[^"]*"|'[^']*')*>"#).expect("data control tags");
        let attrs = regex::Regex::new(r#"(?is)\b([\w:]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
            .expect("quoted attributes");
        for tag in tags.find_iter(&masked) {
            let attributes: Vec<_> = attrs
                .captures_iter(tag.as_str())
                .map(|c| {
                    (
                        c[1].to_string(),
                        c.get(2)
                            .or_else(|| c.get(3))
                            .map_or("", |m| m.as_str())
                            .to_string(),
                    )
                })
                .collect();
            if !attributes
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("runat") && v.eq_ignore_ascii_case("server"))
            {
                continue;
            }
            for (event, handler) in &attributes {
                if !event.eq_ignore_ascii_case("OnRowDataBound")
                    && !event.eq_ignore_ascii_case("OnItemDataBound")
                {
                    continue;
                }
                if handler.is_empty() || !handler.chars().all(|c| c == '_' || c.is_alphanumeric()) {
                    continue;
                }
                let line = masked[..tag.start()]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count()
                    + 1;
                result.entry(handler.to_ascii_lowercase()).or_default().push(json!({"kind":"markup_row_binding","event":event,"file":markup,"start_line":line,"source_hash":hash}));
            }
        }
        result
    }
    pub fn analyze_file(&mut self, path: &str) {
        if !self.load(path) {
            return;
        }
        let markup = self.markup_bindings(path);
        let invocations = self.sources[path].report.invocations.clone();
        let mut methods = BTreeMap::<String, BTreeSet<u32>>::new();
        for inv in &invocations {
            if let Some(r) = &inv.repetition {
                methods
                    .entry(r.method_name.to_ascii_lowercase())
                    .or_default()
                    .insert(r.method_start_line);
            }
        }
        for (index, inv) in invocations.iter().enumerate() {
            let Some(r) = &inv.repetition else {
                continue;
            };
            let mut contexts:Vec<Value>=r.contexts.iter().map(|c|json!({"kind":c.kind,"file":path,"start_line":c.start_line,"end_line":c.end_line,"source_hash":self.sources[path].hash,"binding":c.binding})).collect();
            let name = r.method_name.to_ascii_lowercase();
            if methods.get(&name).is_some_and(|m| m.len() == 1) {
                if let Some(bindings) = markup.get(&name) {
                    contexts.extend(bindings.iter().cloned());
                }
            }
            if r.contexts_truncated {
                self.truncated = true;
                self.gap(format!(
                    "{path}:{}: enclosing repetition contexts truncated",
                    inv.start_line
                ));
            }
            if contexts.is_empty() {
                continue;
            }
            if self.steps >= 2048 || self.findings.len() >= 256 {
                self.truncated = true;
                self.gap("Analysis reached 2048 call steps or 256 findings; narrow file scope; absence beyond this limit is unknown".into());
                break;
            }
            self.walk(path, index, contexts);
        }
    }
    fn walk(&mut self, path: &str, index: usize, contexts: Vec<Value>) {
        let root = self.site(path, &self.sources[path].report.invocations[index]);
        let mut queue = VecDeque::from([(path.to_string(), index, Vec::<Value>::new(), 0usize)]);
        let mut visited = BTreeSet::new();
        while let Some((file, index, mut chain, depth)) = queue.pop_front() {
            if !visited.insert((file.clone(), index)) {
                continue;
            }
            if self.steps >= 2048 || visited.len() > 128 || self.findings.len() >= 256 {
                self.truncated = true;
                self.gap(
                    "Bounded call expansion stopped; unexpanded helpers remain unknown".into(),
                );
                break;
            }
            self.steps += 1;
            let inv = self.sources[&file].report.invocations[index].clone();
            let Some(repetition) = &inv.repetition else {
                continue;
            };
            chain.push(self.site(&file, &inv));
            if let Some(db) = &repetition.database_call {
                let write = db.method.eq_ignore_ascii_case("ExecuteNonQuery")
                    || db.method.eq_ignore_ascii_case("ExecuteNonQueryAsync")
                    || db.method.eq_ignore_ascii_case("Update")
                    || [
                        "SubmitChanges",
                        "SaveChanges",
                        "SaveChangesAsync",
                        "ExecuteCommand",
                    ]
                    .iter()
                    .any(|name| db.method.eq_ignore_ascii_case(name));
                let loop_count = contexts
                    .iter()
                    .filter(|c| {
                        matches!(
                            c["kind"].as_str(),
                            Some("for_each" | "for" | "while" | "do_loop")
                        )
                    })
                    .count();
                let row_binding = contexts.iter().any(|c| {
                    matches!(
                        c["kind"].as_str(),
                        Some("row_data_bound" | "item_data_bound" | "markup_row_binding")
                    )
                });
                self.findings.push(json!({"status":"possible_n_plus_one","repeated_call":root,"contexts":contexts,"call_path":chain,
                    "database_call":{"site":self.site(&file,&inv),"evidence":db},
                    "severity":if loop_count+usize::from(row_binding)>1{"high"}else{"medium"},"severity_basis":"Heuristic repeated-database pattern; actual cardinality and latency unknown",
                    "estimated_iterations":null,"estimated_database_calls":null,
                    "suggested_fix":if write{"Batch the writes where semantics permit; preserve transaction boundaries, validation and per-row side effects."}else{"JOIN the needed data into the initial query, or batch-fetch it before the loop/row binding and look up each row locally; preserve filtering, ordering and access rules."},
                    "limits":"Static possible call path. Guards, caching, dispatch, exceptions, dataset size and actual database execution are not proved."}));
                continue;
            }
            if depth >= 6 {
                self.truncated = true;
                self.gap(format!("{file}:{}: helper depth 6 reached", inv.start_line));
                continue;
            }
            let mut targets = Vec::new();
            if let Some(id) = &inv.target_method_id {
                for (i, other) in self.sources[&file].report.invocations.iter().enumerate() {
                    if other
                        .repetition
                        .as_ref()
                        .is_some_and(|r| r.method_id.as_ref() == Some(id))
                    {
                        targets.push((file.clone(), i));
                    }
                }
            }
            if targets.is_empty() {
                targets = self.graph_targets(&file, &inv);
            }
            for (next, next_index) in targets {
                queue.push_back((next, next_index, chain.clone(), depth + 1));
            }
        }
    }
    fn graph_targets(&mut self, path: &str, inv: &VbInvocation) -> Vec<(String, usize)> {
        let Some(r) = &inv.repetition else {
            return vec![];
        };
        let source = &self.sources[path];
        let owners: Vec<_> = source
            .nodes
            .iter()
            .filter(|n| {
                n.start_line == r.method_start_line
                    && self.selected.get(path) == Some(&n.generation)
            })
            .cloned()
            .collect();
        if owners.len() != 1 {
            self.unresolved_calls += 1;
            return vec![];
        }
        let callee =
            vb_utf16_span_text(&source.text, inv.callee_span_start, inv.callee_span_length)
                .unwrap_or("");
        let member = callee
            .rsplit('.')
            .next()
            .unwrap_or(callee)
            .split('(')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches(['[', ']'])
            .to_string();
        let (edges, truncated) = match self.graph.outgoing_edges_of_kind(
            self.project,
            EdgeKind::Calls,
            &owners[0].node_id,
            129,
        ) {
            Ok(v) => v,
            Err(e) => {
                self.gap(e.to_string());
                return vec![];
            }
        };
        if truncated || edges.len() > 128 {
            self.truncated = true;
            self.gap(format!("{path}: method call edges truncated"));
            return vec![];
        }
        let mut targets = BTreeMap::new();
        for edge in edges {
            if edge.generation != owners[0].generation
                || !edge_at_line(edge.metadata.as_ref(), inv.start_line)
            {
                continue;
            }
            let Some(target) = self
                .graph
                .get_node(self.project, &edge.target_id)
                .ok()
                .flatten()
            else {
                continue;
            };
            let name = target
                .name
                .rsplit('.')
                .next()
                .unwrap_or(&target.name)
                .split('(')
                .next()
                .unwrap_or("");
            if !name.eq_ignore_ascii_case(&member) {
                continue;
            }
            let file = target.file_path.as_str();
            if self.selected.get(file) != Some(&target.generation) {
                self.gap(format!(
                    "{file}: helper target revision differs from selection"
                ));
                continue;
            }
            targets.insert(target.node_id.clone(), target);
        }
        if targets.len() != 1 {
            self.unresolved_calls += 1;
            return vec![];
        }
        let target = targets.into_values().next().expect("one target");
        let file = target.file_path.to_string();
        if !self.load(&file) {
            return vec![];
        }
        self.sources[&file]
            .report
            .invocations
            .iter()
            .enumerate()
            .filter(|(_, inv)| {
                inv.repetition.as_ref().is_some_and(|r| {
                    r.method_start_line == target.start_line && r.method_end_line == target.end_line
                })
            })
            .map(|(i, _)| (file.clone(), i))
            .collect()
    }
    pub fn coverage(&self) -> Value {
        json!({"vb_files_parsed":self.sources.len(),"unavailable_files":self.failed,"source_bytes_read":self.bytes,"call_steps":self.steps,"unresolved_calls_observed":self.unresolved_calls,
            "truncated":self.truncated,"gaps":self.gaps,"source_admission":"Selected membership, physical file/node revision and current BLAKE3 fingerprint; fresh Roslyn syntax report SHA256 checked. No atomic source/graph snapshot.",
            "scope":"VB.NET For Each/For/While/Do bodies or repeated conditions, explicit Handles RowDataBound/ItemDataBound and paired selected markup bindings. Deferred lambdas are excluded. Typed ADO.NET receiver declarations, declared DataContext/DbContext query materialization and direct execution methods provide candidate evidence. Query origin tracing is bounded to 8 syntax steps; materialized collections and reassigned local query aliases are excluded. Constructor SQL text is not effective command-state proof. Constructors/accessors, provider inheritance, injected receiver parameters, translated SQL and dynamic dispatch remain unassessed.",
            "runtime_query_count_known":false,"no_findings_means":"No supported repeated database-call evidence found within this file page and bounds; not proof that N+1 queries are absent."})
    }
}

fn edge_at_line(metadata: Option<&Value>, line: u32) -> bool {
    let number = |v: &Value| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    };
    metadata.is_some_and(|m| {
        m.get("call_site_line").and_then(number) == Some(line as u64)
            || m.get("call_site_lines")
                .and_then(Value::as_array)
                .is_some_and(|rows| rows.iter().any(|v| number(v) == Some(line as u64)))
    })
}

pub fn validate_report(source: &str, report: &VbInvocationReport) -> anyhow::Result<()> {
    anyhow::ensure!(
        report.repetition_version.as_deref() == Some("vb-repetition-contexts-v1"),
        "Sidecar repetition contexts unavailable; use the matching server/sidecar release"
    );
    anyhow::ensure!(
        report.parse_status == "complete" && report.parse_error_count == 0,
        "VB syntax errors; repetition evidence withheld"
    );
    anyhow::ensure!(
        report.invocations.len() <= 512,
        "Invocation inventory exceeds 512"
    );
    let lines = source.lines().count() as u32;
    for inv in &report.invocations {
        let Some(r) = &inv.repetition else {
            continue;
        };
        anyhow::ensure!(
            r.method_start_line > 0
                && r.method_end_line <= lines
                && r.method_start_line <= inv.start_line
                && inv.end_line <= r.method_end_line,
            "Invalid enclosing method span"
        );
        anyhow::ensure!(r.contexts.len() <= 8, "Repetition contexts exceed 8");
        for c in &r.contexts {
            let end = c
                .span_start
                .checked_add(c.span_length)
                .ok_or_else(|| anyhow::anyhow!("Context span overflow"))?;
            let invocation_end = inv
                .span_start
                .checked_add(inv.span_length)
                .ok_or_else(|| anyhow::anyhow!("Invocation span overflow"))?;
            anyhow::ensure!(
                c.start_line > 0
                    && c.end_line >= c.start_line
                    && c.end_line <= lines
                    && c.span_start <= inv.span_start
                    && invocation_end <= end
                    && vb_utf16_span_text(source, c.span_start, c.span_length).is_some(),
                "Invalid repetition ownership span"
            );
        }
        if let Some(db) = &r.database_call {
            anyhow::ensure!(
                db.declaration_line > 0 && db.declaration_line <= lines,
                "Invalid database receiver declaration line"
            );
        }
    }
    Ok(())
}
