use crate::{EngramError, Result};
use serde::{Deserialize, Serialize};

/// Defines how data in a namespace is updated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NamespaceVersioning {
    /// Entire state is replaced per generation (atomic codebase view).
    Snapshot,
    /// New docs are added, old ones stay (event stream).
    AppendOnly,
    /// Single global state, updates overwrite by unique key (global knowledge).
    GlobalMutable,
}

/// Defines how long data in a namespace is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NamespaceRetention {
    /// Only keep the latest generation.
    KeepLatestOnly,
    /// Keep the last N generations.
    KeepLastGenerations(u32),
    /// Data is never purged based on generation.
    KeepForever,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespacePolicy {
    pub versioning: NamespaceVersioning,
    pub retention: NamespaceRetention,
}

/// Defines the lifecycle scope of a namespace.
/// Deprecated: Use NamespacePolicy instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NamespaceScope {
    /// Data in this namespace is tied to a specific project generation and should be purged when the generation changes.
    GenerationScoped,
    /// Data in this namespace is global to the project and persists across generations.
    Global,
}

pub const NAMESPACE_MEMORY: &str = "memory";
pub const NAMESPACE_HISTORY: &str = "history";
pub const NAMESPACE_ANTIPATTERN: &str = "antipattern";
pub const NAMESPACE_WONTFIX: &str = "wontfix_patterns";
pub const NAMESPACE_MEMORY_BANK: &str = "memory_bank";
pub const NAMESPACE_INSIGHTS: &str = "insights";
pub const NAMESPACE_BUSINESS_LOGIC: &str = "business_logic";

pub const KNOWN_NAMESPACES: &[&str] = &[
    NAMESPACE_MEMORY,
    NAMESPACE_HISTORY,
    NAMESPACE_ANTIPATTERN,
    NAMESPACE_WONTFIX,
    NAMESPACE_MEMORY_BANK,
    NAMESPACE_INSIGHTS,
    NAMESPACE_BUSINESS_LOGIC,
];

/// Returns the policy for a given namespace name.
pub fn get_policy(namespace: &str) -> Result<NamespacePolicy> {
    match namespace {
        NAMESPACE_MEMORY => Ok(NamespacePolicy {
            versioning: NamespaceVersioning::Snapshot,
            retention: NamespaceRetention::KeepLatestOnly,
        }),
        NAMESPACE_HISTORY => Ok(NamespacePolicy {
            // Merged-PR change units carry STABLE doc_ids (`pr:<id>`) and
            // describe immutable history — they must not rot with the
            // generation counter. The old AppendOnly +
            // KeepLastGenerations(10) policy assumed generations bump only
            // on full reindexes; incremental updates bump the counter per
            // run, so any 10 routine updates (observed: 33 in one day)
            // purged the whole corpus. GlobalMutable = indexed at gen 0,
            // pk delete-then-add overwrite, no generation filter at query
            // time, kept forever — same as memory_bank/business_logic.
            versioning: NamespaceVersioning::GlobalMutable,
            retention: NamespaceRetention::KeepForever,
        }),
        // Review-corpus rules (anti-patterns + wontFix suppressions) carry
        // STABLE content-hash doc_ids and describe team review history —
        // the same shape as `history` above, and the same failure mode:
        // AppendOnly pinned them to the ingest-time generation, so routine
        // incremental updates (which bump the counter) orphaned the whole
        // corpus for every generation-scoped reader (live: get_chunk at
        // gen 36 couldn't fetch rules written at gen ~30). GlobalMutable =
        // gen 0, pk upsert, survives wipe_and_reindex.
        NAMESPACE_ANTIPATTERN => Ok(NamespacePolicy {
            versioning: NamespaceVersioning::GlobalMutable,
            retention: NamespaceRetention::KeepForever,
        }),
        NAMESPACE_WONTFIX => Ok(NamespacePolicy {
            versioning: NamespaceVersioning::GlobalMutable,
            retention: NamespaceRetention::KeepForever,
        }),
        NAMESPACE_MEMORY_BANK => Ok(NamespacePolicy {
            versioning: NamespaceVersioning::GlobalMutable,
            retention: NamespaceRetention::KeepForever,
        }),
        NAMESPACE_INSIGHTS => Ok(NamespacePolicy {
            versioning: NamespaceVersioning::GlobalMutable,
            retention: NamespaceRetention::KeepForever,
        }),
        NAMESPACE_BUSINESS_LOGIC => Ok(NamespacePolicy {
            versioning: NamespaceVersioning::GlobalMutable,
            retention: NamespaceRetention::KeepForever,
        }),
        _ => Err(EngramError::Internal(format!(
            "Unknown namespace: {}",
            namespace
        ))),
    }
}

/// Returns the scope for a given namespace name.
/// Deprecated: Use get_policy instead.
pub fn get_namespace_scope(namespace: &str) -> NamespaceScope {
    if let Ok(policy) = get_policy(namespace) {
        match policy.versioning {
            NamespaceVersioning::Snapshot | NamespaceVersioning::AppendOnly => {
                NamespaceScope::GenerationScoped
            }
            NamespaceVersioning::GlobalMutable => NamespaceScope::Global,
        }
    } else {
        NamespaceScope::GenerationScoped // Default to safe-to-purge
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_policies_are_consistent() {
        assert_eq!(
            get_policy(NAMESPACE_MEMORY).unwrap().versioning,
            NamespaceVersioning::Snapshot
        );
        assert_ne!(
            get_policy(NAMESPACE_MEMORY_BANK).unwrap().versioning,
            NamespaceVersioning::Snapshot
        );
        assert!(get_policy("unknown").is_err());
    }

    #[test]
    fn business_logic_namespace_is_global_mutable() {
        let policy = get_policy(NAMESPACE_BUSINESS_LOGIC).unwrap();
        assert_eq!(policy.versioning, NamespaceVersioning::GlobalMutable);
        assert_eq!(policy.retention, NamespaceRetention::KeepForever);
    }
}
