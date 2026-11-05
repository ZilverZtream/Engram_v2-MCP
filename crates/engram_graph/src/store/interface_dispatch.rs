//! Managed, exact interface-member evidence. Extractor strings cannot mint
//! these objects; ingestion admits their source locators before storing them.
use super::{Edge, JsonValue, Node};
use serde::{Deserialize, Serialize};

pub const INTERFACE_BINDINGS: &str = "_engram_interface_bindings";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterfaceMemberLocator {
    pub version: u32,
    pub name: String,
    pub semantic_id: String,
    pub file_path: String,
    pub start_line: u32,
    pub span_start: u32,
    pub span_length: u32,
    pub syntax_source_sha256: String,
    pub contract_type_key: String,
    pub contract_type: String,
}

impl InterfaceMemberLocator {
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
            && hash(&self.contract_type_key)
            && text(&self.contract_type, 2048)
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
            && node.file_path.as_str() == self.file_path
            && node.name == self.name
            && node.start_line == self.start_line
            && field("interface_member_declaration") == Some("true")
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
pub struct InterfaceBinding {
    pub target: InterfaceMemberLocator,
    pub source_semantic_id: String,
    pub source_generation: u64,
    pub source_file_hash: String,
    pub target_generation: u64,
    pub target_file_hash: String,
    pub parser: JsonValue,
    /// Positions belong to this particular constructed contract, not the
    /// edge-wide union of calls to different constructed generic instances.
    pub reference_lines: Vec<u32>,
    pub reference_lines_truncated: bool,
}

impl InterfaceBinding {
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
        self.source_generation > 0
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
pub struct InterfaceBindings {
    pub version: u32,
    pub complete: bool,
    pub bindings: Vec<InterfaceBinding>,
}

pub fn interface_bindings(edge: &Edge) -> anyhow::Result<Option<InterfaceBindings>> {
    let Some(value) = edge
        .metadata
        .as_ref()
        .and_then(|m| m.get(INTERFACE_BINDINGS))
    else {
        return Ok(None);
    };
    anyhow::ensure!(value.is_object(), "Invalid managed interface evidence");
    let bindings: InterfaceBindings = serde_json::from_value(value.clone())?;
    anyhow::ensure!(
        bindings.version == 1
            && !bindings.bindings.is_empty()
            && bindings.bindings.len() <= 16
            && bindings
                .bindings
                .iter()
                .all(|binding| binding.target.valid() && binding.reference_lines.len() <= 32),
        "Invalid managed interface evidence"
    );
    Ok(Some(bindings))
}

/// One graph key may contain calls to several constructed generic contracts.
/// Keep each contract's positions together rather than retaining first-wins.
pub fn merge_interface_bindings(
    base: &mut serde_json::Map<String, JsonValue>,
    extra: &serde_json::Map<String, JsonValue>,
) -> anyhow::Result<()> {
    let Some(extra) = extra.get(INTERFACE_BINDINGS) else {
        return Ok(());
    };
    let mut next: InterfaceBindings = serde_json::from_value(extra.clone())?;
    let Some(existing) = base.get_mut(INTERFACE_BINDINGS) else {
        base.insert(INTERFACE_BINDINGS.into(), extra.clone());
        return Ok(());
    };
    let mut kept: InterfaceBindings = serde_json::from_value(existing.clone())?;
    anyhow::ensure!(
        kept.version == 1 && next.version == 1,
        "Unsupported interface evidence"
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
    // Refuse excessive producer evidence before publication; never silently
    // discard the second constructed type or publish an undecodable record.
    let encoded = serde_json::to_value(&kept)?;
    anyhow::ensure!(
        serde_json::to_vec(&encoded)?.len() <= 128 * 1024,
        "Interface evidence byte limit"
    );
    *existing = encoded;
    Ok(())
}
