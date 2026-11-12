//! Go package/import evidence, independent of compiler or build selection.
pub use crate::typescript_modules::Site;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MODULE_KEY: &str = "go_module_v1";
pub const CONFIG_KEY: &str = "go_config_v1";
pub const CALL_KEY: &str = "go_module_call_v1";
pub const UNBOUND_PREFIX: &str = "::go_module:";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Import {
    pub alias: Option<String>,
    pub path: String,
    pub site: Site,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Declaration {
    pub name: String,
    pub kind: String,
    pub site: Site,
    pub has_body: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Call {
    pub version: u32,
    pub local: String,
    pub qualifier: Option<String>,
    pub imported: String,
    pub module: String,
    pub import_site: Site,
    pub path_from_use: bool,
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
    pub package: String,
    pub imports: Vec<Import>,
    pub declarations: Vec<Declaration>,
    pub calls: Vec<OwnedCall>,
    pub test_file: bool,
    pub constraint: Option<String>,
    pub issues: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Replacement {
    pub module: String,
    pub version: Option<String>,
    pub destination: String,
    pub destination_version: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    pub module: Option<String>,
    pub requires: BTreeMap<String, String>,
    pub replaces: Vec<Replacement>,
    pub uses: Vec<String>,
    pub issues: Vec<String>,
}
