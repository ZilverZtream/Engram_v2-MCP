//! Authored ES-module evidence. Resolution is syntax-based, not compiler or
//! runtime proof. Sites are one-based lines/columns and zero-based UTF-8 bytes.
use serde::{Deserialize, Serialize};

pub const MODULE_KEY: &str = "typescript_module_v1";
pub const CALL_KEY: &str = "typescript_module_call_v1";
pub const CONFIG_KEY: &str = "typescript_config_v1";
pub const UNBOUND_PREFIX: &str = "::typescript_module:";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Site {
    pub offset: usize,
    pub end_offset: usize,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Import {
    pub local: String,
    /// `default`, `*` (namespace), or the authored export name.
    pub imported: String,
    pub module: String,
    pub type_only: bool,
    pub site: Site,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Export {
    pub exported: String,
    pub local: String,
    pub module: Option<String>,
    /// local, reexport, star or namespace. Stars exclude default exports.
    pub kind: String,
    pub type_only: bool,
    pub site: Site,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Declaration {
    pub local: String,
    pub site: Site,
    pub callable: bool,
    pub has_body: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Module {
    pub version: u32,
    pub imports: Vec<Import>,
    pub exports: Vec<Export>,
    pub declarations: Vec<Declaration>,
    pub calls: Vec<OwnedCall>,
    /// Incomplete or unsupported syntax must not silently produce proof.
    pub issues: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnedCall {
    pub source_name: String,
    pub source_kind: String,
    pub source_line: u32,
    pub call: Call,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Call {
    pub version: u32,
    pub local: String,
    pub imported: String,
    pub module: String,
    pub import_site: Site,
    pub site: Site,
    /// None means eligible for module resolution, not already resolved.
    pub unbound_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    pub extends: Option<String>,
    pub base_url: Option<String>,
    pub paths: Option<std::collections::BTreeMap<String, Vec<String>>>,
    pub issues: Vec<String>,
}
