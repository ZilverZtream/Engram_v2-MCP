//! Managed, exact compiler-selected ordinary source-call evidence. Extractor strings cannot mint
//! these objects; ingestion admits their source locators before storing them.
use super::{Edge, JsonValue, Node};
use serde::{Deserialize, Serialize};

pub const SOURCE_CALL_BINDINGS: &str = "_engram_source_call_bindings";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMemberLocator {
    pub version: u32,
    pub name: String,
    pub semantic_id: String,
    pub file_path: String,
    pub start_line: u32,
    pub span_start: u32,
    pub span_length: u32,
    pub syntax_source_sha256: String,
}

impl SourceMemberLocator {
    pub fn valid(&self) -> bool {
        let text = |value: &str, max| {
            !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
        };
        let hash = |value: &str| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit());
        self.version == 1
            && text(&self.name, 2048)
            && text(&self.semantic_id, 4096)
            && text(&self.file_path, 2048)
            && !self.file_path.contains(['\\', ':'])
            && self
                .file_path
                .split('/')
                .all(|part| !matches!(part, "" | "." | ".."))
            && self.start_line > 0
            && self.span_length > 0
            && self.span_start.checked_add(self.span_length).is_some()
            && hash(&self.syntax_source_sha256)
    }

    pub fn matches(&self, node: &Node, file: &Node) -> bool {
        let metadata = node.metadata.as_ref();
        let field = |key: &str| {
            metadata
                .and_then(|m| m.get(key))
                .and_then(JsonValue::as_str)
        };
        self.valid()
            && node.node_type == "function"
            && field("interface_member_declaration") != Some("true")
            && node.generation > 0
            && node.node_id == engram_core::ids::NodeId::symbol("function", None,
                node.file_path.as_str(), &node.name, node.start_line).0
            && file.node_id == engram_core::ids::NodeId::file(node.file_path.as_str()).0
            && node.file_path.as_str() == self.file_path
            && node.name == self.name
            && node.start_line == self.start_line
            && field("semantic_member_id") == Some(self.semantic_id.as_str())
            && field("semantic_span_start").and_then(|s| s.parse::<u32>().ok())
                == Some(self.span_start)
            && field("semantic_span_length").and_then(|s| s.parse::<u32>().ok())
                == Some(self.span_length)
            && file.node_type == "file"
            && file.file_path == node.file_path
            && file.generation == node.generation
            && file
                .metadata
                .as_ref()
                .and_then(|m| m.get("vb_syntax_source_sha256"))
                .and_then(JsonValue::as_str)
                == Some(self.syntax_source_sha256.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceCallBinding {
    pub target: SourceMemberLocator,
    pub source_semantic_id: String,
    pub source_generation: u64,
    pub source_file_hash: String,
    pub target_generation: u64,
    pub target_file_hash: String,
    pub parser: JsonValue,
    /// Positions belong to this admitted source and exact selected declaration.
    pub reference_lines: Vec<u32>,
    pub reference_lines_truncated: bool,
}

impl SourceCallBinding {
    pub fn matches(
        &self,
        source: &Node,
        source_file: &Node,
        target: &Node,
        target_file: &Node,
    ) -> bool {
        let meta =
            |file: &Node, key: &str| file.metadata.as_ref().and_then(|m| m.get(key)).cloned();
        let hash =
            |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
        source.node_type == "function"
            && source.namespace == engram_core::namespaces::NAMESPACE_MEMORY
            && target.namespace == engram_core::namespaces::NAMESPACE_MEMORY
            && source_file.namespace == engram_core::namespaces::NAMESPACE_MEMORY
            && target_file.namespace == engram_core::namespaces::NAMESPACE_MEMORY
            && self.source_generation > 0
            && self.target_generation > 0
            && hash(&self.source_file_hash)
            && hash(&self.target_file_hash)
            && source.node_id
                == engram_core::ids::NodeId::symbol(
                    "function",
                    None,
                    source.file_path.as_str(),
                    &source.name,
                    source.start_line,
                )
                .0
            && target.node_id
                == engram_core::ids::NodeId::symbol(
                    "function",
                    None,
                    target.file_path.as_str(),
                    &target.name,
                    target.start_line,
                )
                .0
            && source_file.node_id == engram_core::ids::NodeId::file(source.file_path.as_str()).0
            && target_file.node_id == engram_core::ids::NodeId::file(target.file_path.as_str()).0
            && self.source_semantic_id.len() <= 4096
            && !self.source_semantic_id.is_empty()
            && !self.source_semantic_id.chars().any(char::is_control)
            && source
                .metadata
                .as_ref()
                .and_then(|m| m.get("semantic_member_id"))
                .and_then(JsonValue::as_str)
                == Some(self.source_semantic_id.as_str())
            && source.generation == self.source_generation
            && source_file.generation == source.generation
            && source_file.node_type == "file"
            && source_file.file_path == source.file_path
            && target.generation == self.target_generation
            && self.target.matches(target, target_file)
            && meta(source_file, "file_hash")
                .as_ref()
                .and_then(JsonValue::as_str)
                == Some(self.source_file_hash.as_str())
            && meta(target_file, "file_hash")
                .as_ref()
                .and_then(JsonValue::as_str)
                == Some(self.target_file_hash.as_str())
            && self.parser.get("backend").and_then(JsonValue::as_str) == Some("roslyn")
            && meta(source_file, "vb_parser_provenance").as_ref() == Some(&self.parser)
            && meta(target_file, "vb_parser_provenance").as_ref() == Some(&self.parser)
            && self.reference_lines.len() <= 32
            && self
                .reference_lines
                .iter()
                .all(|line| *line >= source.start_line && *line <= source.end_line)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceCallBindings {
    pub version: u32,
    pub complete: bool,
    pub bindings: Vec<SourceCallBinding>,
}

pub fn source_call_bindings(edge: &Edge) -> anyhow::Result<Option<SourceCallBindings>> {
    let Some(value) = edge
        .metadata
        .as_ref()
        .and_then(|m| m.get(SOURCE_CALL_BINDINGS))
    else {
        return Ok(None);
    };
    let bindings = decode(value)?;
    Ok(Some(bindings))
}

fn decode(value: &JsonValue) -> anyhow::Result<SourceCallBindings> {
    anyhow::ensure!(value.is_object() && serde_json::to_vec(value)?.len() <= 128 * 1024, "Invalid managed source-call evidence");
    let bindings: SourceCallBindings = serde_json::from_value(value.clone())?;
    anyhow::ensure!(
        bindings.version == 1
            && !bindings.bindings.is_empty()
            && bindings.bindings.len() <= 16
            && bindings
                .bindings
                .iter()
                .all(|binding| binding.target.valid() && binding.reference_lines.len() <= 32),
        "Invalid managed source-call evidence"
    );
    Ok(bindings)
}

/// Preserve positions only with the exact proof that admitted them.
pub fn merge_source_call_bindings(
    base: &mut serde_json::Map<String, JsonValue>,
    extra: &serde_json::Map<String, JsonValue>,
) -> anyhow::Result<()> {
    let Some(extra) = extra.get(SOURCE_CALL_BINDINGS) else {
        return Ok(());
    };
    let mut next = decode(extra)?;
    let Some(existing) = base.get_mut(SOURCE_CALL_BINDINGS) else {
        base.insert(SOURCE_CALL_BINDINGS.into(), extra.clone());
        return Ok(());
    };
    let mut kept = decode(existing)?;
    anyhow::ensure!(
        kept.version == 1 && next.version == 1,
        "Unsupported source-call evidence"
    );
    kept.complete &= next.complete;
    for mut binding in next.bindings.drain(..) {
        if let Some(same) = kept.bindings.iter_mut().find(|old| {
            let mut old = (*old).clone();
            old.reference_lines.clear();
            old.reference_lines_truncated = false;
            let mut new = binding.clone();
            new.reference_lines.clear();
            new.reference_lines_truncated = false;
            old == new
        }) {
            same.reference_lines.append(&mut binding.reference_lines);
            same.reference_lines.sort_unstable();
            same.reference_lines.dedup();
            same.reference_lines_truncated |=
                binding.reference_lines_truncated || same.reference_lines.len() > 32;
            same.reference_lines.truncate(32);
        } else if kept.bindings.len() < 16 {
            kept.bindings.push(binding);
        } else {
            kept.complete = false;
        }
    }
    // Bound merged proof bytes before publication.
    let encoded = serde_json::to_value(&kept)?;
    anyhow::ensure!(
        serde_json::to_vec(&encoded)?.len() <= 128 * 1024,
        "Source-call evidence byte limit"
    );
    *existing = encoded;
    Ok(())
}

/// Presence of any producer or managed source-call marker prohibits legacy
/// name-based admission, including malformed or explicitly unknown evidence.
pub(crate) fn requires_source_call_binding(edge: &Edge) -> bool {
    edge.edge_kind == super::EdgeKind::Calls &&
        (edge.target_id.starts_with("unverified-source-call:") ||
        edge.metadata.as_ref().is_some_and(|m| [SOURCE_CALL_BINDINGS,
            "source_call_target_v1", "source_call_status", "source_call_binding_admission"]
            .iter().any(|key| m.get(key).is_some())))
}

pub(crate) fn source_call_binding_matches(edge: &Edge, source: &Node, source_file: &Node,
    target: &Node, target_file: &Node) -> bool {
    edge.source_id == source.node_id && edge.target_id == target.node_id
        && edge.metadata.as_ref().and_then(|m| m.get("source_call_binding_admission"))
            .and_then(JsonValue::as_str) == Some("verified_source_declaration")
        && source_call_bindings(edge).ok().flatten().is_some_and(|proofs|
            proofs.complete && proofs.bindings.iter().all(|proof|
                proof.matches(source, source_file, target, target_file)))
}
