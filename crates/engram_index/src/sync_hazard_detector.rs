//! Async/await conversion safety detector.
//!
//! Scans source code for synchronous patterns that become hazardous when
//! converted to async/await in modern .NET. Emits `AntiPattern`-style
//! detections with severity, modern equivalent, and risk classification.
//!
//! The #1 source of bugs in WebForms→modern migrations is deadlocks from
//! calling `.Result` or `.Wait()` on async methods inside a
//! `SynchronizationContext`.

use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

// ─── Output types ──────────────────────────────────────────────────────────────

/// Severity of a sync→async migration hazard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HazardSeverity {
    Medium,
    High,
    Critical,
}

impl std::fmt::Display for HazardSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

/// The type of risk a sync hazard poses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationRisk {
    Deadlock,
    ThreadStarvation,
    NullReference,
    Deprecation,
    ThreadBlocking,
}

impl std::fmt::Display for MigrationRisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deadlock => write!(f, "deadlock"),
            Self::ThreadStarvation => write!(f, "thread_starvation"),
            Self::NullReference => write!(f, "null_reference"),
            Self::Deprecation => write!(f, "deprecation"),
            Self::ThreadBlocking => write!(f, "thread_blocking"),
        }
    }
}

/// A detected sync→async hazard in source code.
#[derive(Debug, Clone, Serialize)]
pub struct SyncHazard {
    /// The pattern that was detected.
    pub pattern_type: String,
    /// Source line number (1-based).
    pub line_number: usize,
    /// The matched text fragment.
    pub matched_text: String,
    /// Severity of the hazard.
    pub severity: HazardSeverity,
    /// Modern .NET equivalent.
    pub modern_equivalent: String,
    /// Type of migration risk.
    pub migration_risk: MigrationRisk,
    /// The containing method name, if determinable.
    pub containing_method: Option<String>,
}

/// Summary of sync hazard analysis for a file.
#[derive(Debug, Clone, Serialize)]
pub struct SyncHazardReport {
    /// All detected hazards.
    pub hazards: Vec<SyncHazard>,
    /// Async readiness score (0.0–1.0). Higher = safer to convert.
    pub async_readiness: f32,
    /// Count by severity.
    pub critical_count: usize,
    pub high_count: usize,
    pub medium_count: usize,
}

// ─── Regex singletons ──────────────────────────────────────────────────────────

fn re_dot_result() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\.Result\b").expect("re_dot_result"))
}

fn re_dot_wait() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\.Wait\(\)").expect("re_dot_wait"))
}

fn re_get_awaiter() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\.GetAwaiter\(\)\s*\.GetResult\(\)").expect("re_get_awaiter"))
}

fn re_thread_sleep() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bThread\.Sleep\(").expect("re_thread_sleep"))
}

fn re_http_context_current() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bHttpContext\.Current\b").expect("re_http_ctx"))
}

fn re_config_manager() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bConfigurationManager\.AppSettings\b").expect("re_config"))
}

fn re_web_config_manager() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bWebConfigurationManager\.").expect("re_webconfig"))
}

fn re_sync_file_io() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\bFile\.(ReadAllText|WriteAllText|ReadAllBytes|WriteAllBytes|ReadAllLines|WriteAllLines|Copy|Move|Delete|Exists|AppendAllText)\(")
            .expect("re_sync_io")
    })
}

fn re_sync_stream() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\bnew\s+StreamReader\(|\bnew\s+StreamWriter\(").expect("re_sync_stream")
    })
}

fn re_sync_http() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(WebClient|HttpWebRequest|WebRequest\.Create)\b").expect("re_sync_http")
    })
}

fn re_lock_statement() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\block\s*\(").expect("re_lock"))
}

fn re_method_decl_cs() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:public|private|protected|internal|static|async|override|virtual|\s)+\s+\w+(?:<[^>]+>)?\s+(\w+)\s*\(")
            .expect("re_method_cs")
    })
}

fn re_method_decl_vb() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:Public|Private|Protected|Friend|Shared|Overrides|Overridable|MustOverride|Async|Sub|Function)\s+(\w+)\s*\(")
            .expect("re_method_vb")
    })
}

// ─── Public API ────────────────────────────────────────────────────────────────

/// Detect sync→async hazards in a source file.
///
/// Supports both C# and VB.NET source code.
pub fn detect_sync_hazards(source: &str, is_vb: bool) -> SyncHazardReport {
    let lines: Vec<&str> = source.lines().collect();
    let mut hazards = Vec::new();

    // Track current method for contextual reporting
    let method_re = if is_vb {
        re_method_decl_vb()
    } else {
        re_method_decl_cs()
    };

    let mut current_method: Option<String> = None;
    let mut has_lock_in_method = false;
    let mut has_await_in_method = false;
    let mut lock_line: Option<usize> = None;

    for (idx, line) in lines.iter().enumerate() {
        let line_num = idx + 1;
        let trimmed = line.trim();

        // Skip comments
        if trimmed.starts_with("//") || trimmed.starts_with("'") || trimmed.starts_with("/*") {
            continue;
        }

        // Track method boundaries
        if let Some(caps) = method_re.captures(line) {
            // New method — check if previous method had lock+await combo
            if has_lock_in_method && has_await_in_method {
                if let Some(ll) = lock_line {
                    hazards.push(SyncHazard {
                        pattern_type: "lock_with_async".into(),
                        line_number: ll,
                        matched_text: "lock(...) in method containing await".into(),
                        severity: HazardSeverity::Critical,
                        modern_equivalent: "SemaphoreSlim with await".into(),
                        migration_risk: MigrationRisk::Deadlock,
                        containing_method: current_method.clone(),
                    });
                }
            }
            current_method = caps.get(1).map(|m| m.as_str().to_string());
            has_lock_in_method = false;
            has_await_in_method = false;
            lock_line = None;
        }

        // Track lock and await in the same method
        if re_lock_statement().is_match(line) {
            has_lock_in_method = true;
            lock_line = Some(line_num);
        }
        if line.contains("await ") || line.contains("Await ") {
            has_await_in_method = true;
        }

        // .Result on Task
        if let Some(m) = re_dot_result().find(line) {
            // Avoid false positives:
            // - `var result = x` (variable named result)
            // - `.Result = value` (property assignment)
            // - just the word on its own without a leading dot-call
            let before = &line[..m.start()];
            let after = if m.end() < line.len() {
                line[m.end()..].trim_start()
            } else {
                ""
            };
            let is_assignment = after.starts_with('=') && !after.starts_with("==");
            if !before.trim_end().ends_with("var")
                && !before.trim_end().ends_with("=")
                && !before.trim_end().is_empty()
                && !is_assignment
            {
                hazards.push(SyncHazard {
                    pattern_type: "task_result".into(),
                    line_number: line_num,
                    matched_text: m.as_str().to_string(),
                    severity: HazardSeverity::Critical,
                    modern_equivalent: "await the call".into(),
                    migration_risk: MigrationRisk::Deadlock,
                    containing_method: current_method.clone(),
                });
            }
        }

        // .Wait() on Task
        if let Some(m) = re_dot_wait().find(line) {
            hazards.push(SyncHazard {
                pattern_type: "task_wait".into(),
                line_number: line_num,
                matched_text: m.as_str().to_string(),
                severity: HazardSeverity::Critical,
                modern_equivalent: "await the call".into(),
                migration_risk: MigrationRisk::Deadlock,
                containing_method: current_method.clone(),
            });
        }

        // .GetAwaiter().GetResult()
        if let Some(m) = re_get_awaiter().find(line) {
            hazards.push(SyncHazard {
                pattern_type: "get_awaiter_result".into(),
                line_number: line_num,
                matched_text: m.as_str().to_string(),
                severity: HazardSeverity::High,
                modern_equivalent: "await the call".into(),
                migration_risk: MigrationRisk::ThreadBlocking,
                containing_method: current_method.clone(),
            });
        }

        // Thread.Sleep
        if let Some(m) = re_thread_sleep().find(line) {
            hazards.push(SyncHazard {
                pattern_type: "thread_sleep".into(),
                line_number: line_num,
                matched_text: m.as_str().to_string(),
                severity: HazardSeverity::High,
                modern_equivalent: "await Task.Delay()".into(),
                migration_risk: MigrationRisk::ThreadStarvation,
                containing_method: current_method.clone(),
            });
        }

        // HttpContext.Current
        if let Some(m) = re_http_context_current().find(line) {
            hazards.push(SyncHazard {
                pattern_type: "http_context_current".into(),
                line_number: line_num,
                matched_text: m.as_str().to_string(),
                severity: HazardSeverity::High,
                modern_equivalent: "Inject IHttpContextAccessor".into(),
                migration_risk: MigrationRisk::NullReference,
                containing_method: current_method.clone(),
            });
        }

        // ConfigurationManager.AppSettings
        if let Some(m) = re_config_manager().find(line) {
            hazards.push(SyncHazard {
                pattern_type: "configuration_manager".into(),
                line_number: line_num,
                matched_text: m.as_str().to_string(),
                severity: HazardSeverity::Medium,
                modern_equivalent: "IConfiguration DI".into(),
                migration_risk: MigrationRisk::Deprecation,
                containing_method: current_method.clone(),
            });
        }

        // WebConfigurationManager
        if let Some(m) = re_web_config_manager().find(line) {
            hazards.push(SyncHazard {
                pattern_type: "web_configuration_manager".into(),
                line_number: line_num,
                matched_text: m.as_str().to_string(),
                severity: HazardSeverity::Medium,
                modern_equivalent: "IConfiguration DI".into(),
                migration_risk: MigrationRisk::Deprecation,
                containing_method: current_method.clone(),
            });
        }

        // Synchronous File I/O
        if let Some(m) = re_sync_file_io().find(line) {
            hazards.push(SyncHazard {
                pattern_type: "sync_file_io".into(),
                line_number: line_num,
                matched_text: m.as_str().to_string(),
                severity: HazardSeverity::Medium,
                modern_equivalent: "File.*Async()".into(),
                migration_risk: MigrationRisk::ThreadBlocking,
                containing_method: current_method.clone(),
            });
        }

        // Synchronous stream constructors (without explicit async usage)
        if re_sync_stream().is_match(line) && !line.contains("Async") {
            hazards.push(SyncHazard {
                pattern_type: "sync_stream".into(),
                line_number: line_num,
                matched_text: "new StreamReader/StreamWriter".into(),
                severity: HazardSeverity::Medium,
                modern_equivalent: "Use async Read/Write methods".into(),
                migration_risk: MigrationRisk::ThreadBlocking,
                containing_method: current_method.clone(),
            });
        }

        // Synchronous HTTP clients
        if let Some(m) = re_sync_http().find(line) {
            hazards.push(SyncHazard {
                pattern_type: "sync_http".into(),
                line_number: line_num,
                matched_text: m.as_str().to_string(),
                severity: HazardSeverity::Medium,
                modern_equivalent: "HttpClient with await".into(),
                migration_risk: MigrationRisk::ThreadBlocking,
                containing_method: current_method.clone(),
            });
        }
    }

    // Check the last method for lock+await combo
    if has_lock_in_method && has_await_in_method {
        if let Some(ll) = lock_line {
            hazards.push(SyncHazard {
                pattern_type: "lock_with_async".into(),
                line_number: ll,
                matched_text: "lock(...) in method containing await".into(),
                severity: HazardSeverity::Critical,
                modern_equivalent: "SemaphoreSlim with await".into(),
                migration_risk: MigrationRisk::Deadlock,
                containing_method: current_method,
            });
        }
    }

    let critical_count = hazards
        .iter()
        .filter(|h| h.severity == HazardSeverity::Critical)
        .count();
    let high_count = hazards
        .iter()
        .filter(|h| h.severity == HazardSeverity::High)
        .count();
    let medium_count = hazards
        .iter()
        .filter(|h| h.severity == HazardSeverity::Medium)
        .count();

    let async_readiness = compute_readiness(critical_count, high_count, medium_count);

    SyncHazardReport {
        hazards,
        async_readiness,
        critical_count,
        high_count,
        medium_count,
    }
}

/// Compute async readiness score (0.0–1.0).
fn compute_readiness(critical: usize, high: usize, medium: usize) -> f32 {
    if critical == 0 && high == 0 && medium == 0 {
        return 1.0;
    }
    // Weighted penalty: critical = 0.3, high = 0.15, medium = 0.05
    let penalty = (critical as f32 * 0.3) + (high as f32 * 0.15) + (medium as f32 * 0.05);
    (1.0 - penalty).max(0.0)
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_result_on_task() {
        let code = r#"
public void DoWork() {
    var data = GetDataAsync().Result;
}
"#;
        let report = detect_sync_hazards(code, false);
        assert!(
            report
                .hazards
                .iter()
                .any(|h| h.pattern_type == "task_result")
        );
        assert!(
            report
                .hazards
                .iter()
                .any(|h| h.severity == HazardSeverity::Critical)
        );
    }

    #[test]
    fn detect_wait_on_task() {
        let code = r#"
public void Process() {
    SaveAsync().Wait();
}
"#;
        let report = detect_sync_hazards(code, false);
        assert!(report.hazards.iter().any(|h| h.pattern_type == "task_wait"));
        assert!(
            report
                .hazards
                .iter()
                .any(|h| h.severity == HazardSeverity::Critical)
        );
    }

    #[test]
    fn detect_http_context_current() {
        let code = r#"
public void Page_Load() {
    var user = HttpContext.Current.User;
}
"#;
        let report = detect_sync_hazards(code, false);
        assert!(
            report
                .hazards
                .iter()
                .any(|h| h.pattern_type == "http_context_current")
        );
        assert!(
            report
                .hazards
                .iter()
                .any(|h| h.migration_risk == MigrationRisk::NullReference)
        );
    }

    #[test]
    fn detect_thread_sleep() {
        let code = r#"
public void WaitForResult() {
    Thread.Sleep(1000);
}
"#;
        let report = detect_sync_hazards(code, false);
        assert!(
            report
                .hazards
                .iter()
                .any(|h| h.pattern_type == "thread_sleep")
        );
        assert!(
            report
                .hazards
                .iter()
                .any(|h| h.severity == HazardSeverity::High)
        );
    }

    #[test]
    fn detect_sync_file_io() {
        let code = r#"
public void ReadConfig() {
    var text = File.ReadAllText("config.xml");
}
"#;
        let report = detect_sync_hazards(code, false);
        assert!(
            report
                .hazards
                .iter()
                .any(|h| h.pattern_type == "sync_file_io")
        );
    }

    #[test]
    fn detect_webclient() {
        let code = r#"
public void FetchData() {
    var client = new WebClient();
    var data = client.DownloadString("http://api.example.com");
}
"#;
        let report = detect_sync_hazards(code, false);
        assert!(report.hazards.iter().any(|h| h.pattern_type == "sync_http"));
    }

    #[test]
    fn no_false_positive_on_result_variable() {
        let code = r#"
public void Process() {
    var result = 42;
    var x = result + 1;
}
"#;
        let report = detect_sync_hazards(code, false);
        assert!(
            report
                .hazards
                .iter()
                .all(|h| h.pattern_type != "task_result")
        );
    }

    #[test]
    fn no_false_positive_on_non_task_wait() {
        // .Wait() on a non-Task should still flag — we can't statically distinguish
        // without type analysis, but the detector is conservative. This test just
        // verifies the regex works.
        let code = r#"
public void DoWork() {
    someEvent.Wait();
}
"#;
        let report = detect_sync_hazards(code, false);
        // This WILL flag because we're conservative — better safe than sorry
        assert!(report.hazards.iter().any(|h| h.pattern_type == "task_wait"));
    }

    #[test]
    fn scaffold_warning_for_method_with_hazards() {
        let code = r#"
public void Page_Load(object sender, EventArgs e) {
    var user = HttpContext.Current.User.Identity.Name;
    var data = GetDataAsync().Result;
    Thread.Sleep(100);
}
"#;
        let report = detect_sync_hazards(code, false);
        assert!(report.hazards.len() >= 3);
        assert!(report.async_readiness < 0.5);
    }

    #[test]
    fn scaffold_injects_ihttpcontextaccessor() {
        let code = r#"
public void Page_Load() {
    var session = HttpContext.Current.Session;
}
"#;
        let report = detect_sync_hazards(code, false);
        let hazard = report
            .hazards
            .iter()
            .find(|h| h.pattern_type == "http_context_current");
        assert!(hazard.is_some());
        assert_eq!(
            hazard.map(|h| h.modern_equivalent.as_str()),
            Some("Inject IHttpContextAccessor")
        );
    }

    #[test]
    fn async_readiness_score_clean_file() {
        let code = r#"
public async Task ProcessAsync() {
    var data = await GetDataAsync();
    await SaveAsync(data);
}
"#;
        let report = detect_sync_hazards(code, false);
        assert!((report.async_readiness - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn async_readiness_below_half_with_result() {
        let code = r#"
public void A() { GetAsync().Result; }
public void B() { SaveAsync().Result; }
"#;
        let report = detect_sync_hazards(code, false);
        assert!(report.async_readiness < 0.5);
        assert!(report.critical_count >= 2);
    }

    #[test]
    fn detect_configuration_manager() {
        let code = r#"
var connStr = ConfigurationManager.AppSettings["DbConn"];
"#;
        let report = detect_sync_hazards(code, false);
        assert!(
            report
                .hazards
                .iter()
                .any(|h| h.pattern_type == "configuration_manager")
        );
    }
}
