use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;
mod transport;
mod inventory;
mod compilation;
pub use compilation::CompilationPolicy;

// SQL detection for the fallback extractor. The Roslyn sidecar emits richer
// SQL edges, but when it is unavailable the fallback must still keep the
// DB half of the graph wired — losing sql_calls edges silently disconnects
// stored procedures from their VB callers.
static RE_VB_SQL_CMD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)New\s+SqlCommand\s*\(\s*"([^"]*)""#).expect("valid VB SqlCommand regex")
});
static RE_VB_COMMAND_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)CommandText\s*=\s*"([^"]*)""#).expect("valid VB CommandText regex")
});
// TODO-17: dynamic SQL — the command text is a VARIABLE, not a literal.
// These were silently dropped; now they emit a marked sql_calls edge so
// reports can say "plus N dynamic SQL commands the graph can't parse".
static RE_VB_SQL_CMD_DYN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)New\s+SqlCommand\s*\(\s*([A-Za-z_]\w*)\s*[,)]"#)
        .expect("valid VB dynamic SqlCommand regex")
});
static RE_VB_COMMAND_TEXT_DYN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)CommandText\s*=\s*([A-Za-z_]\w*)\s*$"#)
        .expect("valid VB dynamic CommandText regex")
});
// Qualified call sites (Foo.Bar(...)). The optional "New" capture lets us
// skip constructor invocations without lookbehind support.
static RE_VB_QUALIFIED_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(New\s+)?([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)+)\s*\(")
        .expect("valid VB qualified call regex")
});
/// `Dim x As [New] Some.Type` / `Using x As ...` inside a method body.
static RE_VB_LOCAL_DECL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:Dim|Using)\s+([A-Za-z_]\w*)\s+As\s+(?:New\s+)?([A-Za-z_][\w.]*)")
        .expect("valid VB local declaration regex")
});
/// External audit round 2, item 8: a broker's `Select Case` arm names the API
/// function it serves — `Case "athDeleteByID"` — and the arm's call is the
/// route to the implementation.
static RE_VB_CASE_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)^Case\s+"([A-Za-z_]\w*)"\s*$"#).expect("valid VB Case literal regex")
});
/// The arm's call: `s = Impl(qry)`, `Call Impl(qry)`, `Return Impl(qry)`, `Impl(qry)`.
static RE_VB_ARM_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:[A-Za-z_][\w.()]*\s*=\s*|Call\s+|Return\s+)?([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)\s*\(")
        .expect("valid VB arm call regex")
});
/// Keywords and conversion functions that look like calls inside an arm.
const VB_ARM_CALL_STOPWORDS: [&str; 24] = [
    "if",
    "iif",
    "new",
    "cint",
    "cstr",
    "cbool",
    "cdbl",
    "clng",
    "cdate",
    "cdec",
    "ctype",
    "directcast",
    "trycast",
    "isnothing",
    "len",
    "trim",
    "ucase",
    "lcase",
    "mid",
    "left",
    "right",
    "instr",
    "format",
    "gettype",
];
/// Heads of qualified names that are framework/state noise, not project calls.
const VB_CALL_HEAD_STOPWORDS: [&str; 10] = [
    "me", "my", "mybase", "string", "convert", "integer", "double", "math", "response", "request",
];
// Designer-style control fields: `Protected WithEvents btnSave As ...Button`.
// Parity with the C# tree-sitter pass, which maps designer fields to
// control_ref symbols so they merge with the page's control nodes.
static RE_VB_WITHEVENTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bWithEvents\s+([A-Za-z_]\w*)\s+As\b").expect("valid VB WithEvents regex")
});
// Only declaration prefixes may precede Sub/Function. Searching for these
// keywords anywhere in a line mistakes documentation, strings and trailing
// comments for methods, and gives those phantom methods real body ranges.
static RE_VB_METHOD_DECL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)^(?:<(?:[^>"]|"(?:[^"]|"")*")*>\s*|(?:Public|Protected|Friend|Private|Shared|Shadows|Overloads|Overrides|Overridable|NotOverridable|MustOverride|Partial|Async|Iterator|Declare|Ansi|Unicode|Auto|Delegate)\s+)*(?:Sub|Function)\s+"#,
    )
    .expect("valid VB method declaration regex")
});
// Settings reads: ConfigurationManager.AppSettings("Key") (VB call syntax)
// and My.Settings.Key. Generic name shapes only.
static RE_VB_APPSETTINGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)AppSettings\s*\(\s*"([^"]+)"\s*\)"#).expect("valid VB appsettings regex")
});
static RE_VB_MY_SETTINGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bMy\.Settings\.([A-Za-z_]\w*)").expect("valid My.Settings regex")
});

/// Keep byte offsets stable while excluding VB string contents and comments
/// from name-shape detection. Literal setting keys are still read from the
/// original line, but only when their AppSettings call starts in code.
fn vb_settings_code_mask(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut masked = bytes.to_vec();
    let mut i = 0;
    let mut statement_start = true;
    while i < bytes.len() {
        if bytes[i] == b'\''
            || (statement_start
                && bytes.get(i..i + 3).is_some_and(|s| s.eq_ignore_ascii_case(b"rem"))
                && bytes.get(i + 3).is_none_or(u8::is_ascii_whitespace))
        {
            masked[i..].fill(b' ');
            break;
        }
        if bytes[i] == b'"' {
            masked[i] = b' ';
            i += 1;
            while i < bytes.len() {
                masked[i] = b' ';
                if bytes[i] == b'"' {
                    i += 1;
                    if bytes.get(i) == Some(&b'"') {
                        masked[i] = b' ';
                    } else {
                        break;
                    }
                }
                i += 1;
            }
            statement_start = false;
            continue;
        }
        if bytes[i] == b':' {
            statement_start = true;
        } else if !bytes[i].is_ascii_whitespace() {
            statement_start = false;
        }
        i += 1;
    }
    // Replacing complete string/comment byte spans with ASCII preserves UTF-8.
    String::from_utf8(masked).expect("VB code mask preserves UTF-8")
}
// Settings-STORE property reads: the dominant house pattern in mature apps
// is a static store class (ConfigSettings.Multitenant.IsMaster,
// SystemSettingStore.General.RoqEnableListTypeDimension) — NOT raw
// AppSettings("...") calls. Without this shape, settings intelligence
// (list_settings / derive_test_matrix) sees zero settings on exactly the
// codebases that need it most. Generic: root identifier must carry a
// settings/config token; 1-3 dotted property segments; excludes the
// ConfigurationManager/WebConfigurationManager framework roots (already
// covered by the AppSettings shape above).
static RE_VB_SETTINGS_STORE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b((?:[A-Za-z_]\w*)?(?:Setting|Config|UserAccess|Permission)\w*)\.((?:[A-Za-z_]\w*\.){0,2}[A-Za-z_]\w*)\b",
    )
    .expect("valid VB settings-store regex")
});
// Permission checks by call-name shape (IsInRole, IsUserInRole, IsXxxAdmin,
// CheckAccessLevel, HasPermission, RequireRole, DemandAdmin, Authorize...).
static RE_VB_GUARD_CALL: LazyLock<Regex> = LazyLock::new(|| {
    // `check(read|write)` = the CheckRead/CheckWrite(permissionObject)
    // idiom, and `check_<entity>id` = the Check_pr_id/Check_rv_id
    // project-access-scoping family — both are the DOMINANT guard style
    // in DAO-delegated codebases, and both were invisible to the old
    // pattern (it demanded a literal access/permission/role substring,
    // so guard-map tools reported provably-guarded methods as UNGUARDED
    // — knowledge-pack harvest 2026-07-06, corroborated across domains).
    Regex::new(
        r"(?i)\b(is[a-z0-9_]*admin[a-z0-9_]*|isinrole|isuserinrole|is[a-z0-9_]*role|check[a-z0-9_]*(access|permission|role)[a-z0-9_]*|check(?:read|write)[a-z0-9_]*|check_[a-z0-9_]*id|has[a-z0-9_]*(permission|access|role)[a-z0-9_]*|require[a-z0-9_]*(role|permission|admin)[a-z0-9_]*|demand[a-z0-9_]*|authorize[a-z0-9_]*)\s*\(",
    )
    .expect("valid VB guard regex")
});
static RE_VB_GUARD_ROLE_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:isinrole|isuserinrole)\s*\(\s*"([^"]+)""#)
        .expect("valid VB role literal regex")
});
// LINQ-to-SQL / EF context variables: `Dim db As New iCoreDataContext` /
// `Using db As New FooDbContext` / `db = New BarDataContext`. The ORM DAL
// idiom is otherwise completely invisible to SQL-literal extraction.
static RE_VB_CTX_DECL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:Dim|Using)\s+(\w+)\s+As\s+New\s+\w*D(?:ata|b)Context\b")
        .expect("valid VB ctx decl regex")
});
static RE_VB_CTX_ASSIGN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(\w+)\s*=\s*New\s+\w*D(?:ata|b)Context\b")
        .expect("valid VB ctx assign regex")
});
/// `ctx.TableProp` member access. Method calls (followed by `(`) are
/// filtered by the caller — the regex crate has no lookahead.
static RE_VB_CTX_MEMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\w+)\.([A-Za-z_]\w*)").expect("valid ctx member regex"));
/// Write calls: `ctx.Table.InsertOnSubmit(x)` etc.
static RE_VB_CTX_WRITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(\w+)\.(\w+)\.(?:InsertOnSubmit|InsertAllOnSubmit|DeleteOnSubmit|DeleteAllOnSubmit|Add|AddRange|Remove|RemoveRange)\s*\(")
        .expect("valid ctx write regex")
});
/// LINQ navigation chain `rangeVar.NavProp.Column` — the middle member is
/// a related TABLE reached through a foreign key (LINQ-to-SQL / EF
/// association property), read by the query even though no context
/// variable names it. Only considered on query-clause lines (see
/// `is_linq_query_clause`) and only for table-shaped members.
static RE_VB_NAV_CHAIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b([A-Za-z_]\w*)\.([A-Za-z_]\w*)\.([A-Za-z_]\w*)\b")
        .expect("valid VB nav chain regex")
});

/// A LINQ query-expression clause line: `From x In …`, `Where …`,
/// `Order By …`, `Select …`, `Group By …`, `Join … On …`.
fn is_linq_query_clause(lower: &str) -> bool {
    lower.starts_with("where ")
        || lower.starts_with("order by ")
        || lower.starts_with("select ")
        || lower.starts_with("group by ")
        || lower.starts_with("join ")
        || (lower.contains(" from ") && lower.contains(" in "))
        || lower.starts_with("from ")
}

/// Table-shaped identifier for an ORM association: a schema-style
/// `prefix_name` (underscore, no uppercase) — the naming LINQ-to-SQL keeps
/// from the DDL. PascalCase EF navigation properties are not matched (they
/// need the DDL table set to disambiguate from ordinary members).
fn looks_like_table_member(member: &str) -> bool {
    member.contains('_')
        && member.len() >= 4
        && !member.chars().any(|c| c.is_ascii_uppercase())
        && !member.starts_with('_')
        && !member.ends_with('_')
}

/// Context members that are ORM machinery, not tables.
const CTX_NON_TABLE_MEMBERS: [&str; 12] = [
    "connection",
    "transaction",
    "log",
    "commandtimeout",
    "deferredloadingenabled",
    "loadoptions",
    "objecttrackingenabled",
    "mapping",
    "database",
    "changetracker",
    "configuration",
    "entry",
];

/// Identity of the actually loaded ingestion parser modules. This does not
/// attest project references, compilation options, or every external input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VbLoadedParserIdentity {
    pub contract: String,
    pub entry_mvid: String,
    pub roslyn_core_mvid: String,
    pub roslyn_vb_mvid: String,
}

impl VbLoadedParserIdentity {
    pub fn is_supported(&self) -> bool {
        let mvid = |value: &str| value.len() == 36
            && value.bytes().enumerate().all(|(n, byte)| if [8, 13, 18, 23].contains(&n) { byte == b'-' } else { byte.is_ascii_hexdigit() })
            && value.bytes().any(|byte| byte != b'0' && byte != b'-');
        self.contract == "vb-ingestion-parser-v4" && mvid(&self.entry_mvid)
            && mvid(&self.roslyn_core_mvid) && mvid(&self.roslyn_vb_mvid)
    }
}

/// Available parser code and effective policy, distinct from the backend a
/// particular file actually used. No PID/nonce or project-reference claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VbParserContext {
    pub fallback_contract: u64,
    pub roslyn: Option<VbLoadedParserIdentity>,
    pub max_source_bytes: usize,
    pub request_timeout_secs: u64,
    pub compilation: CompilationPolicy,
}

impl VbParserContext {
    fn from_authority(roslyn: Option<VbLoadedParserIdentity>) -> Self {
        Self {
            fallback_contract: crate::SOURCE_INDEX_VERSION, roslyn,
            max_source_bytes: std::env::var("ENGRAM_VB_SIDECAR_MAX_SOURCE_BYTES").ok()
                .and_then(|value| value.parse::<usize>().ok()).filter(|value| *value > 0).unwrap_or(2_000_000),
            request_timeout_secs: std::env::var("ENGRAM_VB_SIDECAR_TIMEOUT_SECS").ok()
                .and_then(|value| value.parse::<u64>().ok()).filter(|value| *value > 0).unwrap_or(60).min(300),
            compilation: CompilationPolicy::default(),
        }
    }
    fn is_supported(&self) -> bool {
        self.fallback_contract == crate::SOURCE_INDEX_VERSION && self.max_source_bytes > 0
            && (1..=300).contains(&self.request_timeout_secs)
            && self.compilation.supported()
            && self.roslyn.as_ref().is_none_or(VbLoadedParserIdentity::is_supported)
    }
    /// Borrowed metadata/GUID comparisons avoid deserialization allocations.
    /// Existing path classification is a separate operation. Reusable
    /// degraded output remains labelled fallback, never promoted to Roslyn.
    pub(crate) fn matches_metadata(&self, actual: &serde_json::Value) -> bool {
        let Some(object) = actual.as_object() else { return false; };
        let Some(context) = object.get("context").and_then(serde_json::Value::as_object) else { return false; };
        if !self.is_supported() || context.len() != 5
            || context.get("fallback_contract").and_then(|v| v.as_u64()) != Some(self.fallback_contract)
            || context.get("max_source_bytes").and_then(|v| v.as_u64()).and_then(|v| usize::try_from(v).ok()) != Some(self.max_source_bytes)
            || context.get("request_timeout_secs").and_then(|v| v.as_u64()) != Some(self.request_timeout_secs)
            || !context.get("compilation").is_some_and(|value| self.compilation.matches_metadata(value)) { return false; }
        let authority_matches = match &self.roslyn {
            None => context.get("roslyn").is_some_and(serde_json::Value::is_null),
            Some(expected) => context.get("roslyn").and_then(serde_json::Value::as_object).is_some_and(|actual| {
                actual.len() == 4
                    && actual.get("contract").and_then(|v| v.as_str()) == Some(expected.contract.as_str())
                    && actual.get("entry_mvid").and_then(|v| v.as_str()) == Some(expected.entry_mvid.as_str())
                    && actual.get("roslyn_core_mvid").and_then(|v| v.as_str()) == Some(expected.roslyn_core_mvid.as_str())
                    && actual.get("roslyn_vb_mvid").and_then(|v| v.as_str()) == Some(expected.roslyn_vb_mvid.as_str())
            }),
        };
        authority_matches && match object.get("backend").and_then(|v| v.as_str()) {
            Some("roslyn") => object.len() == 2 && self.roslyn.is_some(),
            Some("rust_fallback") => object.len() == 3 && match object.get("reason").and_then(|v| v.as_str()) {
                Some("unavailable") => self.roslyn.is_none(),
                Some("source_limit" | "parse_error" | "transport_failure") => self.roslyn.is_some(),
                _ => false,
            },
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VbFallbackReason { Unavailable, SourceLimit, ParseError, TransportFailure }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum VbParserProvenance {
    Roslyn { context: VbParserContext },
    RustFallback { context: VbParserContext, reason: VbFallbackReason },
    UnverifiedRoslyn,
}

impl VbParserProvenance {
    fn fallback() -> Self { Self::RustFallback { context: VbParserContext::from_authority(None), reason: VbFallbackReason::Unavailable } }
    pub fn context(&self) -> Option<&VbParserContext> {
        match self { Self::Roslyn { context } | Self::RustFallback { context, .. } => Some(context), Self::UnverifiedRoslyn => None }
    }
    pub fn is_verified(&self) -> bool {
        match self {
            Self::Roslyn { context } => context.is_supported() && context.roslyn.is_some(),
            Self::RustFallback { context, reason } => context.is_supported()
                && (context.roslyn.is_none() == matches!(reason, VbFallbackReason::Unavailable)),
            Self::UnverifiedRoslyn => false,
        }
    }
}

pub fn requires_parser_provenance(path: &str) -> bool {
    Path::new(path).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("vb"))
        && !engram_core::is_vendor_path(path)
}

struct Sidecar {
    incarnation: Arc<()>,
    binary: PathBuf,
    transport: transport::Client,
    /// Root the child's shared compilation was built from, if any. Lets an
    /// incremental update skip the O(project) `begin_project` rebuild.
    /// Reset on every respawn, because a fresh child has no compilation.
    project_root: Option<PathBuf>,
    inventory_signature: Option<blake3::Hash>,
    cached_policy: Option<CompilationPolicy>,
    cached_charge: Option<Arc<compilation::CachedCharge>>,
    /// False once a deployed sidecar has told us it does not know the
    /// `invalidate` command, so we stop asking it every update.
    supports_invalidate: bool,
    identity: Option<VbLoadedParserIdentity>,
}

impl Sidecar {
    fn stop(&mut self) -> std::io::Result<()> {
        self.transport.stop()
    }
}

static SIDECAR: OnceLock<Mutex<Option<Sidecar>>> = OnceLock::new();
static COMPILATION: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

/// One warmed compilation belongs to one entire ingestion operation. Workers
/// retain this owner even when their async waiter is cancelled. This fences
/// project-root/cache ownership, not external references or project options.
pub(crate) struct VbCompilationLease {
    _permit: tokio::sync::OwnedSemaphorePermit,
    _parse_admission: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    root: PathBuf,
    incarnation: Option<Arc<()>>,
    context: VbParserContext,
    _inventory_memory: inventory::PreparationMemory,
}

fn is_vb_path(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("vb"))
}

pub(crate) async fn prepare_ingestion_batch(root: &Path, files: &[PathBuf],
    inventory: Option<Arc<Vec<PathBuf>>>,
    memory_budget: Option<Arc<engram_core::memory::MemoryBudget>>,
    parse_admission: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    #[cfg(test)] before_prepare: Option<Box<dyn FnOnce() + Send>>)
    -> anyhow::Result<Option<Arc<VbCompilationLease>>> {
    if !files.iter().any(|path| is_vb_path(path)) { return Ok(None); }
    let inventory = inventory.ok_or_else(|| anyhow::anyhow!(
        "Trusted VB ingestion requires the complete admitted source inventory; use standalone extraction for analysis"))?;
    let mut inventory_memory = inventory::PreparationMemory::admit(root, files, &inventory,
        memory_budget.as_deref())?;
    let vb = files.iter().filter(|path| engram_core::RelPath::from_relative(root, path)
        .is_some_and(|rel| requires_parser_provenance(rel.as_str())))
        .map(|path| path.to_string_lossy().into_owned()).collect::<Vec<_>>();
    if vb.is_empty() { return Ok(None); }
    let permit = COMPILATION.clone().acquire_owned().await?;
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        // The permit moves into actual work before any blocking preparation.
        // Parse capacity follows the same actual owner, not the async waiter.
        let parse_admission = parse_admission;
        #[cfg(test)]
        if let Some(pause) = before_prepare { pause(); }
        let input_root = root;
        let root = std::fs::canonicalize(&input_root)?;
        inventory_memory.reserve_prepared(&input_root, &root, &inventory, &vb, memory_budget.as_deref())?;
        let vb = vb.iter().map(|path| -> anyhow::Result<String> {
            let source = Path::new(path);
            let relative = source.strip_prefix(&input_root)?;
            let path = std::fs::canonicalize(source)?;
            anyhow::ensure!(path == root.join(relative),
                "VB warmup source changed its admitted path identity: {}", path.display());
            Ok(path.to_str().ok_or_else(|| anyhow::anyhow!("VB preparation requires UTF-8 source paths"))?.to_owned())
        }).collect::<anyhow::Result<Vec<_>>>()?;
        // This is the already-admitted scan inventory. Normalize its lexical
        // relative keys without restatting every unchanged peer on each edit.
        // Actual changed sources above still receive canonical containment.
        let inventory = inventory.iter().map(|path| {
            let relative = engram_core::RelPath::from_relative(&input_root, path)
                .ok_or_else(|| anyhow::anyhow!("VB inventory source outside admitted root: {}", path.display()))?;
            anyhow::ensure!(requires_parser_provenance(relative.as_str()),
                "VB compilation inventory contains a non-VB source");
            Ok(relative)
        }).collect::<anyhow::Result<Vec<_>>>()?;
        let admitted = inventory.iter().collect::<std::collections::HashSet<_>>();
        anyhow::ensure!(vb.iter().all(|path| engram_core::RelPath::from_relative(&root, Path::new(path))
            .is_some_and(|relative| admitted.contains(&relative))),
            "Changed VB source is absent from its admitted compilation inventory");
        let mut guard = get_or_spawn_sidecar().lock()
            .map_err(|error| anyhow::anyhow!("VB compilation coordinator unavailable: {error}"))?;
        let context = parser_context_locked(&mut guard)?;
        if context.roslyn.is_some() {
            refresh_project_admitted(&mut guard, &root, &inventory, &vb, &context,
                memory_budget.clone().unwrap_or_else(|| compilation::DEFAULT_BUDGET.clone()))?;
            anyhow::ensure!(guard.as_ref().is_some_and(|s| s.project_root.as_deref() == Some(root.as_path())
                && s.identity == context.roslyn && s.transport.is_running()),
                "VB compilation could not be prepared for {}; retry with the matching companion", root.display());
        }
        let incarnation = guard.as_ref().map(|s| s.incarnation.clone());
        Ok(Some(Arc::new(VbCompilationLease { _permit: permit, _parse_admission: parse_admission,
            root, incarnation, context, _inventory_memory: inventory_memory })))
    }).await?
}

/// Observe the cached live IO owner's PID without spawning/restarting a child.
/// This is operational state only; PIDs never participate in source provenance.
pub fn cached_parser_process_id() -> anyhow::Result<Option<u32>> {
    let Some(coordinator) = SIDECAR.get() else { return Ok(None); };
    let guard = coordinator.try_lock().map_err(|error| anyhow::anyhow!("VB parser observation is busy/unavailable: {error}"))?;
    Ok(guard.as_ref().filter(|sidecar| sidecar.transport.is_running()).map(|sidecar| sidecar.transport.pid()))
}

/// Close and join the idle process-global parser. Callers must first quiesce
/// their source requests; active extraction refuses shutdown rather than being
/// interrupted. The slot remains locked until exit, and future work may spawn
/// a new child. Serialized fixtures must not close another client's parser.
pub fn shutdown_idle_parser() -> anyhow::Result<()> {
    let _idle = COMPILATION.clone().try_acquire_owned()
        .map_err(|_| anyhow::anyhow!("Cannot shut down a parser owned by an active ingestion batch"))?;
    let Some(coordinator) = SIDECAR.get() else {
        anyhow::ensure!(transport::lifecycle_ready(), "VB process teardown remains unresolved");
        return Ok(());
    };
    let mut guard = coordinator.try_lock().map_err(|error| anyhow::anyhow!("Cannot shut down active/unavailable VB parser: {error}"))?;
    if let Some(sidecar) = guard.as_mut() { sidecar.stop()?; }
    *guard = None;
    anyhow::ensure!(transport::lifecycle_ready(), "VB process teardown remains unresolved");
    Ok(())
}

fn sidecar_binary_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ENGRAM_VB_SIDECAR_PATH").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("engram_server"));
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let name = match std::env::consts::OS {
        "windows" => "vb_roslyn_sidecar-win-x64.exe",
        "macos" => "vb_roslyn_sidecar-osx-x64",
        _ => "vb_roslyn_sidecar-linux-x64",
    };
    let packaged = dir.join("sidecar").join(name);
    if packaged.is_file() {
        packaged
    } else {
        dir.join(name)
    }
}

fn get_or_spawn_sidecar() -> &'static Mutex<Option<Sidecar>> {
    SIDECAR.get_or_init(|| Mutex::new(None))
}

fn ensure_sidecar(guard: &mut Option<Sidecar>) -> std::io::Result<&mut Sidecar> {
    ensure_sidecar_at(guard, &sidecar_binary_path())
}

fn ensure_sidecar_at<'a>(guard: &'a mut Option<Sidecar>, sidecar_bin: &Path) -> std::io::Result<&'a mut Sidecar> {
    if let Some(sidecar) = guard.as_mut() {
        if !sidecar.transport.is_running() { sidecar.stop()?; *guard = None; }
    }
    if guard.is_none() {
        let mut sidecar = Sidecar {
            incarnation: Arc::new(()),
            binary: sidecar_bin.to_owned(),
            transport: transport::Client::spawn(sidecar_bin)?,
            project_root: None,
            inventory_signature: None,
            cached_policy: None,
            cached_charge: None,
            supports_invalidate: true,
            identity: None,
        };
        // The bounded receipt is cached with this child, never inferred from
        // an executable path, release manifest or report-schema version.
        let line = source_request_with_limits(&mut sidecar, Path::new("<parser-identity>"), "",
            "{\"cmd\":\"identity\"}".into(), 4096, Duration::from_secs(5))
            .map_err(|error| std::io::Error::other(format!("VB identity transport failed: {error:?}")))?;
        if let Ok(response) = serde_json::from_str::<SidecarResponse>(&line) {
            if response.error.is_none() {
                if let Some(identity) = response.parser_identity.filter(VbLoadedParserIdentity::is_supported) {
                    sidecar.identity = Some(identity);
                }
            }
        }
        *guard = Some(sidecar);
    }
    Ok(guard.as_mut().expect("sidecar initialized"))
}

/// Probe once per admission/coverage scan, off the async executor. An absent
/// child is retried so a newly available parser cannot inherit fallback output.
pub fn current_parser_provenance() -> anyhow::Result<VbParserProvenance> {
    let context = current_parser_context()?;
    Ok(if context.roslyn.is_some() { VbParserProvenance::Roslyn { context } }
        else { VbParserProvenance::RustFallback { context, reason: VbFallbackReason::Unavailable } })
}

pub fn current_parser_context() -> anyhow::Result<VbParserContext> {
    let mut guard = get_or_spawn_sidecar().try_lock().map_err(|error| anyhow::anyhow!(
        "VB parser authority is busy or unavailable; retry after current extraction finishes: {error}"))?;
    parser_context_locked(&mut guard)
}

fn parser_context_locked(guard: &mut Option<Sidecar>) -> anyhow::Result<VbParserContext> {
    match ensure_sidecar(guard) {
        Ok(sidecar) => {
            anyhow::ensure!(sidecar.identity.as_ref().is_some_and(VbLoadedParserIdentity::is_supported),
                "VB parser identity is unavailable/unsupported. Deploy the matching identity-capable companion before trusted ingestion; running without a companion uses explicitly recorded Rust fallback. Inherited VB extraction cannot be certified against this child");
            Ok(VbParserContext::from_authority(sidecar.identity.clone()))
        }
        Err(error) => {
            anyhow::ensure!(guard.is_none(), "VB parser startup/teardown is unresolved; fallback cannot be certified");
            anyhow::ensure!(transport::lifecycle_ready(), "VB previous process/IO teardown is unresolved; parser reuse and replacement are refused");
            tracing::debug!(error = %error, "VB parser unavailable; current extraction backend is Rust fallback");
            Ok(VbParserContext::from_authority(None))
        }
    }
}

pub(crate) fn with_parser_publication_fence<T>(expected: &VbParserContext,
    publish: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    let mut coordinator = get_or_spawn_sidecar().try_lock().map_err(|error| anyhow::anyhow!(
        "VB parser is busy or unavailable before source publication; retry: {error}"))?;
    let current = parser_context_locked(&mut coordinator)?;
    anyhow::ensure!(current == *expected,
        "VB parser code or extraction policy changed after source scan; source publication refused, rescan and retry");
    // Deliberately keep the same coordinator held through the synchronous CAS.
    // Never call async work or reenter VB extraction from this closure.
    publish()
}

#[derive(Debug)]
enum SidecarParseError {
    Protocol(anyhow::Error),
    LocalEncoding(anyhow::Error),
    Sidecar(String),
    Identity,
    SourceLimit,
}

#[derive(Serialize)]
struct SidecarRequest<'a> {
    cmd: &'a str,
    path: String,
    source: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_root: Option<&'a str>,
    standalone: bool,
}

#[derive(Debug, Deserialize)]
struct SidecarResponse {
    #[serde(default)]
    compilation_plan: Option<compilation::Plan>,
    #[serde(default)]
    compilation_error: bool,
    #[serde(default)]
    project_root: Option<String>,
    #[serde(default)]
    standalone: bool,
    #[serde(default)]
    parser_identity: Option<VbLoadedParserIdentity>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    symbols: Vec<SidecarSymbol>,
    #[serde(default)]
    edges: Vec<SidecarEdge>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    invocation_report: Option<VbInvocationReport>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VbInvocationReport {
    pub version: String,
    pub request_id: Option<String>,
    pub source_sha256: String,
    pub source_count: u32,
    pub scope: String,
    pub parse_status: String,
    pub parse_error_count: u32,
    #[serde(default)]
    pub invocations: Vec<VbInvocation>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub omitted_invocation_count: u32,
    #[serde(default)]
    pub omitted_argument_count: u32,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VbInvocation {
    pub source_index: u32,
    pub span_start: u32,
    pub span_length: u32,
    pub callee_span_start: u32,
    pub callee_span_length: u32,
    pub start_line: u32,
    pub end_line: u32,
    pub target_method_id: Option<String>,
    pub resolution: String,
    #[serde(default)]
    pub arguments: Vec<VbInvocationArgument>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VbInvocationArgument {
    pub source_index: u32,
    pub span_start: u32,
    pub span_length: u32,
    pub start_line: u32,
    pub end_line: u32,
    pub name: Option<String>,
    pub classification: String,
    pub syntax_ordinal: u32,
    pub parameter_ordinal: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct SidecarSymbol {
    name: String,
    kind: String,
    start_line: u32,
    end_line: u32,
    #[serde(default)]
    metadata: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct SidecarEdge {
    source_name: String,
    source_kind: String,
    source_start_line: u32,
    source_language: String,
    target_name: String,
    target_kind: Option<String>,
    target_start_line: Option<u32>,
    kind: String,
    #[serde(default)]
    metadata: HashMap<String, String>,
}

fn source_request_via_sidecar(
    sidecar: &mut Sidecar,
    path: &Path,
    source: &str,
    payload: String,
) -> Result<String, SidecarParseError> {
    let context = VbParserContext::from_authority(sidecar.identity.clone());
    if source.len() > context.max_source_bytes { return Err(SidecarParseError::SourceLimit); }
    source_request_with_limits(sidecar, path, source, payload, 64 * 1024 * 1024, Duration::from_secs(context.request_timeout_secs))
}

fn source_request_with_limits(
    sidecar: &mut Sidecar, path: &Path, _source: &str, payload: String,
    response_limit: usize, timeout: Duration,
) -> Result<String, SidecarParseError> {
    // One async IO owner applies cancellation/deadlines to BOTH pipe directions;
    // no blocked reader/writer helper can outlive an abandoned request.
    sidecar.transport.request(payload, response_limit, timeout)
        .map_err(|error| SidecarParseError::Protocol(anyhow::anyhow!("VB request for {} failed: {error}", path.display())))
}

fn parse_via_sidecar(
    sidecar: &mut Sidecar,
    path: &Path,
    source: &str,
    context: &VbParserContext,
    project_root: Option<&Path>,
) -> Result<(Vec<ExtractedSymbol>, Vec<ExtractedEdge>, VbParserProvenance), SidecarParseError> {
    if source.len() > context.max_source_bytes { return Err(SidecarParseError::SourceLimit); }
    let root_text = project_root.map(|root| root.to_string_lossy());
    let req = SidecarRequest { cmd: "parse", path: path.display().to_string(), source,
        project_root: root_text.as_deref(), standalone: project_root.is_none() };
    let payload = transport::encode_request(&req).map_err(|error| {
        if error.io_error_kind() == Some(std::io::ErrorKind::InvalidInput) {
            SidecarParseError::SourceLimit
        } else {
            SidecarParseError::LocalEncoding(error.into())
        }
    })?;
    let line = source_request_with_limits(sidecar, path, source, payload, 64 * 1024 * 1024,
        Duration::from_secs(context.request_timeout_secs))?;
    let response: SidecarResponse =
        serde_json::from_str(&line).map_err(|e| SidecarParseError::Protocol(e.into()))?;
    if response.compilation_error { return Err(SidecarParseError::Identity); }
    if let Some(error) = response.error {
        return Err(SidecarParseError::Sidecar(error));
    }
    if sidecar.identity.is_some() && match project_root {
        Some(root) => response.standalone || response.project_root.as_deref().map(Path::new) != Some(root),
        None => !response.standalone || response.project_root.is_some(),
    } { return Err(SidecarParseError::Identity); }
    let provenance = match (&sidecar.identity, response.parser_identity) {
        (Some(identity), Some(actual)) if actual == *identity && actual.is_supported() => VbParserProvenance::Roslyn { context: context.clone() },
        (None, _) => VbParserProvenance::UnverifiedRoslyn,
        _ => return Err(SidecarParseError::Identity),
    };

    let mut sidecar_edge_kind_counts: HashMap<String, usize> = HashMap::new();
    for edge in &response.edges {
        *sidecar_edge_kind_counts
            .entry(edge.kind.clone())
            .or_insert(0) += 1;
    }
    if !sidecar_edge_kind_counts.is_empty() {
        tracing::debug!(
            path = %path.display(),
            edge_kind_counts = ?sidecar_edge_kind_counts,
            "VB sidecar parse_via_sidecar deserialized edge counts"
        );
    }

    let symbols = response
        .symbols
        .into_iter()
        .map(|s| ExtractedSymbol {
            name: dedupe_fqn(&s.name),
            kind: s.kind,
            start_line: s.start_line,
            end_line: s.end_line,
            metadata: if s.metadata.is_empty() {
                None
            } else {
                Some(dedupe_fqn_metadata(s.metadata))
            },
        })
        .collect::<Vec<_>>();

    let edges = response
        .edges
        .into_iter()
        .map(|e| ExtractedEdge {
            source_name: dedupe_fqn(&e.source_name),
            source_kind: e.source_kind,
            source_start_line: e.source_start_line,
            source_language: e.source_language,
            target_name: dedupe_fqn(&e.target_name),
            target_kind: e.target_kind,
            target_start_line: e.target_start_line,
            kind: e.kind,
            metadata: if e.metadata.is_empty() {
                None
            } else {
                Some(dedupe_fqn_metadata(e.metadata))
            },
        })
        .collect::<Vec<_>>();

    Ok((symbols, edges, provenance))
}

/// Collapse repeated leading segment chains in a dotted name:
/// `_api2._api2.Logger.LogError` → `_api2.Logger.LogError`,
/// `a.b.c.a.b.c.X` → `a.b.c.X`. Older sidecar builds composed FQNs by
/// concatenating namespaces with an already-qualified type stack; this
/// normalizer makes ingestion correct regardless of deployed sidecar
/// version.
pub(crate) fn dedupe_fqn(name: &str) -> String {
    if !name.contains('.') {
        return name.to_string();
    }
    let mut parts: Vec<&str> = name.split('.').collect();
    loop {
        let mut collapsed = false;
        let max_k = parts.len() / 2;
        for k in (1..=max_k).rev() {
            if parts[..k] == parts[k..2 * k] {
                parts.drain(..k);
                collapsed = true;
                break;
            }
        }
        if !collapsed {
            break;
        }
    }
    parts.join(".")
}

/// Test-only re-export.
pub fn dedupe_fqn_for_test(name: &str) -> String {
    dedupe_fqn(name)
}

fn dedupe_fqn_metadata(mut m: HashMap<String, String>) -> HashMap<String, String> {
    if let Some(fqn) = m.get("fqn").cloned() {
        m.insert("fqn".to_string(), dedupe_fqn(&fqn));
    }
    m
}

/// Outcome of one small request/response exchange with the sidecar.
enum Exchange {
    Ok(serde_json::Value),
    /// Protocol failure. `kill` means the child is in an unknown state and
    /// must be torn down so the next call respawns it.
    Failed {
        kill: bool,
    },
}

/// Send one small command and read its single-line response.
///
/// Control messages use the same bounded cancellable IO owner as source text.
fn exchange(sidecar: &mut Sidecar, req: &serde_json::Value, label: &str) -> Exchange {
    let line = match sidecar.transport.request(req.to_string(), 4096, Duration::from_secs(60)) {
        Ok(line) => line,
        Err(error) => { tracing::warn!(error=%error, "{label} transport failed"); return Exchange::Failed { kill: true }; }
    };
    match serde_json::from_str(&line) {
        Ok(v) => Exchange::Ok(v),
        Err(e) => {
            tracing::warn!("{label} response parse failed: {e}");
            Exchange::Failed { kill: true }
        }
    }
}

/// Run `exchange` against the locked sidecar, spawning it if needed and
/// tearing it down on a fatal protocol failure.
fn exchange_locked(
    guard: &mut Option<Sidecar>,
    req: &serde_json::Value,
    label: &str,
) -> Option<serde_json::Value> {
    let outcome = match ensure_sidecar(guard) {
        Ok(s) => exchange(s, req, label),
        Err(e) => {
            tracing::warn!("failed to spawn sidecar for {label}: {e}");
            return None;
        }
    };
    match outcome {
        Exchange::Ok(v) => Some(v),
        Exchange::Failed { kill } => {
            if kill && let Some(s) = guard.as_mut() {
                let _ = s.stop();
            }
            *guard = None;
            None
        }
    }
}

/// Build the sidecar's shared project compilation from scratch.
///
/// O(files): the child parses each supplied `.vb` file into one shared
/// compilation. On a large solution that is 7-18 seconds, so this belongs
/// on a FULL index — `prepare_project` picks it only when the cheap path
/// cannot apply.
pub fn begin_project(project_root: &Path, files: &[String]) {
    let Ok(_idle) = COMPILATION.clone().try_acquire_owned() else {
        tracing::warn!("VB begin_project refused while ingestion owns the compilation"); return;
    };
    let Ok(project_root) = std::fs::canonicalize(project_root) else { return; };
    let mutex = get_or_spawn_sidecar();
    let mut guard = match mutex.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!("VB sidecar mutex poisoned during begin_project: {poisoned}");
            return;
        }
    };
    begin_project_locked(&mut guard, &project_root, files);
}

fn begin_project_locked(
    guard: &mut Option<Sidecar>,
    project_root: &Path,
    files: &[String],
) -> bool {
    if guard.as_ref().is_some_and(|sidecar| sidecar.cached_charge.is_some()) {
        tracing::warn!("Legacy VB begin_project refused: use admitted ingestion to replace a charged compilation");
        return false;
    }
    if let Some(sidecar) = guard.as_mut() { sidecar.project_root = None; sidecar.inventory_signature = None; }
    // Hand over the exact file list. Left to walk the tree itself the
    // sidecar has no idea which files the indexer actually ingests — it
    // applies no ignore rules and no extension presets. On MiniLangCompiler
    // that difference is 73,072 `.vb` files (56,432 of them generated,
    // under `.tmp/`) against 917 real sources, which is what produced 188
    // 60-second parse timeouts and 12 sidecar crashes in the daemon log.
    let req = serde_json::json!({
        "cmd": "begin_project",
        "project_root": project_root.display().to_string(),
        "files": files,
    });
    let Some(parsed) = exchange_locked(guard, &req, "begin_project") else {
        return false;
    };
    if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
        tracing::warn!("begin_project sidecar error: {err}");
        if let Some(s) = guard.as_mut() {
            let _ = s.stop();
        }
        *guard = None;
        return false;
    }
    if let Some(s) = guard.as_mut() {
        if s.identity.is_some() && parsed.get("project_root").and_then(|v| v.as_str()).map(Path::new) != Some(project_root) {
            tracing::error!("VB companion did not attest the prepared compilation root");
            return false;
        }
        s.project_root = Some(project_root.to_path_buf());
    }
    tracing::info!(
        "VB sidecar begin_project completed for {}",
        project_root.display()
    );
    true
}

/// Trusted ingestion never asks the child to enumerate its own filesystem.
/// A stable admitted inventory sends only changed paths; a cold child, root
/// change, or inventory change receives the exact complete list once.
#[cfg(test)]
fn refresh_project_locked(guard: &mut Option<Sidecar>, project_root: &Path,
    inventory: &[engram_core::RelPath], changed: &[String]) -> bool {
    let context = VbParserContext::from_authority(guard.as_ref().and_then(|sidecar| sidecar.identity.clone()));
    refresh_project_admitted(guard, project_root, inventory, changed, &context,
        compilation::DEFAULT_BUDGET.clone()).is_ok()
}

fn restart_owned_compilation(guard: &mut Option<Sidecar>, expected: &VbParserContext) -> anyhow::Result<()> {
    let sidecar = guard.as_mut().ok_or_else(|| anyhow::anyhow!("VB child missing before cache eviction"))?;
    let binary = sidecar.binary.clone();
    sidecar.stop()?;
    // Old cache/owner charges are released only after actual join above.
    *guard = None;
    let sidecar = ensure_sidecar_at(guard, &binary)?;
    anyhow::ensure!(sidecar.identity == expected.roslyn && sidecar.transport.is_running(),
        "VB parser code changed during cache eviction; rescan the source operation");
    Ok(())
}

fn refresh_project_admitted(guard: &mut Option<Sidecar>, project_root: &Path,
    inventory: &[engram_core::RelPath], changed: &[String], context: &VbParserContext,
    budget: Arc<engram_core::memory::MemoryBudget>) -> anyhow::Result<()> {
    if guard.as_ref().is_some_and(|sidecar| sidecar.project_root.is_some() && sidecar.cached_charge.is_none()) {
        // An explicitly legacy warmup has no bounded-cache authority.
        restart_owned_compilation(guard, context)?;
    }
    let mut sorted = inventory.iter().map(|path| path.as_str()).collect::<Vec<_>>();
    sorted.sort_unstable();
    sorted.dedup();
    let mut hash = blake3::Hasher::new();
    for path in &sorted {
        hash.update(&(path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
    }
    let signature = hash.finalize();
    struct Inventory<'a> { root: &'a Path, paths: &'a [&'a str] }
    impl Serialize for Inventory<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeSeq;
            let mut sequence = serializer.serialize_seq(Some(self.paths.len()))?;
            // Join one path at a time into the bounded encoder. Do not first
            // duplicate an unbounded complete inventory as absolute strings.
            for path in self.paths { sequence.serialize_element(&self.root.join(path))?; }
            sequence.end()
        }
    }
    #[derive(Serialize)]
    struct Refresh<'a> {
        cmd: &'static str,
        project_root: &'a Path,
        paths: &'a [String],
        #[serde(skip_serializing_if = "Option::is_none")]
        files: Option<Inventory<'a>>,
        compilation_policy: &'a CompilationPolicy,
    }
    for attempt in 0..2 {
        let warm = guard.as_ref().is_some_and(|sidecar| sidecar.project_root.as_deref() == Some(project_root)
            && sidecar.inventory_signature == Some(signature) && sidecar.cached_policy.as_ref() == Some(&context.compilation)
            && sidecar.cached_charge.as_ref().is_some_and(|charge| charge.same_budget(&budget)));
        let request = Refresh { cmd: "plan_refresh", project_root, paths: changed,
            files: (!warm).then_some(Inventory { root: project_root, paths: &sorted }),
            compilation_policy: &context.compilation };
        let payload = transport::encode_request(&request)?;
        let sidecar = guard.as_mut().ok_or_else(|| anyhow::anyhow!("VB child missing before compilation plan"))?;
        let line = sidecar.transport.request(payload, 4096, Duration::from_secs(60))?;
        let response: SidecarResponse = serde_json::from_str(&line)?;
        if let Some(error) = response.error {
            sidecar.project_root = None;
            sidecar.inventory_signature = None;
            anyhow::bail!("VB compilation planning refused: {error}");
        }
        anyhow::ensure!(!response.compilation_error && response.parser_identity.as_ref() == context.roslyn.as_ref(),
            "VB compilation plan identity mismatch; source publication refused");
        let plan = response.compilation_plan.ok_or_else(|| anyhow::anyhow!("VB child omitted compilation plan"))?;
        anyhow::ensure!(plan.full == !warm && plan.tree_count == sorted.len() as u64,
            "VB compilation plan does not cover the entire admitted inventory");
        let (retained, scratch_bytes) = plan.charges(&context.compilation)?;
        let allocation = (|| -> anyhow::Result<_> {
            let charge = if warm {
                let charge = sidecar.cached_charge.as_ref().unwrap().clone();
                charge.ensure(retained, context.compilation.charge(context.compilation.max_total_source_bytes,
                    context.compilation.max_trees)?)?;
                charge
            } else { compilation::CachedCharge::new(budget.clone(), retained)? };
            let scratch = compilation::CachedCharge::scratch(budget.clone(), scratch_bytes)?;
            Ok((charge, scratch))
        })();
        let (charge, scratch) = match allocation {
            Ok(allocation) => allocation,
            Err(_) if attempt == 0 && sidecar.cached_charge.is_some() => {
                restart_owned_compilation(guard, context)?;
                continue;
            }
            Err(error) => return Err(error),
        };
        let mut holds = vec![charge.clone(), scratch.clone()];
        if let Some(old) = &sidecar.cached_charge { holds.push(old.clone()); }
        sidecar.transport.retain_compilation_holds(holds);
        sidecar.project_root = None;
        sidecar.inventory_signature = None;
        let payload = transport::encode_request(&serde_json::json!({"cmd":"refresh_project", "plan_id":plan.id}))?;
        let committed = (|| -> anyhow::Result<()> {
            let line = sidecar.transport.request(payload, 4096, Duration::from_secs(60))?;
            let response: SidecarResponse = serde_json::from_str(&line)?;
            anyhow::ensure!(response.error.is_none() && !response.compilation_error
                && response.project_root.as_deref().map(Path::new) == Some(project_root)
                && response.parser_identity.as_ref() == context.roslyn.as_ref(),
                "VB compilation refresh refused: {}", response.error.as_deref().unwrap_or("invalid receipt"));
            Ok(())
        })();
        if let Err(error) = committed {
            sidecar.stop()?;
            *guard = None;
            return Err(error);
        }
        sidecar.cached_charge = Some(charge.clone());
        sidecar.cached_policy = Some(context.compilation.clone());
        sidecar.project_root = Some(project_root.to_owned());
        sidecar.inventory_signature = Some(signature);
        sidecar.transport.retain_compilation_holds(vec![charge]);
        drop(scratch);
        return Ok(());
    }
    anyhow::bail!("VB compilation cache admission did not complete")
}

/// Result of asking the sidecar to drop cached trees.
enum Invalidate {
    Done,
    /// The deployed sidecar predates the command; never ask it again.
    Unsupported,
    Failed,
}

fn invalidate_locked(guard: &mut Option<Sidecar>, paths: &[String]) -> Invalidate {
    if paths.is_empty() {
        return Invalidate::Done;
    }
    let req = serde_json::json!({ "cmd": "invalidate", "paths": paths });
    let Some(parsed) = exchange_locked(guard, &req, "invalidate") else {
        return Invalidate::Failed;
    };
    if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
        // An older deployed binary answers "unknown command invalidate".
        // That is a protocol answer, not a broken child — keep it alive and
        // fall back to the full rebuild from now on.
        if err.contains("unknown command") {
            tracing::info!(
                "VB sidecar does not support incremental invalidate ({err}); \
                 falling back to full begin_project — redeploy the sidecar to \
                 get incremental updates"
            );
            if let Some(s) = guard.as_mut() {
                s.supports_invalidate = false;
            }
            return Invalidate::Unsupported;
        }
        tracing::warn!("invalidate sidecar error: {err}");
        if let Some(s) = guard.as_mut() {
            let _ = s.stop();
        }
        *guard = None;
        return Invalidate::Failed;
    }
    tracing::debug!(
        dropped = parsed
            .get("invalidated")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1),
        requested = paths.len(),
        "VB sidecar invalidated cached trees"
    );
    Invalidate::Done
}

/// Above this many changed files an incremental invalidate stops paying:
/// each invalidated file is re-parsed individually during extraction, and a
/// full `begin_project` batches that work into one compilation build.
fn incremental_invalidate_max() -> usize {
    std::env::var("ENGRAM_VB_SIDECAR_INCREMENTAL_MAX")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(200)
}

fn vb_paths(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("vb")))
        .map(|p| p.display().to_string())
        .collect()
}

/// Make the sidecar ready to extract `files` under `project_root`.
///
/// The whole point is to NOT pay a project-wide re-parse for an incremental
/// update. Live 2026-08-20: a watcher update with `changed=1` still spent
/// 7-18 s in `begin_project`, and the watcher fired every 13-25 s for 45
/// minutes. When the child is already warm for this root and the change set
/// is small, we drop just the stale trees instead — O(changed).
pub fn prepare_project(project_root: &Path, files: &[PathBuf]) {
    let vb_files = vb_paths(files);
    if vb_files.is_empty() { return; }
    let Ok(_idle) = COMPILATION.clone().try_acquire_owned() else {
        tracing::warn!("VB prepare_project refused while ingestion owns the compilation"); return;
    };
    let Ok(project_root) = std::fs::canonicalize(project_root) else { return; };
    let Ok(vb_files) = vb_files.iter().map(|path| std::fs::canonicalize(path)
        .map(|path| path.to_string_lossy().into_owned())).collect::<std::io::Result<Vec<_>>>() else { return; };
    let mutex = get_or_spawn_sidecar();
    let mut guard = match mutex.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!("VB sidecar mutex poisoned during prepare_project: {poisoned}");
            return;
        }
    };

    if guard.as_ref().is_some_and(|sidecar| sidecar.cached_charge.is_some()) {
        tracing::warn!("Legacy VB prepare_project refused: use admitted ingestion to refresh a charged compilation");
        return;
    }
    prepare_project_locked(&mut guard, &project_root, &vb_files);
}

fn prepare_project_locked(guard: &mut Option<Sidecar>, project_root: &Path, vb_files: &[String]) {
    let warm = guard
        .as_ref()
        .is_some_and(|s| s.project_root.as_deref() == Some(project_root) && s.supports_invalidate);

    let is_small_batch = vb_files.len() <= incremental_invalidate_max();

    if warm && is_small_batch {
        match invalidate_locked(guard, vb_files) {
            Invalidate::Done => return,
            // Fall through to the full rebuild.
            Invalidate::Unsupported | Invalidate::Failed => {}
        }
    }

    // `files` is what THIS run indexes, which is the whole project on a full
    // index but only the changed files on an incremental one. Handing the
    // sidecar a three-file compilation after a daemon restart would silently
    // cost every cross-file resolution in the project, so a small batch
    // against a cold child falls back to the sidecar's own walk — which is
    // pruned of build output and scratch trees, and so is no longer the
    // 73,072-file liability that made it time out.
    let supplied: &[String] = if is_small_batch { &[] } else { vb_files };
    begin_project_locked(guard, project_root, supplied);
}

/// Drop the sidecar's cached trees for files that no longer exist.
///
/// Deletions never reach `prepare_project` (it only sees files to index), so
/// without this a removed type would keep resolving until the next full
/// rebuild.
pub fn forget_files(paths: &[PathBuf]) -> anyhow::Result<()> {
    let vb = vb_paths(paths);
    if vb.is_empty() {
        return Ok(());
    }
    let _idle = COMPILATION.clone().try_acquire_owned()
        .map_err(|_| anyhow::anyhow!("VB cache deletion is busy with an active ingestion batch; retry the source update"))?;
    let mutex = get_or_spawn_sidecar();
    let mut guard = mutex.lock().map_err(|error| anyhow::anyhow!("VB cache coordinator unavailable: {error}"))?;
    // Nothing to forget if the child was never warmed for a project.
    if guard
        .as_ref()
        .is_none_or(|s| s.project_root.is_none() || !s.supports_invalidate)
    {
        return Ok(());
    }
    // Deleted files cannot themselves be canonicalized. Existing parents give
    // the same verbatim Windows spelling as warmup/parse. If an entire parent
    // was deleted, invalidate the remembered warm root so next ingestion must
    // rebuild instead of retaining trees under an unprovable guessed key.
    let normalized = vb.iter().map(|path| {
        let path = Path::new(path);
        Some(std::fs::canonicalize(path.parent()?).ok()?.join(path.file_name()?).to_string_lossy().into_owned())
    }).collect::<Option<Vec<_>>>();
    let Some(normalized) = normalized else {
        if let Some(sidecar) = guard.as_mut() { sidecar.project_root = None; }
        return Ok(());
    };
    match invalidate_locked(&mut guard, &normalized) {
        Invalidate::Done | Invalidate::Unsupported => Ok(()),
        Invalidate::Failed => anyhow::bail!("VB cache deletion failed; retry the source update"),
    }
}

pub fn extract_vb(path: &Path, source: &str) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let (symbols, edges, _) = extract_vb_with_provenance(path, source);
    (symbols, edges)
}

/// The receipt accompanies the actual result, including empty/zero-symbol
/// source and per-file fallback after a Roslyn parse or transport failure.
pub fn extract_vb_with_provenance(path: &Path, source: &str)
    -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>, VbParserProvenance) {
    let Ok(_idle) = COMPILATION.clone().try_acquire_owned() else {
        let (symbols, edges) = fallback_extract_vb(path, source);
        tracing::warn!(path=%path.display(), "Context-free VB extraction refused while ingestion owns the compilation");
        return (symbols, edges, VbParserProvenance::UnverifiedRoslyn);
    };
    let mutex = get_or_spawn_sidecar();
    let mut guard = match mutex.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!("VB sidecar mutex poisoned: {poisoned}");
            let (symbols, edges) = fallback_extract_vb(path, source);
            return (symbols, edges, VbParserProvenance::UnverifiedRoslyn);
        }
    };

    extract_vb_with_client(&mut guard, &sidecar_binary_path(), path, source)
}

fn extract_vb_with_client(guard: &mut Option<Sidecar>, companion: &Path, path: &Path, source: &str)
    -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>, VbParserProvenance) {
    let fallback = || {
        let (symbols, edges) = fallback_extract_vb(path, source);
        (symbols, edges, VbParserProvenance::fallback())
    };

    let sidecar = match ensure_sidecar_at(guard, companion) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("failed to spawn sidecar: {e}; using fallback VB extraction");
            if guard.is_some() || !transport::lifecycle_ready() {
                let (symbols, edges) = fallback_extract_vb(path, source);
                return (symbols, edges, VbParserProvenance::UnverifiedRoslyn);
            }
            return fallback();
        }
    };

    let context = VbParserContext::from_authority(sidecar.identity.clone());
    extract_vb_in_context(guard, path, source, context, None)
}

fn extract_vb_in_context(guard: &mut Option<Sidecar>, path: &Path, source: &str, context: VbParserContext,
    project_root: Option<&Path>)
    -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>, VbParserProvenance) {
    let Some(sidecar) = guard.as_mut() else {
        let (symbols, edges) = fallback_extract_vb(path, source);
        return (symbols, edges, VbParserProvenance::UnverifiedRoslyn);
    };
    if !context.is_supported() || context.roslyn != sidecar.identity {
        let (symbols, edges) = fallback_extract_vb(path, source);
        return (symbols, edges, VbParserProvenance::UnverifiedRoslyn);
    }
    match parse_via_sidecar(sidecar, path, source, &context, project_root) {
        Ok((mut symbols, mut edges, provenance)) => {
            // The Roslyn sidecar gives symbols/calls/SQL — but knows nothing
            // about Engram's settings/guard/hierarchy extraction. Enrich its
            // output with the same line-scan pass the fallback performs, so
            // production (sidecar) and degraded (fallback) modes agree.
            enrich_vb_source(source, &mut symbols, &mut edges);
            if source.bytes().all(|byte| byte.is_ascii_whitespace()) {
                symbols.clear();
                edges.clear();
            }
            (symbols, edges, provenance)
        }
        Err(SidecarParseError::Sidecar(err)) => {
            tracing::warn!("VB sidecar parse error for {}: {err}", path.display());
            degraded_fallback(path, source, context, VbFallbackReason::ParseError)
        }
        Err(SidecarParseError::Protocol(err)) => {
            tracing::warn!("VB sidecar protocol failure for {}: {err}", path.display());
            let stopped = sidecar.stop();
            *guard = None;
            if stopped.is_err() || !transport::lifecycle_ready() {
                let (symbols, edges) = fallback_extract_vb(path, source);
                (symbols, edges, VbParserProvenance::UnverifiedRoslyn)
            } else { degraded_fallback(path, source, context, VbFallbackReason::TransportFailure) }
        }
        Err(SidecarParseError::LocalEncoding(err)) => {
            // Nothing reached the transport. Keep the healthy child, but do
            // not allocate another fallback result after a local reservation
            // failure or certify empty extraction as successful source data.
            tracing::warn!(path = %path.display(), error = %err,
                "VB request encoding refused locally; trusted extraction is unavailable");
            (Vec::new(), Vec::new(), VbParserProvenance::UnverifiedRoslyn)
        }
        Err(SidecarParseError::Identity) => {
            tracing::error!(path = %path.display(), "VB parse identity does not match the loaded parser receipt; trusted ingestion is refused");
            *guard = None;
            let (symbols, edges) = fallback_extract_vb(path, source);
            (symbols, edges, VbParserProvenance::UnverifiedRoslyn)
        }
        Err(SidecarParseError::SourceLimit) => {
            tracing::warn!(path=%path.display(), cap=context.max_source_bytes, "VB source exceeds parser policy; retaining healthy child and recording stable fallback");
            degraded_fallback(path, source, context, VbFallbackReason::SourceLimit)
        }
    }
}

fn degraded_fallback(path: &Path, source: &str, context: VbParserContext, reason: VbFallbackReason)
    -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>, VbParserProvenance) {
    let (symbols, edges) = fallback_extract_vb(path, source);
    let provenance = if context.roslyn.is_some() {
        VbParserProvenance::RustFallback { context, reason }
    } else { VbParserProvenance::UnverifiedRoslyn };
    (symbols, edges, provenance)
}

pub fn extract_vb_for_ingestion(path: &Path, source: &str)
    -> anyhow::Result<(Vec<ExtractedSymbol>, Vec<ExtractedEdge>, VbParserProvenance)> {
    let result = extract_vb_with_provenance(path, source);
    anyhow::ensure!(!matches!(&result.2, VbParserProvenance::Roslyn { .. }),
        "Standalone Roslyn extraction cannot certify project bindings; use index_files with the complete admitted VB inventory");
    anyhow::ensure!(result.2.is_verified(),
        "VB parser identity unavailable or changed during extraction of {}; deploy the matching companion and retry (unverified output was not published)", path.display());
    Ok(result)
}

pub(crate) fn extract_vb_in_batch(lease: &VbCompilationLease, path: &Path, source: &str)
    -> anyhow::Result<(Vec<ExtractedSymbol>, Vec<ExtractedEdge>, VbParserProvenance)> {
    let mut guard = get_or_spawn_sidecar().lock()
        .map_err(|error| anyhow::anyhow!("VB compilation coordinator unavailable: {error}"))?;
    let canonical_path = std::fs::canonicalize(path)?;
    anyhow::ensure!(canonical_path.starts_with(&lease.root),
        "VB source lies outside its intended compilation root: {}", path.display());
    let result = match &lease.incarnation {
        Some(expected) => {
            anyhow::ensure!(guard.as_ref().is_some_and(|s| Arc::ptr_eq(expected, &s.incarnation)
                && s.project_root.as_ref() == Some(&lease.root) && s.transport.is_running()),
                "VB child/root changed during ingestion; retry the whole source update");
            extract_vb_in_context(&mut guard, &canonical_path, source, lease.context.clone(), Some(&lease.root))
        }
        None => {
            anyhow::ensure!(guard.is_none() && transport::lifecycle_ready(),
                "VB parser availability changed during fallback ingestion; retry the whole source update");
            let (symbols, edges) = fallback_extract_vb(path, source);
            (symbols, edges, VbParserProvenance::RustFallback {
                context: lease.context.clone(), reason: VbFallbackReason::Unavailable,
            })
        }
    };
    anyhow::ensure!(result.2.is_verified(), "VB compilation identity was not verified; retry the whole source update");
    Ok(result)
}

#[cfg(test)]
mod provenance_tests;

/// Test-only re-export: the enrichment pass can't be exercised through
/// `extract_vb` in tests (no sidecar binary), so integration tests call it
/// directly with synthetic real-ranged symbols.
pub fn enrich_vb_source_for_test(
    source: &str,
    symbols: &mut [ExtractedSymbol],
    edges: &mut Vec<ExtractedEdge>,
) {
    enrich_vb_source(source, symbols, edges)
}

/// Enrichment pass over sidecar output: settings reads (ReadsSetting edges),
/// permission-check metadata on the enclosing functions, and class-level
/// Inherits/Implements hierarchy edges. Sidecar symbols carry REAL line
/// ranges, so association is range-based (same approach as the C# path).
/// External audit round 2, item 8 — the broker's `Select Case` arms, shared
/// by BOTH extraction paths (Roslyn sidecar and regex fallback; live r43 found
/// the scan on the fallback path only, so production carried no keys).
/// `Case "athDeleteByID"` followed by `s = DeleteChangeRequest(qry)` yields a
/// Calls edge from the enclosing function to the callee carrying
/// `dispatch_key = "athDeleteByID"`; the graph's route pass joins
/// `api.ajax('athDeleteByID')` to it. `fn_ranges` = (start, end, name) of the
/// file's functions; the innermost range owns the arm.
pub fn vb_dispatch_arm_edges(source: &str, fn_ranges: &[(u32, u32, String)]) -> Vec<ExtractedEdge> {
    let enclosing = |line: u32| -> Option<&str> {
        fn_ranges
            .iter()
            .filter(|(s, e, _)| *s <= line && line <= *e)
            .min_by_key(|(s, e, _)| e - s)
            .map(|(_, _, n)| n.as_str())
    };
    let mut out = Vec::new();
    let mut dispatch_key: Option<String> = None;
    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;
        let line = raw_line.trim();
        let lower = line.to_ascii_lowercase();
        if let Some(c) = RE_VB_CASE_LITERAL.captures(line) {
            dispatch_key = c.get(1).map(|m| m.as_str().to_string());
            continue;
        }
        if lower.starts_with("case ")
            || lower.starts_with("end select")
            || lower.starts_with("select case")
        {
            dispatch_key = None;
            continue;
        }
        let Some(key) = dispatch_key.as_deref() else {
            continue;
        };
        let Some(callee) = RE_VB_ARM_CALL
            .captures(line)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
        else {
            continue;
        };
        let head = callee.split('.').next().unwrap_or("").to_ascii_lowercase();
        if VB_ARM_CALL_STOPWORDS.contains(&head.as_str())
            || VB_CALL_HEAD_STOPWORDS.contains(&head.as_str())
        {
            continue;
        }
        let Some(method) = enclosing(line_no) else {
            continue;
        };
        out.push(ExtractedEdge {
            source_name: method.to_string(),
            source_kind: "function".to_string(),
            source_start_line: line_no,
            source_language: "vb".to_string(),
            target_name: callee.to_string(),
            target_kind: None,
            target_start_line: None,
            kind: "calls".to_string(),
            metadata: Some(HashMap::from([(
                "dispatch_key".to_string(),
                key.to_string(),
            )])),
        });
    }
    out
}

/// Function ranges for the regex fallback, whose symbols carry no end line:
/// each function owns the lines up to the next function's start.
fn fallback_fn_ranges(symbols: &[ExtractedSymbol]) -> Vec<(u32, u32, String)> {
    let mut starts: Vec<(u32, String)> = symbols
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| (s.start_line, s.name.clone()))
        .collect();
    starts.sort_by_key(|(l, _)| *l);
    let mut out = Vec::with_capacity(starts.len());
    for (i, (start, name)) in starts.iter().enumerate() {
        let end = starts
            .get(i + 1)
            .map(|(next, _)| next.saturating_sub(1))
            .unwrap_or(u32::MAX);
        out.push((*start, end.max(*start), name.clone()));
    }
    out
}

fn enrich_vb_source(source: &str, symbols: &mut [ExtractedSymbol], edges: &mut Vec<ExtractedEdge>) {
    // (start, end, name) for functions and classes, for range association.
    let fn_ranges: Vec<(u32, u32, String)> = symbols
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| (s.start_line, s.end_line, s.name.clone()))
        .collect();
    let class_ranges: Vec<(u32, u32, String)> = symbols
        .iter()
        .filter(|s| s.kind == "class")
        .map(|s| (s.start_line, s.end_line, s.name.clone()))
        .collect();
    // Item 8: the broker's dispatch arms (sidecar path).
    edges.extend(vb_dispatch_arm_edges(source, &fn_ranges));
    let enclosing = |ranges: &[(u32, u32, String)], line: u32| -> Option<String> {
        ranges
            .iter()
            .filter(|(s, e, _)| *s <= line && *e >= line)
            // Innermost: the tightest containing range.
            .min_by_key(|(s, e, _)| e - s)
            .map(|(_, _, n)| n.clone())
    };

    // Avoid duplicating hierarchy edges the sidecar may already emit.
    let existing_hierarchy: std::collections::HashSet<(String, String)> = edges
        .iter()
        .filter(|e| e.kind == "inherits_from" || e.kind == "implements_interface")
        .map(|e| (e.source_name.clone(), e.target_name.clone()))
        .collect();

    let mut guard_hits: Vec<(u32, String)> = Vec::new();
    let mut role_hits: Vec<(u32, String)> = Vec::new();

    // ── Pass 0: collect ORM context variable names (file-scoped) ────────────
    let mut ctx_vars: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in source.lines() {
        for cap in RE_VB_CTX_DECL
            .captures_iter(line)
            .chain(RE_VB_CTX_ASSIGN.captures_iter(line))
        {
            if let Some(v) = cap.get(1) {
                ctx_vars.insert(v.as_str().to_lowercase());
            }
        }
    }
    // (function, table, access) — dedup before emitting; 900 LINQ queries in
    // one file must not become 900 identical edges.
    let mut table_accesses: std::collections::HashSet<(String, String, &'static str)> =
        std::collections::HashSet::new();
    let mut table_access_lines: std::collections::HashMap<(String, String), u32> =
        std::collections::HashMap::new();
    let mut nav_accesses: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    let mut nav_access_lines: std::collections::HashMap<(String, String), u32> =
        std::collections::HashMap::new();

    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;
        let line = raw_line.trim();
        let lower = line.to_ascii_lowercase();

        // ── TODO-17: dynamic SQL on the sidecar path ───────────────────────
        // Roslyn's SQL detection (like the fallback's) only sees string
        // literals; variable command text was invisible in production.
        for dyn_cap in RE_VB_SQL_CMD_DYN
            .captures_iter(line)
            .chain(RE_VB_COMMAND_TEXT_DYN.captures_iter(line))
        {
            let Some(ident) = dyn_cap.get(1).map(|m| m.as_str()) else {
                continue;
            };
            if ident.eq_ignore_ascii_case("new") {
                continue;
            }
            let func = enclosing(&fn_ranges, line_no).unwrap_or_else(|| "file".to_string());
            let mut meta = std::collections::HashMap::new();
            meta.insert("dynamic".to_string(), "true".to_string());
            meta.insert("identifier".to_string(), ident.to_string());
            edges.push(ExtractedEdge {
                source_name: func,
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: format!("sql:dynamic:{ident}"),
                target_kind: Some("inline_sql".to_string()),
                target_start_line: None,
                kind: "sql_calls".to_string(),
                metadata: Some(meta),
            });
        }

        // ── ORM table access (LINQ-to-SQL / EF) ────────────────────────────
        if !ctx_vars.is_empty() {
            for cap in RE_VB_CTX_WRITE.captures_iter(line) {
                let (Some(var), Some(table)) = (cap.get(1), cap.get(2)) else {
                    continue;
                };
                if !ctx_vars.contains(&var.as_str().to_lowercase()) {
                    continue;
                }
                let table_l = table.as_str().to_lowercase();
                if CTX_NON_TABLE_MEMBERS.contains(&table_l.as_str()) {
                    continue;
                }
                let func = enclosing(&fn_ranges, line_no).unwrap_or_else(|| "file".to_string());
                table_access_lines
                    .entry((func.clone(), table_l.clone()))
                    .or_insert(line_no);
                table_accesses.insert((func, table_l, "write"));
            }
            for cap in RE_VB_CTX_MEMBER.captures_iter(line) {
                let (Some(var), Some(table)) = (cap.get(1), cap.get(2)) else {
                    continue;
                };
                if !ctx_vars.contains(&var.as_str().to_lowercase()) {
                    continue;
                }
                // Method calls are not table properties: skip when the next
                // non-space char after the member is `(`.
                let after = line[table.end()..].trim_start();
                if after.starts_with('(') {
                    continue;
                }
                let table_l = table.as_str().to_lowercase();
                if CTX_NON_TABLE_MEMBERS.contains(&table_l.as_str()) {
                    continue;
                }
                let func = enclosing(&fn_ranges, line_no).unwrap_or_else(|| "file".to_string());
                table_access_lines
                    .entry((func.clone(), table_l.clone()))
                    .or_insert(line_no);
                table_accesses.insert((func, table_l, "read"));
            }
        }

        // Navigation-property reads inside query clauses (row-4 audit A5).
        if is_linq_query_clause(&lower) {
            for cap in RE_VB_NAV_CHAIN.captures_iter(line) {
                let (Some(var), Some(member)) = (cap.get(1), cap.get(2)) else {
                    continue;
                };
                if ctx_vars.contains(&var.as_str().to_lowercase()) {
                    continue; // `db.Table.Column` is the ctx path above
                }
                if !looks_like_table_member(member.as_str()) {
                    continue;
                }
                let table_l = member.as_str().to_lowercase();
                if CTX_NON_TABLE_MEMBERS.contains(&table_l.as_str()) {
                    continue;
                }
                let func = enclosing(&fn_ranges, line_no).unwrap_or_else(|| "file".to_string());
                nav_access_lines
                    .entry((func.clone(), table_l.clone()))
                    .or_insert(line_no);
                nav_accesses.insert((func, table_l));
            }
        }

        // Settings reads → edges from the enclosing function (or file).
        let settings_code = vb_settings_code_mask(line);
        for cap in RE_VB_APPSETTINGS
            .captures_iter(line)
            .chain(RE_VB_MY_SETTINGS.captures_iter(line))
        {
            if cap.get(0).is_none_or(|m| settings_code.as_bytes()[m.start()] == b' ') {
                continue;
            }
            let Some(key) = cap.get(1).map(|m| m.as_str()) else {
                continue;
            };
            edges.push(ExtractedEdge {
                source_name: enclosing(&fn_ranges, line_no).unwrap_or_else(|| "file".to_string()),
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: key.to_string(),
                target_kind: Some("app_setting".to_string()),
                target_start_line: None,
                kind: "reads_setting".to_string(),
                metadata: None,
            });
        }

        // Settings-store property reads (ConfigSettings.X.Y — see the
        // RE_VB_SETTINGS_STORE rationale above).
        for cap in RE_VB_SETTINGS_STORE.captures_iter(&settings_code) {
            let (Some(root), Some(tail)) = (cap.get(1), cap.get(2)) else {
                continue;
            };
            let root_l = root.as_str().to_lowercase();
            // My.Settings is already handled by its framework-key pattern.
            // Do not emit a second, unresolved Settings.Member dependency.
            if root_l == "settings" && RE_VB_MY_SETTINGS.find_iter(&settings_code)
                .any(|m| m.start() + 3 == root.start())
            {
                continue;
            }
            // Framework roots covered by the AppSettings shape; `My.Settings`
            // covered above; declarations like `Dim x As ConfigSettings` are
            // filtered by requiring a property tail (regex already does).
            if matches!(
                root_l.as_str(),
                "configurationmanager" | "webconfigurationmanager" | "configurationsettings"
            ) {
                continue;
            }
            // Method calls are not settings reads: skip when `(` follows.
            let after = line[tail.end()..].trim_start();
            if after.starts_with('(') {
                continue;
            }
            // Skip tails that are themselves store-plumbing members.
            let tail_l = tail.as_str().to_lowercase();
            if tail_l == "appsettings" || tail_l.starts_with("appsettings.") {
                continue;
            }
            edges.push(ExtractedEdge {
                source_name: enclosing(&fn_ranges, line_no).unwrap_or_else(|| "file".to_string()),
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: format!("{}.{}", root.as_str(), tail.as_str()),
                target_kind: Some("app_setting".to_string()),
                target_start_line: None,
                kind: "reads_setting".to_string(),
                metadata: None,
            });
        }

        // Guard calls + role literals (annotated onto functions below).
        for cap in RE_VB_GUARD_CALL.captures_iter(line) {
            if let Some(name) = cap.get(1) {
                guard_hits.push((line_no, name.as_str().to_lowercase()));
            }
        }
        for cap in RE_VB_GUARD_ROLE_LITERAL.captures_iter(line) {
            if let Some(role) = cap.get(1) {
                role_hits.push((line_no, role.as_str().to_string()));
            }
        }

        // Class-level Inherits / Implements.
        if lower.starts_with("inherits ") || lower.starts_with("implements ") {
            let Some(class_name) = enclosing(&class_ranges, line_no) else {
                continue;
            };
            let is_implements = lower.starts_with("implements ");
            let list = &line[if is_implements { 11 } else { 9 }..];
            for raw_base in list.split(',') {
                let base = raw_base
                    .split('(')
                    .next()
                    .unwrap_or(raw_base)
                    .trim()
                    .trim_start_matches("Global.")
                    .trim();
                if base.is_empty()
                    || existing_hierarchy.contains(&(class_name.clone(), base.to_string()))
                {
                    continue;
                }
                edges.push(ExtractedEdge {
                    source_name: class_name.clone(),
                    source_kind: "class".to_string(),
                    source_start_line: line_no,
                    source_language: "vb".to_string(),
                    target_name: base.to_string(),
                    target_kind: Some("class".to_string()),
                    target_start_line: None,
                    kind: if is_implements {
                        "implements_interface".to_string()
                    } else {
                        "inherits_from".to_string()
                    },
                    metadata: None,
                });
            }
        }
    }

    // ── Emit deduplicated ORM table-access edges ────────────────────────────
    // One edge per (function, table); access metadata distinguishes
    // read / write / readwrite. Targets resolve to the DDL-extracted
    // db_table nodes via NodeId::table (lowercased) in ingest.
    let mut per_pair: std::collections::HashMap<(String, String), (bool, bool)> =
        std::collections::HashMap::new();
    for (func, table, access) in table_accesses {
        let entry = per_pair.entry((func, table)).or_insert((false, false));
        if access == "write" {
            entry.1 = true;
        } else {
            entry.0 = true;
        }
    }
    for ((func, table), (read, write)) in per_pair {
        let access = match (read, write) {
            (true, true) => "readwrite",
            (false, true) => "write",
            _ => "read",
        };
        let line = table_access_lines
            .get(&(func.clone(), table.clone()))
            .copied()
            .unwrap_or(0);
        let mut meta = HashMap::new();
        meta.insert("orm".to_string(), "true".to_string());
        meta.insert("access".to_string(), access.to_string());
        edges.push(ExtractedEdge {
            source_name: func,
            source_kind: "function".to_string(),
            source_start_line: line,
            source_language: "vb".to_string(),
            target_name: table,
            target_kind: Some("db_table".to_string()),
            target_start_line: None,
            kind: "queries_table".to_string(),
            metadata: Some(meta),
        });
    }

    // Navigation-property reads (row-4 audit A5): one READ edge per
    // (function, table) not already covered by a context access.
    let covered: std::collections::HashSet<(String, String)> = edges
        .iter()
        .filter(|e| e.kind == "queries_table")
        .map(|e| (e.source_name.clone(), e.target_name.to_lowercase()))
        .collect();
    let mut nav_sorted: Vec<(String, String)> = nav_accesses.into_iter().collect();
    nav_sorted.sort();
    for (func, table) in nav_sorted {
        if covered.contains(&(func.clone(), table.clone())) {
            continue;
        }
        let line = nav_access_lines
            .get(&(func.clone(), table.clone()))
            .copied()
            .unwrap_or(0);
        let mut meta = HashMap::new();
        meta.insert("orm".to_string(), "nav".to_string());
        meta.insert("access".to_string(), "read".to_string());
        edges.push(ExtractedEdge {
            source_name: func,
            source_kind: "function".to_string(),
            source_start_line: line,
            source_language: "vb".to_string(),
            target_name: table,
            target_kind: Some("db_table".to_string()),
            target_start_line: None,
            kind: "queries_table".to_string(),
            metadata: Some(meta),
        });
    }
    // Range-based guard annotation (shared with the C# extractor).
    crate::cs_extractor::annotate_guards(symbols, &guard_hits, &role_hits);
}

/// Detect a VB.NET type declaration on a line and return (graph node-kind, name).
/// `Module`/`Structure`/`Enum` are treated as `class`-kind containers so their
/// members get a correct `Type.Member` FQN + `contains` edge and participate in
/// all class-based navigation/blast-radius logic; `Interface` keeps its kind.
/// Word-boundary matched (surrounding spaces) so `Enumerable`, `X.Class`, and
/// `End Class` don't false-positive.
fn detect_vb_type_decl(line: &str) -> Option<(&'static str, String)> {
    let lower = line.to_ascii_lowercase();
    if lower.trim_start().starts_with("end ") {
        return None;
    }
    const KW: &[(&str, &str)] = &[
        ("class", "class"),
        ("module", "class"),
        ("structure", "class"),
        ("interface", "interface"),
        ("enum", "class"),
    ];
    for (kw, kind) in KW {
        if lower.contains(&format!(" {kw} ")) || lower.starts_with(&format!("{kw} ")) {
            let mut name = line
                .split_whitespace()
                .skip_while(|t| t.to_ascii_lowercase() != *kw)
                .nth(1)
                .unwrap_or_default()
                .to_string();
            if let Some(p) = name.find('(') {
                name.truncate(p); // drop generic params: Foo(Of T) -> Foo
            }
            if name
                .chars()
                .next()
                .map(|c| c.is_alphabetic() || c == '_')
                .unwrap_or(false)
            {
                return Some((kind, name));
            }
        }
    }
    None
}

fn fallback_extract_vb(_path: &Path, source: &str) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    // Lightweight fallback if sidecar isn't available.
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let mut ns = String::new();
    let mut ty = String::new();
    // FQN of the enclosing Sub/Function — sql_calls edges must name their
    // real source symbol (matching the FQN-named node minted below) or fall
    // back to the "file" sentinel.
    let mut current_method: Option<String> = None;
    // Declared types of locals in the enclosing method, so `db.Save(...)` on
    // `Dim db As New DataLayer()` can be emitted as `DataLayer.Save`.
    let mut local_types: HashMap<String, String> = HashMap::new();
    // Guard calls per enclosing method: (guard names, role literals).
    let mut method_guards: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();

    let mut add_symbol =
        |name: String, kind: &str, line: u32, metadata: Option<HashMap<String, String>>| {
            symbols.push(ExtractedSymbol {
                name,
                kind: kind.to_string(),
                start_line: line,
                end_line: line,
                metadata,
            });
        };

    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = idx as u32 + 1;
        let line = raw_line.trim();
        let lower = line.to_ascii_lowercase();

        // SQL detection runs on every line (independent of the declaration
        // branches below): New SqlCommand("...") and .CommandText = "...".
        for sql_cap in RE_VB_SQL_CMD
            .captures_iter(line)
            .chain(RE_VB_COMMAND_TEXT.captures_iter(line))
        {
            let sql = sql_cap.get(1).map(|m| m.as_str()).unwrap_or("");
            if sql.trim().is_empty() {
                continue;
            }
            let (target_name, target_kind) = crate::cs_extractor::classify_cs_sql(sql);
            let mut meta = HashMap::new();
            meta.insert(
                "sql_snippet".to_string(),
                sql.chars().take(200).collect::<String>(),
            );
            // TODO-17: literal head + string concat ("SELECT ... " & var)
            // means the real statement is built at runtime — keep the edge
            // (the head still names tables) but mark it.
            if let Some(end) = sql_cap.get(1).map(|m| m.end())
                && line[end..].trim_start().starts_with('"')
                && line[end + 1..].trim_start().starts_with('&')
            {
                meta.insert("dynamic".to_string(), "true".to_string());
            }
            edges.push(ExtractedEdge {
                source_name: current_method.clone().unwrap_or_else(|| "file".to_string()),
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name,
                target_kind: Some(target_kind.to_string()),
                target_start_line: None,
                kind: "sql_calls".to_string(),
                metadata: Some(meta),
            });
        }

        // TODO-17: dynamic SQL (variable command text) — emit a marked edge
        // instead of dropping the access entirely.
        for dyn_cap in RE_VB_SQL_CMD_DYN
            .captures_iter(line)
            .chain(RE_VB_COMMAND_TEXT_DYN.captures_iter(line))
        {
            let Some(ident) = dyn_cap.get(1).map(|m| m.as_str()) else {
                continue;
            };
            // Skip literal matches re-captured by the loose pattern.
            if ident.eq_ignore_ascii_case("new") {
                continue;
            }
            let mut meta = HashMap::new();
            meta.insert("dynamic".to_string(), "true".to_string());
            meta.insert("identifier".to_string(), ident.to_string());
            edges.push(ExtractedEdge {
                source_name: current_method.clone().unwrap_or_else(|| "file".to_string()),
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: format!("sql:dynamic:{ident}"),
                target_kind: Some("inline_sql".to_string()),
                target_start_line: None,
                kind: "sql_calls".to_string(),
                metadata: Some(meta),
            });
        }

        // Settings reads (web.config appSettings + My.Settings members).
        let settings_code = vb_settings_code_mask(line);
        for cap in RE_VB_APPSETTINGS
            .captures_iter(line)
            .chain(RE_VB_MY_SETTINGS.captures_iter(line))
        {
            if cap.get(0).is_none_or(|m| settings_code.as_bytes()[m.start()] == b' ') {
                continue;
            }
            let Some(key) = cap.get(1).map(|m| m.as_str()) else {
                continue;
            };
            edges.push(ExtractedEdge {
                source_name: current_method.clone().unwrap_or_else(|| "file".to_string()),
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: key.to_string(),
                target_kind: Some("app_setting".to_string()),
                target_start_line: None,
                kind: "reads_setting".to_string(),
                metadata: None,
            });
        }

        // Settings-store property reads (ConfigSettings.X.Y) — same shape as
        // the sidecar path; see RE_VB_SETTINGS_STORE.
        for cap in RE_VB_SETTINGS_STORE.captures_iter(&settings_code) {
            let (Some(root), Some(tail)) = (cap.get(1), cap.get(2)) else {
                continue;
            };
            let root_l = root.as_str().to_lowercase();
            // My.Settings is already handled by its framework-key pattern.
            // Do not emit a second, unresolved Settings.Member dependency.
            if root_l == "settings" && RE_VB_MY_SETTINGS.find_iter(&settings_code)
                .any(|m| m.start() + 3 == root.start())
            {
                continue;
            }
            if matches!(
                root_l.as_str(),
                "configurationmanager" | "webconfigurationmanager" | "configurationsettings"
            ) {
                continue;
            }
            let after = line[tail.end()..].trim_start();
            if after.starts_with('(') {
                continue;
            }
            let tail_l = tail.as_str().to_lowercase();
            if tail_l == "appsettings" || tail_l.starts_with("appsettings.") {
                continue;
            }
            edges.push(ExtractedEdge {
                source_name: current_method.clone().unwrap_or_else(|| "file".to_string()),
                source_kind: "function".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: format!("{}.{}", root.as_str(), tail.as_str()),
                target_kind: Some("app_setting".to_string()),
                target_start_line: None,
                kind: "reads_setting".to_string(),
                metadata: None,
            });
        }

        // Permission checks: collected per enclosing method, annotated after
        // the scan (fallback symbols are line-anchored, so association is by
        // the current-method context rather than line ranges).
        if let Some(ref method_fqn) = current_method {
            for cap in RE_VB_GUARD_CALL.captures_iter(line) {
                if let Some(name) = cap.get(1) {
                    method_guards
                        .entry(method_fqn.clone())
                        .or_default()
                        .0
                        .push(name.as_str().to_lowercase());
                }
            }
            for cap in RE_VB_GUARD_ROLE_LITERAL.captures_iter(line) {
                if let Some(role) = cap.get(1) {
                    method_guards
                        .entry(method_fqn.clone())
                        .or_default()
                        .1
                        .push(role.as_str().to_string());
                }
            }
        }

        // Qualified call edges (Foo.Bar(...)) inside method bodies keep the
        // handler → DAL → SQL chain connected when the sidecar is absent.
        // Targets go out unresolved (target_kind: None → "::name"). A receiver
        // that is a local with a declared type is replaced by that type so the
        // resolver can bind `Type.Member` by qualified suffix; any other
        // receiver stays as written and remains part of the call's identity.
        if let Some(ref method_fqn) = current_method {
            if let Some(decl) = RE_VB_LOCAL_DECL.captures(line) {
                local_types.insert(decl[1].to_ascii_lowercase(), decl[2].to_string());
            }
            for c in RE_VB_QUALIFIED_CALL.captures_iter(line) {
                if c.get(1).is_some() {
                    continue; // constructor: New Foo.Bar(...)
                }
                let callee = c.get(2).map(|m| m.as_str()).unwrap_or("");
                let head = callee.split('.').next().unwrap_or("").to_ascii_lowercase();
                if callee.is_empty() || VB_CALL_HEAD_STOPWORDS.contains(&head.as_str()) {
                    continue;
                }
                let typed_callee = match callee.split_once('.') {
                    Some((receiver, member)) => local_types
                        .get(&receiver.to_ascii_lowercase())
                        .map(|declared| format!("{declared}.{member}"))
                        .unwrap_or_else(|| callee.to_string()),
                    None => callee.to_string(),
                };
                edges.push(ExtractedEdge {
                    source_name: method_fqn.clone(),
                    source_kind: "function".to_string(),
                    source_start_line: line_no,
                    source_language: "vb".to_string(),
                    target_name: typed_callee,
                    target_kind: None,
                    target_start_line: None,
                    kind: "calls".to_string(),
                    metadata: None,
                });
            }
        }

        if lower.starts_with("end sub") || lower.starts_with("end function") {
            current_method = None;
            local_types.clear();
        }

        if lower.starts_with("namespace ") {
            ns = line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
        } else if let Some((kind, name)) = detect_vb_type_decl(line) {
            // class / module / structure / interface / enum all open a type
            // scope. VB.NET uses `Module` pervasively for shared utility code,
            // and Structure/Interface/Enum were previously invisible to the
            // fallback — so their Subs/Functions lost both their `Type.Member`
            // FQN and the `contains` edge to their container (breaking
            // blast-radius and contains-based navigation in degraded mode).
            ty = name.clone();
            add_symbol(name.clone(), kind, line_no, None);
            if !ns.is_empty() {
                edges.push(ExtractedEdge {
                    source_name: ns.clone(),
                    source_kind: "namespace".to_string(),
                    source_start_line: 1,
                    source_language: "vb".to_string(),
                    target_name: name,
                    target_kind: Some(kind.to_string()),
                    target_start_line: Some(line_no),
                    kind: "contains".to_string(),
                    metadata: None,
                });
            }
        } else if let Some(we) = RE_VB_WITHEVENTS.captures(line) {
            if let Some(name_m) = we.get(1) {
                let name = name_m.as_str().to_string();
                add_symbol(name.clone(), "control_ref", line_no, None);
                if !ty.is_empty() {
                    edges.push(ExtractedEdge {
                        source_name: ty.clone(),
                        source_kind: "class".to_string(),
                        source_start_line: line_no,
                        source_language: "vb".to_string(),
                        target_name: name,
                        target_kind: Some("control_ref".to_string()),
                        target_start_line: Some(line_no),
                        kind: "contains".to_string(),
                        metadata: None,
                    });
                }
            }
        } else if let Some(declaration) = RE_VB_METHOD_DECL.find(line) {
            // Attribute strings may themselves contain Sub/Function tokens.
            // The prefix match ends after the actual declaration keyword.
            if let Some(method_token) = line[declaration.end()..].split_whitespace().next() {
                let mut method = method_token.to_string();
                if let Some(p) = method.find('(') {
                    method.truncate(p);
                }
                let mut meta = fallback_method_metadata(source, line_no as usize);
                // TODO-13: parameter count from the signature line so the
                // degraded path also feeds arity-aware resolution. Counts
                // top-level commas inside the first paren group; nested
                // parens (generics, defaults) are tracked by depth.
                if let Some(open) = line.find('(') {
                    let mut depth = 0i32;
                    let mut args: u32 = 0;
                    let mut saw_content = false;
                    for ch in line[open..].chars() {
                        match ch {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            ',' if depth == 1 => args += 1,
                            c if depth >= 1 && !c.is_whitespace() => saw_content = true,
                            _ => {}
                        }
                    }
                    let arity = if saw_content { args + 1 } else { 0 };
                    meta.insert("arity".to_string(), arity.to_string());
                }
                if lower.contains("async ") {
                    meta.insert("async".to_string(), "true".to_string());
                }
                if let Some((stage, seq)) = webforms_lifecycle_info(&method) {
                    meta.insert("lifecycle_stage".to_string(), stage.to_string());
                    meta.insert("lifecycle_sequence".to_string(), seq.to_string());
                }
                let fqn = if ns.is_empty() {
                    if ty.is_empty() {
                        method.clone()
                    } else {
                        format!("{ty}.{method}")
                    }
                } else if ty.is_empty() {
                    format!("{ns}.{method}")
                } else {
                    format!("{ns}.{ty}.{method}")
                };
                add_symbol(
                    fqn.clone(),
                    "function",
                    line_no,
                    if meta.is_empty() {
                        None
                    } else {
                        Some(meta.clone())
                    },
                );
                if !ty.is_empty() {
                    edges.push(ExtractedEdge {
                        source_name: ty.clone(),
                        source_kind: "class".to_string(),
                        source_start_line: line_no,
                        source_language: "vb".to_string(),
                        target_name: fqn.clone(),
                        target_kind: Some("function".to_string()),
                        target_start_line: Some(line_no),
                        kind: "contains".to_string(),
                        metadata: None,
                    });
                }
                current_method = Some(fqn.clone());
                local_types.clear();
                if let Some(handles_pos) = lower.find(" handles ") {
                    let handles = &line[handles_pos + 9..];
                    for part in handles.split(',') {
                        let part = part.trim();
                        if let Some((control, _evt)) = part.split_once('.') {
                            let mut m = HashMap::new();
                            m.insert("fqn".to_string(), fqn.clone());
                            edges.push(ExtractedEdge {
                                source_name: control.trim().to_string(),
                                source_kind: "control".to_string(),
                                source_start_line: line_no,
                                source_language: "vb".to_string(),
                                target_name: method.clone(),
                                target_kind: Some("function".to_string()),
                                target_start_line: Some(line_no),
                                kind: "event_wiring".to_string(),
                                metadata: Some(m),
                            });
                        }
                    }
                }
            }
        } else if (lower.starts_with("inherits ") || lower.starts_with("implements "))
            && !ty.is_empty()
        {
            // Type hierarchy: class-level Inherits / Implements statements.
            let is_implements = lower.starts_with("implements ");
            let list = &line[if is_implements { 11 } else { 9 }..];
            for raw_base in list.split(',') {
                let base = raw_base
                    .split('(')
                    .next()
                    .unwrap_or(raw_base)
                    .trim()
                    .trim_start_matches("Global.")
                    .trim();
                if base.is_empty() {
                    continue;
                }
                edges.push(ExtractedEdge {
                    source_name: ty.clone(),
                    source_kind: "class".to_string(),
                    source_start_line: line_no,
                    source_language: "vb".to_string(),
                    target_name: base.to_string(),
                    target_kind: Some("class".to_string()),
                    target_start_line: None,
                    kind: if is_implements {
                        "implements_interface".to_string()
                    } else {
                        "inherits_from".to_string()
                    },
                    metadata: None,
                });
            }
        } else if lower.starts_with("imports ") {
            let target = line[8..].trim().to_string();
            edges.push(ExtractedEdge {
                // "file" sentinel: ingest substitutes the file's project-relative
                // path. Passing path.display() here put an ABSOLUTE path into
                // the edge source, which process_ingest_stats rejects with a
                // bail! — aborting ingestion of the ENTIRE batch for any VB
                // project containing a single Imports statement.
                source_name: "file".to_string(),
                source_kind: "file".to_string(),
                source_start_line: line_no,
                source_language: "vb".to_string(),
                target_name: target,
                target_kind: Some("namespace".to_string()),
                target_start_line: None,
                kind: "imports".to_string(),
                metadata: None,
            });
        }
    }

    // Attach collected guard facts to the (FQN-named) function symbols.
    for sym in symbols.iter_mut() {
        if sym.kind != "function" {
            continue;
        }
        let Some((guards, roles)) = method_guards.get(&sym.name) else {
            continue;
        };
        let mut guards = guards.clone();
        guards.sort();
        guards.dedup();
        let mut roles = roles.clone();
        roles.sort();
        roles.dedup();
        let mut meta = sym.metadata.take().unwrap_or_default();
        if !guards.is_empty() {
            meta.insert("permission_checks".to_string(), guards.join(";"));
        }
        if !roles.is_empty() {
            meta.insert("guard_roles".to_string(), roles.join(";"));
        }
        sym.metadata = Some(meta);
    }

    // Backfill end_line for Sub/Function bodies. The line-scan emits each method
    // with end_line == start_line (it can't know the end at the declaration
    // line); without this every VB method shows a 1-line range in degraded
    // (no-sidecar) mode, breaking get_method_edit_context / read-lines body
    // retrieval. VB Subs/Functions don't nest, so each body ends at the first
    // `End Sub`/`End Function` at-or-after its start.
    let method_ends: Vec<u32> = source
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let lo = l.trim().to_ascii_lowercase();
            (lo.starts_with("end sub") || lo.starts_with("end function")).then_some(i as u32 + 1)
        })
        .collect();
    for s in &mut symbols {
        if s.kind == "function"
            && let Some(&end) = method_ends.iter().find(|&&e| e >= s.start_line)
            && end > s.start_line
        {
            s.end_line = end;
        }
    }

    // TODO-18: tag degraded-mode output. The sidecar silently falls back to
    // this line-scan extractor on spawn/protocol failure; without a marker
    // the lower-fidelity symbols are indistinguishable from Roslyn output,
    // so confidence scoring and audits can't discount them.
    for s in &mut symbols {
        s.metadata
            .get_or_insert_with(Default::default)
            .insert("extraction_fallback".to_string(), "true".to_string());
    }
    // Item 8: the broker's dispatch arms (fallback path).
    let ranges = fallback_fn_ranges(&symbols);
    edges.extend(vb_dispatch_arm_edges(source, &ranges));

    (symbols, edges)
}

// Preserve the logical declaration, including multiline parameter lists. The
// degraded parser only claims explicitly written accessibility/type facts.
fn fallback_method_metadata(source: &str, start_line: usize) -> HashMap<String, String> {
    let mut meta = HashMap::new();
    let mut signature = String::new();
    let mut depth = 0i32;
    for line in source.lines().skip(start_line.saturating_sub(1)) {
        let mut quoted = false;
        let mut clean = String::new();
        for ch in line.trim().chars() {
            if ch == '"' {
                quoted = !quoted;
            }
            if ch == '\'' && !quoted {
                break;
            }
            if !quoted {
                if ch == '(' {
                    depth += 1;
                }
                if ch == ')' {
                    depth -= 1;
                }
            }
            clean.push(ch);
        }
        let clean = clean.trim();
        let continued = clean.ends_with('_');
        if !signature.is_empty() {
            signature.push(' ');
        }
        signature.push_str(clean.trim_end_matches('_').trim_end());
        if depth <= 0 && !continued {
            break;
        }
    }
    if depth != 0 {
        return meta;
    }
    let Some(decl) = RE_VB_METHOD_DECL.find(&signature) else {
        return meta;
    };
    let prefix = signature[..decl.end()].to_ascii_lowercase();
    // Ignore attribute contents when inspecting modifiers.
    let prefix = prefix.rsplit('>').next().unwrap_or(&prefix);
    let words: Vec<_> = prefix.split_whitespace().collect();
    let access = if words.contains(&"private") && words.contains(&"protected") {
        Some("Private Protected")
    } else if words.contains(&"protected") && words.contains(&"friend") {
        Some("Protected Friend")
    } else if words.contains(&"public") {
        Some("Public")
    } else if words.contains(&"private") {
        Some("Private")
    } else if words.contains(&"protected") {
        Some("Protected")
    } else if words.contains(&"friend") {
        Some("Friend")
    } else {
        None
    };
    if let Some(access) = access {
        meta.insert("access_level".into(), access.into());
    }
    if words.last() == Some(&"sub") {
        meta.insert("return_type".into(), "Void".into());
    } else {
        // The last parameter-list close precedes As; the type itself may have
        // generic parentheses, so find the first top-level As after the name.
        let tail = &signature[decl.end()..];
        let mut nesting = 0i32;
        let mut quoted = false;
        for (i, ch) in tail.char_indices() {
            if ch == '"' {
                quoted = !quoted;
            }
            if quoted {
                continue;
            }
            if ch == '(' {
                nesting += 1;
            }
            if ch == ')' {
                nesting -= 1;
            }
            if nesting == 0 && tail[i..].to_ascii_lowercase().starts_with(" as ") {
                let ty = &tail[i + 4..];
                let lower = ty.to_ascii_lowercase();
                let end = [" handles ", " implements "]
                    .iter()
                    .filter_map(|s| lower.find(s))
                    .min()
                    .unwrap_or(ty.len());
                meta.insert("return_type".into(), ty[..end].trim().into());
                break;
            }
        }
    }
    meta.insert("signature".into(), signature);
    meta
}

/// Positive declaration candidates from these exact bytes only. Never starts
/// Roslyn or consults project state. Regex candidates have declaration-line
/// ranges only; an empty result cannot establish symbol absence or deletion.
pub fn snapshot_declaration_candidates(source: &str) -> Vec<ExtractedSymbol> {
    if source.len() > 128 * 1024 {
        return Vec::new();
    }
    fallback_extract_vb(Path::new("captured-snapshot.vb"), source).0
}

/// Eval/test entry to the degraded-mode extractor: deterministic (no
/// sidecar dependency), used by the graph-accuracy eval (TODO-49) and
/// unit tests. Not part of the indexing API.
#[doc(hidden)]
pub fn extract_vb_fallback_for_eval(
    path: &Path,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let (mut symbols, mut edges) = fallback_extract_vb(path, source);
    // Run the shared enrichment pass too (ORM/hierarchy/etc.) — it dedups
    // against already-emitted edges, mirroring full pipeline behavior.
    enrich_vb_source(source, &mut symbols, &mut edges);
    (symbols, edges)
}

/// Test-only re-export of the degraded-mode extractor so tests can pin its
/// behavior without depending on sidecar availability in the environment.
#[cfg(test)]
pub(crate) fn fallback_extract_vb_for_test(
    path: &Path,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    fallback_extract_vb(path, source)
}

fn webforms_lifecycle_info(name: &str) -> Option<(&'static str, &'static str)> {
    match name.to_ascii_lowercase().as_str() {
        "page_preinit" => Some(("PreInit", "1")),
        "page_init" | "oninit" => Some(("Init", "2")),
        "page_preload" => Some(("PreLoad", "4")),
        "page_load" | "onload" => Some(("Load", "5")),
        "page_prerender" | "onprerender" => Some(("PreRender", "7")),
        "page_unload" | "onunload" => Some(("Unload", "11")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::extract_vb;
    use std::path::Path;

    /// Only `.vb` files live in the sidecar's tree cache, so the invalidate
    /// payload must not carry the rest of a mixed change set — an `.aspx` or
    /// `.resx` path would just be a no-op round-trip on every update.
    #[test]
    fn vb_paths_filters_to_visual_basic_only() {
        let input = [
            std::path::PathBuf::from("repo/Site/a.vb"),
            std::path::PathBuf::from("repo/Site/b.VB"),
            std::path::PathBuf::from("repo/Site/c.aspx"),
            std::path::PathBuf::from("repo/Site/d.designer.vb"),
            std::path::PathBuf::from("repo/Site/no_extension"),
        ];
        let out = super::vb_paths(&input);
        assert_eq!(out.len(), 3, "got {out:?}");
        assert!(out.iter().any(|p| p.ends_with("a.vb")));
        assert!(out.iter().any(|p| p.ends_with("b.VB")));
        assert!(out.iter().any(|p| p.ends_with("d.designer.vb")));
    }

    #[test]
    fn guard_regex_matches_checkreadwrite_and_project_access_families() {
        let re = &*super::RE_VB_GUARD_CALL;
        // Previously MISSED (no access/permission/role substring) — the
        // guard-map DAO-delegation blind spot.
        assert!(re.is_match("If _us.UserAccess.CheckWrite(obj) Then"));
        assert!(re.is_match("If _us.UserAccess.CheckRead(obj) Then"));
        assert!(re.is_match("_us.accessctrl.Check_pr_id(project_id)"));
        assert!(re.is_match("_us.accessctrl.Check_rv_id(rv_id)"));
        // Still matched via the existing "access" alternative.
        assert!(re.is_match("check_siteaccessbyid(x)"));
        assert!(re.is_match("If IsUserInRole(\"Admin\") Then"));
        // Must NOT match ordinary calls.
        assert!(!re.is_match("Dim n = GetCount(list)"));
        assert!(!re.is_match("results.CheckSum(data)"));
    }

    #[test]
    fn settings_dependencies_ignore_literals_and_comments_on_both_extraction_paths() {
        let code = r#"Class Reader
  Public Sub Load()
    Dim cached = Session("PermissionCache.CurrentUser")
    Dim quoted = "åäö ""ConfigSettings.FalsePositive"" My.Settings.Fake"
    Dim text = "My.Settings.Fake" : Dim real = ConfigSettings.General.Enabled
    Dim text2 = "it's text" : Dim actual = My.Settings.Locale
    Dim plain = ConfigurationManager.AppSettings("PermissionCache.CurrentUser")
    ' ConfigSettings.Commented My.Settings.Commented AppSettings("Commented")
    Rem ConfigSettings.RemComment My.Settings.RemComment AppSettings("RemComment")
    Dim n = 1 : rEm ConfigSettings.InlineComment
    Dim trailing = 2 ' ConfigSettings.Trailing
    Dim escaped = """AppSettings(""NotARead"")"""
  End Sub
End Class"#;
        let (mut symbols, fallback_edges) =
            super::fallback_extract_vb_for_test(Path::new("Reader.vb"), code);
        // Sidecar enrichment receives real method ranges rather than fallback
        // one-line symbols. No external parser process is needed for this check.
        for symbol in &mut symbols {
            symbol.end_line = code.lines().count() as u32;
        }
        let mut enriched_edges = Vec::new();
        super::enrich_vb_source(code, &mut symbols, &mut enriched_edges);
        for (path, edges) in [("fallback", fallback_edges), ("sidecar enrichment", enriched_edges)] {
            let mut reads: Vec<_> = edges.iter()
                .filter(|e| e.kind == "reads_setting")
                .map(|e| (e.target_name.as_str(), e.source_start_line))
                .collect();
            reads.sort_unstable();
            assert_eq!(reads, vec![
                ("ConfigSettings.General.Enabled", 5),
                ("Locale", 6),
                ("PermissionCache.CurrentUser", 7),
            ], "{path}: quoted cache keys/comments must not become settings dependencies");
        }
    }

    #[test]
    fn settings_code_mask_preserves_unicode_offsets_and_statement_boundaries() {
        let source = "Dim å = \"é\" : Dim x = My.Settings.Locale ' ignored";
        let masked = super::vb_settings_code_mask(source);
        assert_eq!(masked.len(), source.len());
        assert_eq!(masked.find("My.Settings.Locale"), source.find("My.Settings.Locale"));
        assert!(!masked.contains("é") && !masked.contains("ignored"));
        assert!(super::vb_settings_code_mask("Dim remember = ConfigSettings.Real")
            .contains("ConfigSettings.Real"));
        assert!(super::vb_settings_code_mask("Rem").trim().is_empty());
        assert!(!super::vb_settings_code_mask("Dim x = \"unterminated ConfigSettings.Fake")
            .contains("ConfigSettings.Fake"));
    }

    #[test]
    fn settings_store_property_reads_emit_reads_setting_edges() {
        // The house pattern in mature apps: static store classes, not raw
        // AppSettings("..."). Missing this shape made settings intelligence
        // blind on exactly the codebases that need it.
        let code = "Class P\n  Public Sub Page_Load()\n    If ConfigSettings.Multitenant.IsMaster Then\n    End If\n    Dim x = SystemSettingStore.General.RoqEnableListTypeDimension\n    Dim y = ConfigurationManager.AppSettings(\"PlainKey\")\n    Dim z = SettingsHelper.Load(\"skip-method-calls\")\n    If Not _us.UserAccess.CheckWrite(_us.UserAccessObject.approval_of_inspection) Then\n    End If\n  End Sub\nEnd Class";
        let (_, edges) = super::fallback_extract_vb_for_test(Path::new("p.aspx.vb"), code);
        let settings: Vec<&str> = edges
            .iter()
            .filter(|e| e.kind == "reads_setting")
            .map(|e| e.target_name.as_str())
            .collect();
        assert!(
            settings.contains(&"ConfigSettings.Multitenant.IsMaster"),
            "{settings:?}"
        );
        assert!(
            settings
                .iter()
                .any(|s| s.starts_with("SystemSettingStore.General.RoqEnable")),
            "{settings:?}"
        );
        assert!(settings.contains(&"PlainKey"), "{settings:?}");
        assert!(
            !settings
                .iter()
                .any(|s| s.contains("skip-method-calls") || s.ends_with(".Load")),
            "method calls must not become settings reads: {settings:?}"
        );
        // Token-INITIAL roots (UserAccessObject) regressed silently when the
        // root pattern required a char before the token; permission reads
        // never extracted.
        assert!(
            settings.contains(&"UserAccessObject.approval_of_inspection"),
            "token-initial roots must extract: {settings:?}"
        );
        assert!(
            !settings.iter().any(|s| s.ends_with(".CheckWrite")),
            "method calls on access helpers must not extract: {settings:?}"
        );
        assert!(
            !settings
                .iter()
                .any(|s| s.starts_with("ConfigurationManager.")),
            "framework root must stay excluded: {settings:?}"
        );
    }

    #[test]
    fn fallback_symbols_are_tagged_extraction_fallback() {
        // TODO-18: degraded-mode output must be distinguishable from
        // Roslyn sidecar output so confidence scoring can discount it.
        let code = "Class Foo\n  Public Sub Bar()\n  End Sub\nEnd Class";
        let (symbols, _) = super::fallback_extract_vb_for_test(Path::new("foo.vb"), code);
        assert!(!symbols.is_empty());
        for s in &symbols {
            assert_eq!(
                s.metadata
                    .as_ref()
                    .and_then(|m| m.get("extraction_fallback"))
                    .map(String::as_str),
                Some("true"),
                "symbol {} missing extraction_fallback tag",
                s.name
            );
        }
    }

    #[test]
    fn enrich_marks_dynamic_sql_on_sidecar_path() {
        let code = "Class D
  Sub Run()
    cmd.CommandText = dynQry
  End Sub
End Class";
        let mut symbols = vec![crate::parsing::ExtractedSymbol {
            name: "D.Run".to_string(),
            kind: "function".to_string(),
            start_line: 2,
            end_line: 4,
            metadata: None,
        }];
        let mut edges = Vec::new();
        super::enrich_vb_source_for_test(code, &mut symbols, &mut edges);
        let dyn_edge = edges
            .iter()
            .find(|e| e.target_name == "sql:dynamic:dynQry")
            .expect("dynamic sql edge from enrichment");
        assert_eq!(dyn_edge.source_name, "D.Run", "attributed to enclosing fn");
        assert!(
            dyn_edge
                .metadata
                .as_ref()
                .is_some_and(|m| m.get("dynamic").is_some_and(|v| v == "true"))
        );
    }

    #[test]
    fn dynamic_sql_is_marked_not_dropped() {
        let code = "Class D
  Sub Run()
    Dim cmd As New SqlCommand(strSql)
    cmd.CommandText = dynamicQuery
  End Sub
End Class";
        let (_, edges) = super::fallback_extract_vb_for_test(Path::new("d.vb"), code);
        let dyn_edges: Vec<_> = edges
            .iter()
            .filter(|e| {
                e.kind == "sql_calls"
                    && e.metadata
                        .as_ref()
                        .is_some_and(|m| m.get("dynamic").is_some_and(|v| v == "true"))
            })
            .collect();
        assert_eq!(
            dyn_edges.len(),
            2,
            "both variable-SQL sites must emit: {edges:?}"
        );
        assert!(
            dyn_edges
                .iter()
                .any(|e| e.target_name == "sql:dynamic:strSql")
        );
        assert!(
            dyn_edges
                .iter()
                .any(|e| e.target_name == "sql:dynamic:dynamicQuery")
        );
    }

    #[test]
    fn fallback_records_arity_from_signature() {
        let code = "Class Foo
  Public Sub NoArgs()
  End Sub
  Public Function TwoArgs(ByVal a As Integer, b As String) As Boolean
  End Function
  Public Sub Defaulted(Optional ByVal x As List(Of String) = Nothing)
  End Sub
End Class";
        let (symbols, _) = super::fallback_extract_vb_for_test(Path::new("a.vb"), code);
        let arity_of = |name: &str| -> Option<String> {
            symbols
                .iter()
                .find(|s| s.name.contains(name))
                .and_then(|s| s.metadata.as_ref())
                .and_then(|m| m.get("arity"))
                .cloned()
        };
        assert_eq!(arity_of("NoArgs").as_deref(), Some("0"));
        assert_eq!(arity_of("TwoArgs").as_deref(), Some("2"));
        assert_eq!(
            arity_of("Defaulted").as_deref(),
            Some("1"),
            "nested parens (List(Of String)) must not inflate the count"
        );
    }

    #[test]
    fn indexes_partial_class_without_modifier() {
        let code = "Partial Class Foo\n  Public Sub Bar()\n  End Sub\nEnd Class";
        let (symbols, _) = extract_vb(Path::new("foo.vb"), code);
        assert!(symbols.iter().any(|s| s.name.contains("Foo")));
        assert!(symbols.iter().any(|s| s.name.contains("Bar")));
    }

    #[test]
    fn indexes_async_function() {
        let code =
            "Class Foo\n  Async Function Bar() As Task(Of String)\n  End Function\nEnd Class";
        let (symbols, _) = extract_vb(Path::new("foo.vb"), code);
        let bar = symbols.iter().find(|s| s.name.contains("Bar")).unwrap();
        let md = bar.metadata.as_ref().unwrap();
        assert_eq!(md.get("async").map(String::as_str), Some("true"));
    }

    #[test]
    fn parses_interpolation_and_null_conditional() {
        let code = "Class Foo\n Sub Bar()\n   Dim x = app?.Name\n   Dim s = $\"Hello {name}\"\n End Sub\nEnd Class";
        let (symbols, _) = extract_vb(Path::new("foo.vb"), code);
        assert!(symbols.iter().any(|s| s.name.contains("Bar")));
    }

    #[test]
    fn fallback_backfills_method_end_line() {
        // Degraded mode must give a real body range, not start==end (else
        // get_method_edit_context shows a 1-line VB method).
        let code = "Module M\n  Public Sub Bar()\n    Dim x = 1\n    x = 2\n  End Sub\nEnd Module";
        let (symbols, _) = super::fallback_extract_vb_for_test(Path::new("m.vb"), code);
        let bar = symbols
            .iter()
            .find(|s| s.name.contains("Bar"))
            .expect("Bar extracted");
        assert_eq!(bar.start_line, 2, "start at Sub line");
        assert_eq!(bar.end_line, 5, "end at End Sub line");
        assert!(bar.end_line > bar.start_line);
    }

    #[test]
    fn sidecar_parse_smoke_when_enabled() {
        if std::env::var("ENGRAM_VB_SIDECAR_TEST").ok().as_deref() != Some("1") {
            return;
        }
        let sidecar_path = super::sidecar_binary_path();
        if !sidecar_path.exists() {
            return;
        }

        let code = "Namespace App_Code\nPublic Class SharedFunc\nPublic Sub SafeRedirect()\nEnd Sub\nEnd Class\nEnd Namespace";
        let (symbols, edges) = extract_vb(Path::new("App_Code/SharedFunc.vb"), code);
        assert!(!symbols.is_empty(), "expected sidecar/fallback symbols");
        assert!(edges.iter().any(|e| e.kind == "contains"));
    }
}

#[cfg(test)]
mod linq_navigation_property_tests {
    //! Row-4 audit A5 (ingestion gap D8): a LINQ range-variable navigation
    //! chain `la.kk_kostnadskategorier.pr_id` inside a query clause is a
    //! read of that table — the extractor only saw `<ctx>.<Table>` members of
    //! a declared DataContext variable.
    use super::*;

    const SRC: &str = "Public Class leveransartiklar\n\
    Public Shared Function GetAll(projectId As Integer) As List(Of la_leveransartiklar)\n\
        Using db As New iCoreDataContext()\n\
            Dim q = From la In db.la_leveransartiklars\n\
                    Where la.kk_kostnadskategorier.pr_id = projectId\n\
                    Order By la.kk_kostnadskategorier.kk_ordning, la.la_ordning\n\
                    Select la\n\
            Return q.ToList()\n\
        End Using\n\
    End Function\n\
End Class\n";

    fn queries_table_edges(src: &str) -> Vec<ExtractedEdge> {
        let (_symbols, edges) = extract_vb_fallback_for_eval(
            Path::new("Site/App_Code/leverans/code/leveransartiklar.vb"),
            src,
        );
        edges
            .into_iter()
            .filter(|e| e.kind == "queries_table")
            .collect()
    }

    fn targets(src: &str) -> Vec<String> {
        let mut t: Vec<String> = queries_table_edges(src)
            .iter()
            .map(|e| e.target_name.to_lowercase())
            .collect();
        t.sort();
        t.dedup();
        t
    }

    #[test]
    fn navigation_property_reads_become_queries_table_edges() {
        let t = targets(SRC);
        assert!(
            t.iter().any(|x| x == "la_leveransartiklars"),
            "the FROM table (ctx member) must still be an edge: {t:?}"
        );
        assert!(
            t.iter().any(|x| x == "kk_kostnadskategorier"),
            "the navigation-property table must be an edge too: {t:?}"
        );
    }

    #[test]
    fn navigation_edges_carry_the_nav_marker_and_a_read_access() {
        let edges = queries_table_edges(SRC);
        let nav = edges
            .iter()
            .find(|e| e.target_name.eq_ignore_ascii_case("kk_kostnadskategorier"))
            .expect("nav edge");
        let meta = nav.metadata.as_ref().expect("metadata");
        assert_eq!(meta.get("orm").map(String::as_str), Some("nav"));
        assert_eq!(meta.get("access").map(String::as_str), Some("read"));
        assert!(nav.source_name.ends_with("GetAll"), "{}", nav.source_name);
    }

    #[test]
    fn ordinary_member_access_is_not_a_table() {
        // `item.statusId`, `s.SetOK(qry.params)` — no table-shaped member in
        // a query clause.
        let src = "Public Class api\n    Public Function f() As String\n        Dim v = item.statusId\n        s.SetOK(qry.params)\n        Return v\n    End Function\nEnd Class\n";
        assert!(targets(src).is_empty(), "{:?}", targets(src));
    }
}

/// Fresh source request over the serialized, timeout-protected sidecar pipe.
/// An old/missing sidecar is unavailable, never a fallback path analysis.
pub fn vb_return_paths(path: &Path, source: &str, start: u32, end: u32) -> Result<serde_json::Value, String> {
    if source.len() > 2_000_000 || start == 0 || end < start {
        return Err("return-path source/bounds exceed supported limits".into());
    }
    let request_id = format!("{}:{start}:{end}", blake3::hash(source.as_bytes()).to_hex());
    let payload = serde_json::json!({"cmd":"return_paths", "path":path.display().to_string(),
        "source":source,"method_start_line":start,"method_end_line":end,"request_id":request_id}).to_string();
    let mut guard = get_or_spawn_sidecar().lock().map_err(|_| "sidecar mutex poisoned")?;
    let sidecar = ensure_sidecar(&mut guard).map_err(|e| format!("return-path sidecar unavailable: {e}"))?;
    let line = match source_request_via_sidecar(sidecar,path,source,payload) {
        Ok(line) => line,
        Err(error) => {
            let _ = sidecar.stop();
            *guard = None;
            return Err(format!("return-path transport failed: {error:?}"));
        }
    };
    if line.len() > 256 * 1024 { return Err("return-path response exceeds 256 KiB".into()); }
    let response: serde_json::Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if let Some(error) = response.get("error").and_then(|v| v.as_str()) { return Err(error.to_owned()); }
    let result = response.get("return_paths").ok_or("return-path response missing")?;
    // An echoed request id alone cannot detect console transcoding of source.
    // Bind evidence to the bytes independently hashed by the sidecar.
    if !return_path_source_matches(result, source) {
        return Err("return-path response source fingerprint mismatch".into());
    }
    if result.get("request_id").and_then(|v|v.as_str()) != Some(request_id.as_str())
        || result.get("version").and_then(|v|v.as_str()) != Some("vb-return-paths-v1")
        || result.get("method_start_line").and_then(|v|v.as_u64()) != Some(start as u64)
        || result.get("method_end_line").and_then(|v|v.as_u64()) != Some(end as u64) {
        return Err("return-path response identity mismatch".into());
    }
    Ok(result.clone())
}

/// Parse the supplied VB source bytes fresh and return bounded Roslyn
/// invocation syntax. This never falls back to lexical extraction.
pub fn vb_invocations(path: &Path, source: &str) -> Result<VbInvocationReport, String> {
    if source.len() > 2_000_000 {
        return Err("invocation source exceeds the 2,000,000-byte transport cap".into());
    }
    let request_id = format!("invocations:{}", blake3::hash(source.as_bytes()).to_hex());
    let payload = serde_json::json!({
        "cmd": "invocations",
        "path": path.display().to_string(),
        "source": source,
        "request_id": request_id,
    })
    .to_string();
    let mut guard = get_or_spawn_sidecar()
        .lock()
        .map_err(|_| "sidecar mutex poisoned")?;
    let sidecar = ensure_sidecar(&mut guard)
        .map_err(|error| format!("invocation sidecar unavailable: {error}"))?;
    let line = match source_request_via_sidecar(sidecar, path, source, payload) {
        Ok(line) => line,
        Err(error) => {
            let _ = sidecar.stop();
            *guard = None;
            return Err(format!("invocation transport failed: {error:?}"));
        }
    };
    if line.len() > 512 * 1024 {
        return Err("invocation response exceeds the 512-KiB response cap".into());
    }
    let response: SidecarResponse = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if let Some(error) = response.error {
        return Err(error);
    }
    let expected_path = path.display().to_string();
    if response.path.as_deref() != Some(expected_path.as_str()) {
        return Err("invocation response path mismatch".into());
    }
    let report = response
        .invocation_report
        .ok_or("invocation response missing report")?;
    if report.version != "vb-invocations-v1"
        || report.request_id.as_deref() != Some(request_id.as_str())
        || report.scope != "full_source"
        || report.source_count != 1
        || !invocation_source_matches(&report, source)
    {
        return Err("invocation response source identity mismatch".into());
    }
    for invocation in &report.invocations {
        let invocation_end = invocation
            .span_start
            .checked_add(invocation.span_length)
            .ok_or("invocation span overflow")?;
        let callee_end = invocation
            .callee_span_start
            .checked_add(invocation.callee_span_length)
            .ok_or("invocation callee span overflow")?;
        if invocation.source_index != 0
            || invocation.callee_span_start < invocation.span_start
            || callee_end > invocation_end
            || vb_utf16_span_text(
                source,
                invocation.callee_span_start,
                invocation.callee_span_length,
            )
            .is_none()
        {
            return Err("invocation response contains an invalid callee/source span".into());
        }
        for argument in &invocation.arguments {
            let argument_end = argument
                .span_start
                .checked_add(argument.span_length)
                .ok_or("invocation argument span overflow")?;
            if argument.source_index != 0
                || argument.span_start < invocation.span_start
                || argument_end > invocation_end
                || vb_utf16_span_text(source, argument.span_start, argument.span_length).is_none()
            {
                return Err("invocation response contains an invalid argument/source span".into());
            }
        }
    }
    Ok(report)
}

/// Slice a Roslyn UTF-16 span from the exact source whose fingerprint was
/// verified by [`vb_invocations`]. Returns `None` for malformed boundaries.
pub fn vb_utf16_span_text(source: &str, start: u32, length: u32) -> Option<&str> {
    let start = start as usize;
    let end = start.checked_add(length as usize)?;
    let mut utf16 = 0_usize;
    let mut start_byte = None;
    let mut end_byte = None;
    for (byte, character) in source.char_indices() {
        if utf16 == start {
            start_byte = Some(byte);
        }
        if utf16 == end {
            end_byte = Some(byte);
            break;
        }
        utf16 += character.len_utf16();
        if (utf16 > start && start_byte.is_none()) || utf16 > end {
            return None;
        }
    }
    if utf16 == start && start_byte.is_none() {
        start_byte = Some(source.len());
    }
    if utf16 == end && end_byte.is_none() {
        end_byte = Some(source.len());
    }
    source.get(start_byte?..end_byte?)
}

fn invocation_source_matches(report: &VbInvocationReport, source: &str) -> bool {
    use sha2::{Digest, Sha256};
    let expected = format!("{:x}", Sha256::digest(source.as_bytes()));
    report.source_sha256 == expected
}

fn return_path_source_matches(result: &serde_json::Value, source: &str) -> bool {
    use sha2::{Digest, Sha256};
    let expected = format!("{:x}", Sha256::digest(source.as_bytes()));
    result.get("source_sha256").and_then(|v| v.as_str()) == Some(expected.as_str())
}

#[cfg(test)]
mod return_path_source_identity_tests {
    use super::return_path_source_matches;
    use sha2::{Digest, Sha256};

    #[test]
    fn echoed_request_id_does_not_validate_transcoded_or_changed_source() {
        let source = "\u{feff}Class Sample\r\n' åäö 日本語\r\nEnd Class\r\n";
        let mut result = serde_json::json!({
            "request_id": "unchanged-echo",
            "source_sha256": format!("{:x}", Sha256::digest(source.as_bytes()))
        });
        assert!(return_path_source_matches(&result, source));
        assert!(!return_path_source_matches(&result, source.trim_start_matches('\u{feff}')));
        assert!(!return_path_source_matches(&result, &source.replace("åäö", "???")));
        assert!(!return_path_source_matches(&result, &source.replace("\r\n", "\n")));
        for invalid in [serde_json::Value::Null, serde_json::json!(42), serde_json::json!("0000")] {
            result["source_sha256"] = invalid;
            assert!(!return_path_source_matches(&result, source));
        }
    }
}
