//! Source-declared LINQ-to-SQL mappings for already reached VB ORM references.
//! This does not translate LINQ, inspect a database, or prove query execution.
use super::routing::{self, Source};
use engram_graph::{Edge, GraphStore, Node};
use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const NS: &[u8] = b"http://schemas.microsoft.com/linqtosql/dbml/2007";
const MAX_MAPPINGS: usize = 4096;

struct Mapping {
    context: String,
    member: String,
    table: String,
    path: String,
    line: u32,
    hash: String,
}

pub(super) struct Catalog {
    mappings: Vec<Mapping>,
    contexts: BTreeMap<String, usize>,
    complete: bool,
    pub gaps: Vec<String>,
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}

fn parse(source: &Source<'_>) -> anyhow::Result<(String, Vec<Mapping>)> {
    let mut reader = NsReader::from_str(source.content);
    reader.config_mut().expand_empty_elements = true;
    let lines: Vec<_> = std::iter::once(0)
        .chain(source.content.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let hash = blake3::hash(source.content.as_bytes()).to_hex().to_string();
    let mut depth = 0usize;
    let mut seen_root = false;
    let mut context = String::new();
    let mut mappings = Vec::new();
    loop {
        let position = reader.buffer_position() as usize;
        let (namespace, event) = reader.read_resolved_event()?;
        match event {
            Event::Start(element) => {
                anyhow::ensure!(depth < 64, "DBML nesting limit exceeded");
                let dbml = matches!(namespace, ResolveResult::Bound(ns) if ns.as_ref() == NS);
                let mut attributes = BTreeMap::new();
                for attribute in element.attributes() {
                    let attribute = attribute?;
                    // Only predefined XML entities; DTDs are rejected below.
                    attributes.insert(
                        std::str::from_utf8(attribute.key.as_ref())?.to_owned(),
                        attribute.unescape_value()?.into_owned(),
                    );
                }
                if depth == 0 {
                    anyhow::ensure!(
                        !seen_root && dbml && element.local_name().as_ref() == b"Database",
                        "Expected one DBML Database root"
                    );
                    seen_root = true;
                    let class = attributes.get("Class").map(String::as_str).unwrap_or("");
                    anyhow::ensure!(
                        identifier(class),
                        "Missing or unsupported DBML context class"
                    );
                    let namespace = attributes
                        .get("ContextNamespace")
                        .map(String::as_str)
                        .unwrap_or("");
                    anyhow::ensure!(
                        namespace.is_empty() || namespace.split('.').all(identifier),
                        "Unsupported DBML context namespace"
                    );
                    context = if namespace.is_empty() {
                        class.into()
                    } else {
                        format!("{namespace}.{class}")
                    };
                } else if depth == 1 && dbml && element.local_name().as_ref() == b"Table" {
                    let member = attributes.get("Member").map(String::as_str).unwrap_or("");
                    let table = attributes.get("Name").map(String::as_str).unwrap_or("");
                    anyhow::ensure!(
                        identifier(member)
                            && !table.is_empty()
                            && table.len() <= 512
                            && !table.chars().any(char::is_control),
                        "Invalid DBML table mapping"
                    );
                    anyhow::ensure!(mappings.len() < MAX_MAPPINGS, "DBML mapping limit exceeded");
                    mappings.push(Mapping {
                        context: context.clone(),
                        member: member.into(),
                        table: table.into(),
                        path: source.path.into(),
                        line: lines.partition_point(|start| *start <= position) as u32,
                        hash: hash.clone(),
                    });
                }
                depth += 1;
            }
            Event::End(_) => {
                anyhow::ensure!(depth > 0, "Unexpected DBML end element");
                depth -= 1;
            }
            Event::DocType(_) => anyhow::bail!("DBML document types are not supported"),
            Event::Text(text) => {
                anyhow::ensure!(text.unescape()?.trim().is_empty(), "Unexpected DBML text")
            }
            Event::CData(_) => anyhow::bail!("Unexpected DBML CDATA"),
            Event::Eof => {
                anyhow::ensure!(seen_root && depth == 0, "Incomplete DBML document");
                break;
            }
            _ => {}
        }
    }
    Ok((context, mappings))
}

impl Catalog {
    pub fn new(
        graph: &GraphStore,
        project: &str,
        sources: &[Source<'_>],
        selected: &BTreeMap<String, u64>,
    ) -> anyhow::Result<Self> {
        let mut result = Self {
            mappings: Vec::new(),
            contexts: BTreeMap::new(),
            complete: true,
            gaps: Vec::new(),
        };
        for (path, revision) in selected
            .iter()
            .filter(|(path, _)| path.to_ascii_lowercase().ends_with(".dbml"))
        {
            let source = sources.iter().find(|source| source.path == path);
            let file = graph.get_node(project, &format!("file:{path}"))?;
            let admitted = source.zip(file.as_ref()).filter(|(source, file)| {
                file.generation == *revision
                    && file
                        .metadata
                        .as_ref()
                        .and_then(|v| v.get("file_hash"))
                        .and_then(Value::as_str)
                        .is_some_and(|hash| {
                            hash == blake3::hash(source.content.as_bytes()).to_hex().as_str()
                        })
            });
            let parsed = admitted.map(|(source, _)| parse(source));
            match parsed {
                Some(Ok((context, mappings)))
                    if result.mappings.len() + mappings.len() <= MAX_MAPPINGS =>
                {
                    *result
                        .contexts
                        .entry(context.to_ascii_lowercase())
                        .or_default() += 1;
                    result.mappings.extend(mappings);
                }
                _ => {
                    result.complete = false;
                    if result.gaps.len() < 16 {
                        result.gaps.push(format!("{path}: DBML unavailable, stale, malformed or over limit; mapping uniqueness is unproven, so DBML bindings are withheld"));
                    }
                }
            }
        }
        Ok(result)
    }

    pub fn resolve(&self, method: &Node, body: &str, edge: &Edge) -> Result<Value, &'static str> {
        if !self.complete {
            return Err("DBML inventory incomplete; mapping withheld");
        }
        let member = edge
            .target_id
            .strip_prefix("table:")
            .filter(|name| identifier(name))
            .ok_or("ORM reference has no supported member name")?;
        let metadata = edge
            .metadata
            .as_ref()
            .ok_or("ORM reference lacks source evidence")?;
        let line = metadata
            .get("src_line")
            .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
            .ok_or("ORM reference lacks source line")?;
        if line < method.start_line as u64 || line > method.end_line as u64 {
            return Err("ORM source line outside reached member");
        }
        let (receiver, context) =
            context_at(body, member, (line - method.start_line as u64 + 1) as usize)
                .ok_or("Local ORM receiver is unknown, aliased, reassigned or ambiguous")?;
        if self.contexts.get(&context.to_ascii_lowercase()) != Some(&1) {
            return Err(
                "DBML context is missing or ambiguous; no namespace/default-context inference",
            );
        }
        let matches: Vec<_> = self
            .mappings
            .iter()
            .filter(|m| {
                m.context.eq_ignore_ascii_case(&context) && m.member.eq_ignore_ascii_case(member)
            })
            .collect();
        let [mapping] = matches.as_slice() else {
            return Err("DBML context/member mapping is missing or ambiguous");
        };
        Ok(json!({
            "table": mapping.table, "context": mapping.context, "member": mapping.member, "receiver": receiver,
            "mapping_source": {"file": mapping.path, "line": mapping.line, "source_blake3": mapping.hash},
            "query_source": {"file": method.file_path.as_str(), "line": line},
            "basis": "selected_current_dbml_and_explicit_local_vb_context",
            "interpretation": "Source-declared table mapping for this indexed ORM reference. Not a table definition, generated SQL, deployed database identity, proof of query execution, write completion or trigger execution."
        }))
    }
}

/// Conservative local construction binding. Unknown aliases, shadowing or
/// receiver escapes are withheld rather than treated as a compiler type proof.
fn context_at(body: &str, member: &str, line: usize) -> Option<(String, String)> {
    let tokens = routing::tokens(body, true);
    let word = |i: usize, value: &str| {
        tokens
            .get(i)
            .is_some_and(|t| !t.string && t.text.eq_ignore_ascii_case(value))
    };
    let receivers: BTreeSet<_> = (0..tokens.len().saturating_sub(2))
        .filter(|&i| {
            !tokens[i].string
                && identifier(&tokens[i].text)
                && word(i + 1, ".")
                && word(i + 2, member)
                && tokens[i + 2].line == line
                && (i == 0 || !word(i - 1, "."))
        })
        .map(|i| tokens[i].text.to_ascii_lowercase())
        .collect();
    if receivers.len() != 1 {
        return None;
    }
    let receiver = receivers.into_iter().next()?;
    let declarations: Vec<_> = (0..tokens.len().saturating_sub(4))
        .filter(|&i| word(i, "Dim") && word(i + 1, &receiver))
        .collect();
    let [declaration] = declarations.as_slice() else {
        return None;
    };
    let i = *declaration;
    if tokens[i].line >= line || !(word(i + 2, "As") || word(i + 2, "=")) || !word(i + 3, "New") {
        return None;
    }
    let mut j = i + 4;
    let mut parts = vec![tokens.get(j)?.text.clone()];
    if tokens[j].string || !identifier(&tokens[j].text) {
        return None;
    }
    while word(j + 1, ".") {
        j += 2;
        if tokens.get(j)?.string || !identifier(&tokens[j].text) {
            return None;
        }
        parts.push(tokens[j].text.clone());
    }
    if parts
        .first()
        .is_some_and(|s| s.eq_ignore_ascii_case("Global"))
    {
        parts.remove(0);
    }
    if parts.is_empty() {
        return None;
    }
    // Require the constructor result itself, not a generic/array/chained value.
    if word(j + 1, "(") {
        if word(j + 2, "Of") {
            return None;
        }
        let mut nesting = 1usize;
        j += 2;
        while j < tokens.len() && nesting > 0 {
            if word(j, "(") {
                nesting += 1;
            }
            if word(j, ")") {
                nesting -= 1;
            }
            if nesting == 0 {
                break;
            }
            j += 1;
        }
        if nesting != 0 {
            return None;
        }
    }
    if tokens
        .get(j + 1)
        .is_some_and(|t| t.line == tokens[j].line && t.text != ":")
    {
        return None;
    }
    for k in 0..tokens.len() {
        if word(k, &receiver) && k != i + 1 {
            if !word(k + 1, ".") || (k > 0 && word(k - 1, ".")) {
                return None;
            }
        }
    }
    Some((receiver, parts.join(".")))
}
