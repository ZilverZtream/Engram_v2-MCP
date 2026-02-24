// Ticket 3: Page Lifecycle Mapping Service
//
// Extracts WebForms page lifecycle events (Page_Init, Page_Load, Page_PreRender, etc.)
// and control events from code-behind files. Maps each event to its modern equivalent
// in Blazor, React, or Angular with IsPostBack branch analysis.

use engram_graph::GraphStore;
use regex::Regex;
use serde::Serialize;
use std::sync::{Arc, LazyLock};

// ── Result structs ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct PageLifecycleMap {
    pub file_path: String,
    pub base_class: Option<String>,
    pub lifecycle_events: Vec<LifecycleEventMapping>,
    pub control_events: Vec<ControlEventMapping>,
    pub implicit_behaviors: Vec<ImplicitBehavior>,
    pub page_directives: PageDirectiveInfo,
    pub migration_notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LifecycleEventMapping {
    pub event_name: String,
    pub handler_name: String,
    pub has_ispostback_branch: bool,
    pub first_load_actions: Vec<String>,
    pub postback_actions: Vec<String>,
    pub always_actions: Vec<String>,
    pub modern_blazor: String,
    pub modern_react: String,
    pub modern_angular: String,
    pub migration_notes: Vec<String>,
    pub line_number: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ControlEventMapping {
    pub control_id: String,
    pub control_type: String,
    pub event_name: String,
    pub handler_name: String,
    pub is_postback_trigger: bool,
    pub modern_blazor: String,
    pub modern_react: String,
    pub line_number: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImplicitBehavior {
    pub behavior: String,
    pub webforms_mechanism: String,
    pub modern_replacement: String,
    pub severity: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageDirectiveInfo {
    pub enable_viewstate: Option<bool>,
    pub enable_session_state: Option<String>,
    pub enable_event_validation: Option<bool>,
    pub auto_event_wireup: Option<bool>,
    pub master_page_file: Option<String>,
    pub inherits: Option<String>,
    pub codebehind: Option<String>,
}

// ── Lifecycle event definitions ───────────────────────────────────────────

const LIFECYCLE_EVENTS: &[(&str, &str, &str, &str)] = &[
    // (event_name, blazor_equivalent, react_equivalent, angular_equivalent)
    (
        "Page_PreInit",
        "// No direct equivalent — use constructor or OnInitialized for early init",
        "constructor()",
        "constructor()",
    ),
    (
        "Page_Init",
        "OnInitialized() / OnInitializedAsync()",
        "constructor() or useRef for one-time setup",
        "ngOnInit()",
    ),
    (
        "Page_InitComplete",
        "// Combine with OnInitialized — no separate InitComplete needed",
        "// No equivalent — combine with constructor",
        "// Combine with ngOnInit",
    ),
    (
        "Page_PreLoad",
        "// No equivalent — combine with OnParametersSet",
        "// No equivalent — combine with useEffect",
        "// Combine with ngOnInit",
    ),
    (
        "Page_Load",
        "OnParametersSetAsync() for param-dependent logic; OnInitializedAsync() for first-load logic",
        "useEffect(() => {}, [deps]) for load; useEffect(() => {}, []) for first-load",
        "ngOnInit() for first-load; ngOnChanges() for param changes",
    ),
    (
        "Page_LoadComplete",
        "OnAfterRender(firstRender: true) for post-load work",
        "useEffect cleanup/next-tick",
        "ngAfterViewInit()",
    ),
    (
        "Page_PreRender",
        "OnAfterRender(firstRender) / OnAfterRenderAsync()",
        "useLayoutEffect() or useMemo()",
        "ngAfterViewChecked()",
    ),
    (
        "Page_PreRenderComplete",
        "// Combine with OnAfterRender",
        "// Combine with useLayoutEffect",
        "// Combine with ngAfterViewChecked",
    ),
    (
        "Page_SaveStateComplete",
        "// No ViewState in Blazor — manage component state explicitly",
        "// No equivalent — React manages state automatically",
        "// No equivalent — Angular manages state via services",
    ),
    (
        "Page_Unload",
        "Dispose() via IDisposable / IAsyncDisposable",
        "useEffect cleanup: return () => { ... }",
        "ngOnDestroy()",
    ),
];

// ── Regex patterns ────────────────────────────────────────────────────────

// VB.NET lifecycle handlers
static RE_VB_LIFECYCLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:Private|Protected|Public)?\s*(?:Overrides\s+)?Sub\s+(Page_(?:PreInit|Init|InitComplete|PreLoad|Load|LoadComplete|PreRender|PreRenderComplete|SaveStateComplete|Unload))\s*\(([^)]*)\)")
        .expect("valid regex")
});

// C# lifecycle handlers
static RE_CS_LIFECYCLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:private|protected|public)?\s*(?:override\s+)?(?:async\s+)?(?:void|Task)\s+(Page_(?:PreInit|Init|InitComplete|PreLoad|Load|LoadComplete|PreRender|PreRenderComplete|SaveStateComplete|Unload))\s*\(([^)]*)\)")
        .expect("valid regex")
});

// VB.NET override On* methods
static RE_VB_OVERRIDE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:Protected\s+)?Overrides\s+Sub\s+On(Init|Load|PreRender|Unload|LoadComplete)\s*\(([^)]*)\)")
        .expect("valid regex")
});

// C# override On* methods
static RE_CS_OVERRIDE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:protected\s+)?override\s+(?:async\s+)?(?:void|Task)\s+On(Init|Load|PreRender|Unload|LoadComplete)\s*\(([^)]*)\)")
        .expect("valid regex")
});

// VB.NET Handles clause
static RE_VB_HANDLES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:Private|Protected)?\s*Sub\s+(\w+)\s*\([^)]*\)\s+Handles\s+(Me|MyBase)\.(Init|Load|PreRender|Unload)\b")
        .expect("valid regex")
});

// IsPostBack detection
static RE_IS_POSTBACK_CS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)if\s*\(\s*(!?\s*(?:Page\.)?IsPostBack)\s*\)").expect("valid regex"));

static RE_IS_POSTBACK_VB: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)If\s+(Not\s+)?(?:Page\.)?IsPostBack\b").expect("valid regex"));

// Control event handlers — VB
static RE_VB_CONTROL_EVENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:Private|Protected|Public)?\s*Sub\s+(\w+)\s*\(([^)]*)\)\s+Handles\s+(\w+)\.(Click|Command|TextChanged|SelectedIndexChanged|CheckedChanged|RowCommand|RowEditing|RowUpdating|RowDeleting|RowCancelingEdit|PageIndexChanging|Sorting|ItemCommand|ServerClick|ServerChange|ServerValidate|SelectedDateChanged|DayRender|VisibleMonthChanged)\b")
        .expect("valid regex")
});

// C# event handler pattern (naming convention: controlId_EventName)
static RE_CS_CONTROL_EVENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:protected|private|public)?\s*(?:async\s+)?void\s+(\w+)_(Click|Command|TextChanged|SelectedIndexChanged|CheckedChanged|RowCommand|RowEditing|RowUpdating|RowDeleting|RowCancelingEdit|PageIndexChanging|Sorting|ItemCommand|ServerClick|ServerChange|ServerValidate)\s*\(")
        .expect("valid regex")
});

// Page directive attributes
static RE_PAGE_DIRECTIVE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<%@\s+(?:Page|Control|Master)\b([^%]*)%>").expect("valid regex"));

/// Matches `<asp:SomeControl … ID="theId" …>` — used to enrich control types from markup.
static RE_ENRICH_CONTROL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)<asp:(\w+)\b[^>]*\bID\s*=\s*"([^"]*)""#).expect("valid regex"));

fn extract_directive_attr(tag: &str, attr: &str) -> Option<String> {
    let pattern = format!(r#"(?i){}\s*=\s*"([^"]*)""#, regex::escape(attr));
    Regex::new(&pattern)
        .ok()
        .and_then(|re| re.captures(tag))
        .map(|c| c[1].to_string())
}

// ── Main analysis function ────────────────────────────────────────────────

pub fn analyze_page_lifecycle(
    _graph: &Arc<GraphStore>,
    _project_id: &str,
    file_path: &str,
    codebehind_content: &str,
    aspx_content: Option<&str>,
) -> anyhow::Result<PageLifecycleMap> {
    let mut lifecycle_events = Vec::new();
    let mut control_events = Vec::new();
    let mut migration_notes = Vec::new();

    let is_vb = file_path.ends_with(".vb");

    // ── Parse page directives from ASPX ──

    let page_directives = if let Some(aspx) = aspx_content {
        parse_page_directives(aspx)
    } else {
        PageDirectiveInfo {
            enable_viewstate: None,
            enable_session_state: None,
            enable_event_validation: None,
            auto_event_wireup: None,
            master_page_file: None,
            inherits: None,
            codebehind: None,
        }
    };

    // ── Extract base class ──

    let base_class = extract_base_class(codebehind_content, is_vb);

    // ── Find lifecycle event handlers ──

    let lifecycle_re = if is_vb {
        &*RE_VB_LIFECYCLE
    } else {
        &*RE_CS_LIFECYCLE
    };

    for cap in lifecycle_re.captures_iter(codebehind_content) {
        let handler_name = cap[1].to_string();
        let line_num = find_line_number(codebehind_content, cap.get(0).expect("group 0 always present").start());

        let body = extract_method_body(codebehind_content, cap.get(0).expect("group 0 always present").start(), is_vb);
        let (has_postback, first_load, postback_actions, always) =
            analyze_postback_branching(&body, is_vb);

        let event_name = handler_name.clone();
        let (blazor, react, angular, notes) =
            map_lifecycle_event(&event_name, has_postback, &first_load, &postback_actions);

        lifecycle_events.push(LifecycleEventMapping {
            event_name: event_name.clone(),
            handler_name,
            has_ispostback_branch: has_postback,
            first_load_actions: first_load,
            postback_actions,
            always_actions: always,
            modern_blazor: blazor,
            modern_react: react,
            modern_angular: angular,
            migration_notes: notes,
            line_number: line_num,
        });
    }

    // Also check override On* methods
    let override_re = if is_vb {
        &*RE_VB_OVERRIDE
    } else {
        &*RE_CS_OVERRIDE
    };
    for cap in override_re.captures_iter(codebehind_content) {
        let event_suffix = &cap[1];
        let handler_name = format!("On{event_suffix}");
        let mapped_event = format!("Page_{event_suffix}");

        // Skip if we already have a Page_* handler for this event
        if lifecycle_events
            .iter()
            .any(|e| e.event_name == mapped_event)
        {
            continue;
        }

        let line_num = find_line_number(codebehind_content, cap.get(0).expect("group 0 always present").start());
        let body = extract_method_body(codebehind_content, cap.get(0).expect("group 0 always present").start(), is_vb);
        let (has_postback, first_load, postback_actions, always) =
            analyze_postback_branching(&body, is_vb);

        let (blazor, react, angular, notes) =
            map_lifecycle_event(&mapped_event, has_postback, &first_load, &postback_actions);

        lifecycle_events.push(LifecycleEventMapping {
            event_name: mapped_event,
            handler_name,
            has_ispostback_branch: has_postback,
            first_load_actions: first_load,
            postback_actions,
            always_actions: always,
            modern_blazor: blazor,
            modern_react: react,
            modern_angular: angular,
            migration_notes: notes,
            line_number: line_num,
        });
    }

    // VB Handles clause
    for cap in RE_VB_HANDLES.captures_iter(codebehind_content) {
        let handler_name = cap[1].to_string();
        let event_suffix = &cap[3];
        let mapped_event = format!("Page_{event_suffix}");

        if lifecycle_events
            .iter()
            .any(|e| e.event_name == mapped_event)
        {
            continue;
        }

        let line_num = find_line_number(codebehind_content, cap.get(0).expect("group 0 always present").start());
        let body = extract_method_body(codebehind_content, cap.get(0).expect("group 0 always present").start(), is_vb);
        let (has_postback, first_load, postback_actions, always) =
            analyze_postback_branching(&body, is_vb);

        let (blazor, react, angular, notes) =
            map_lifecycle_event(&mapped_event, has_postback, &first_load, &postback_actions);

        lifecycle_events.push(LifecycleEventMapping {
            event_name: mapped_event,
            handler_name,
            has_ispostback_branch: has_postback,
            first_load_actions: first_load,
            postback_actions,
            always_actions: always,
            modern_blazor: blazor,
            modern_react: react,
            modern_angular: angular,
            migration_notes: notes,
            line_number: line_num,
        });
    }

    // ── Find control event handlers ──

    if is_vb {
        for cap in RE_VB_CONTROL_EVENT.captures_iter(codebehind_content) {
            let handler_name = cap[1].to_string();
            let control_id = cap[3].to_string();
            let event_name = cap[4].to_string();
            let line_num = find_line_number(codebehind_content, cap.get(0).expect("group 0 always present").start());

            let (blazor, react) = map_control_event(&event_name, &control_id);
            let is_postback = is_postback_trigger_event(&event_name);

            control_events.push(ControlEventMapping {
                control_id,
                control_type: String::new(), // Filled from ASPX if available
                event_name,
                handler_name,
                is_postback_trigger: is_postback,
                modern_blazor: blazor,
                modern_react: react,
                line_number: line_num,
            });
        }
    } else {
        for cap in RE_CS_CONTROL_EVENT.captures_iter(codebehind_content) {
            let control_id = cap[1].to_string();
            let event_name = cap[2].to_string();
            let handler_name = format!("{}_{}", control_id, event_name);
            let line_num = find_line_number(codebehind_content, cap.get(0).expect("group 0 always present").start());

            let (blazor, react) = map_control_event(&event_name, &control_id);
            let is_postback = is_postback_trigger_event(&event_name);

            control_events.push(ControlEventMapping {
                control_id,
                control_type: String::new(),
                event_name,
                handler_name,
                is_postback_trigger: is_postback,
                modern_blazor: blazor,
                modern_react: react,
                line_number: line_num,
            });
        }
    }

    // Enrich control types from ASPX if available
    if let Some(aspx) = aspx_content {
        enrich_control_types(aspx, &mut control_events);
    }

    // ── Generate implicit behaviors ──

    let implicit_behaviors =
        build_implicit_behaviors(&lifecycle_events, &control_events, &page_directives);

    // ── Build migration notes ──

    if lifecycle_events.iter().any(|e| e.has_ispostback_branch) {
        migration_notes.push("Contains IsPostBack branching — carefully split first-load vs postback logic into separate modern lifecycle methods".to_string());
    }

    if lifecycle_events.len() > 3 {
        migration_notes.push(format!(
            "Uses {} lifecycle events — consider consolidating into fewer modern lifecycle methods",
            lifecycle_events.len()
        ));
    }

    let postback_count = control_events
        .iter()
        .filter(|e| e.is_postback_trigger)
        .count();
    if postback_count > 5 {
        migration_notes.push(format!(
            "{postback_count} postback-triggering events — each becomes an async operation in the modern stack"
        ));
    }

    Ok(PageLifecycleMap {
        file_path: file_path.to_string(),
        base_class,
        lifecycle_events,
        control_events,
        implicit_behaviors,
        page_directives,
        migration_notes,
    })
}

// ── Helper functions ──────────────────────────────────────────────────────

fn parse_page_directives(aspx: &str) -> PageDirectiveInfo {
    let cap = RE_PAGE_DIRECTIVE.captures(aspx);
    let tag = cap.map(|c| c[1].to_string()).unwrap_or_default();

    PageDirectiveInfo {
        enable_viewstate: extract_directive_attr(&tag, "EnableViewState")
            .map(|v| v.eq_ignore_ascii_case("true")),
        enable_session_state: extract_directive_attr(&tag, "EnableSessionState"),
        enable_event_validation: extract_directive_attr(&tag, "EnableEventValidation")
            .map(|v| v.eq_ignore_ascii_case("true")),
        auto_event_wireup: extract_directive_attr(&tag, "AutoEventWireup")
            .map(|v| v.eq_ignore_ascii_case("true")),
        master_page_file: extract_directive_attr(&tag, "MasterPageFile"),
        inherits: extract_directive_attr(&tag, "Inherits"),
        codebehind: extract_directive_attr(&tag, "CodeBehind")
            .or_else(|| extract_directive_attr(&tag, "CodeFile")),
    }
}

fn extract_base_class(content: &str, is_vb: bool) -> Option<String> {
    let re = if is_vb {
        Regex::new(r"(?im)^\s*(?:Partial\s+)?(?:Public\s+)?Class\s+\w+\s+Inherits\s+(\S+)").ok()?
    } else {
        Regex::new(r"(?im)^\s*(?:public\s+)?(?:partial\s+)?class\s+\w+\s*:\s*(\S+)").ok()?
    };
    re.captures(content)
        .map(|c| c[1].trim_end_matches(',').to_string())
}

fn find_line_number(content: &str, byte_offset: usize) -> usize {
    content[..byte_offset.min(content.len())]
        .chars()
        .filter(|&c| c == '\n')
        .count()
        + 1
}

fn extract_method_body(content: &str, start: usize, is_vb: bool) -> String {
    let remaining = &content[start..];

    if is_vb {
        // Find End Sub
        if let Some(end_pos) = remaining
            .find("End Sub")
            .or_else(|| remaining.find("End Function"))
        {
            return remaining[..end_pos + 7].to_string();
        }
    } else {
        // Find matching closing brace
        let mut depth = 0i32;
        let mut found_open = false;
        for (i, ch) in remaining.char_indices() {
            match ch {
                '{' => {
                    depth += 1;
                    found_open = true;
                }
                '}' => {
                    depth -= 1;
                    if found_open && depth == 0 {
                        return remaining[..i + 1].to_string();
                    }
                }
                _ => {}
            }
        }
    }

    // Fallback: return next 50 lines
    remaining.lines().take(50).collect::<Vec<_>>().join("\n")
}

fn analyze_postback_branching(
    body: &str,
    is_vb: bool,
) -> (bool, Vec<String>, Vec<String>, Vec<String>) {
    let has_postback = if is_vb {
        RE_IS_POSTBACK_VB.is_match(body)
    } else {
        RE_IS_POSTBACK_CS.is_match(body)
    };

    if !has_postback {
        let actions = extract_action_summaries(body);
        return (false, vec![], vec![], actions);
    }

    // Try to split into first-load vs postback sections
    let mut first_load = Vec::new();
    let mut postback = Vec::new();
    let mut always = Vec::new();

    // Simple heuristic: split at the IsPostBack check
    let postback_pos = if is_vb {
        RE_IS_POSTBACK_VB.find(body).map(|m| m.start())
    } else {
        RE_IS_POSTBACK_CS.find(body).map(|m| m.start())
    };

    if let Some(pos) = postback_pos {
        let before = &body[..pos];
        let after = &body[pos..];

        // Actions before the IsPostBack check are "always" actions
        always = extract_action_summaries(before);

        // Determine which branch is first-load vs postback
        let is_negated = if is_vb {
            RE_IS_POSTBACK_VB
                .captures(after)
                .map(|c| c.get(1).is_some())
                .unwrap_or(false)
        } else {
            RE_IS_POSTBACK_CS
                .captures(after)
                .map(|c| c[1].starts_with('!'))
                .unwrap_or(false)
        };

        // Extract the if-body and else-body
        let (if_body, else_body) = split_if_else(after, is_vb);

        if is_negated {
            // If Not IsPostBack Then ... first load is in the if branch
            first_load = extract_action_summaries(&if_body);
            postback = extract_action_summaries(&else_body);
        } else {
            // If IsPostBack Then ... postback is in the if branch
            postback = extract_action_summaries(&if_body);
            first_load = extract_action_summaries(&else_body);
        }
    }

    (has_postback, first_load, postback, always)
}

fn split_if_else(body: &str, is_vb: bool) -> (String, String) {
    if is_vb {
        // Find the Then ... Else ... End If structure
        if let Some(then_pos) = body.find("Then") {
            let after_then = &body[then_pos + 4..];
            if let Some(else_pos) = find_vb_else(after_then) {
                let if_body = after_then[..else_pos].to_string();
                let after_else = &after_then[else_pos + 4..]; // skip "Else"
                if let Some(end_pos) = after_else.find("End If") {
                    let else_body = after_else[..end_pos].to_string();
                    return (if_body, else_body);
                }
                return (if_body, after_else.to_string());
            }
            return (after_then.to_string(), String::new());
        }
    } else {
        // C# — find matching braces
        let mut depth = 0i32;
        let mut if_start = None;
        let mut if_end = None;
        let mut else_start = None;
        let mut else_end = None;

        for (i, ch) in body.char_indices() {
            match ch {
                '{' => {
                    depth += 1;
                    if depth == 1 && if_start.is_none() {
                        if_start = Some(i + 1);
                    }
                    if depth == 1 && else_start.is_some() && else_end.is_none() {
                        else_start = Some(i + 1);
                    }
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 && if_end.is_none() {
                        if_end = Some(i);
                    } else if depth == 0 && else_end.is_none() && else_start.is_some() {
                        else_end = Some(i);
                    }
                }
                _ => {}
            }
            if if_end.is_some()
                && else_start.is_none()
                && body[i..].trim_start().starts_with("else")
            {
                else_start = Some(i);
            }
        }

        let if_body = if let (Some(s), Some(e)) = (if_start, if_end) {
            body[s..e].to_string()
        } else {
            String::new()
        };
        let else_body = if let (Some(s), Some(e)) = (else_start, else_end) {
            body[s..e].to_string()
        } else {
            String::new()
        };
        return (if_body, else_body);
    }

    (String::new(), String::new())
}

fn find_vb_else(body: &str) -> Option<usize> {
    // Find "Else" at the same nesting level
    let mut depth = 0i32;
    for (i, line) in body.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("If ") && trimmed.contains(" Then") && !trimmed.ends_with("Then") {
            // Single-line If — don't track depth
        } else if trimmed.starts_with("If ") && trimmed.ends_with("Then") {
            depth += 1;
        } else if trimmed == "End If" {
            depth -= 1;
        } else if depth == 0 && (trimmed == "Else" || trimmed.starts_with("ElseIf ")) {
            // Calculate byte offset
            let offset: usize = body.lines().take(i).map(|l| l.len() + 1).sum();
            return Some(offset);
        }
    }
    None
}

fn extract_action_summaries(body: &str) -> Vec<String> {
    let mut actions = Vec::new();

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("'") || trimmed.starts_with("//") {
            continue;
        }

        // Data binding
        if trimmed.contains(".DataBind()") || trimmed.contains(".DataBind(") {
            actions.push(format!("DataBind: {}", summarize_line(trimmed)));
        }
        // SQL / data access
        else if trimmed.contains("SqlCommand")
            || trimmed.contains("SqlDataAdapter")
            || trimmed.contains(".ExecuteReader")
            || trimmed.contains(".ExecuteNonQuery")
            || trimmed.contains(".ExecuteScalar")
            || trimmed.contains(".Fill(")
        {
            actions.push(format!("SQL: {}", summarize_line(trimmed)));
        }
        // Control property assignment
        else if trimmed.contains(".DataSource")
            || trimmed.contains(".SelectedValue")
            || trimmed.contains(".SelectedIndex")
            || trimmed.contains(".Visible")
            || trimmed.contains(".Enabled")
            || trimmed.contains(".Text =")
        {
            actions.push(format!("UI: {}", summarize_line(trimmed)));
        }
        // Session/ViewState
        else if trimmed.contains("Session(")
            || trimmed.contains("Session[")
            || trimmed.contains("ViewState(")
            || trimmed.contains("ViewState[")
        {
            actions.push(format!("State: {}", summarize_line(trimmed)));
        }
        // Redirect
        else if trimmed.contains("Response.Redirect") || trimmed.contains("Server.Transfer") {
            actions.push(format!("Navigate: {}", summarize_line(trimmed)));
        }
    }

    actions
}

fn summarize_line(line: &str) -> String {
    let s = line.trim();
    if s.len() > 80 {
        format!("{}...", &s[..77])
    } else {
        s.to_string()
    }
}

fn map_lifecycle_event(
    event_name: &str,
    has_postback: bool,
    first_load: &[String],
    postback_actions: &[String],
) -> (String, String, String, Vec<String>) {
    let mut notes = Vec::new();

    let (blazor, react, angular) = LIFECYCLE_EVENTS
        .iter()
        .find(|(name, _, _, _)| name.eq_ignore_ascii_case(event_name))
        .map(|(_, b, r, a)| (b.to_string(), r.to_string(), a.to_string()))
        .unwrap_or_else(|| {
            (
                format!("// No mapping for {event_name}"),
                format!("// No mapping for {event_name}"),
                format!("// No mapping for {event_name}"),
            )
        });

    if has_postback && event_name.eq_ignore_ascii_case("Page_Load") {
        notes.push("Split !IsPostBack logic into OnInitializedAsync (runs once) and IsPostBack logic into event handlers".to_string());
        if !first_load.is_empty() {
            notes.push(format!(
                "First-load actions ({} items) → OnInitializedAsync()",
                first_load.len()
            ));
        }
        if !postback_actions.is_empty() {
            notes.push(format!(
                "Postback actions ({} items) → move to respective control event handlers",
                postback_actions.len()
            ));
        }
    }

    (blazor, react, angular, notes)
}

fn map_control_event(event_name: &str, _control_id: &str) -> (String, String) {
    match event_name.to_lowercase().as_str() {
        "click" | "serverclick" => (
            "@onclick=\"HandleClick\"".to_string(),
            "onClick={handleClick}".to_string(),
        ),
        "command" => (
            "@onclick=\"() => HandleCommand(commandArg)\"".to_string(),
            "onClick={() => handleCommand(arg)}".to_string(),
        ),
        "textchanged" | "serverchange" => (
            "@onchange=\"HandleChange\" or @bind-Value".to_string(),
            "onChange={handleChange}".to_string(),
        ),
        "selectedindexchanged" => (
            "@onchange or @bind-Value on <select>".to_string(),
            "onChange={handleSelect}".to_string(),
        ),
        "checkedchanged" => (
            "@bind-Value on checkbox".to_string(),
            "onChange={handleCheck}".to_string(),
        ),
        "rowcommand" => (
            "Per-row button @onclick in DataGrid row template".to_string(),
            "onClick handler per row in table/grid".to_string(),
        ),
        "rowediting" | "rowupdating" | "rowdeleting" | "rowcancelingedit" => (
            "Inline editing state management in DataGrid component".to_string(),
            "State-based edit mode per row".to_string(),
        ),
        "pageindexchanging" => (
            "Pagination component callback".to_string(),
            "Page change handler / pagination component".to_string(),
        ),
        "sorting" => (
            "SortBy parameter on QuickGrid or custom sort handler".to_string(),
            "Sort handler / column header click".to_string(),
        ),
        "servervalidate" => (
            "Custom validation logic in FluentValidation or EditContext".to_string(),
            "Custom validation in form handler".to_string(),
        ),
        _ => (
            format!("// Map {event_name} to appropriate Blazor event"),
            format!("// Map {event_name} to appropriate React handler"),
        ),
    }
}

fn is_postback_trigger_event(event_name: &str) -> bool {
    matches!(
        event_name.to_lowercase().as_str(),
        "click"
            | "command"
            | "serverclick"
            | "selectedindexchanged"
            | "textchanged"
            | "checkedchanged"
            | "rowcommand"
            | "pageindexchanging"
            | "sorting"
    )
}

fn enrich_control_types(aspx: &str, events: &mut [ControlEventMapping]) {
    let mut type_map = std::collections::HashMap::new();
    for cap in RE_ENRICH_CONTROL.captures_iter(aspx) {
        type_map.insert(cap[2].to_string(), cap[1].to_string());
    }
    for event in events.iter_mut() {
        if let Some(ctype) = type_map.get(&event.control_id) {
            event.control_type = ctype.clone();
        }
    }
}

fn build_implicit_behaviors(
    _lifecycle: &[LifecycleEventMapping],
    control_events: &[ControlEventMapping],
    directives: &PageDirectiveInfo,
) -> Vec<ImplicitBehavior> {
    let mut behaviors = Vec::new();

    // ViewState-related implicit behaviors
    let viewstate_disabled = directives.enable_viewstate == Some(false);
    if !viewstate_disabled {
        behaviors.push(ImplicitBehavior {
            behavior: "Control state automatically persisted across postbacks via ViewState".to_string(),
            webforms_mechanism: "ViewState serialization in hidden __VIEWSTATE field".to_string(),
            modern_replacement: "Explicit component state: private fields, @bind, or state management (Redux/Zustand/Flux)".to_string(),
            severity: "High".to_string(),
        });
    }

    // Event validation
    if directives.enable_event_validation != Some(false) {
        behaviors.push(ImplicitBehavior {
            behavior: "Event validation prevents tampering with postback event targets".to_string(),
            webforms_mechanism: "__EVENTVALIDATION hidden field".to_string(),
            modern_replacement:
                "Anti-forgery tokens: @Html.AntiForgeryToken() or [ValidateAntiForgeryToken]"
                    .to_string(),
            severity: "Medium".to_string(),
        });
    }

    // Auto event wireup
    if directives.auto_event_wireup != Some(false) {
        behaviors.push(ImplicitBehavior {
            behavior: "Page_Load, Page_Init, etc. automatically wired by naming convention".to_string(),
            webforms_mechanism: "AutoEventWireup=true in page directive".to_string(),
            modern_replacement: "Override OnInitialized/OnParametersSet explicitly in Blazor components".to_string(),
            severity: "Low".to_string(),
        });
    }

    // Postback model
    if !control_events.is_empty() {
        let postback_count = control_events
            .iter()
            .filter(|e| e.is_postback_trigger)
            .count();
        if postback_count > 0 {
            behaviors.push(ImplicitBehavior {
                behavior: format!("{postback_count} controls trigger full-page postbacks"),
                webforms_mechanism: "__doPostBack JavaScript + form POST to server".to_string(),
                modern_replacement: "Each postback becomes an async event handler with component re-render (no full page reload)".to_string(),
                severity: "Medium".to_string(),
            });
        }
    }

    behaviors
}

// ── Format ────────────────────────────────────────────────────────────────

pub fn format_lifecycle_map(report: &PageLifecycleMap) -> String {
    let mut out = String::with_capacity(4096);

    out.push_str(&format!("## Page Lifecycle Map: {}\n\n", report.file_path));

    if let Some(ref base) = report.base_class {
        out.push_str(&format!("**Base Class:** `{base}`\n\n"));
    }

    // Lifecycle events
    if !report.lifecycle_events.is_empty() {
        out.push_str("### Lifecycle Events\n\n");
        for ev in &report.lifecycle_events {
            out.push_str(&format!(
                "#### {} (line {}){}\n",
                ev.event_name,
                ev.line_number,
                if ev.has_ispostback_branch {
                    " [IsPostBack branching]"
                } else {
                    ""
                }
            ));

            if !ev.first_load_actions.is_empty() {
                out.push_str("**First load (!IsPostBack):**\n");
                for a in &ev.first_load_actions {
                    out.push_str(&format!("  - {a}\n"));
                }
            }
            if !ev.postback_actions.is_empty() {
                out.push_str("**Postback:**\n");
                for a in &ev.postback_actions {
                    out.push_str(&format!("  - {a}\n"));
                }
            }
            if !ev.always_actions.is_empty() {
                out.push_str("**Always:**\n");
                for a in &ev.always_actions {
                    out.push_str(&format!("  - {a}\n"));
                }
            }

            out.push_str(&format!("- Blazor: `{}`\n", ev.modern_blazor));
            out.push_str(&format!("- React: `{}`\n", ev.modern_react));
            out.push_str(&format!("- Angular: `{}`\n", ev.modern_angular));

            for note in &ev.migration_notes {
                out.push_str(&format!("  > {note}\n"));
            }
            out.push('\n');
        }
    }

    // Control events
    if !report.control_events.is_empty() {
        out.push_str("### Control Events\n\n");
        out.push_str("| Control | Type | Event | Handler | Postback | Blazor |\n");
        out.push_str("|---|---|---|---|---|---|\n");
        for ev in &report.control_events {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | `{}` |\n",
                ev.control_id,
                if ev.control_type.is_empty() {
                    "-"
                } else {
                    &ev.control_type
                },
                ev.event_name,
                ev.handler_name,
                if ev.is_postback_trigger { "Yes" } else { "No" },
                ev.modern_blazor
            ));
        }
        out.push('\n');
    }

    // Implicit behaviors
    if !report.implicit_behaviors.is_empty() {
        out.push_str("### Implicit Behaviors (Require Explicit Handling)\n\n");
        for b in &report.implicit_behaviors {
            out.push_str(&format!("- **[{}]** {}\n", b.severity, b.behavior));
            out.push_str(&format!("  WebForms: {}\n", b.webforms_mechanism));
            out.push_str(&format!("  Modern: {}\n", b.modern_replacement));
        }
    }

    // Migration notes
    if !report.migration_notes.is_empty() {
        out.push_str("\n### Migration Notes\n\n");
        for note in &report.migration_notes {
            out.push_str(&format!("- {note}\n"));
        }
    }

    out
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_graph() -> Arc<GraphStore> {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph.redb");
        Arc::new(GraphStore::open(&db_path).unwrap())
    }

    #[test]
    fn test_vb_page_load_with_postback() {
        let graph = make_graph();
        let code = r#"
Partial Class CustomerSearch
    Inherits System.Web.UI.Page

    Protected Sub Page_Load(sender As Object, e As EventArgs)
        If Not IsPostBack Then
            ddlStates.DataBind()
            gvResults.DataSource = Nothing
        Else
            lblStatus.Text = "Postback detected"
        End If
        lblTitle.Text = "Customer Search"
    End Sub
End Class
        "#;

        let result = analyze_page_lifecycle(&graph, "test", "Page.aspx.vb", code, None).unwrap();
        assert_eq!(result.lifecycle_events.len(), 1);
        let ev = &result.lifecycle_events[0];
        assert_eq!(ev.event_name, "Page_Load");
        assert!(ev.has_ispostback_branch);
        assert!(!ev.first_load_actions.is_empty());
    }

    #[test]
    fn test_cs_page_load() {
        let graph = make_graph();
        let code = r#"
public partial class Search : System.Web.UI.Page
{
    protected void Page_Load(object sender, EventArgs e)
    {
        if (!IsPostBack)
        {
            BindDropdowns();
        }
        lblTitle.Text = "Search";
    }
}
        "#;

        let result = analyze_page_lifecycle(&graph, "test", "Page.aspx.cs", code, None).unwrap();
        assert_eq!(result.lifecycle_events.len(), 1);
        assert!(result.lifecycle_events[0].has_ispostback_branch);
    }

    #[test]
    fn test_multiple_lifecycle_events() {
        let graph = make_graph();
        let code = r#"
Partial Class MyPage
    Inherits BasePage

    Protected Sub Page_Init(sender As Object, e As EventArgs)
        ' Init logic
    End Sub

    Protected Sub Page_Load(sender As Object, e As EventArgs)
        ' Load logic
    End Sub

    Protected Sub Page_PreRender(sender As Object, e As EventArgs)
        ' PreRender logic
    End Sub
End Class
        "#;

        let result = analyze_page_lifecycle(&graph, "test", "Page.aspx.vb", code, None).unwrap();
        assert_eq!(result.lifecycle_events.len(), 3);

        let names: Vec<&str> = result
            .lifecycle_events
            .iter()
            .map(|e| e.event_name.as_str())
            .collect();
        assert!(names.contains(&"Page_Init"));
        assert!(names.contains(&"Page_Load"));
        assert!(names.contains(&"Page_PreRender"));
    }

    #[test]
    fn test_vb_handles_clause() {
        let graph = make_graph();
        let code = r#"
Partial Class MyPage
    Inherits System.Web.UI.Page

    Private Sub MyPage_Load(sender As Object, e As EventArgs) Handles Me.Load
        ' Load via Handles clause
    End Sub
End Class
        "#;

        let result = analyze_page_lifecycle(&graph, "test", "Page.aspx.vb", code, None).unwrap();
        assert_eq!(result.lifecycle_events.len(), 1);
        assert_eq!(result.lifecycle_events[0].event_name, "Page_Load");
        assert_eq!(result.lifecycle_events[0].handler_name, "MyPage_Load");
    }

    #[test]
    fn test_control_events_vb() {
        let graph = make_graph();
        let code = r#"
Partial Class MyPage
    Inherits System.Web.UI.Page

    Protected Sub btnSearch_Click(sender As Object, e As EventArgs) Handles btnSearch.Click
        ' Search logic
    End Sub

    Protected Sub gvResults_PageIndexChanging(sender As Object, e As GridViewPageEventArgs) Handles gvResults.PageIndexChanging
        ' Paging logic
    End Sub

    Protected Sub ddlState_SelectedIndexChanged(sender As Object, e As EventArgs) Handles ddlState.SelectedIndexChanged
        ' Filter logic
    End Sub
End Class
        "#;

        let aspx = r#"
            <asp:Button ID="btnSearch" runat="server" />
            <asp:GridView ID="gvResults" runat="server" />
            <asp:DropDownList ID="ddlState" runat="server" />
        "#;

        let result =
            analyze_page_lifecycle(&graph, "test", "Page.aspx.vb", code, Some(aspx)).unwrap();
        assert_eq!(result.control_events.len(), 3);

        let btn = result
            .control_events
            .iter()
            .find(|e| e.control_id == "btnSearch")
            .unwrap();
        assert_eq!(btn.event_name, "Click");
        assert!(btn.is_postback_trigger);
        assert_eq!(btn.control_type, "Button");
    }

    #[test]
    fn test_page_directives() {
        let graph = make_graph();
        let aspx = r#"<%@ Page Language="VB" AutoEventWireup="false" CodeBehind="Search.aspx.vb" Inherits="MyApp.Search" MasterPageFile="~/Site.Master" EnableViewState="true" %>"#;
        let code = "Partial Class Search\n    Inherits System.Web.UI.Page\nEnd Class";

        let result =
            analyze_page_lifecycle(&graph, "test", "Search.aspx.vb", code, Some(aspx)).unwrap();
        assert_eq!(result.page_directives.auto_event_wireup, Some(false));
        assert_eq!(
            result.page_directives.master_page_file.as_deref(),
            Some("~/Site.Master")
        );
        assert_eq!(
            result.page_directives.inherits.as_deref(),
            Some("MyApp.Search")
        );
    }

    #[test]
    fn test_base_class_extraction() {
        let graph = make_graph();
        let code = r#"
Partial Class AdminPage
    Inherits BasePage

    Protected Sub Page_Load(sender As Object, e As EventArgs)
    End Sub
End Class
        "#;

        let result = analyze_page_lifecycle(&graph, "test", "Admin.aspx.vb", code, None).unwrap();
        assert_eq!(result.base_class.as_deref(), Some("BasePage"));
    }

    #[test]
    fn test_implicit_behaviors_generated() {
        let graph = make_graph();
        let code = r#"
Partial Class MyPage
    Inherits System.Web.UI.Page

    Protected Sub btnSave_Click(sender As Object, e As EventArgs) Handles btnSave.Click
    End Sub
End Class
        "#;
        let aspx = r#"<%@ Page Language="VB" AutoEventWireup="true" EnableViewState="true" %>"#;

        let result =
            analyze_page_lifecycle(&graph, "test", "Page.aspx.vb", code, Some(aspx)).unwrap();
        assert!(!result.implicit_behaviors.is_empty());
        assert!(
            result
                .implicit_behaviors
                .iter()
                .any(|b| b.behavior.contains("ViewState"))
        );
    }
}
