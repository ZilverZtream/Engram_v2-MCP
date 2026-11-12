//! Python source bindings. These facts describe syntax, not import execution.
pub use crate::typescript_modules::{Call, OwnedCall, Site};
use serde::{Deserialize, Serialize};

pub const MODULE_KEY: &str = "python_module_v1";
pub const CALL_KEY: &str = "python_module_call_v1";
pub const UNBOUND_PREFIX: &str = "::python_module:";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Binding {
    pub local: String,
    /// Spelling bound by an import, e.g. `pkg.api` or its explicit alias.
    pub access: String,
    pub module: String,
    /// `*` denotes a module object, not a wildcard import.
    pub imported: String,
    pub kind: String,
    pub site: Site,
    pub scope: usize,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Module {
    pub version: u32,
    pub bindings: Vec<Binding>,
    pub calls: Vec<OwnedCall>,
    pub issues: Vec<String>,
}
