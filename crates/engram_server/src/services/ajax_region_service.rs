// Ticket 6: AJAX Region Mapping Service
//
// Scans ASPX markup for ASP.NET AJAX controls: UpdatePanel, Timer,
// ScriptManager/ScriptManagerProxy, and UpdateProgress. Maps each region
// to modern component boundaries and generates migration recommendations.

use engram_graph::GraphStore;
use regex::Regex;
use serde::Serialize;
use std::sync::{Arc, LazyLock};

// ── Result structs ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct AjaxRegionMap {
    pub file_path: String,
    pub has_script_manager: bool,
    pub enable_partial_rendering: bool,
    pub enable_page_methods: bool,
    pub update_panels: Vec<UpdatePanelInfo>,
    pub timers: Vec<TimerInfo>,
    pub update_progress_controls: Vec<UpdateProgressInfo>,
    pub service_references: Vec<String>,
    /// Controls (ID, type) found outside any UpdatePanel — these trigger full postbacks.
    pub full_postback_controls: Vec<String>,
    pub suggested_components: Vec<ComponentSuggestion>,
    pub migration_complexity: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdatePanelInfo {
    pub panel_id: String,
    pub update_mode: String, // "Always" | "Conditional"
    pub children_as_triggers: bool,
    pub async_triggers: Vec<TriggerInfo>,
    pub postback_triggers: Vec<String>,
    /// (id, type) pairs for controls inside the panel's ContentTemplate.
    pub controls_inside: Vec<(String, String)>,
    pub modern_pattern: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TriggerInfo {
    pub control_id: String,
    pub event_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimerInfo {
    pub timer_id: String,
    pub interval_ms: u32,
    pub enabled: bool,
    pub associated_update_panel: Option<String>,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateProgressInfo {
    pub progress_id: String,
    pub associated_update_panel: Option<String>,
    pub display_after_ms: u32,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentSuggestion {
    pub name: String,
    pub controls: Vec<String>,
    pub reason: String,
    pub blazor_pattern: String,
}

// ── Regex patterns ────────────────────────────────────────────────────────

// Matches the full <asp:ScriptManager ...> or <asp:ScriptManagerProxy ...> tag
static RE_SCRIPT_MANAGER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)<asp:ScriptManager(?:Proxy)?\b([^>]*?)(?:/\s*>|>(.*?)</asp:ScriptManager(?:Proxy)?\s*>)",
    )
    .unwrap()
});

// Matches the full <asp:UpdatePanel ...>...</asp:UpdatePanel> block
static RE_UPDATE_PANEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<asp:UpdatePanel\b([^>]*?)>(.*?)</asp:UpdatePanel\s*>").unwrap()
});

// Matches <asp:AsyncPostBackTrigger ... />
static RE_ASYNC_TRIGGER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)<asp:AsyncPostBackTrigger\b([^>]*?)(?:/\s*>|>.*?</asp:AsyncPostBackTrigger\s*>)",
    )
    .unwrap()
});

// Matches <asp:PostBackTrigger ... />
static RE_POSTBACK_TRIGGER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<asp:PostBackTrigger\b([^>]*?)(?:/\s*>|>.*?</asp:PostBackTrigger\s*>)")
        .unwrap()
});

// Matches <asp:Timer ...> anywhere in the markup
static RE_TIMER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<asp:Timer\b([^>]*?)(?:/\s*>|>.*?</asp:Timer\s*>)").unwrap()
});

// Matches <asp:UpdateProgress ...>...</asp:UpdateProgress>
static RE_UPDATE_PROGRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<asp:UpdateProgress\b([^>]*?)(?:/\s*>|>(.*?)</asp:UpdateProgress\s*>)")
        .unwrap()
});

// Matches <asp:ServiceReference Path="..." />
static RE_SERVICE_REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<asp:ServiceReference\b[^>]*?Path\s*=\s*"([^"]*)"[^>]*?(?:/\s*>|>.*?</asp:ServiceReference\s*>)"#)
        .unwrap()
});

// Matches any asp: control with an ID attribute — used to enumerate controls inside a panel body
static RE_ASP_CONTROL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<asp:(\w+)\b[^>]*?\bID\s*=\s*"([^"]*)"[^>]*?(?:/\s*>|>)"#).unwrap()
});

// ── Attribute helpers ─────────────────────────────────────────────────────

fn extract_attr(tag: &str, attr: &str) -> String {
    let pattern = format!(r#"(?i){}\s*=\s*"([^"]*)""#, regex::escape(attr));
    Regex::new(&pattern)
        .ok()
        .and_then(|re| re.captures(tag))
        .map(|c| c[1].to_string())
        .unwrap_or_default()
}

fn extract_attr_bool(tag: &str, attr: &str, default: bool) -> bool {
    let val = extract_attr(tag, attr);
    if val.is_empty() {
        return default;
    }
    val.eq_ignore_ascii_case("true")
}

fn extract_attr_u32(tag: &str, attr: &str, default: u32) -> u32 {
    let val = extract_attr(tag, attr);
    val.parse::<u32>().unwrap_or(default)
}

// ── Main analysis function ────────────────────────────────────────────────

pub fn analyze_ajax_regions(
    _graph: &Arc<GraphStore>,
    _project_id: &str,
    file_path: &str,
    aspx_content: &str,
) -> anyhow::Result<AjaxRegionMap> {
    // ── ScriptManager / ScriptManagerProxy ──

    let (has_script_manager, enable_partial_rendering, enable_page_methods) =
        parse_script_manager(aspx_content);

    // ── ServiceReference paths ──

    let service_references = parse_service_references(aspx_content);

    // ── Timer controls ──

    let timers_raw = parse_timers(aspx_content);

    // ── UpdateProgress controls ──

    let update_progress_controls = parse_update_progress(aspx_content);

    // ── UpdatePanel controls (with containment analysis) ──

    let update_panels = parse_update_panels(aspx_content, &timers_raw);

    // Resolve timer → UpdatePanel associations from trigger lists
    let timers = resolve_timer_associations(timers_raw, &update_panels);

    // ── Identify full-postback controls (outside any UpdatePanel) ──

    let full_postback_controls = find_full_postback_controls(aspx_content, &update_panels);

    // ── Generate component suggestions ──

    let suggested_components = suggest_components(&update_panels, &timers);

    // ── Migration complexity ──

    let migration_complexity = compute_complexity(
        &update_panels,
        &timers,
        &update_progress_controls,
        enable_page_methods,
        &service_references,
    );

    Ok(AjaxRegionMap {
        file_path: file_path.to_string(),
        has_script_manager,
        enable_partial_rendering,
        enable_page_methods,
        update_panels,
        timers,
        update_progress_controls,
        service_references,
        full_postback_controls,
        suggested_components,
        migration_complexity,
    })
}

// ── Parsing helpers ───────────────────────────────────────────────────────

fn parse_script_manager(content: &str) -> (bool, bool, bool) {
    match RE_SCRIPT_MANAGER.captures(content) {
        None => (false, true, false), // default: if no SM found, assume partial rendering off
        Some(cap) => {
            let attrs = &cap[1];
            // Gather inner body too (group 2 may be absent for self-closing)
            let inner = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            // EnablePartialRendering defaults to true when ScriptManager is present
            let epr = extract_attr_bool(attrs, "EnablePartialRendering", true);
            let epm = extract_attr_bool(attrs, "EnablePageMethods", false)
                || extract_attr_bool(inner, "EnablePageMethods", false);
            (true, epr, epm)
        }
    }
}

fn parse_service_references(content: &str) -> Vec<String> {
    RE_SERVICE_REF
        .captures_iter(content)
        .map(|cap| cap[1].to_string())
        .collect()
}

/// Raw timer parse — association to UpdatePanel resolved later.
fn parse_timers(content: &str) -> Vec<TimerInfo> {
    RE_TIMER
        .captures_iter(content)
        .map(|cap| {
            let attrs = &cap[1];
            let timer_id = extract_attr(attrs, "ID");
            let interval_ms = extract_attr_u32(attrs, "Interval", 60000);
            let enabled = extract_attr_bool(attrs, "Enabled", true);
            let modern_equivalent = build_timer_modern_equivalent(interval_ms, &timer_id);
            TimerInfo {
                timer_id,
                interval_ms,
                enabled,
                associated_update_panel: None,
                modern_equivalent,
            }
        })
        .collect()
}

fn parse_update_progress(content: &str) -> Vec<UpdateProgressInfo> {
    RE_UPDATE_PROGRESS
        .captures_iter(content)
        .map(|cap| {
            let attrs = &cap[1];
            let progress_id = extract_attr(attrs, "ID");
            let assoc = extract_attr(attrs, "AssociatedUpdatePanelID");
            let display_after_ms = extract_attr_u32(attrs, "DisplayAfter", 500);
            let modern_equivalent =
                build_progress_modern_equivalent(assoc.as_str(), display_after_ms);
            UpdateProgressInfo {
                progress_id,
                associated_update_panel: if assoc.is_empty() { None } else { Some(assoc) },
                display_after_ms,
                modern_equivalent,
            }
        })
        .collect()
}

fn parse_update_panels(content: &str, timers: &[TimerInfo]) -> Vec<UpdatePanelInfo> {
    RE_UPDATE_PANEL
        .captures_iter(content)
        .map(|cap| {
            let opening_attrs = &cap[1];
            let body = &cap[2];

            let panel_id = extract_attr(opening_attrs, "ID");
            let raw_mode = extract_attr(opening_attrs, "UpdateMode");
            let update_mode = if raw_mode.is_empty() {
                "Always".to_string()
            } else {
                raw_mode
            };
            let children_as_triggers = extract_attr_bool(opening_attrs, "ChildrenAsTriggers", true);

            // Parse triggers from <Triggers> section inside body
            let async_triggers = parse_async_triggers(body);
            let postback_triggers = parse_postback_triggers(body);

            // Find controls inside <ContentTemplate> section
            let controls_inside = extract_controls_in_content_template(body);

            // Check if any Timer is wired as an async trigger for this panel
            let timer_triggered = async_triggers.iter().any(|t| {
                timers
                    .iter()
                    .any(|ti| ti.timer_id.eq_ignore_ascii_case(&t.control_id))
            });

            let modern_pattern = build_panel_modern_pattern(
                &panel_id,
                &update_mode,
                children_as_triggers,
                timer_triggered,
                async_triggers.len(),
            );

            UpdatePanelInfo {
                panel_id,
                update_mode,
                children_as_triggers,
                async_triggers,
                postback_triggers,
                controls_inside,
                modern_pattern,
            }
        })
        .collect()
}

fn parse_async_triggers(panel_body: &str) -> Vec<TriggerInfo> {
    RE_ASYNC_TRIGGER
        .captures_iter(panel_body)
        .map(|cap| {
            let attrs = &cap[1];
            TriggerInfo {
                control_id: extract_attr(attrs, "ControlID"),
                event_name: {
                    let ev = extract_attr(attrs, "EventName");
                    if ev.is_empty() {
                        "Click".to_string()
                    } else {
                        ev
                    }
                },
            }
        })
        .collect()
}

fn parse_postback_triggers(panel_body: &str) -> Vec<String> {
    RE_POSTBACK_TRIGGER
        .captures_iter(panel_body)
        .map(|cap| extract_attr(&cap[1], "ControlID"))
        .filter(|id| !id.is_empty())
        .collect()
}

/// Returns (id, type) pairs for every asp: control inside the ContentTemplate section.
fn extract_controls_in_content_template(panel_body: &str) -> Vec<(String, String)> {
    // Find the <ContentTemplate> ... </ContentTemplate> region
    let lower = panel_body.to_lowercase();
    let start = lower
        .find("<contenttemplate")
        .and_then(|p| lower[p..].find('>').map(|off| p + off + 1));
    let end = lower.find("</contenttemplate>");

    let template_content = match (start, end) {
        (Some(s), Some(e)) if e > s => &panel_body[s..e],
        _ => panel_body, // fall back to full body if ContentTemplate not found
    };

    RE_ASP_CONTROL
        .captures_iter(template_content)
        .filter(|cap| {
            // Exclude nested UpdatePanel openers themselves
            !cap[1].eq_ignore_ascii_case("UpdatePanel")
                && !cap[1].eq_ignore_ascii_case("AsyncPostBackTrigger")
                && !cap[1].eq_ignore_ascii_case("PostBackTrigger")
        })
        .map(|cap| (cap[2].to_string(), cap[1].to_string()))
        .collect()
}

/// After panels are parsed, link each timer to the panel that lists it as a trigger.
fn resolve_timer_associations(
    mut timers: Vec<TimerInfo>,
    panels: &[UpdatePanelInfo],
) -> Vec<TimerInfo> {
    for timer in &mut timers {
        for panel in panels {
            let triggered_by_panel = panel
                .async_triggers
                .iter()
                .any(|t| t.control_id.eq_ignore_ascii_case(&timer.timer_id));
            // Also check if the timer is physically inside the panel's ContentTemplate
            let inside_panel = panel.controls_inside.iter().any(|(id, ctype)| {
                id.eq_ignore_ascii_case(&timer.timer_id) && ctype.eq_ignore_ascii_case("Timer")
            });
            if triggered_by_panel || inside_panel {
                timer.associated_update_panel = Some(panel.panel_id.clone());
                break;
            }
        }
    }
    timers
}

/// Collect IDs of controls that appear outside every UpdatePanel body.
fn find_full_postback_controls(content: &str, panels: &[UpdatePanelInfo]) -> Vec<String> {
    // Build the set of IDs that are known to be inside at least one panel
    let inside_ids: std::collections::HashSet<String> = panels
        .iter()
        .flat_map(|p| p.controls_inside.iter().map(|(id, _)| id.clone()))
        .collect();

    // Find all button-like controls in the full markup
    let re_buttons = Regex::new(
        r#"(?is)<asp:(Button|LinkButton|ImageButton)\b[^>]*?\bID\s*=\s*"([^"]*)"[^>]*?(?:/\s*>|>)"#,
    )
    .unwrap();

    re_buttons
        .captures_iter(content)
        .map(|cap| cap[2].to_string())
        .filter(|id| !inside_ids.contains(id.as_str()))
        .collect()
}

// ── Modern-pattern builders ───────────────────────────────────────────────

fn build_panel_modern_pattern(
    panel_id: &str,
    update_mode: &str,
    children_as_triggers: bool,
    timer_triggered: bool,
    async_trigger_count: usize,
) -> String {
    if timer_triggered {
        return format!(
            "Blazor: Dedicated @page component for '{}' with timer-driven polling using a System.Threading.Timer in OnInitializedAsync; cancel in IAsyncDisposable. \
             React: setInterval() in useEffect with axios/fetch; clear on component unmount.",
            panel_id
        );
    }

    match update_mode.to_lowercase().as_str() {
        "conditional" => {
            if async_trigger_count > 0 {
                format!(
                    "Blazor: Extract '{}' into a child component (<{}Component />) that fetches its own data via injected service on specific @onclick / @onchange events. \
                     React: Isolated component with useState + useCallback for async fetch on explicit user action.",
                    panel_id,
                    pascal_case(panel_id)
                )
            } else if !children_as_triggers {
                format!(
                    "Blazor: '{}' only refreshes via external triggers — use EventCallback<T> from parent or CascadingParameter to signal refresh. \
                     React: Lift state to parent and pass refresh callback as prop.",
                    panel_id
                )
            } else {
                format!(
                    "Blazor: Extract '{}' into a self-contained child component; child controls trigger their own async refreshes via @onclick. \
                     React: Isolated functional component with internal state.",
                    panel_id
                )
            }
        }
        _ => {
            // Always mode — panel refreshes on every async postback in the page
            format!(
                "Blazor: Consider whether '{}' (UpdateMode=Always) needs full re-render on every async event — \
                 if so, keep logic in the parent component and use StateHasChanged(); if not, switch to Conditional mode equivalent. \
                 React: Shared state in parent component re-renders this region on every dispatch.",
                panel_id
            )
        }
    }
}

fn build_timer_modern_equivalent(interval_ms: u32, timer_id: &str) -> String {
    let seconds = interval_ms / 1000;
    format!(
        "Blazor: In OnInitializedAsync, start a System.Threading.Timer with {interval_ms}ms interval calling InvokeAsync(StateHasChanged) after fetching data. \
         Implement IAsyncDisposable to cancel. \
         React: useEffect(() => {{ const id = setInterval(fetchData, {interval_ms}); return () => clearInterval(id); }}, []). \
         Timer ID: '{timer_id}', Interval: {seconds}s.",
    )
}

fn build_progress_modern_equivalent(assoc: &str, display_after_ms: u32) -> String {
    let target = if assoc.is_empty() {
        "any async operation"
    } else {
        assoc
    };
    format!(
        "Blazor: Use a boolean `isLoading` field toggled before/after awaited calls; render a loading spinner with `@if (isLoading)`. \
         Delayed display ({display_after_ms}ms): add a Task.Delay({display_after_ms}) before showing the spinner. \
         React: useState isLoading + conditional JSX; use setTimeout for the {display_after_ms}ms delay. \
         Associated with: {target}."
    )
}

// ── Component suggestion logic ────────────────────────────────────────────

fn suggest_components(
    panels: &[UpdatePanelInfo],
    timers: &[TimerInfo],
) -> Vec<ComponentSuggestion> {
    let mut suggestions = Vec::new();

    for panel in panels {
        let control_ids: Vec<String> = panel
            .controls_inside
            .iter()
            .map(|(id, _)| id.clone())
            .collect();

        if control_ids.is_empty() && panel.async_triggers.is_empty() {
            continue;
        }

        let name = format!("{}Component", pascal_case(&panel.panel_id));
        let is_timer_driven = timers
            .iter()
            .any(|t| t.associated_update_panel.as_deref() == Some(&panel.panel_id));

        let (reason, blazor_pattern) = if is_timer_driven {
            (
                format!(
                    "UpdatePanel '{}' is refreshed by a Timer — ideal candidate for a polling component",
                    panel.panel_id
                ),
                format!(
                    "@page \"/{}region\"\n\
                     @implements IAsyncDisposable\n\
                     @inject IMyService Service\n\n\
                     <div><!-- {} content here --></div>\n\n\
                     @code {{\n\
                     \x20   private System.Threading.Timer? _timer;\n\
                     \x20   protected override Task OnInitializedAsync() {{\n\
                     \x20       _timer = new Timer(async _ => await InvokeAsync(async () => {{\n\
                     \x20           Data = await Service.GetDataAsync();\n\
                     \x20           StateHasChanged();\n\
                     \x20       }}), null, 0, TimerInterval);\n\
                     \x20       return Task.CompletedTask;\n\
                     \x20   }}\n\
                     \x20   public async ValueTask DisposeAsync() => _timer?.Dispose();\n\
                     }}",
                    panel.panel_id.to_lowercase(),
                    panel.panel_id
                ),
            )
        } else if panel.update_mode.eq_ignore_ascii_case("Conditional") {
            (
                format!(
                    "UpdatePanel '{}' uses Conditional update mode — maps well to an isolated Blazor component with EventCallback",
                    panel.panel_id
                ),
                format!(
                    "@* {}Component.razor *@\n\
                     @inject IMyService Service\n\n\
                     <div>\n\
                     \x20   <!-- {} controls: {} -->\n\
                     </div>\n\n\
                     @code {{\n\
                     \x20   [Parameter] public EventCallback OnRefresh {{ get; set; }}\n\
                     \x20   private async Task HandleAction() {{\n\
                     \x20       await Service.DoActionAsync();\n\
                     \x20       await OnRefresh.InvokeAsync();\n\
                     \x20   }}\n\
                     }}",
                    pascal_case(&panel.panel_id),
                    panel.panel_id,
                    control_ids.join(", ")
                ),
            )
        } else {
            (
                format!(
                    "UpdatePanel '{}' (UpdateMode=Always) — extract to component to make refresh boundaries explicit",
                    panel.panel_id
                ),
                format!(
                    "@* {}Component.razor *@\n\
                     @inject IMyService Service\n\n\
                     <div>\n\
                     \x20   <!-- {} controls: {} -->\n\
                     </div>\n\n\
                     @code {{\n\
                     \x20   private async Task Refresh() {{\n\
                     \x20       Data = await Service.GetDataAsync();\n\
                     \x20   }}\n\
                     }}",
                    pascal_case(&panel.panel_id),
                    panel.panel_id,
                    control_ids.join(", ")
                ),
            )
        };

        let mut all_controls = control_ids;
        for t in &panel.async_triggers {
            if !all_controls.contains(&t.control_id) {
                all_controls.push(t.control_id.clone());
            }
        }

        suggestions.push(ComponentSuggestion {
            name,
            controls: all_controls,
            reason,
            blazor_pattern,
        });
    }

    suggestions
}

// ── Complexity scoring ────────────────────────────────────────────────────

fn compute_complexity(
    panels: &[UpdatePanelInfo],
    timers: &[TimerInfo],
    progress: &[UpdateProgressInfo],
    enable_page_methods: bool,
    service_references: &[String],
) -> String {
    let panel_count = panels.len();
    let timer_count = timers.len();
    let has_conditional = panels
        .iter()
        .any(|p| p.update_mode.eq_ignore_ascii_case("Conditional"));
    let has_progress = !progress.is_empty();
    let has_services = !service_references.is_empty();

    if panel_count == 0 && timer_count == 0 {
        return "None: no AJAX controls found — page uses standard postbacks".to_string();
    }

    let mut score = 0u32;
    score += panel_count as u32 * 2;
    score += timer_count as u32 * 3;
    if has_conditional {
        score += 2;
    }
    if has_progress {
        score += 1;
    }
    if enable_page_methods {
        score += 4;
    }
    if has_services {
        score += (service_references.len() as u32) * 3;
    }

    if score <= 4 {
        format!(
            "Low: {} UpdatePanel(s), straightforward extraction to Blazor child components",
            panel_count
        )
    } else if score <= 10 {
        format!(
            "Medium: {} UpdatePanel(s), {} Timer(s){}{} — plan async state management and loading indicators",
            panel_count,
            timer_count,
            if has_conditional {
                ", Conditional panels"
            } else {
                ""
            },
            if has_services {
                format!(", {} service reference(s)", service_references.len())
            } else {
                String::new()
            }
        )
    } else {
        format!(
            "High: {} UpdatePanel(s), {} Timer(s){}{}{} — consider a full SPA architecture \
             (Blazor WASM or React) with dedicated API endpoints replacing ScriptManager services",
            panel_count,
            timer_count,
            if has_conditional {
                ", Conditional panels"
            } else {
                ""
            },
            if enable_page_methods {
                ", PageMethods"
            } else {
                ""
            },
            if has_services {
                format!(", {} service reference(s)", service_references.len())
            } else {
                String::new()
            }
        )
    }
}

// ── Utility ───────────────────────────────────────────────────────────────

/// Convert a camelCase or hyphenated id to PascalCase for component naming.
fn pascal_case(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    // Strip common ASPX prefixes (up, pnl, etc.) only when the remainder starts uppercase
    let stripped = s
        .trim_start_matches("up")
        .trim_start_matches("pnl")
        .trim_start_matches("panel");
    let src = if stripped.is_empty() { s } else { stripped };

    // Capitalise first char; preserve the rest
    let mut chars = src.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

// ── Format function ───────────────────────────────────────────────────────

pub fn format_ajax_region_map(report: &AjaxRegionMap) -> String {
    let mut out = String::with_capacity(4096);

    out.push_str(&format!("## AJAX Region Map: {}\n\n", report.file_path));
    out.push_str(&format!(
        "**ScriptManager:** {} | **Partial Rendering:** {} | **PageMethods:** {} | **Complexity:** {}\n\n",
        if report.has_script_manager { "Yes" } else { "No" },
        if report.enable_partial_rendering { "Enabled" } else { "Disabled" },
        if report.enable_page_methods { "Enabled" } else { "Disabled" },
        report.migration_complexity
    ));

    if !report.service_references.is_empty() {
        out.push_str("### Service References (Web Services via ScriptManager)\n\n");
        for path in &report.service_references {
            out.push_str(&format!(
                "- `{path}` → migrate to ASP.NET Core Web API endpoint\n"
            ));
        }
        out.push('\n');
    }

    if report.update_panels.is_empty() {
        out.push_str("No UpdatePanel controls found.\n");
    } else {
        out.push_str("### UpdatePanels\n\n");
        out.push_str("| ID | Mode | ChildrenAsTriggers | Async Triggers | PostBack Triggers | Controls Inside |\n");
        out.push_str("|---|---|---|---|---|---|\n");
        for p in &report.update_panels {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                p.panel_id,
                p.update_mode,
                p.children_as_triggers,
                p.async_triggers
                    .iter()
                    .map(|t| format!("{}/{}", t.control_id, t.event_name))
                    .collect::<Vec<_>>()
                    .join(", "),
                p.postback_triggers.join(", "),
                p.controls_inside
                    .iter()
                    .map(|(id, t)| format!("{id}({t})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out.push('\n');

        out.push_str("### Modern Patterns per UpdatePanel\n\n");
        for p in &report.update_panels {
            out.push_str(&format!("**{}**: {}\n\n", p.panel_id, p.modern_pattern));
        }
    }

    if !report.timers.is_empty() {
        out.push_str("### Timer Controls\n\n");
        for t in &report.timers {
            out.push_str(&format!(
                "- **{}**: Interval={}ms, Enabled={}{}\n",
                t.timer_id,
                t.interval_ms,
                t.enabled,
                t.associated_update_panel
                    .as_ref()
                    .map(|p| format!(", Panel={p}"))
                    .unwrap_or_default()
            ));
            out.push_str(&format!("  Modern: {}\n\n", t.modern_equivalent));
        }
    }

    if !report.update_progress_controls.is_empty() {
        out.push_str("### UpdateProgress Controls\n\n");
        for p in &report.update_progress_controls {
            out.push_str(&format!(
                "- **{}**: DisplayAfter={}ms{}\n",
                p.progress_id,
                p.display_after_ms,
                p.associated_update_panel
                    .as_ref()
                    .map(|a| format!(", Associated={a}"))
                    .unwrap_or_default()
            ));
            out.push_str(&format!("  Modern: {}\n\n", p.modern_equivalent));
        }
    }

    if !report.full_postback_controls.is_empty() {
        out.push_str("### Full-Postback Controls (outside all UpdatePanels)\n\n");
        for id in &report.full_postback_controls {
            out.push_str(&format!(
                "- `{id}` — triggers full page postback; in modern stack this becomes a regular form submission or navigation\n"
            ));
        }
        out.push('\n');
    }

    if !report.suggested_components.is_empty() {
        out.push_str("### Suggested Component Decomposition\n\n");
        for comp in &report.suggested_components {
            out.push_str(&format!("#### {}\n", comp.name));
            out.push_str(&format!("Controls: {}\n", comp.controls.join(", ")));
            out.push_str(&format!("Reason: {}\n", comp.reason));
            out.push_str(&format!("```razor\n{}\n```\n\n", comp.blazor_pattern));
        }
    }

    out
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_graph() -> Arc<GraphStore> {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_graph.redb");
        let graph = Arc::new(GraphStore::open(&db_path).unwrap());
        // Keep dir alive long enough for the store to initialise its tables;
        // the GraphStore holds its own file handle so the tempdir can be
        // dropped after open() returns.
        drop(dir);
        graph
    }

    // ── Test 1: Single UpdatePanel with async triggers ──

    #[test]
    fn test_single_update_panel_with_async_triggers() {
        let graph = make_graph();
        let aspx = r#"
            <asp:ScriptManager ID="ScriptManager1" runat="server" EnablePartialRendering="true" />
            <asp:UpdatePanel ID="upSearch" runat="server" UpdateMode="Conditional" ChildrenAsTriggers="false">
                <ContentTemplate>
                    <asp:GridView ID="gvResults" runat="server" />
                    <asp:Label ID="lblCount" runat="server" />
                </ContentTemplate>
                <Triggers>
                    <asp:AsyncPostBackTrigger ControlID="btnSearch" EventName="Click" />
                    <asp:AsyncPostBackTrigger ControlID="ddlFilter" EventName="SelectedIndexChanged" />
                </Triggers>
            </asp:UpdatePanel>
        "#;

        let result = analyze_ajax_regions(&graph, "test", "Search.aspx", aspx).unwrap();

        assert!(result.has_script_manager);
        assert!(result.enable_partial_rendering);
        assert_eq!(result.update_panels.len(), 1);

        let panel = &result.update_panels[0];
        assert_eq!(panel.panel_id, "upSearch");
        assert_eq!(panel.update_mode, "Conditional");
        assert!(!panel.children_as_triggers);
        assert_eq!(panel.async_triggers.len(), 2);
        assert_eq!(panel.async_triggers[0].control_id, "btnSearch");
        assert_eq!(panel.async_triggers[0].event_name, "Click");
        assert_eq!(panel.async_triggers[1].control_id, "ddlFilter");
        assert_eq!(panel.async_triggers[1].event_name, "SelectedIndexChanged");

        // Controls inside ContentTemplate
        let ids: Vec<&str> = panel
            .controls_inside
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert!(ids.contains(&"gvResults"));
        assert!(ids.contains(&"lblCount"));

        assert!(
            panel.modern_pattern.contains("child component")
                || panel.modern_pattern.contains("EventCallback")
        );
    }

    // ── Test 2: Multiple UpdatePanels with different UpdateModes ──

    #[test]
    fn test_multiple_update_panels_different_modes() {
        let graph = make_graph();
        let aspx = r#"
            <asp:ScriptManager ID="sm" runat="server" />
            <asp:UpdatePanel ID="upHeader" runat="server" UpdateMode="Always">
                <ContentTemplate>
                    <asp:Label ID="lblTime" runat="server" />
                </ContentTemplate>
            </asp:UpdatePanel>
            <asp:UpdatePanel ID="upContent" runat="server" UpdateMode="Conditional" ChildrenAsTriggers="true">
                <ContentTemplate>
                    <asp:GridView ID="gvData" runat="server" />
                    <asp:Button ID="btnRefresh" runat="server" />
                </ContentTemplate>
                <Triggers>
                    <asp:AsyncPostBackTrigger ControlID="btnRefresh" EventName="Click" />
                </Triggers>
            </asp:UpdatePanel>
            <asp:UpdatePanel ID="upFooter" runat="server" UpdateMode="Conditional">
                <ContentTemplate>
                    <asp:Label ID="lblStatus" runat="server" />
                </ContentTemplate>
                <Triggers>
                    <asp:PostBackTrigger ControlID="btnFullPostBack" />
                </Triggers>
            </asp:UpdatePanel>
        "#;

        let result = analyze_ajax_regions(&graph, "test", "Multi.aspx", aspx).unwrap();

        assert_eq!(result.update_panels.len(), 3);

        let always_panel = result
            .update_panels
            .iter()
            .find(|p| p.panel_id == "upHeader")
            .unwrap();
        assert_eq!(always_panel.update_mode, "Always");

        let cond_panel = result
            .update_panels
            .iter()
            .find(|p| p.panel_id == "upContent")
            .unwrap();
        assert_eq!(cond_panel.update_mode, "Conditional");
        assert!(cond_panel.children_as_triggers);
        assert_eq!(cond_panel.async_triggers.len(), 1);

        let footer = result
            .update_panels
            .iter()
            .find(|p| p.panel_id == "upFooter")
            .unwrap();
        assert_eq!(footer.postback_triggers.len(), 1);
        assert_eq!(footer.postback_triggers[0], "btnFullPostBack");

        // Complexity should be medium or high for 3 panels
        assert!(
            result.migration_complexity.contains("Medium")
                || result.migration_complexity.contains("High")
        );
    }

    // ── Test 3: Timer-based refresh ──

    #[test]
    fn test_timer_based_refresh() {
        let graph = make_graph();
        let aspx = r#"
            <asp:ScriptManager ID="sm" runat="server" />
            <asp:Timer ID="tmrRefresh" runat="server" Interval="5000" Enabled="true" />
            <asp:UpdatePanel ID="upLive" runat="server" UpdateMode="Always">
                <ContentTemplate>
                    <asp:Label ID="lblLiveData" runat="server" />
                    <asp:GridView ID="gvLive" runat="server" />
                </ContentTemplate>
                <Triggers>
                    <asp:AsyncPostBackTrigger ControlID="tmrRefresh" EventName="Tick" />
                </Triggers>
            </asp:UpdatePanel>
        "#;

        let result = analyze_ajax_regions(&graph, "test", "LiveDashboard.aspx", aspx).unwrap();

        assert_eq!(result.timers.len(), 1);
        let timer = &result.timers[0];
        assert_eq!(timer.timer_id, "tmrRefresh");
        assert_eq!(timer.interval_ms, 5000);
        assert!(timer.enabled);
        assert_eq!(timer.associated_update_panel.as_deref(), Some("upLive"));
        assert!(
            timer.modern_equivalent.contains("5000")
                || timer.modern_equivalent.contains("5s")
                || timer.modern_equivalent.contains("Timer")
        );

        let panel = &result.update_panels[0];
        assert!(
            panel.modern_pattern.contains("timer")
                || panel.modern_pattern.contains("Timer")
                || panel.modern_pattern.contains("polling")
        );

        // Should suggest a timer-driven component
        assert!(!result.suggested_components.is_empty());
        let comp = result
            .suggested_components
            .iter()
            .find(|c| {
                c.controls
                    .iter()
                    .any(|id| id == "tmrRefresh" || id == "lblLiveData")
            })
            .unwrap_or(&result.suggested_components[0]);
        assert!(comp.blazor_pattern.contains("Timer") || comp.reason.contains("timer"));
    }

    // ── Test 4: UpdateProgress controls ──

    #[test]
    fn test_update_progress_controls() {
        let graph = make_graph();
        let aspx = r#"
            <asp:ScriptManager ID="sm" runat="server" />
            <asp:UpdatePanel ID="upData" runat="server">
                <ContentTemplate>
                    <asp:GridView ID="gvData" runat="server" />
                </ContentTemplate>
            </asp:UpdatePanel>
            <asp:UpdateProgress ID="uprogData" runat="server"
                AssociatedUpdatePanelID="upData"
                DisplayAfter="300">
                <ProgressTemplate>
                    <asp:Image ID="imgSpinner" runat="server" ImageUrl="~/images/spinner.gif" />
                </ProgressTemplate>
            </asp:UpdateProgress>
            <asp:UpdateProgress ID="uprogGlobal" runat="server" DisplayAfter="1000">
                <ProgressTemplate>Loading...</ProgressTemplate>
            </asp:UpdateProgress>
        "#;

        let result = analyze_ajax_regions(&graph, "test", "Progress.aspx", aspx).unwrap();

        assert_eq!(result.update_progress_controls.len(), 2);

        let assoc = result
            .update_progress_controls
            .iter()
            .find(|p| p.progress_id == "uprogData")
            .unwrap();
        assert_eq!(assoc.associated_update_panel.as_deref(), Some("upData"));
        assert_eq!(assoc.display_after_ms, 300);
        assert!(
            assoc.modern_equivalent.contains("isLoading")
                || assoc.modern_equivalent.contains("loading")
        );

        let global = result
            .update_progress_controls
            .iter()
            .find(|p| p.progress_id == "uprogGlobal")
            .unwrap();
        assert!(global.associated_update_panel.is_none());
        assert_eq!(global.display_after_ms, 1000);
    }

    // ── Test 5: ScriptManager with service references ──

    #[test]
    fn test_script_manager_with_service_references() {
        let graph = make_graph();
        let aspx = r#"
            <asp:ScriptManager ID="sm" runat="server"
                EnablePartialRendering="true"
                EnablePageMethods="true">
                <Services>
                    <asp:ServiceReference Path="~/Services/CustomerService.asmx" />
                    <asp:ServiceReference Path="~/Services/OrderService.asmx" />
                </Services>
            </asp:ScriptManager>
            <asp:UpdatePanel ID="upCustomer" runat="server">
                <ContentTemplate>
                    <asp:TextBox ID="txtCustomerName" runat="server" />
                </ContentTemplate>
            </asp:UpdatePanel>
        "#;

        let result = analyze_ajax_regions(&graph, "test", "ServicePage.aspx", aspx).unwrap();

        assert!(result.has_script_manager);
        assert!(result.enable_partial_rendering);
        assert!(result.enable_page_methods);
        assert_eq!(result.service_references.len(), 2);
        assert!(
            result
                .service_references
                .iter()
                .any(|s| s.contains("CustomerService"))
        );
        assert!(
            result
                .service_references
                .iter()
                .any(|s| s.contains("OrderService"))
        );

        // PageMethods + service references push complexity up
        assert!(
            result.migration_complexity.contains("High")
                || result.migration_complexity.contains("Medium")
        );
    }

    // ── Test 6: Controls inside vs outside UpdatePanels ──

    #[test]
    fn test_controls_inside_vs_outside_update_panels() {
        let graph = make_graph();
        let aspx = r#"
            <asp:ScriptManager ID="sm" runat="server" />
            <form>
                <asp:Button ID="btnFullPostBack" runat="server" Text="Save" />
                <asp:UpdatePanel ID="upPartial" runat="server" UpdateMode="Conditional">
                    <ContentTemplate>
                        <asp:Button ID="btnAsync" runat="server" Text="Async Action" />
                        <asp:Label ID="lblResult" runat="server" />
                    </ContentTemplate>
                    <Triggers>
                        <asp:AsyncPostBackTrigger ControlID="btnAsync" EventName="Click" />
                    </Triggers>
                </asp:UpdatePanel>
                <asp:Button ID="btnCancel" runat="server" Text="Cancel" CausesValidation="false" />
            </form>
        "#;

        let result = analyze_ajax_regions(&graph, "test", "Mixed.aspx", aspx).unwrap();

        // Controls inside the panel
        let panel = &result.update_panels[0];
        let inside_ids: Vec<&str> = panel
            .controls_inside
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert!(inside_ids.contains(&"btnAsync"));
        assert!(inside_ids.contains(&"lblResult"));

        // Controls outside all panels → full postback
        assert!(
            result
                .full_postback_controls
                .iter()
                .any(|id| id == "btnFullPostBack" || id == "btnCancel")
        );
        // Async button inside UpdatePanel should NOT be in full_postback_controls
        assert!(
            !result
                .full_postback_controls
                .contains(&"btnAsync".to_string())
        );
    }

    // ── Test 7: Component decomposition suggestions ──

    #[test]
    fn test_component_decomposition_suggestions() {
        let graph = make_graph();
        let aspx = r#"
            <asp:ScriptManager ID="sm" runat="server" />
            <asp:UpdatePanel ID="upOrderList" runat="server" UpdateMode="Conditional">
                <ContentTemplate>
                    <asp:GridView ID="gvOrders" runat="server" />
                    <asp:DropDownList ID="ddlStatus" runat="server" />
                    <asp:Button ID="btnFilter" runat="server" Text="Filter" />
                </ContentTemplate>
                <Triggers>
                    <asp:AsyncPostBackTrigger ControlID="btnFilter" EventName="Click" />
                    <asp:AsyncPostBackTrigger ControlID="ddlStatus" EventName="SelectedIndexChanged" />
                </Triggers>
            </asp:UpdatePanel>
            <asp:UpdatePanel ID="upOrderDetail" runat="server" UpdateMode="Always">
                <ContentTemplate>
                    <asp:FormView ID="fvDetail" runat="server" />
                </ContentTemplate>
            </asp:UpdatePanel>
        "#;

        let result = analyze_ajax_regions(&graph, "test", "Orders.aspx", aspx).unwrap();

        assert_eq!(result.update_panels.len(), 2);
        assert!(!result.suggested_components.is_empty());

        // Each panel should produce a suggestion
        let names: Vec<&str> = result
            .suggested_components
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n.contains("OrderList") || n.contains("upOrderList")),
            "Expected suggestion for upOrderList panel, got: {:?}",
            names
        );

        // The Blazor pattern should contain component skeleton
        for comp in &result.suggested_components {
            assert!(
                comp.blazor_pattern.contains("@inject")
                    || comp.blazor_pattern.contains("@code")
                    || comp.blazor_pattern.contains("razor"),
                "Blazor pattern should contain component structure"
            );
            assert!(!comp.reason.is_empty());
        }
    }

    // ── Test 8: No AJAX controls (empty result) ──

    #[test]
    fn test_no_ajax_controls() {
        let graph = make_graph();
        let aspx = r#"
            <%@ Page Language="C#" AutoEventWireup="true" %>
            <html>
            <body>
                <form>
                    <asp:TextBox ID="txtName" runat="server" />
                    <asp:Button ID="btnSubmit" runat="server" Text="Submit" />
                    <asp:RequiredFieldValidator ID="rfv1" ControlToValidate="txtName"
                        ErrorMessage="Name required" runat="server" />
                </form>
            </body>
            </html>
        "#;

        let result = analyze_ajax_regions(&graph, "test", "Simple.aspx", aspx).unwrap();

        assert!(!result.has_script_manager);
        assert!(result.update_panels.is_empty());
        assert!(result.timers.is_empty());
        assert!(result.update_progress_controls.is_empty());
        assert!(result.service_references.is_empty());
        assert!(result.suggested_components.is_empty());
        assert!(
            result.migration_complexity.contains("None")
                || result.migration_complexity.contains("no AJAX")
        );

        // Format should still work cleanly
        let formatted = format_ajax_region_map(&result);
        assert!(formatted.contains("AJAX Region Map"));
        assert!(formatted.contains("No UpdatePanel"));
    }

    // ── Test 9: Default attribute values ──

    #[test]
    fn test_default_attribute_values() {
        let graph = make_graph();
        // UpdatePanel with no explicit UpdateMode or ChildrenAsTriggers
        // Timer with no explicit Enabled attribute
        let aspx = r#"
            <asp:ScriptManager ID="sm" runat="server" />
            <asp:Timer ID="tmr" runat="server" Interval="30000" />
            <asp:UpdatePanel ID="upDefault" runat="server">
                <ContentTemplate>
                    <asp:Label ID="lblMsg" runat="server" />
                </ContentTemplate>
                <Triggers>
                    <asp:AsyncPostBackTrigger ControlID="tmr" EventName="Tick" />
                </Triggers>
            </asp:UpdatePanel>
        "#;

        let result = analyze_ajax_regions(&graph, "test", "Defaults.aspx", aspx).unwrap();

        // UpdateMode defaults to "Always"
        let panel = &result.update_panels[0];
        assert_eq!(panel.update_mode, "Always");
        // ChildrenAsTriggers defaults to true
        assert!(panel.children_as_triggers);

        // Timer defaults: Enabled = true
        let timer = &result.timers[0];
        assert!(timer.enabled);
        assert_eq!(timer.interval_ms, 30000);

        // AsyncPostBackTrigger EventName defaults to "Click" when omitted
        // (this trigger has "Tick" explicitly, so it should be preserved)
        assert_eq!(panel.async_triggers[0].event_name, "Tick");
    }

    // ── Test 10: Format output is well-formed ──

    #[test]
    fn test_format_output_well_formed() {
        let graph = make_graph();
        let aspx = r#"
            <asp:ScriptManager ID="sm" runat="server" EnablePageMethods="true">
                <Services>
                    <asp:ServiceReference Path="~/api/Data.asmx" />
                </Services>
            </asp:ScriptManager>
            <asp:UpdatePanel ID="upPanel1" runat="server" UpdateMode="Conditional">
                <ContentTemplate>
                    <asp:Label ID="lblInfo" runat="server" />
                </ContentTemplate>
                <Triggers>
                    <asp:AsyncPostBackTrigger ControlID="btnGo" EventName="Click" />
                </Triggers>
            </asp:UpdatePanel>
            <asp:UpdateProgress ID="uprog1" runat="server"
                AssociatedUpdatePanelID="upPanel1" DisplayAfter="500" />
        "#;

        let result = analyze_ajax_regions(&graph, "test", "Format.aspx", aspx).unwrap();
        let formatted = format_ajax_region_map(&result);

        assert!(formatted.contains("AJAX Region Map"));
        assert!(formatted.contains("upPanel1"));
        assert!(formatted.contains("Service References"));
        assert!(formatted.contains("~/api/Data.asmx"));
        assert!(formatted.contains("UpdateProgress"));
        assert!(formatted.contains("uprog1"));
        assert!(formatted.contains("Modern Patterns"));
    }
}
