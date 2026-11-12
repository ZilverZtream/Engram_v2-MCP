//! Authored method syntax shared by native extraction, the VB companion and
//! the post-ingest definition/use pass. This is not an execution trace.
use serde::{Deserialize, Serialize};

pub const METADATA_KEY: &str = "value_flow_v1";
pub const MAX_SYNTAX_NODES: usize = 4096;
// Keep the serialized object/array nesting below serde_json's 128-level limit.
pub const MAX_SYNTAX_DEPTH: usize = 32;
pub const MAX_SOURCE_TEXT_CHARS: usize = 512;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FlowSite {
    pub offset: u32,
    pub end_offset: u32,
    pub line: u32,
    pub column: u32,
    pub text: String,
    pub text_truncated: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FlowParameter {
    pub name: String,
    pub index: u32,
    pub site: FlowSite,
    pub by_ref: bool,
    pub variadic: bool,
    pub optional: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FlowArgument {
    pub index: u32,
    pub parameter_index: Option<u32>,
    pub name: Option<String>,
    pub by_ref: bool,
    pub site: FlowSite,
    pub value: FlowExpression,
}

/// `children` contains value operands, never a type annotation or the member
/// name in `receiver.Member`. Call arguments keep their authored order/names.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FlowExpression {
    pub kind: String,
    pub site: FlowSite,
    pub name: Option<String>,
    pub operator: Option<String>,
    pub children: Vec<FlowExpression>,
    pub arguments: Vec<FlowArgument>,
    /// Optional compiler identity, corroborated against an admitted Calls edge
    /// and the target declaration before making an inter-method link.
    pub target_semantic_id: Option<String>,
}

/// Statements retain blocks and branches; flattening them would confuse
/// shadowed locals and invent definition/use paths across assignments.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FlowStatement {
    pub kind: String,
    pub site: FlowSite,
    pub name: Option<String>,
    pub expression: Option<FlowExpression>,
    pub body: Vec<FlowStatement>,
    pub otherwise: Vec<FlowStatement>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FlowProgram {
    pub version: u32,
    pub language: String,
    /// Both offsets and columns use this unit (native UTF-8 / Roslyn UTF-16).
    pub offset_unit: String,
    pub parameters: Vec<FlowParameter>,
    pub body: FlowStatement,
    pub issues: Vec<String>,
}
