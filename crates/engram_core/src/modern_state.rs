//! Retained source observations for state references resolved after indexing.
use crate::typescript_modules::{Import, Site};
use serde::{Deserialize, Serialize};

pub const KEY: &str = "modern_state_v1";
pub const VERSION: u32 = 2;
pub const BINDING_KEY: &str = "modern_state_binding_v1";
pub const UNBOUND_PREFIX: &str = "::modern_state:";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reference {
    pub source_name: String,
    pub source_kind: String,
    pub source_line: u32,
    pub site: Site,
    pub import: Import,
    pub imported: String,
    pub suffix: String,
    /// None selects ordinary mutable module/singleton state only.
    pub category: Option<String>,
    pub reads: bool,
    pub writes: bool,
    #[serde(default)]
    pub unbound_reason: Option<String>,
}
