//! jQuery Plugin Ecosystem Inventory
//!
//! Detects jQuery core version, UI widgets, third-party plugins, custom plugins,
//! and deprecated patterns across JS and markup files.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

// ── Output structs ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct JQueryInventory {
    pub core_version: Option<String>,
    pub core_vulnerable: bool,
    pub vulnerability_notes: Vec<String>,
    pub ui_widgets: Vec<JQueryPluginUsage>,
    pub third_party_plugins: Vec<JQueryPluginUsage>,
    pub custom_plugins: Vec<JQueryPluginUsage>,
    pub deprecated_patterns: Vec<JQueryPluginUsage>,
    pub files_analyzed: usize,
    pub total_usages: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct JQueryPluginUsage {
    pub name: String,
    pub file_path: String,
    pub line_number: u32,
    pub modern_equivalent: String,
    pub migration_complexity: String,
}

// ── Regex patterns ───────────────────────────────────────────────────────────

// jQuery version from <script src="...jquery-X.Y.Z...">
static JQUERY_SCRIPT_VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<script[^>]+src\s*=\s*["'][^"']*jquery[.-](\d+\.\d+(?:\.\d+)?)[^"']*["']"#)
        .expect("jquery_script_version")
});

// jQuery version from JS file internal identifier
static JQUERY_INTERNAL_VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)jQuery\s+(?:JavaScript\s+Library\s+)?v?(\d+\.\d+(?:\.\d+)?)")
        .expect("jquery_internal_version")
});

// jQuery UI widgets
static JQUERY_UI_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.\s*(datepicker|dialog|autocomplete|tabs|accordion|sortable|draggable|droppable|slider|progressbar|tooltip)\s*\(")
        .expect("jquery_ui")
});

// Third-party plugins
static DATATABLES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.\s*(?:dataTable|DataTable)\s*\(").expect("datatables"));
static JQUERY_VALIDATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\.\s*validate\s*\(|\$\s*\.\s*validator\b)").expect("jquery_validate")
});
static SELECT2_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.\s*select2\s*\(").expect("select2"));
static CHOSEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.\s*chosen\s*\(").expect("chosen"));
static TOASTR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\btoastr\s*\.\s*(?:success|error|warning|info|clear|remove)\s*\(")
        .expect("toastr")
});
static FANCYBOX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.\s*fancybox\s*\(").expect("fancybox"));
static LIGHTBOX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\.\s*lightbox\s*\(|\blightbox\s*\.\s*init\b)").expect("lightbox")
});
static MASKED_INPUT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.\s*(?:mask|inputmask)\s*\(").expect("masked_input"));
static JQGRID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.\s*jqGrid\s*\(").expect("jqgrid"));
static TINYMCE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\btinymce\s*\.\s*init\s*\(").expect("tinymce"));
static CKEDITOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bCKEDITOR\s*\.\s*(?:replace|inline|ClassicEditor)\s*\(").expect("ckeditor")
});
static SIGNALR_JQUERY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\$\s*\.\s*(?:connection|hubConnection)\b").expect("signalr_jquery")
});
static FILE_UPLOAD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.\s*fileupload\s*\(").expect("file_upload"));

// Inline <script> block extraction from markup files
static INLINE_SCRIPT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<script\b[^>]*>(.+?)</script>").expect("inline_script"));

// Custom plugins
static CUSTOM_PLUGIN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\s*\.\s*fn\s*\.\s*(\w+)\s*=").expect("custom_plugin"));
static WIDGET_FACTORY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\$\s*\.\s*widget\s*\(\s*["'](?:ui\.)?(\w+)["']"#).expect("widget_factory")
});

// Deprecated patterns
static DEPRECATED_LIVE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.\s*live\s*\(").expect("live"));
static DEPRECATED_DELEGATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.\s*delegate\s*\(").expect("delegate"));
static DEPRECATED_BIND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.\s*bind\s*\(").expect("bind"));
static DEPRECATED_READY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\$\s*\(\s*document\s*\)\s*\.\s*ready\s*\(").expect("ready"));
static DEPRECATED_AJAXSETUP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\$\s*\.\s*ajaxSetup\s*\(").expect("ajaxsetup"));
static DEPRECATED_SIZE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.\s*size\s*\(\s*\)").expect("size"));
static DEPRECATED_ANDSELF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.\s*andSelf\s*\(").expect("andself"));
static DEPRECATED_ERROR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.\s*error\s*\(\s*function\b").expect("error_handler"));

static DEPRECATED_PATTERNS: &[(&LazyLock<Regex>, &str, &str, &str)] = &[
    (
        &DEPRECATED_LIVE,
        ".live()",
        "Use .on() with delegation",
        "medium",
    ),
    (
        &DEPRECATED_DELEGATE,
        ".delegate()",
        "Use .on() with delegation",
        "low",
    ),
    (&DEPRECATED_BIND, ".bind()", "Use .on()", "low"),
    (
        &DEPRECATED_READY,
        "$(document).ready()",
        "Use DOMContentLoaded or defer attribute",
        "low",
    ),
    (
        &DEPRECATED_AJAXSETUP,
        "$.ajaxSetup()",
        "Use per-request configuration or Axios interceptors",
        "medium",
    ),
    (&DEPRECATED_SIZE, ".size()", "Use .length property", "low"),
    (&DEPRECATED_ANDSELF, ".andSelf()", "Use .addBack()", "low"),
    (
        &DEPRECATED_ERROR,
        ".error(handler)",
        "Use .on('error', handler)",
        "low",
    ),
];

// ── UI widget table ──────────────────────────────────────────────────────────

struct UiWidgetInfo {
    name: &'static str,
    modern_equivalent: &'static str,
    complexity: &'static str,
}

const UI_WIDGET_TABLE: &[UiWidgetInfo] = &[
    UiWidgetInfo {
        name: "datepicker",
        modern_equivalent: "<input type=\"date\"> or date-fns + custom component",
        complexity: "medium",
    },
    UiWidgetInfo {
        name: "dialog",
        modern_equivalent: "Native <dialog> element or Headless UI Dialog",
        complexity: "low",
    },
    UiWidgetInfo {
        name: "autocomplete",
        modern_equivalent: "React-Select, Downshift, or <datalist>",
        complexity: "medium",
    },
    UiWidgetInfo {
        name: "tabs",
        modern_equivalent: "Headless UI Tabs or CSS-only tabs",
        complexity: "low",
    },
    UiWidgetInfo {
        name: "accordion",
        modern_equivalent: "<details>/<summary> or Headless UI Disclosure",
        complexity: "low",
    },
    UiWidgetInfo {
        name: "sortable",
        modern_equivalent: "dnd-kit or SortableJS",
        complexity: "medium",
    },
    UiWidgetInfo {
        name: "draggable",
        modern_equivalent: "dnd-kit or HTML5 Drag & Drop API",
        complexity: "medium",
    },
    UiWidgetInfo {
        name: "droppable",
        modern_equivalent: "dnd-kit or HTML5 Drag & Drop API",
        complexity: "medium",
    },
    UiWidgetInfo {
        name: "slider",
        modern_equivalent: "Native <input type=\"range\">",
        complexity: "low",
    },
    UiWidgetInfo {
        name: "progressbar",
        modern_equivalent: "Native <progress> element",
        complexity: "low",
    },
    UiWidgetInfo {
        name: "tooltip",
        modern_equivalent: "Tippy.js or CSS title attribute",
        complexity: "low",
    },
];

fn lookup_ui_widget(name: &str) -> (&'static str, &'static str) {
    UI_WIDGET_TABLE
        .iter()
        .find(|w| w.name.eq_ignore_ascii_case(name))
        .map(|w| (w.modern_equivalent, w.complexity))
        .unwrap_or(("Custom replacement needed", "medium"))
}

// ── Third-party plugin table ─────────────────────────────────────────────────

struct ThirdPartyInfo {
    regex: &'static LazyLock<Regex>,
    name: &'static str,
    modern_equivalent: &'static str,
    complexity: &'static str,
}

static THIRD_PARTY_PLUGINS: &[ThirdPartyInfo] = &[
    ThirdPartyInfo {
        regex: &DATATABLES_RE,
        name: "DataTables",
        modern_equivalent: "AG Grid or TanStack Table",
        complexity: "high",
    },
    ThirdPartyInfo {
        regex: &JQUERY_VALIDATE_RE,
        name: "jQuery Validate",
        modern_equivalent: "React Hook Form + Zod, or FluentValidation + DataAnnotations",
        complexity: "medium",
    },
    ThirdPartyInfo {
        regex: &SELECT2_RE,
        name: "Select2",
        modern_equivalent: "React-Select or Blazor InputSelect with search",
        complexity: "medium",
    },
    ThirdPartyInfo {
        regex: &CHOSEN_RE,
        name: "Chosen",
        modern_equivalent: "React-Select or native <select> with search",
        complexity: "medium",
    },
    ThirdPartyInfo {
        regex: &TOASTR_RE,
        name: "Toastr",
        modern_equivalent: "React-Toastify or Blazor Toast component",
        complexity: "low",
    },
    ThirdPartyInfo {
        regex: &FANCYBOX_RE,
        name: "Fancybox",
        modern_equivalent: "CSS-only lightbox or GLightbox",
        complexity: "low",
    },
    ThirdPartyInfo {
        regex: &LIGHTBOX_RE,
        name: "Lightbox",
        modern_equivalent: "CSS-only lightbox or GLightbox",
        complexity: "low",
    },
    ThirdPartyInfo {
        regex: &MASKED_INPUT_RE,
        name: "Masked Input",
        modern_equivalent: "<input pattern> + IMask.js or react-input-mask",
        complexity: "low",
    },
    ThirdPartyInfo {
        regex: &JQGRID_RE,
        name: "jqGrid",
        modern_equivalent: "AG Grid or Blazor DataGrid",
        complexity: "high",
    },
    ThirdPartyInfo {
        regex: &TINYMCE_RE,
        name: "TinyMCE",
        modern_equivalent: "TinyMCE 6+ React/Blazor wrapper (same vendor)",
        complexity: "medium",
    },
    ThirdPartyInfo {
        regex: &CKEDITOR_RE,
        name: "CKEditor",
        modern_equivalent: "CKEditor 5 React/Blazor integration",
        complexity: "medium",
    },
    ThirdPartyInfo {
        regex: &SIGNALR_JQUERY_RE,
        name: "SignalR (jQuery client)",
        modern_equivalent: "@microsoft/signalr (modern JS client)",
        complexity: "medium",
    },
    ThirdPartyInfo {
        regex: &FILE_UPLOAD_RE,
        name: "jQuery File Upload",
        modern_equivalent: "React-Dropzone or native <input type=\"file\"> with Fetch API",
        complexity: "medium",
    },
];

// ── Main detection function ──────────────────────────────────────────────────

/// Build a jQuery ecosystem inventory from JS and markup files.
pub fn build_jquery_inventory(
    js_files: &[(&str, &str)],
    markup_files: &[(&str, &str)],
) -> JQueryInventory {
    let mut inventory = JQueryInventory {
        core_version: None,
        core_vulnerable: false,
        vulnerability_notes: Vec::new(),
        ui_widgets: Vec::new(),
        third_party_plugins: Vec::new(),
        custom_plugins: Vec::new(),
        deprecated_patterns: Vec::new(),
        files_analyzed: 0,
        total_usages: 0,
    };

    // 1. Detect jQuery core version from markup script tags
    for &(_path, content) in markup_files {
        if let Some(cap) = JQUERY_SCRIPT_VERSION_RE.captures(content) {
            let version = cap[1].to_string();
            if inventory.core_version.is_none() {
                inventory.core_version = Some(version);
            }
        }
        inventory.files_analyzed += 1;
    }

    // 2. Detect jQuery version from JS file internal identifier
    for &(_path, content) in js_files {
        if inventory.core_version.is_none()
            && let Some(cap) = JQUERY_INTERNAL_VERSION_RE.captures(content) {
                inventory.core_version = Some(cap[1].to_string());
            }
    }

    // 3. Check vulnerability status
    if let Some(ref version) = inventory.core_version {
        let (vulnerable, notes) = check_vulnerability(version);
        inventory.core_vulnerable = vulnerable;
        inventory.vulnerability_notes = notes;
    }

    // 4. Scan all JS files for plugins, widgets, custom plugins, deprecated patterns
    for &(path, content) in js_files {
        inventory.files_analyzed += 1;
        scan_js_file(path, content, &mut inventory);
    }

    // 5. Scan inline <script> blocks in markup files for plugin usage
    for &(path, content) in markup_files {
        for cap in INLINE_SCRIPT_RE.captures_iter(content) {
            let script_body = &cap[1];
            // Skip script tags that only have a src attribute (external scripts)
            if script_body.trim().is_empty() {
                continue;
            }
            scan_js_file(path, script_body, &mut inventory);
        }
    }

    inventory.total_usages = inventory.ui_widgets.len()
        + inventory.third_party_plugins.len()
        + inventory.custom_plugins.len()
        + inventory.deprecated_patterns.len();

    inventory
}

fn scan_js_file(path: &str, content: &str, inventory: &mut JQueryInventory) {
    // jQuery UI widgets
    for cap in JQUERY_UI_RE.captures_iter(content) {
        let widget_name = &cap[1];
        let Some(whole) = cap.get(0) else { continue };
        let line = line_number(content, whole.start());
        let (modern, complexity) = lookup_ui_widget(widget_name);
        inventory.ui_widgets.push(JQueryPluginUsage {
            name: widget_name.to_string(),
            file_path: path.to_string(),
            line_number: line,
            modern_equivalent: modern.to_string(),
            migration_complexity: complexity.to_string(),
        });
    }

    // Third-party plugins
    for plugin in THIRD_PARTY_PLUGINS {
        for m in plugin.regex.find_iter(content) {
            let line = line_number(content, m.start());
            inventory.third_party_plugins.push(JQueryPluginUsage {
                name: plugin.name.to_string(),
                file_path: path.to_string(),
                line_number: line,
                modern_equivalent: plugin.modern_equivalent.to_string(),
                migration_complexity: plugin.complexity.to_string(),
            });
        }
    }

    // Custom plugins: $.fn.pluginName = ...
    for cap in CUSTOM_PLUGIN_RE.captures_iter(content) {
        let plugin_name = &cap[1];
        let Some(whole) = cap.get(0) else { continue };
        let line = line_number(content, whole.start());
        inventory.custom_plugins.push(JQueryPluginUsage {
            name: plugin_name.to_string(),
            file_path: path.to_string(),
            line_number: line,
            modern_equivalent: "Custom — requires manual migration analysis".to_string(),
            migration_complexity: "high".to_string(),
        });
    }

    // Widget factory: $.widget("ui.pluginName", ...)
    for cap in WIDGET_FACTORY_RE.captures_iter(content) {
        let plugin_name = &cap[1];
        let Some(whole) = cap.get(0) else { continue };
        let line = line_number(content, whole.start());
        inventory.custom_plugins.push(JQueryPluginUsage {
            name: format!("{plugin_name} (widget factory)"),
            file_path: path.to_string(),
            line_number: line,
            modern_equivalent: "Custom — requires manual migration analysis".to_string(),
            migration_complexity: "high".to_string(),
        });
    }

    // Deprecated patterns
    for &(regex, name, modern, complexity) in DEPRECATED_PATTERNS {
        for m in regex.find_iter(content) {
            let line = line_number(content, m.start());
            inventory.deprecated_patterns.push(JQueryPluginUsage {
                name: name.to_string(),
                file_path: path.to_string(),
                line_number: line,
                modern_equivalent: modern.to_string(),
                migration_complexity: complexity.to_string(),
            });
        }
    }
}

// ── Version vulnerability check ──────────────────────────────────────────────

fn check_vulnerability(version: &str) -> (bool, Vec<String>) {
    let parts: Vec<u32> = version.split('.').filter_map(|p| p.parse().ok()).collect();
    let (major, minor, _patch) = (
        parts.first().copied().unwrap_or(0),
        parts.get(1).copied().unwrap_or(0),
        parts.get(2).copied().unwrap_or(0),
    );

    let mut vulnerable = false;
    let mut notes = Vec::new();

    // jQuery < 1.12.0: Multiple XSS vulnerabilities
    if major < 1 || (major == 1 && minor < 12) {
        vulnerable = true;
        notes.push(format!(
            "jQuery {version} has known XSS vulnerabilities (CVE-2015-9251). Upgrade to 3.5+."
        ));
    }

    // jQuery < 3.5.0: prototype pollution via jQuery.htmlPrefilter
    if major < 3 || (major == 3 && minor < 5) {
        vulnerable = true;
        notes.push(format!(
            "jQuery {version} vulnerable to prototype pollution via htmlPrefilter (CVE-2020-11022/11023). Upgrade to 3.5+."
        ));
    }

    (vulnerable, notes)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn line_number(content: &str, byte_offset: usize) -> u32 {
    (content[..byte_offset.min(content.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1) as u32
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory_js(js: &str) -> JQueryInventory {
        build_jquery_inventory(&[("app.js", js)], &[])
    }

    fn inventory_markup(markup: &str) -> JQueryInventory {
        build_jquery_inventory(&[], &[("Default.aspx", markup)])
    }

    // ── Version detection ────────────────────────────────────────────────

    #[test]
    fn detect_version_from_script_tag() {
        let markup = r#"<script src="Scripts/jquery-1.12.4.min.js"></script>"#;
        let inv = inventory_markup(markup);
        assert_eq!(inv.core_version.as_deref(), Some("1.12.4"));
    }

    #[test]
    fn detect_version_from_cdn() {
        let markup = r#"<script src="https://code.jquery.com/jquery-3.6.0.min.js"></script>"#;
        let inv = inventory_markup(markup);
        assert_eq!(inv.core_version.as_deref(), Some("3.6.0"));
    }

    #[test]
    fn detect_version_from_js_content() {
        let js = r"/*! jQuery JavaScript Library v2.2.4 */";
        let inv = inventory_js(js);
        assert_eq!(inv.core_version.as_deref(), Some("2.2.4"));
    }

    #[test]
    fn vulnerability_old_jquery() {
        let markup = r#"<script src="jquery-1.8.3.min.js"></script>"#;
        let inv = inventory_markup(markup);
        assert!(inv.core_vulnerable);
        assert!(!inv.vulnerability_notes.is_empty());
    }

    #[test]
    fn no_vulnerability_modern_jquery() {
        let markup = r#"<script src="jquery-3.7.1.min.js"></script>"#;
        let inv = inventory_markup(markup);
        assert!(!inv.core_vulnerable);
    }

    // ── jQuery UI widgets ────────────────────────────────────────────────

    #[test]
    fn detect_datepicker() {
        let js = "$(\"#startDate\").datepicker({ dateFormat: 'yy-mm-dd' });";
        let inv = inventory_js(js);
        assert_eq!(inv.ui_widgets.len(), 1);
        assert_eq!(inv.ui_widgets[0].name, "datepicker");
    }

    #[test]
    fn detect_dialog() {
        let js = "$(\"#confirmDialog\").dialog({ modal: true });";
        let inv = inventory_js(js);
        assert_eq!(inv.ui_widgets.len(), 1);
        assert_eq!(inv.ui_widgets[0].name, "dialog");
    }

    #[test]
    fn detect_autocomplete() {
        let js = "$('.search-input').autocomplete({ source: items });";
        let inv = inventory_js(js);
        assert_eq!(inv.ui_widgets.len(), 1);
        assert_eq!(inv.ui_widgets[0].name, "autocomplete");
    }

    #[test]
    fn detect_tabs() {
        let js = "$(\"#myTabs\").tabs();";
        let inv = inventory_js(js);
        assert_eq!(inv.ui_widgets.len(), 1);
        assert_eq!(inv.ui_widgets[0].name, "tabs");
    }

    #[test]
    fn detect_accordion() {
        let js = "$(\"#faq\").accordion({ collapsible: true });";
        let inv = inventory_js(js);
        assert_eq!(inv.ui_widgets.len(), 1);
        assert_eq!(inv.ui_widgets[0].name, "accordion");
    }

    #[test]
    fn detect_sortable() {
        let js = "$('.list').sortable({ connectWith: '.connected' });";
        let inv = inventory_js(js);
        assert_eq!(inv.ui_widgets.len(), 1);
        assert_eq!(inv.ui_widgets[0].name, "sortable");
    }

    #[test]
    fn detect_slider() {
        let js = "$(\"#slider\").slider({ min: 0, max: 100 });";
        let inv = inventory_js(js);
        assert_eq!(inv.ui_widgets.len(), 1);
        assert_eq!(inv.ui_widgets[0].name, "slider");
    }

    #[test]
    fn detect_tooltip() {
        let js = "$('.info').tooltip();";
        let inv = inventory_js(js);
        assert_eq!(inv.ui_widgets.len(), 1);
        assert_eq!(inv.ui_widgets[0].name, "tooltip");
    }

    // ── Third-party plugins ──────────────────────────────────────────────

    #[test]
    fn detect_datatables() {
        let js = r#"$('#gridResults').DataTable({ paging: true });"#;
        let inv = inventory_js(js);
        assert!(
            inv.third_party_plugins
                .iter()
                .any(|p| p.name == "DataTables")
        );
    }

    #[test]
    fn detect_jquery_validate() {
        let js = r#"$('#myForm').validate({ rules: { email: { required: true } } });"#;
        let inv = inventory_js(js);
        assert!(
            inv.third_party_plugins
                .iter()
                .any(|p| p.name == "jQuery Validate")
        );
    }

    #[test]
    fn detect_select2() {
        let js = r#"$('.state-selector').select2({ placeholder: 'Choose' });"#;
        let inv = inventory_js(js);
        assert!(inv.third_party_plugins.iter().any(|p| p.name == "Select2"));
    }

    #[test]
    fn detect_toastr() {
        let js = "toastr.success('Record saved successfully');";
        let inv = inventory_js(js);
        assert!(inv.third_party_plugins.iter().any(|p| p.name == "Toastr"));
    }

    #[test]
    fn detect_masked_input() {
        let js = r#"$('#phone').mask('(999) 999-9999');"#;
        let inv = inventory_js(js);
        assert!(
            inv.third_party_plugins
                .iter()
                .any(|p| p.name == "Masked Input")
        );
    }

    #[test]
    fn detect_jqgrid() {
        let js = r#"$('#parcelGrid').jqGrid({ url: '/api/parcels' });"#;
        let inv = inventory_js(js);
        assert!(inv.third_party_plugins.iter().any(|p| p.name == "jqGrid"));
    }

    #[test]
    fn detect_tinymce() {
        let js = r#"tinymce.init({ selector: '#editor' });"#;
        let inv = inventory_js(js);
        assert!(inv.third_party_plugins.iter().any(|p| p.name == "TinyMCE"));
    }

    #[test]
    fn detect_signalr_jquery() {
        let js = r#"var connection = $.connection.chatHub;"#;
        let inv = inventory_js(js);
        assert!(
            inv.third_party_plugins
                .iter()
                .any(|p| p.name == "SignalR (jQuery client)")
        );
    }

    // ── Custom plugins ───────────────────────────────────────────────────

    #[test]
    fn detect_custom_fn_plugin() {
        let js = r#"
$.fn.parcelHighlight = function(options) {
    return this.each(function() {
        $(this).css('background', options.color);
    });
};
"#;
        let inv = inventory_js(js);
        assert_eq!(inv.custom_plugins.len(), 1);
        assert_eq!(inv.custom_plugins[0].name, "parcelHighlight");
    }

    #[test]
    fn detect_widget_factory_plugin() {
        let js = "$.widget('ui.mapControls', { _create: function() {} });";
        let inv = inventory_js(js);
        assert!(
            inv.custom_plugins
                .iter()
                .any(|p| p.name.contains("mapControls"))
        );
    }

    // ── Deprecated patterns ──────────────────────────────────────────────

    #[test]
    fn detect_live() {
        let js = r#"$('.row').live('click', handler);"#;
        let inv = inventory_js(js);
        assert!(inv.deprecated_patterns.iter().any(|p| p.name == ".live()"));
    }

    #[test]
    fn detect_delegate() {
        let js = r#"$('#table').delegate('td', 'click', handler);"#;
        let inv = inventory_js(js);
        assert!(
            inv.deprecated_patterns
                .iter()
                .any(|p| p.name == ".delegate()")
        );
    }

    #[test]
    fn detect_bind() {
        let js = r#"$('#btn').bind('click', handler);"#;
        let inv = inventory_js(js);
        assert!(inv.deprecated_patterns.iter().any(|p| p.name == ".bind()"));
    }

    #[test]
    fn detect_document_ready() {
        let js = r#"$(document).ready(function() { init(); });"#;
        let inv = inventory_js(js);
        assert!(
            inv.deprecated_patterns
                .iter()
                .any(|p| p.name == "$(document).ready()")
        );
    }

    #[test]
    fn detect_ajaxsetup() {
        let js = r#"$.ajaxSetup({ headers: { 'X-Token': token } });"#;
        let inv = inventory_js(js);
        assert!(
            inv.deprecated_patterns
                .iter()
                .any(|p| p.name == "$.ajaxSetup()")
        );
    }

    // ── Edge cases ───────────────────────────────────────────────────────

    #[test]
    fn no_jquery_present() {
        let js = "function init() { document.getElementById('x'); }";
        let inv = inventory_js(js);
        assert_eq!(inv.total_usages, 0);
        assert!(inv.core_version.is_none());
    }

    #[test]
    fn multiple_plugins_in_one_file() {
        let js = "\
$('#grid').DataTable({ paging: true });\n\
$('#date').datepicker();\n\
$('#confirm').dialog({ modal: true });\n\
$(document).ready(function() {});\n\
$('.search').select2();\n\
toastr.success('done');\n";
        let inv = inventory_js(js);
        assert!(inv.total_usages >= 6);
    }

    #[test]
    fn line_numbers_are_accurate() {
        let js = "line1\nline2\n$('#x').datepicker();\nline4";
        let inv = inventory_js(js);
        assert_eq!(inv.ui_widgets[0].line_number, 3);
    }

    #[test]
    fn detect_plugins_in_inline_script_blocks() {
        let markup = r#"
<html>
<head>
<script src="jquery-3.6.0.min.js"></script>
</head>
<body>
<script>
$(document).ready(function() {
    $('#grid').DataTable({ paging: true });
    $('#start').datepicker();
});
</script>
</body>
</html>"#;
        let inv = build_jquery_inventory(&[], &[("Default.aspx", markup)]);
        assert_eq!(inv.core_version.as_deref(), Some("3.6.0"));
        assert!(
            inv.third_party_plugins
                .iter()
                .any(|p| p.name == "DataTables"),
            "should detect DataTables in inline script"
        );
        assert!(
            inv.ui_widgets.iter().any(|w| w.name == "datepicker"),
            "should detect datepicker in inline script"
        );
        assert!(
            inv.deprecated_patterns
                .iter()
                .any(|p| p.name == "$(document).ready()"),
            "should detect deprecated ready in inline script"
        );
    }
}
