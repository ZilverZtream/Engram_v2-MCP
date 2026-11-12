//! Authored Rust modules and Cargo identities; no macro or compiler proof.
pub use crate::typescript_modules::Site;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub const MODULE_KEY: &str = "rust_module_v1";
pub const CONFIG_KEY: &str = "rust_cargo_v1";
pub const CALL_KEY: &str = "rust_module_call_v1";
pub const UNBOUND_PREFIX: &str = "::rust_module:";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Binding {
    pub name: String,
    pub symbol: String,
    pub kind: String,
    pub context: Vec<String>,
    /// Zero denotes the enclosing module; other values identify lexical blocks.
    pub scope: usize,
    pub path: Vec<String>,
    pub site: Site,
    pub visibility: String,
    pub inline: bool,
    pub path_attribute: Option<String>,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Call {
    pub version: u32,
    pub local: String,
    pub module: String,
    pub imported: String,
    pub context: Vec<String>,
    pub path_from_use: bool,
    pub local_target: Option<(String, u32)>,
    pub glob_paths: Vec<Vec<String>>,
    pub import_site: Site,
    pub site: Site,
    pub unbound_reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnedCall {
    pub source_name: String,
    pub source_kind: String,
    pub source_line: u32,
    pub call: Call,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Module {
    pub version: u32,
    pub bindings: Vec<Binding>,
    pub calls: Vec<OwnedCall>,
    pub issues: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Target {
    pub name: String,
    pub path: Option<String>,
    pub kind: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dependency {
    pub path: Option<String>,
    pub workspace: bool,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub name: Option<String>,
    pub edition: Option<String>,
    pub inherit_edition: bool,
    pub workspace_edition: Option<String>,
    pub targets: Vec<Target>,
    pub autobins: bool,
    pub dependencies: BTreeMap<String, Dependency>,
    pub workspace_dependencies: BTreeMap<String, Dependency>,
    pub issues: Vec<String>,
}
