//! State management migration advisor — analyzes state access patterns across a
//! project and produces per-key migration recommendations with code hints.
//!
//! Analyzes `ReadsState`, `WritesState`, `StateAffinity` edges to recommend
//! Session→JWT/Redis, ViewState→component state, Application→DI singleton, etc.

use engram_graph::{Edge, EdgeKind, GraphStore};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// State store type in ASP.NET.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StateStore {
    Session,
    ViewState,
    Application,
    Cache,
    Cookie,
    QueryString,
    HiddenField,
    Other,
}

impl StateStore {
    fn from_target_id(target_id: &str) -> (Self, String) {
        let normalized = target_id
            .strip_prefix("state:")
            .unwrap_or(target_id)
            .to_string();

        let lower = normalized.to_lowercase();
        if lower.starts_with("session:") || lower.starts_with("session[") {
            let key = normalized
                .splitn(2, |c| c == ':' || c == '[')
                .nth(1)
                .unwrap_or("")
                .trim_matches(|c: char| c == ']' || c == '"' || c == '\'')
                .to_string();
            (Self::Session, key)
        } else if lower.starts_with("viewstate:") || lower.starts_with("viewstate[") {
            let key = normalized
                .splitn(2, |c| c == ':' || c == '[')
                .nth(1)
                .unwrap_or("")
                .trim_matches(|c: char| c == ']' || c == '"' || c == '\'')
                .to_string();
            (Self::ViewState, key)
        } else if lower.starts_with("application:") || lower.starts_with("application[") {
            let key = normalized
                .splitn(2, |c| c == ':' || c == '[')
                .nth(1)
                .unwrap_or("")
                .trim_matches(|c: char| c == ']' || c == '"' || c == '\'')
                .to_string();
            (Self::Application, key)
        } else if lower.starts_with("cache:") || lower.starts_with("cache[") {
            let key = normalized
                .splitn(2, |c| c == ':' || c == '[')
                .nth(1)
                .unwrap_or("")
                .trim_matches(|c: char| c == ']' || c == '"' || c == '\'')
                .to_string();
            (Self::Cache, key)
        } else if lower.contains("cookie") {
            let key = normalized
                .splitn(2, |c| c == ':' || c == '[')
                .nth(1)
                .unwrap_or(&normalized)
                .trim_matches(|c: char| c == ']' || c == '"' || c == '\'')
                .to_string();
            (Self::Cookie, key)
        } else if lower.contains("querystring") {
            let key = normalized
                .splitn(2, |c| c == ':' || c == '[')
                .nth(1)
                .unwrap_or(&normalized)
                .trim_matches(|c: char| c == ']' || c == '"' || c == '\'')
                .to_string();
            (Self::QueryString, key)
        } else {
            (Self::Other, normalized)
        }
    }
}

/// Access pattern for a state key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessPattern {
    WriteOnceReadMany,
    ReadWriteBalanced,
    WriteHeavy,
    ReadOnly,
    WriteOnly,
}

/// Per-key state migration recommendation.
#[derive(Debug, Clone, Serialize)]
pub struct StateKeyRecommendation {
    pub state_key: String,
    pub store_type: StateStore,
    pub readers: Vec<String>,
    pub writers: Vec<String>,
    pub access_pattern: AccessPattern,
    pub data_type_inference: String,
    pub affinity_group: Vec<String>,
    pub recommended_target: String,
    pub reasoning: String,
    pub migration_code_hint: String,
}

/// Full state migration report for a project.
#[derive(Debug, Clone, Serialize)]
pub struct StateMigrationReport {
    pub project_id: String,
    pub recommendations: Vec<StateKeyRecommendation>,
    pub viewstate_report: Option<ViewStateReport>,
    pub summary: StateMigrationSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateMigrationSummary {
    pub total_state_keys: usize,
    pub by_store: BTreeMap<String, usize>,
    pub by_target: BTreeMap<String, usize>,
    pub high_risk_keys: Vec<String>,
}

/// ViewState-specific elimination report.
#[derive(Debug, Clone, Serialize)]
pub struct ViewStateReport {
    pub pages: Vec<ViewStatePageReport>,
    pub total_viewstate_keys: usize,
    pub estimated_payload_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ViewStatePageReport {
    pub file_path: String,
    pub keys: Vec<ViewStateKeyReport>,
    pub estimated_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ViewStateKeyReport {
    pub key: String,
    pub readers: Vec<String>,
    pub writers: Vec<String>,
    pub lifecycle: ViewStateLifecycle,
    pub elimination_strategy: String,
    pub is_url_state_crutch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewStateLifecycle {
    /// Used in one event handler only.
    SinglePostback,
    /// Used across multiple postbacks on the same page.
    CrossPostback,
    /// Transferred to another page (Server.Transfer / PostBackUrl).
    CrossPage,
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Analyze all state access in a project and produce per-key migration recommendations.
pub fn analyze_state_migration(
    graph: &Arc<GraphStore>,
    project_id: &str,
) -> anyhow::Result<StateMigrationReport> {
    let reads = graph.list_edges_by_kind(project_id, EdgeKind::ReadsState, 50_000)?;
    let writes = graph.list_edges_by_kind(project_id, EdgeKind::WritesState, 50_000)?;
    let affinity = graph.list_edges_by_kind(project_id, EdgeKind::StateAffinity, 50_000)?;

    // Group by state key
    let mut key_readers: HashMap<(StateStore, String), Vec<String>> = HashMap::new();
    let mut key_writers: HashMap<(StateStore, String), Vec<String>> = HashMap::new();

    for edge in &reads {
        let (store, key) = StateStore::from_target_id(&edge.target_id);
        if !key.is_empty() {
            key_readers
                .entry((store, key))
                .or_default()
                .push(edge.source_id.clone());
        }
    }

    for edge in &writes {
        let (store, key) = StateStore::from_target_id(&edge.target_id);
        if !key.is_empty() {
            key_writers
                .entry((store, key))
                .or_default()
                .push(edge.source_id.clone());
        }
    }

    // Build affinity groups
    let affinity_groups = build_affinity_groups(&affinity);

    // All unique keys
    let mut all_keys: HashSet<(StateStore, String)> = HashSet::new();
    all_keys.extend(key_readers.keys().cloned());
    all_keys.extend(key_writers.keys().cloned());

    let mut recommendations = Vec::new();

    for (store, key) in &all_keys {
        let readers = key_readers
            .get(&(*store, key.clone()))
            .cloned()
            .unwrap_or_default();
        let writers = key_writers
            .get(&(*store, key.clone()))
            .cloned()
            .unwrap_or_default();

        let access_pattern = classify_access_pattern(&readers, &writers);
        let data_type = infer_data_type(key, &readers, &writers);
        let group = find_affinity_group(key, &affinity_groups);
        let (target, reasoning, code_hint) =
            recommend_migration(*store, key, access_pattern, &readers, &writers, &group);

        recommendations.push(StateKeyRecommendation {
            state_key: format!("{:?}:{key}", store),
            store_type: *store,
            readers,
            writers,
            access_pattern,
            data_type_inference: data_type,
            affinity_group: group,
            recommended_target: target,
            reasoning,
            migration_code_hint: code_hint,
        });
    }

    recommendations.sort_by(|a, b| a.state_key.cmp(&b.state_key));

    // ViewState report
    let viewstate_report = generate_viewstate_report(&recommendations);

    // Summary
    let mut by_store: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_target: BTreeMap<String, usize> = BTreeMap::new();
    let mut high_risk = Vec::new();
    for rec in &recommendations {
        *by_store
            .entry(format!("{:?}", rec.store_type))
            .or_default() += 1;
        *by_target
            .entry(rec.recommended_target.clone())
            .or_default() += 1;
        if rec.writers.len() > 3 || rec.readers.len() > 10 {
            high_risk.push(rec.state_key.clone());
        }
    }

    Ok(StateMigrationReport {
        project_id: project_id.to_string(),
        recommendations,
        viewstate_report,
        summary: StateMigrationSummary {
            total_state_keys: all_keys.len(),
            by_store,
            by_target,
            high_risk_keys: high_risk,
        },
    })
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

fn classify_access_pattern(readers: &[String], writers: &[String]) -> AccessPattern {
    let r = readers.len();
    let w = writers.len();
    if w == 0 && r > 0 {
        AccessPattern::ReadOnly
    } else if r == 0 && w > 0 {
        AccessPattern::WriteOnly
    } else if w <= 1 && r >= 3 {
        AccessPattern::WriteOnceReadMany
    } else if w > r * 2 {
        AccessPattern::WriteHeavy
    } else {
        AccessPattern::ReadWriteBalanced
    }
}

fn infer_data_type(key: &str, _readers: &[String], _writers: &[String]) -> String {
    let lower = key.to_lowercase();
    if lower.contains("id") || lower.contains("count") || lower.contains("index") {
        "int (based on key naming)".into()
    } else if lower.contains("name") || lower.contains("email") || lower.contains("user") {
        "string (based on key naming)".into()
    } else if lower.contains("date") || lower.contains("time") {
        "DateTime (based on key naming)".into()
    } else if lower.contains("is") || lower.contains("has") || lower.contains("flag") {
        "bool (based on key naming)".into()
    } else if lower.contains("amount") || lower.contains("total") || lower.contains("price") {
        "decimal (based on key naming)".into()
    } else {
        "object (unable to infer type)".into()
    }
}

fn recommend_migration(
    store: StateStore,
    key: &str,
    access: AccessPattern,
    readers: &[String],
    writers: &[String],
    affinity_group: &[String],
) -> (String, String, String) {
    let lower = key.to_lowercase();

    match store {
        StateStore::Session => {
            // Auth-related keys → JWT
            if lower.contains("userid")
                || lower.contains("user_id")
                || lower.contains("username")
                || lower.contains("role")
                || lower.contains("auth")
            {
                return (
                    "JWT claim".into(),
                    format!(
                        "Auth-related key, written at login ({} writers), read across {} pages",
                        writers.len(),
                        readers.len()
                    ),
                    format!(
                        "services.AddAuthentication().AddJwtBearer(); // {key} as ClaimTypes.NameIdentifier"
                    ),
                );
            }
            // Write-once, read-many with small affinity → JWT or client state
            if matches!(access, AccessPattern::WriteOnceReadMany) && affinity_group.len() <= 3 {
                return (
                    "JWT claim or client-side state".into(),
                    format!(
                        "Write-once ({} writers), read across {} pages, small affinity group",
                        writers.len(),
                        readers.len()
                    ),
                    "// Consider: ClaimTypes custom claim or browser sessionStorage".into(),
                );
            }
            // Heavy read/write → distributed cache
            if readers.len() + writers.len() > 5 {
                return (
                    "IDistributedCache (Redis)".into(),
                    format!(
                        "Shared across {} pages with {} writes — needs distributed backing",
                        readers.len() + writers.len(),
                        writers.len()
                    ),
                    format!("await _cache.SetStringAsync(\"{key}\", value, options);"),
                );
            }
            // Default
            (
                "Component state or IDistributedCache".into(),
                format!(
                    "{} readers, {} writers — evaluate scope",
                    readers.len(),
                    writers.len()
                ),
                "// Evaluate: component @bind for page-scoped, Redis for cross-page".into(),
            )
        }
        StateStore::ViewState => {
            if writers.len() <= 1 && readers.len() <= 1 {
                (
                    "Local variable (eliminate ViewState)".into(),
                    "Single postback usage — ViewState unnecessary".into(),
                    "// Replace ViewState[\"key\"] with a local field in the component".into(),
                )
            } else if is_url_state_candidate(key) {
                (
                    "URL query parameter".into(),
                    format!(
                        "Key '{key}' appears to store filter/sort state — use URL for bookmarkability"
                    ),
                    format!("NavigationManager.NavigateTo($\"?{key}={{value}}\");"),
                )
            } else {
                (
                    "Component state (@bind / useState)".into(),
                    format!(
                        "{} readers, {} writers across postbacks",
                        readers.len(),
                        writers.len()
                    ),
                    "// Blazor: private field + @bind; React: useState hook".into(),
                )
            }
        }
        StateStore::Application => match access {
            AccessPattern::WriteOnly | AccessPattern::ReadOnly => (
                "Static configuration (IOptions<T>)".into(),
                "Write-once or read-only — treat as config".into(),
                format!(
                    "services.Configure<AppSettings>(c => c.{} = value);",
                    to_pascal_case(key)
                ),
            ),
            _ => (
                "Singleton service (DI)".into(),
                format!(
                    "Write frequency: {} — needs thread-safe singleton",
                    writers.len()
                ),
                "services.AddSingleton<ISharedStateService, SharedStateService>();".into(),
            ),
        },
        StateStore::Cache => (
            "IDistributedCache (Redis)".into(),
            format!(
                "Cache key with {} reads, {} writes",
                readers.len(),
                writers.len()
            ),
            format!(
                "await _cache.SetStringAsync(\"{key}\", serialized, new DistributedCacheEntryOptions {{ SlidingExpiration = TimeSpan.FromMinutes(30) }});"
            ),
        ),
        StateStore::Cookie => {
            if lower.contains("auth") || lower.contains("token") || lower.contains("session") {
                (
                    "JWT in HttpOnly cookie".into(),
                    "Auth-related cookie — use secure JWT".into(),
                    "services.AddAuthentication().AddCookie(o => o.Cookie.HttpOnly = true);".into(),
                )
            } else {
                (
                    "localStorage (preferences) or keep cookie".into(),
                    "Non-auth cookie — client-side storage may suffice".into(),
                    "localStorage.setItem('key', value); // or keep as cookie".into(),
                )
            }
        }
        _ => (
            "Evaluate manually".into(),
            format!("State store {:?} with key '{key}'", store),
            "// Manual evaluation required".into(),
        ),
    }
}

fn build_affinity_groups(affinity_edges: &[Edge]) -> HashMap<String, Vec<String>> {
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for edge in affinity_edges {
        let src_key = edge
            .source_id
            .strip_prefix("state:")
            .unwrap_or(&edge.source_id)
            .to_string();
        let tgt_key = edge
            .target_id
            .strip_prefix("state:")
            .unwrap_or(&edge.target_id)
            .to_string();
        groups
            .entry(src_key.clone())
            .or_default()
            .push(tgt_key.clone());
        groups.entry(tgt_key).or_default().push(src_key);
    }
    // Deduplicate
    for v in groups.values_mut() {
        v.sort();
        v.dedup();
    }
    groups
}

fn find_affinity_group(key: &str, groups: &HashMap<String, Vec<String>>) -> Vec<String> {
    groups.get(key).cloned().unwrap_or_default()
}

fn is_url_state_candidate(key: &str) -> bool {
    let lower = key.to_lowercase();
    lower.contains("sort")
        || lower.contains("filter")
        || lower.contains("page")
        || lower.contains("search")
        || lower.contains("order_by")
        || lower.contains("tab")
}

fn generate_viewstate_report(recommendations: &[StateKeyRecommendation]) -> Option<ViewStateReport> {
    let vs_recs: Vec<&StateKeyRecommendation> = recommendations
        .iter()
        .filter(|r| r.store_type == StateStore::ViewState)
        .collect();

    if vs_recs.is_empty() {
        return None;
    }

    // Group by file (from readers/writers)
    let mut pages: HashMap<String, Vec<ViewStateKeyReport>> = HashMap::new();

    for rec in &vs_recs {
        let key = rec
            .state_key
            .strip_prefix("ViewState:")
            .unwrap_or(&rec.state_key);
        let lifecycle = classify_viewstate_lifecycle(&rec.readers, &rec.writers);
        let is_crutch = is_url_state_candidate(key);
        let strategy = match lifecycle {
            ViewStateLifecycle::SinglePostback => {
                "Eliminate: move to local variable or component field".into()
            }
            ViewStateLifecycle::CrossPostback => {
                if is_crutch {
                    "Move to URL query parameter for bookmarkability".to_string()
                } else {
                    "Move to component state (@bind / useState)".to_string()
                }
            }
            ViewStateLifecycle::CrossPage => "Move to server session (Redis) or URL state".into(),
        };

        let file_paths: HashSet<String> = rec
            .readers
            .iter()
            .chain(rec.writers.iter())
            .filter_map(|s| {
                s.split(':')
                    .last()
                    .map(|p| p.split('.')
                        .take(2)
                        .collect::<Vec<_>>()
                        .join("."))
            })
            .collect();

        let report = ViewStateKeyReport {
            key: key.to_string(),
            readers: rec.readers.clone(),
            writers: rec.writers.clone(),
            lifecycle,
            elimination_strategy: strategy,
            is_url_state_crutch: is_crutch,
        };

        for fp in file_paths {
            pages.entry(fp).or_default().push(report.clone());
        }
    }

    let avg_key_size: usize = 50; // Heuristic: average ViewState value ~50 bytes
    let total_keys = vs_recs.len();
    let estimated_bytes = total_keys * avg_key_size;

    let page_reports: Vec<ViewStatePageReport> = pages
        .into_iter()
        .map(|(file_path, keys)| {
            let kb = keys.len() * avg_key_size;
            ViewStatePageReport {
                file_path,
                keys,
                estimated_bytes: kb,
            }
        })
        .collect();

    Some(ViewStateReport {
        pages: page_reports,
        total_viewstate_keys: total_keys,
        estimated_payload_bytes: estimated_bytes,
    })
}

fn classify_viewstate_lifecycle(readers: &[String], writers: &[String]) -> ViewStateLifecycle {
    let all_locations: HashSet<&str> = readers
        .iter()
        .chain(writers.iter())
        .map(|s| s.as_str())
        .collect();

    // If all accesses are from the same function → single postback
    if all_locations.len() <= 1 {
        return ViewStateLifecycle::SinglePostback;
    }

    // Check if all from the same file
    let files: HashSet<String> = all_locations
        .iter()
        .filter_map(|s| s.split(':').last().map(String::from))
        .collect();

    if files.len() <= 1 {
        ViewStateLifecycle::CrossPostback
    } else {
        ViewStateLifecycle::CrossPage
    }
}

fn to_pascal_case(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = true;
    for c in s.chars() {
        if c == '_' || c == '-' || c == ' ' {
            capitalize_next = true;
            continue;
        }
        if capitalize_next {
            result.push(c.to_uppercase().next().unwrap_or(c));
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_store_from_session_target() {
        let (store, key) = StateStore::from_target_id("state:Session:UserId");
        assert_eq!(store, StateStore::Session);
        assert_eq!(key, "UserId");
    }

    #[test]
    fn state_store_from_viewstate_target() {
        let (store, key) = StateStore::from_target_id("state:ViewState:SortOrder");
        assert_eq!(store, StateStore::ViewState);
        assert_eq!(key, "SortOrder");
    }

    #[test]
    fn state_store_from_application_target() {
        let (store, key) = StateStore::from_target_id("state:Application:GlobalCounter");
        assert_eq!(store, StateStore::Application);
        assert_eq!(key, "GlobalCounter");
    }

    #[test]
    fn state_store_from_cache_target() {
        let (store, key) = StateStore::from_target_id("state:Cache:RecentItems");
        assert_eq!(store, StateStore::Cache);
        assert_eq!(key, "RecentItems");
    }

    #[test]
    fn state_store_from_cookie_target() {
        let (store, key) = StateStore::from_target_id("state:Cookie:AuthToken");
        assert_eq!(store, StateStore::Cookie);
        assert_eq!(key, "AuthToken");
    }

    #[test]
    fn classify_access_read_only() {
        let readers = vec!["a".into(), "b".into(), "c".into()];
        let writers: Vec<String> = vec![];
        assert_eq!(
            classify_access_pattern(&readers, &writers),
            AccessPattern::ReadOnly
        );
    }

    #[test]
    fn classify_access_write_only() {
        let readers: Vec<String> = vec![];
        let writers = vec!["a".into()];
        assert_eq!(
            classify_access_pattern(&readers, &writers),
            AccessPattern::WriteOnly
        );
    }

    #[test]
    fn classify_access_write_once_read_many() {
        let readers = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let writers = vec!["x".into()];
        assert_eq!(
            classify_access_pattern(&readers, &writers),
            AccessPattern::WriteOnceReadMany
        );
    }

    #[test]
    fn recommend_session_user_id() {
        let (target, _, _) = recommend_migration(
            StateStore::Session,
            "UserId",
            AccessPattern::WriteOnceReadMany,
            &["page1".into(), "page2".into(), "page3".into()],
            &["login".into()],
            &["UserName".into(), "UserRole".into()],
        );
        assert_eq!(target, "JWT claim");
    }

    #[test]
    fn recommend_viewstate_single_usage() {
        let (target, _, _) = recommend_migration(
            StateStore::ViewState,
            "TempFlag",
            AccessPattern::ReadWriteBalanced,
            &["handler".into()],
            &["handler".into()],
            &[],
        );
        assert!(target.contains("Local variable"));
    }

    #[test]
    fn recommend_viewstate_sort_as_url() {
        let (target, _, _) = recommend_migration(
            StateStore::ViewState,
            "SortColumn",
            AccessPattern::ReadWriteBalanced,
            &["a".into(), "b".into()],
            &["c".into(), "d".into()],
            &[],
        );
        assert!(target.contains("URL"));
    }

    #[test]
    fn recommend_application_read_only() {
        let (target, _, _) = recommend_migration(
            StateStore::Application,
            "AppName",
            AccessPattern::ReadOnly,
            &["a".into()],
            &[],
            &[],
        );
        assert!(target.contains("IOptions") || target.contains("Static"));
    }

    #[test]
    fn recommend_cache_key() {
        let (target, _, _) = recommend_migration(
            StateStore::Cache,
            "RecentOrders",
            AccessPattern::ReadWriteBalanced,
            &["a".into()],
            &["b".into()],
            &[],
        );
        assert!(target.contains("Redis") || target.contains("IDistributedCache"));
    }

    #[test]
    fn recommend_auth_cookie() {
        let (target, _, _) = recommend_migration(
            StateStore::Cookie,
            "AuthToken",
            AccessPattern::WriteOnceReadMany,
            &["a".into()],
            &["b".into()],
            &[],
        );
        assert!(target.contains("JWT"));
    }

    #[test]
    fn is_url_state_candidates() {
        assert!(is_url_state_candidate("SortColumn"));
        assert!(is_url_state_candidate("FilterBy"));
        assert!(is_url_state_candidate("PageIndex"));
        assert!(is_url_state_candidate("SearchTerm"));
        assert!(!is_url_state_candidate("UserId"));
    }

    #[test]
    fn viewstate_lifecycle_single() {
        let readers = vec!["fn:handler".into()];
        let writers = vec!["fn:handler".into()];
        assert_eq!(
            classify_viewstate_lifecycle(&readers, &writers),
            ViewStateLifecycle::SinglePostback
        );
    }

    #[test]
    fn viewstate_lifecycle_cross_postback() {
        let readers = vec!["fn:Page_Load:Page1.aspx".into()];
        let writers = vec!["fn:Button_Click:Page1.aspx".into()];
        assert_eq!(
            classify_viewstate_lifecycle(&readers, &writers),
            ViewStateLifecycle::CrossPostback
        );
    }

    #[test]
    fn affinity_group_building() {
        let edges = vec![
            Edge {
                source_id: "state:UserId".into(),
                target_id: "state:UserName".into(),
                namespace: "test".into(),
                language: "csharp".into(),
                edge_kind: EdgeKind::StateAffinity,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            },
        ];
        let groups = build_affinity_groups(&edges);
        assert!(groups.get("UserId").is_some_and(|v| v.contains(&"UserName".to_string())));
        assert!(groups.get("UserName").is_some_and(|v| v.contains(&"UserId".to_string())));
    }

    #[test]
    fn infer_data_types() {
        assert!(infer_data_type("UserId", &[], &[]).contains("int"));
        assert!(infer_data_type("UserName", &[], &[]).contains("string"));
        assert!(infer_data_type("CreatedDate", &[], &[]).contains("DateTime"));
        assert!(infer_data_type("IsActive", &[], &[]).contains("bool"));
        assert!(infer_data_type("TotalAmount", &[], &[]).contains("decimal"));
        assert!(infer_data_type("RandomKey", &[], &[]).contains("object"));
    }
}
