//! Retained Java source evidence for package/import and declared-type calls.
pub use crate::typescript_modules::Site;
use serde::{Deserialize, Serialize};
pub const MODULE_KEY: &str = "java_module_v1";
pub const CALL_KEY: &str = "java_module_call_v1";
pub const UNBOUND_PREFIX: &str = "::java_module:";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Import {
    pub path: String,
    pub is_static: bool,
    pub wildcard: bool,
    pub site: Site,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Type {
    pub name: String,
    pub owner: Option<String>,
    pub canonical: String,
    pub access: String,
    pub inherited: bool,
    pub scoped: bool,
    pub site: Site,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Method {
    pub owner: String,
    pub name: String,
    pub access: String,
    pub is_static: bool,
    pub arity: usize,
    pub variadic: bool,
    pub site: Site,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Call {
    pub version: u32,
    pub local: String,
    pub imported: String,
    pub module: String,
    pub import_site: Site,
    pub path_from_use: bool,
    pub site: Site,
    pub owner: String,
    pub qualifier: Option<String>,
    pub receiver_type: Option<String>,
    pub receiver_site: Option<Site>,
    pub invocation: String,
    pub arity: usize,
    pub static_context: bool,
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
    pub types: Vec<Type>,
    pub methods: Vec<Method>,
    pub calls: Vec<OwnedCall>,
    pub named_module: bool,
    pub issues: Vec<String>,
}
