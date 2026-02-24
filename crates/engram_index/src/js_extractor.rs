/// JavaScript → ASP.NET bridge extractor.
///
/// Scans `.js` files for patterns that reference server-side ASP.NET controls
/// and endpoints, emitting edges that link the client-side layer to the
/// server-side graph nodes already produced by `webforms.rs` and
/// `config_extractor.rs`.
///
/// **Feature 3 — Frontend Bridge (JS to ASP.NET Control Mapper)**
///   - jQuery `$("[id$='CtrlId']")` / `$("[id$=CtrlId]")` selectors
///   - ASP.NET inline `<%= Ctrl.ClientID %>` expressions embedded in JS
///   - `__doPostBack('CtrlId', ...)` postback hijacks
///
/// **Feature 4 — Legacy AJAX & WebMethod Mapper**
///   - `$.ajax({ url: '...' })` calls
///   - `$.get('...')` / `$.post('...')` shorthand
///   - `fetch('...')` calls (modern JS in legacy codebases)
///   - `XMLHttpRequest.open('...', '...')` raw XHR calls
///   - `PageMethods.MethodName(...)` ASP.NET AJAX ScriptManager calls
use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

/// Maximum source size we'll run regex extraction on (5 MiB).
/// Minified vendor bundles can be huge; we skip them to keep indexing bounded.
const MAX_JS_SOURCE_BYTES: usize = 5 * 1024 * 1024;

// ── Static Regex Definitions (Compiled Once) ────────────────────────────────

// Feature 3: Frontend Bridge patterns
static JQUERY_ENDS_WITH_RE: OnceLock<Regex> = OnceLock::new();
static ASP_CLIENT_ID_RE: OnceLock<Regex> = OnceLock::new();
static POSTBACK_RE: OnceLock<Regex> = OnceLock::new();
static DOCUMENT_GETELEMENTBYID_RE: OnceLock<Regex> = OnceLock::new();

// Feature 4: AJAX / WebMethod patterns
static AJAX_CALL_RE: OnceLock<Regex> = OnceLock::new();
static AJAX_SHORTHAND_RE: OnceLock<Regex> = OnceLock::new();
static FETCH_CALL_RE: OnceLock<Regex> = OnceLock::new();
static XHR_OPEN_RE: OnceLock<Regex> = OnceLock::new();
static PAGE_METHODS_RE: OnceLock<Regex> = OnceLock::new();

// Feature 5: GIS / Spatial logic patterns
static GOOGLE_MAPS_RE: OnceLock<Regex> = OnceLock::new();
static LEAFLET_RE: OnceLock<Regex> = OnceLock::new();
static OPENLAYERS_RE: OnceLock<Regex> = OnceLock::new();
static GIS_API_KEY_RE: OnceLock<Regex> = OnceLock::new();
static GIS_ZOOM_RE: OnceLock<Regex> = OnceLock::new();
static GIS_CENTER_RE: OnceLock<Regex> = OnceLock::new();
static CTL00_ID_RE: OnceLock<Regex> = OnceLock::new();

// Phase 30 Gap 7: GIS deep extraction patterns
#[allow(dead_code)]
static LEAFLET_TILE_LAYER_RE: OnceLock<Regex> = OnceLock::new();
static LEAFLET_WMS_RE: OnceLock<Regex> = OnceLock::new();
static LEAFLET_GEOJSON_RE: OnceLock<Regex> = OnceLock::new();
static LEAFLET_MARKER_CLUSTER_RE: OnceLock<Regex> = OnceLock::new();
static LEAFLET_DRAW_RE: OnceLock<Regex> = OnceLock::new();
static LEAFLET_CRS_RE: OnceLock<Regex> = OnceLock::new();
static OL_PROJ_RE: OnceLock<Regex> = OnceLock::new();
static OL_DRAW_RE: OnceLock<Regex> = OnceLock::new();
static GMAPS_GEOCODER_RE: OnceLock<Regex> = OnceLock::new();
static GMAPS_DRAWING_RE: OnceLock<Regex> = OnceLock::new();
static GEOCODE_URL_RE: OnceLock<Regex> = OnceLock::new();
static ESRI_AMD_RE: OnceLock<Regex> = OnceLock::new();
static ESRI_ES_RE: OnceLock<Regex> = OnceLock::new();
static ESRI_REST_RE: OnceLock<Regex> = OnceLock::new();
static ESRI_DOJO_RE: OnceLock<Regex> = OnceLock::new();
static TILE_URL_RE: OnceLock<Regex> = OnceLock::new();

fn get_compiled_regex<'a>(
    lock: &'a OnceLock<Regex>,
    pattern: &str,
    label: &str,
) -> Option<&'a Regex> {
    if let Some(re) = lock.get() {
        return Some(re);
    }
    match Regex::new(pattern) {
        Ok(re) => Some(lock.get_or_init(|| re)),
        Err(err) => {
            tracing::error!("failed to compile {label} regex: {err}");
            None
        }
    }
}

// ── Line-number helper ──────────────────────────────────────────────────────

/// Build a byte-offset → 1-based line number lookup using memchr.
fn build_line_index(source: &str) -> Vec<usize> {
    let bytes = source.as_bytes();
    let mut line_starts: Vec<usize> = vec![0];
    let mut pos = 0;
    while pos < bytes.len() {
        if let Some(nl) = memchr::memchr(b'\n', &bytes[pos..]) {
            line_starts.push(pos + nl + 1);
            pos += nl + 1;
        } else {
            break;
        }
    }
    line_starts
}

/// Return 1-based line number for byte offset.
fn line_of(line_starts: &[usize], byte_offset: usize) -> u32 {
    match line_starts.binary_search(&byte_offset) {
        Ok(idx) => (idx + 1) as u32,
        Err(idx) => idx as u32,
    }
}

// ── Relative-path derivation ────────────────────────────────────────────────

/// Derive a display-friendly relative path from the file's `Path`.
/// Returns the file name with extension (e.g., `"Scripts/map_utils.js"`).
fn source_file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown.js")
        .to_string()
}

// ── URL path splitting ──────────────────────────────────────────────────────

/// Split an AJAX URL into `(service_path, method_name)`.
///
/// Examples:
///   `"Services/MapData.asmx/GetPolygons"` → `("Services/MapData.asmx", Some("GetPolygons"))`
///   `"api/Patients/Search"` → `("api/Patients/Search", None)` (no known service ext)
///   `"../Handlers/Export.ashx"` → `("Handlers/Export.ashx", None)` (no method suffix)
fn split_service_url(raw_url: &str) -> (String, Option<String>) {
    // Strip query string and fragment
    let url = raw_url.split('?').next().unwrap_or(raw_url);
    let url = url.split('#').next().unwrap_or(url);
    // Normalise leading ../ and ./ segments
    let url = url.trim_start_matches("../").trim_start_matches("./");
    // Strip leading ~/
    let url = url.strip_prefix("~/").unwrap_or(url);
    // Strip leading /
    let url = url.strip_prefix('/').unwrap_or(url);

    // Known service extensions that can have a /MethodName suffix
    let service_exts = [".asmx", ".aspx", ".svc", ".ashx"];
    let lower = url.to_lowercase();

    for ext in &service_exts {
        if let Some(ext_pos) = lower.find(ext) {
            let after_ext = ext_pos + ext.len();
            if after_ext < url.len() && url.as_bytes()[after_ext] == b'/' {
                let path_part = &url[..after_ext];
                let method_part = &url[after_ext + 1..];
                if !method_part.is_empty() && !method_part.contains('/') {
                    return (path_part.to_string(), Some(method_part.to_string()));
                }
            }
            // Extension found but no /Method suffix
            return (url[..after_ext].to_string(), None);
        }
    }

    // No known service extension — could be a Web API route or page method
    // Try splitting on last '/' if the final segment looks like a method name
    // (PascalCase, no dots, no extension)
    if let Some(slash_pos) = url.rfind('/') {
        let tail = &url[slash_pos + 1..];
        if !tail.is_empty()
            && !tail.contains('.')
            && tail.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        {
            return (url[..slash_pos].to_string(), Some(tail.to_string()));
        }
    }

    (url.to_string(), None)
}

// ── Core extraction ─────────────────────────────────────────────────────────

/// Extract ASP.NET-related edges from a JavaScript source file.
///
/// This runs **in addition to** the default tree-sitter JS extraction (which
/// captures functions, classes, imports, and call edges). The JS extractor
/// focuses exclusively on cross-boundary edges that tree-sitter cannot detect:
/// jQuery control selectors, `__doPostBack`, and AJAX service calls.
pub fn extract_js(path: &Path, source: &str) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut edges = Vec::new();
    let mut syms = Vec::new();

    if source.len() > MAX_JS_SOURCE_BYTES {
        tracing::warn!(
            "js_extractor: skipping {} ({}B > {}B limit)",
            path.display(),
            source.len(),
            MAX_JS_SOURCE_BYTES
        );
        return (syms, edges);
    }

    let line_starts = build_line_index(source);
    let file_name = source_file_name(path);

    // ── Feature 3: DOM manipulation edges ───────────────────────────────

    extract_jquery_selectors(source, &line_starts, &file_name, &mut edges);
    extract_asp_client_ids(source, &line_starts, &file_name, &mut edges);
    extract_postbacks(source, &line_starts, &file_name, &mut edges);
    extract_getelementbyid(source, &line_starts, &file_name, &mut edges);

    // ── Feature 4: AJAX / WebMethod edges ───────────────────────────────

    extract_ajax_calls(source, &line_starts, &file_name, &mut edges);
    extract_ajax_shorthand(source, &line_starts, &file_name, &mut edges);
    extract_fetch_calls(source, &line_starts, &file_name, &mut edges);
    extract_xhr_calls(source, &line_starts, &file_name, &mut edges);
    extract_page_methods(source, &line_starts, &file_name, &mut edges);

    // ── Feature 5: GIS / Spatial logic edges ─────────────────────────────

    extract_google_maps(source, &line_starts, &file_name, &mut edges, &mut syms);
    extract_leaflet(source, &line_starts, &file_name, &mut edges);
    extract_openlayers(source, &line_starts, &file_name, &mut edges);
    extract_gis_configs(source, &line_starts, &file_name, &mut edges, &mut syms);
    extract_ctl00_references(source, &line_starts, &file_name, &mut edges);

    // Phase 30 Gap 7: GIS deep extraction
    extract_gis_layer_inventory(source, &line_starts, &file_name, &mut edges, &mut syms);
    extract_esri_arcgis(source, &line_starts, &file_name, &mut edges, &mut syms);

    // Deduplicate: same (source, target, kind) triple should only appear once.
    dedup_edges(&mut edges);

    (syms, edges)
}

/// Check if a file extension is a JavaScript file that should have the
/// JS bridge extractor run on it.
pub fn is_js_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref(),
        Some("js")
    )
}

// ── Feature 3 extractors ────────────────────────────────────────────────────

/// jQuery `$("[id$='CtrlId']")` and `$("[id$=CtrlId]")` (ends-with selector).
///
/// Matches both single and double quotes, and the optional `$=` vs `$='`:
///   `$("[id$='txtSearch']")`
///   `$('[id$=txtSearch]')`
///   `$("[id$=\"btnSubmit\"]")`
fn extract_jquery_selectors(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &JQUERY_ENDS_WITH_RE,
        r#"(?i)\$\(\s*['"](?:[^'"]*?)id\$?=['"]?(?P<ctrl_id>[A-Za-z0-9_]+)['"]?"#,
        "JQUERY_ENDS_WITH",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let ctrl_id = &cap["ctrl_id"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);

        edges.push(ExtractedEdge {
            source_name: file_name.to_string(),
            source_kind: "file",
            source_start_line: line,
            source_language: "javascript",
            target_name: ctrl_id.to_string(),
            target_kind: Some("control"),
            target_start_line: None,
            kind: "manipulates_dom",
            metadata: Some(HashMap::from([(
                "selector_type".into(),
                "jquery_ends_with".into(),
            )])),
        });
    }
}

/// Inline `<%= CtrlId.ClientID %>` expressions embedded inside JS strings.
///
/// These appear in inline `<script>` blocks inside ASPX pages, but also in
/// external `.js` files that are generated server-side or use server-side
/// includes.
fn extract_asp_client_ids(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &ASP_CLIENT_ID_RE,
        r#"(?i)<%=\s*(?P<ctrl_id>[A-Za-z0-9_]+)\.ClientID\s*%>"#,
        "ASP_CLIENT_ID",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let ctrl_id = &cap["ctrl_id"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);

        edges.push(ExtractedEdge {
            source_name: file_name.to_string(),
            source_kind: "file",
            source_start_line: line,
            source_language: "javascript",
            target_name: ctrl_id.to_string(),
            target_kind: Some("control"),
            target_start_line: None,
            kind: "manipulates_dom",
            metadata: Some(HashMap::from([(
                "selector_type".into(),
                "asp_client_id".into(),
            )])),
        });
    }
}

/// `__doPostBack('CtrlUniqueId', 'arg')` postback hijack calls.
///
/// The first argument to `__doPostBack` is the UniqueID of the control
/// (e.g., `"ctl00$MainContent$gvPatients"` or `"btnRefresh"`).
fn extract_postbacks(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &POSTBACK_RE,
        r#"(?i)__doPostBack\s*\(\s*['"](?P<ctrl_id>[^'"]+)['"]"#,
        "POSTBACK",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let ctrl_id = &cap["ctrl_id"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);

        // Extract the short control name (last segment after $)
        let short_id = ctrl_id.rsplit('$').next().unwrap_or(ctrl_id);

        edges.push(ExtractedEdge {
            source_name: file_name.to_string(),
            source_kind: "file",
            source_start_line: line,
            source_language: "javascript",
            target_name: short_id.to_string(),
            target_kind: Some("control"),
            target_start_line: None,
            kind: "triggers_postback",
            metadata: Some(HashMap::from([("unique_id".into(), ctrl_id.to_string())])),
        });
    }
}

/// `document.getElementById('CtrlId')` or `document.getElementById('<%= Ctrl.ClientID %>')`.
///
/// Falls back to the raw string ID when the ASP expression is not present.
fn extract_getelementbyid(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &DOCUMENT_GETELEMENTBYID_RE,
        r#"(?i)document\.getElementById\s*\(\s*['"](?P<ctrl_id>[A-Za-z0-9_]+)['"]"#,
        "DOCUMENT_GETELEMENTBYID",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let ctrl_id = &cap["ctrl_id"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);

        edges.push(ExtractedEdge {
            source_name: file_name.to_string(),
            source_kind: "file",
            source_start_line: line,
            source_language: "javascript",
            target_name: ctrl_id.to_string(),
            target_kind: Some("control"),
            target_start_line: None,
            kind: "manipulates_dom",
            metadata: Some(HashMap::from([(
                "selector_type".into(),
                "getelementbyid".into(),
            )])),
        });
    }
}

// ── Feature 4 extractors ────────────────────────────────────────────────────

/// `$.ajax({ url: 'Services/MapData.asmx/GetPolygons', ... })`.
///
/// Matches both `url:` and `url :` with any whitespace, and single/double quotes.
fn extract_ajax_calls(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &AJAX_CALL_RE,
        r#"(?i)\$\.ajax\(\s*\{[^}]*url\s*:\s*['"](?P<url>[^'"]+)['"]"#,
        "AJAX_CALL",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let url = &cap["url"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);
        emit_ajax_edge(edges, file_name, line, url, "jquery_ajax");
    }
}

/// `$.get('url')` / `$.post('url')` / `$.getJSON('url')`.
fn extract_ajax_shorthand(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &AJAX_SHORTHAND_RE,
        r#"(?i)\$\.(?:get|post|getJSON)\(\s*['"](?P<url>[^'"]+)['"]"#,
        "AJAX_SHORTHAND",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let url = &cap["url"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);
        emit_ajax_edge(edges, file_name, line, url, "jquery_shorthand");
    }
}

/// `fetch('url')` / `fetch("url")`.
fn extract_fetch_calls(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &FETCH_CALL_RE,
        r#"(?i)\bfetch\(\s*['"](?P<url>[^'"]+)['"]"#,
        "FETCH_CALL",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let url = &cap["url"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);
        emit_ajax_edge(edges, file_name, line, url, "fetch");
    }
}

/// `xhr.open('GET', 'url')` / `new XMLHttpRequest(); ... xhr.open('POST', 'url')`.
fn extract_xhr_calls(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &XHR_OPEN_RE,
        r#"(?i)\.open\(\s*['"](?:GET|POST|PUT|DELETE|PATCH)['"],\s*['"](?P<url>[^'"]+)['"]"#,
        "XHR_OPEN",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let url = &cap["url"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);
        emit_ajax_edge(edges, file_name, line, url, "xhr");
    }
}

/// `PageMethods.MethodName(args, onSuccess, onFailure)` — ASP.NET AJAX ScriptManager.
///
/// These are generated by `<asp:ScriptManager EnablePageMethods="true" />` and
/// route to `[WebMethod]`-decorated static methods on the page's code-behind.
fn extract_page_methods(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &PAGE_METHODS_RE,
        r#"(?i)PageMethods\.(?P<method>[A-Za-z_][A-Za-z0-9_]*)\s*\("#,
        "PAGE_METHODS",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let method_name = &cap["method"];
        let byte_offset = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_of(line_starts, byte_offset);

        let mut meta = HashMap::with_capacity(2);
        meta.insert("ajax_transport".into(), "page_methods".into());
        meta.insert("ajax_target_method".into(), method_name.to_string());

        edges.push(ExtractedEdge {
            source_name: file_name.to_string(),
            source_kind: "file",
            source_start_line: line,
            source_language: "javascript",
            target_name: method_name.to_string(),
            target_kind: Some("function"),
            target_start_line: None,
            kind: "api_call",
            metadata: Some(meta),
        });
    }
}

// ── Shared AJAX edge emitter ────────────────────────────────────────────────

/// Emit an `api_call` edge from a JS file to a resolved service path.
///
/// Splits the URL into `(path, method)` using `split_service_url`, determines
/// the target kind based on the path extension, and attaches `ajax_target_method`
/// metadata when a method name is extracted.
fn emit_ajax_edge(
    edges: &mut Vec<ExtractedEdge>,
    file_name: &str,
    line: u32,
    raw_url: &str,
    transport: &str,
) {
    // Skip data URIs, javascript:, and anchors
    let lower = raw_url.to_lowercase();
    if lower.starts_with("data:")
        || lower.starts_with("javascript:")
        || lower.starts_with('#')
        || lower.is_empty()
    {
        return;
    }

    let (path_part, method_part) = split_service_url(raw_url);

    // Determine target_kind from path extension
    let path_lower = path_part.to_lowercase();
    let target_kind = if path_lower.ends_with(".asmx") {
        "web_service"
    } else if path_lower.ends_with(".ashx") {
        "http_handler"
    } else if path_lower.ends_with(".svc") {
        "wcf_service"
    } else if path_lower.ends_with(".aspx") {
        "page"
    } else {
        "endpoint"
    };

    let mut meta = HashMap::with_capacity(3);
    meta.insert("ajax_transport".into(), transport.into());
    meta.insert("ajax_url".into(), raw_url.to_string());
    if let Some(ref method) = method_part {
        meta.insert("ajax_target_method".into(), method.clone());
    }

    edges.push(ExtractedEdge {
        source_name: file_name.to_string(),
        source_kind: "file",
        source_start_line: line,
        source_language: "javascript",
        target_name: path_part,
        target_kind: Some(target_kind),
        target_start_line: None,
        kind: "api_call",
        metadata: Some(meta),
    });
}

// ── Feature 5: GIS / Spatial Logic ──────────────────────────────────────────

/// Map a GIS library class to its modern React-based equivalent.
fn modern_gis_equivalent(library: &str, class: &str) -> &'static str {
    let cls_lower = class.to_lowercase();
    match (library, cls_lower.as_str()) {
        ("google_maps", "latlng") => "React: google-map-react coords prop",
        ("google_maps", "map") => "React: @react-google-maps/api GoogleMap",
        ("google_maps", "marker") => "React: @react-google-maps/api Marker",
        ("google_maps", "infowindow") => "React: @react-google-maps/api InfoWindow",
        ("google_maps", "geocoder") => "React: @googlemaps/js-api-loader Geocoder",
        ("google_maps", "directionsservice") => "React: @react-google-maps/api DirectionsService",
        ("leaflet", "map") => "React: react-leaflet MapContainer",
        ("leaflet", "marker") => "React: react-leaflet Marker",
        ("leaflet", "tilelayer") => "React: react-leaflet TileLayer",
        ("leaflet", "circle") => "React: react-leaflet Circle",
        ("leaflet", "polygon") => "React: react-leaflet Polygon",
        ("leaflet", "popup") => "React: react-leaflet Popup",
        ("openlayers", "map") => "React: rlayers/RMap",
        ("openlayers", "view") => "React: rlayers/RMap defaultView prop",
        ("openlayers", "feature") => "React: rlayers/RFeature",
        ("openlayers", "overlay") => "React: rlayers/ROverlay",
        ("openlayers", "geolocation") => "React: rlayers with navigator.geolocation",
        _ => "Manual migration analysis required",
    }
}

/// Emit a spatial_call edge.
fn emit_spatial_edge(
    file_name: &str,
    line: u32,
    library: &str,
    class: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let mut meta = HashMap::with_capacity(3);
    meta.insert("gis_library".into(), library.into());
    meta.insert("map_class".into(), class.into());
    meta.insert(
        "modern_equivalent".into(),
        modern_gis_equivalent(library, class).into(),
    );

    edges.push(ExtractedEdge {
        source_name: file_name.to_string(),
        source_kind: "file",
        source_start_line: line,
        source_language: "javascript",
        target_name: format!("gis:{}:{}", library, class.to_lowercase()),
        target_kind: Some("gis_config"),
        target_start_line: None,
        kind: "spatial_call",
        metadata: Some(meta),
    });
}

/// Detect Google Maps API usage: `new google.maps.LatLng(`, `google.maps.event.addListener`
fn extract_google_maps(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
    syms: &mut Vec<ExtractedSymbol>,
) {
    let re = match get_compiled_regex(
        &GOOGLE_MAPS_RE,
        r"(?i)new\s+google\.maps\.(?P<cls>LatLng|LatLngBounds|Map|Marker|InfoWindow|Geocoder|DirectionsService|DirectionsRenderer|DistanceMatrixService|ElevationService|MaxZoomService|StreetViewPanorama|StreetViewService|places\.(?:Autocomplete|SearchBox|PlacesService)|visualization\.HeatmapLayer|KmlLayer|Data|OverlayView|Size|Point|Circle|Rectangle|Polygon|Polyline|GroundOverlay|ImageMapType)\s*\(",
        "google_maps",
    ) {
        Some(r) => r,
        None => return,
    };

    let mut gmaps_classes: Vec<(String, u32)> = Vec::new();
    let mut has_places = false;
    let mut has_streetview = false;
    let mut has_heatmap = false;
    let mut has_kml = false;
    let mut has_directions = false;
    let mut has_distance_matrix = false;
    let mut has_elevation = false;
    let mut has_data_layer = false;
    let mut has_drawing = false;

    for cap in re.captures_iter(source) {
        let m = cap.get(0).expect("group 0 always exists");
        let line = line_of(line_starts, m.start());
        let cls = cap.name("cls").expect("mandatory 'cls' group").as_str();
        emit_spatial_edge(file_name, line, "google_maps", cls, edges);
        gmaps_classes.push((cls.to_string(), line));

        let cls_lower = cls.to_lowercase();
        if cls_lower.contains("autocomplete")
            || cls_lower.contains("searchbox")
            || cls_lower.contains("placesservice")
        {
            has_places = true;
        }
        if cls_lower.contains("streetview") {
            has_streetview = true;
        }
        if cls_lower.contains("heatmap") {
            has_heatmap = true;
        }
        if cls_lower.contains("kml") {
            has_kml = true;
        }
        if cls_lower.contains("directions") {
            has_directions = true;
        }
        if cls_lower.contains("distancematrix") {
            has_distance_matrix = true;
        }
        if cls_lower.contains("elevation") {
            has_elevation = true;
        }
        if cls_lower == "data" {
            has_data_layer = true;
        }
    }

    // Detect google.maps.event.addListener pattern
    static GMAP_EVENT_RE: OnceLock<Regex> = OnceLock::new();
    if let Some(re) = get_compiled_regex(
        &GMAP_EVENT_RE,
        r"(?i)google\.maps\.event\.(?:addListener|addListenerOnce|addDomListener|removeListener|trigger|clearListeners|clearInstanceListeners)\s*\(",
        "google_maps_event",
    ) {
        for m in re.find_iter(source) {
            let line = line_of(line_starts, m.start());
            emit_spatial_edge(file_name, line, "google_maps", "EventListener", edges);
        }
    }

    // Detect google.maps.drawing.DrawingManager
    static GMAP_DRAWING_FULL_RE: OnceLock<Regex> = OnceLock::new();
    if let Some(re) = get_compiled_regex(
        &GMAP_DRAWING_FULL_RE,
        r"(?i)google\.maps\.drawing\.(?:DrawingManager|OverlayType)",
        "gmaps_drawing_full",
    )
        && re.is_match(source) {
            has_drawing = true;
        }

    // Detect Maps JS API library loading parameters (places, drawing, visualization, geometry)
    static GMAP_LIBRARIES_RE: OnceLock<Regex> = OnceLock::new();
    if let Some(re) = get_compiled_regex(
        &GMAP_LIBRARIES_RE,
        r#"(?i)libraries\s*[:=]\s*['"](?P<libs>[^'"]+)['"]"#,
        "gmaps_libraries",
    )
        && let Some(cap) = re.captures(source) {
            let libs = cap.name("libs").map_or("", |m| m.as_str()).to_lowercase();
            if libs.contains("places") {
                has_places = true;
            }
            if libs.contains("drawing") {
                has_drawing = true;
            }
            if libs.contains("visualization") {
                has_heatmap = true;
            }
        }

    // Detect google.maps.geometry.* (spherical, encoding, poly)
    static GMAP_GEOMETRY_RE: OnceLock<Regex> = OnceLock::new();
    let mut has_geometry = false;
    if let Some(re) = get_compiled_regex(
        &GMAP_GEOMETRY_RE,
        r"(?i)google\.maps\.geometry\.(?:spherical|encoding|poly)\.\w+",
        "gmaps_geometry",
    )
        && re.is_match(source) {
            has_geometry = true;
        }

    // Emit detailed inventory if any Google Maps usage found
    if !gmaps_classes.is_empty() {
        let mut meta = HashMap::with_capacity(16);
        meta.insert("library".into(), "google_maps".into());
        meta.insert("class_count".into(), gmaps_classes.len().to_string());
        meta.insert("has_places_api".into(), has_places.to_string());
        meta.insert("has_streetview".into(), has_streetview.to_string());
        meta.insert("has_heatmap".into(), has_heatmap.to_string());
        meta.insert("has_kml".into(), has_kml.to_string());
        meta.insert("has_directions".into(), has_directions.to_string());
        meta.insert(
            "has_distance_matrix".into(),
            has_distance_matrix.to_string(),
        );
        meta.insert("has_elevation".into(), has_elevation.to_string());
        meta.insert("has_data_layer".into(), has_data_layer.to_string());
        meta.insert("has_drawing".into(), has_drawing.to_string());
        meta.insert("has_geometry".into(), has_geometry.to_string());

        // Migration guidance varies by feature usage
        let complexity = if has_places || has_directions || has_streetview || has_heatmap {
            "high"
        } else if has_kml || has_data_layer || has_drawing {
            "medium"
        } else {
            "low"
        };
        meta.insert("migration_complexity".into(), complexity.into());
        meta.insert(
            "modern_target_react".into(),
            "@react-google-maps/api (or @vis.gl/react-google-maps)".into(),
        );
        meta.insert(
            "modern_target_blazor".into(),
            "BlazorGoogleMaps NuGet package".into(),
        );
        meta.insert(
            "modern_target_angular".into(),
            "@angular/google-maps".into(),
        );

        syms.push(ExtractedSymbol {
            name: format!("google_maps_inventory:{}", file_name),
            kind: "insight",
            start_line: gmaps_classes.first().map_or(0, |(_, l)| *l),
            end_line: gmaps_classes.last().map_or(0, |(_, l)| *l),
            metadata: Some(meta),
        });
    }
}

/// Detect Leaflet.js usage: `L.map(`, `L.marker(`, `L.tileLayer(`
fn extract_leaflet(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &LEAFLET_RE,
        r"(?i)\bL\.(?P<cls>map|marker|tileLayer|circle|polygon|popup|polyline|layerGroup|featureGroup|geoJSON|icon|latLng|latLngBounds)\s*\(",
        "leaflet",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let m = cap.get(0).expect("group 0 always exists");
        let line = line_of(line_starts, m.start());
        let cls = cap.name("cls").expect("mandatory 'cls' group").as_str();
        emit_spatial_edge(file_name, line, "leaflet", cls, edges);
    }
}

/// Detect OpenLayers usage: `new ol.Map(`, `new ol.View(`
fn extract_openlayers(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &OPENLAYERS_RE,
        r"(?i)new\s+ol\.(?P<cls>Map|View|Feature|Overlay|Geolocation|layer\.Tile|layer\.Vector|source\.OSM|source\.Vector|proj\.fromLonLat)\s*\(",
        "openlayers",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let m = cap.get(0).expect("group 0 always exists");
        let line = line_of(line_starts, m.start());
        let cls = cap.name("cls").expect("mandatory 'cls' group").as_str();
        // Normalize dotted sub-classes
        let normalized = cls.replace('.', "_");
        emit_spatial_edge(file_name, line, "openlayers", &normalized, edges);
    }
}

/// Extract GIS configuration (API keys, zoom levels, center coordinates).
/// Emits `gis_config` symbols and `spatial_call` edges linking JS file to config.
fn extract_gis_configs(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
    syms: &mut Vec<ExtractedSymbol>,
) {
    // API key detection
    if let Some(re) = get_compiled_regex(
        &GIS_API_KEY_RE,
        r#"(?i)(?:key|apiKey|api_key|apikey)\s*[:=]\s*['"](?P<key>[A-Za-z0-9_\-]{20,})['"]"#,
        "gis_api_key",
    ) {
        for cap in re.captures_iter(source) {
            let m = cap.get(0).expect("group 0 always exists");
            let line = line_of(line_starts, m.start());
            let key_value = cap.name("key").expect("mandatory 'key' group").as_str();
            // Mask the key for safety (show first 8 + last 4 chars)
            let masked = if key_value.len() > 12 {
                format!(
                    "{}...{}",
                    &key_value[..8],
                    &key_value[key_value.len() - 4..]
                )
            } else {
                "***".to_string()
            };

            let mut meta = HashMap::with_capacity(2);
            meta.insert("config_type".into(), "api_key".into());
            meta.insert("masked_value".into(), masked);

            syms.push(ExtractedSymbol {
                name: format!("gis_config:{}:api_key", file_name),
                kind: "gis_config",
                start_line: line,
                end_line: line,
                metadata: Some(meta.clone()),
            });

            edges.push(ExtractedEdge {
                source_name: file_name.to_string(),
                source_kind: "file",
                source_start_line: line,
                source_language: "javascript",
                target_name: format!("gis_config:{}:api_key", file_name),
                target_kind: Some("gis_config"),
                target_start_line: None,
                kind: "spatial_call",
                metadata: Some(meta),
            });
        }
    }

    // Zoom level detection
    if let Some(re) = get_compiled_regex(
        &GIS_ZOOM_RE,
        r"(?i)(?:zoom|zoomLevel|initialZoom)\s*[:=]\s*(?P<val>\d{1,2})",
        "gis_zoom",
    ) {
        for cap in re.captures_iter(source) {
            let m = cap.get(0).expect("group 0 always exists");
            let line = line_of(line_starts, m.start());
            let val = cap.name("val").expect("mandatory 'val' group").as_str();

            let mut meta = HashMap::with_capacity(2);
            meta.insert("config_type".into(), "zoom".into());
            meta.insert("value".into(), val.into());

            syms.push(ExtractedSymbol {
                name: format!("gis_config:{}:zoom", file_name),
                kind: "gis_config",
                start_line: line,
                end_line: line,
                metadata: Some(meta),
            });
        }
    }

    // Center point detection: center: [lat, lng] or center: new L.LatLng(lat, lng)
    if let Some(re) = get_compiled_regex(
        &GIS_CENTER_RE,
        r"(?i)center\s*[:=]\s*\[?\s*(?P<lat>-?\d+\.?\d*)\s*,\s*(?P<lng>-?\d+\.?\d*)",
        "gis_center",
    ) {
        for cap in re.captures_iter(source) {
            let m = cap.get(0).expect("group 0 always exists");
            let line = line_of(line_starts, m.start());
            let lat = cap.name("lat").expect("mandatory 'lat' group").as_str();
            let lng = cap.name("lng").expect("mandatory 'lng' group").as_str();

            let mut meta = HashMap::with_capacity(3);
            meta.insert("config_type".into(), "center".into());
            meta.insert("latitude".into(), lat.into());
            meta.insert("longitude".into(), lng.into());

            syms.push(ExtractedSymbol {
                name: format!("gis_config:{}:center", file_name),
                kind: "gis_config",
                start_line: line,
                end_line: line,
                metadata: Some(meta),
            });
        }
    }
}

/// Detect ASP.NET runtime-generated control IDs with `ctl00_` prefix.
///
/// In WebForms, controls inside NamingContainers get IDs like
/// `ctl00_ContentPlaceHolder1_txtName`. This function reverse-maps
/// these to the original control ID by extracting the final segment.
fn extract_ctl00_references(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
) {
    let re = match get_compiled_regex(
        &CTL00_ID_RE,
        r#"(?i)(?:getElementById|getElement|\$)\s*\(\s*['"](?:#)?(?P<full_id>ctl\d+[_$](?:[A-Za-z0-9_$]+[_$])*(?P<ctrl_id>[A-Za-z][A-Za-z0-9_]*))['"]\s*\)"#,
        "ctl00_id",
    ) {
        Some(r) => r,
        None => return,
    };

    for cap in re.captures_iter(source) {
        let m = cap.get(0).expect("group 0 always exists");
        let line = line_of(line_starts, m.start());
        let full_id = cap.name("full_id").expect("mandatory 'full_id' group").as_str();
        let ctrl_id = cap.name("ctrl_id").expect("mandatory 'ctrl_id' group").as_str();

        let mut meta = HashMap::with_capacity(3);
        meta.insert("selector_type".into(), "ctl00_reverse_map".into());
        meta.insert("full_generated_id".into(), full_id.into());
        meta.insert("resolved_control_id".into(), ctrl_id.into());

        edges.push(ExtractedEdge {
            source_name: file_name.to_string(),
            source_kind: "file",
            source_start_line: line,
            source_language: "javascript",
            target_name: ctrl_id.to_string(),
            target_kind: Some("control"),
            target_start_line: None,
            kind: "manipulates_dom",
            metadata: Some(meta),
        });
    }
}

// ── Phase 30 Gap 7: GIS Deep Extraction ─────────────────────────────────────

/// Deep GIS layer inventory: detects tile layers, WMS, GeoJSON, marker clustering,
/// drawing tools, coordinate systems, and geocoding across Leaflet/Google Maps/OpenLayers.
/// Emits a `gis_inventory` insight symbol with structured metadata.
fn extract_gis_layer_inventory(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
    syms: &mut Vec<ExtractedSymbol>,
) {
    let src_lower = source.to_lowercase();

    // Detect which libraries are present
    let has_leaflet = src_lower.contains("l.map") || src_lower.contains("l.tilelayer");
    let has_gmaps = src_lower.contains("google.maps");
    let has_ol = src_lower.contains("ol.map") || src_lower.contains("new ol.");

    if !has_leaflet && !has_gmaps && !has_ol {
        return;
    }

    let mut layers: Vec<String> = Vec::new();
    let mut tile_sources: Vec<String> = Vec::new();
    let mut has_drawing = false;
    let mut has_geocoding = false;
    let mut has_clustering = false;
    let mut has_geojson = false;
    let mut has_wms = false;
    let mut coordinate_system = "EPSG:4326"; // default WGS84
    let mut api_keys_detected: usize = 0;

    // --- Tile layers & sources ---
    let re_tile = get_compiled_regex(
        &TILE_URL_RE,
        r#"(?i)(?:tileLayer|TileLayer)\s*\(\s*['"](?P<url>[^'"]+)['"]"#,
        "tile_url",
    );
    if let Some(re) = re_tile {
        for cap in re.captures_iter(source) {
            let url = cap.name("url").map_or("", |m| m.as_str());
            layers.push("tile".to_string());
            if url.contains("openstreetmap") {
                tile_sources.push("openstreetmap".to_string());
            } else if url.contains("mapbox") {
                tile_sources.push("mapbox".to_string());
            } else if url.contains("stamen") || url.contains("stadia") {
                tile_sources.push("stamen".to_string());
            } else if url.contains("thunderforest") {
                tile_sources.push("thunderforest".to_string());
            } else if url.contains("here.com") || url.contains("heremaps") {
                tile_sources.push("here".to_string());
            } else {
                tile_sources.push("custom".to_string());
            }
        }
    }

    // --- WMS layers ---
    let re_wms = get_compiled_regex(
        &LEAFLET_WMS_RE,
        r#"(?i)(?:tileLayer\.wms|TileWMS)\s*\(\s*['"](?P<url>[^'"]+)['"]"#,
        "wms_layer",
    );
    if let Some(re) = re_wms {
        for cap in re.captures_iter(source) {
            has_wms = true;
            layers.push("wms".to_string());
            let url = cap.name("url").map_or("", |m| m.as_str());
            let m = cap.get(0).expect("group 0 always exists");
            let line = line_of(line_starts, m.start());

            let mut meta = HashMap::with_capacity(2);
            meta.insert("wms_endpoint".into(), url.to_string());
            meta.insert("layer_type".into(), "wms".into());

            edges.push(ExtractedEdge {
                source_name: file_name.to_string(),
                source_kind: "file",
                source_start_line: line,
                source_language: "javascript",
                target_name: format!("gis_layer:wms:{}", url),
                target_kind: Some("insight"),
                target_start_line: None,
                kind: "spatial_call",
                metadata: Some(meta),
            });
        }
    }

    // --- GeoJSON layers ---
    let re_geojson = get_compiled_regex(
        &LEAFLET_GEOJSON_RE,
        r"(?i)\b(?:L\.geoJSON|L\.geoJson|GeoJSON|geojson)\s*\(",
        "geojson_layer",
    );
    if let Some(re) = re_geojson
        && re.is_match(source) {
            has_geojson = true;
            layers.push("geojson".to_string());
        }

    // --- Marker clustering ---
    let re_cluster = get_compiled_regex(
        &LEAFLET_MARKER_CLUSTER_RE,
        r"(?i)\b(?:markerClusterGroup|MarkerClusterer|Cluster)\s*\(",
        "marker_cluster",
    );
    if let Some(re) = re_cluster
        && re.is_match(source) {
            has_clustering = true;
            layers.push("marker_cluster".to_string());
        }

    // --- Drawing tools ---
    let re_ldraw = get_compiled_regex(
        &LEAFLET_DRAW_RE,
        r"(?i)\b(?:L\.Control\.Draw|L\.Draw|DrawingManager|ol\.interaction\.Draw)\s*\(",
        "drawing_tools",
    );
    if let Some(re) = re_ldraw
        && re.is_match(source) {
            has_drawing = true;
        }
    // Google Maps DrawingManager
    let re_gdraw = get_compiled_regex(
        &GMAPS_DRAWING_RE,
        r"(?i)\bgoogle\.maps\.drawing\.DrawingManager\s*\(",
        "gmaps_drawing",
    );
    if let Some(re) = re_gdraw
        && re.is_match(source) {
            has_drawing = true;
        }
    // OpenLayers Draw interaction
    let re_oldraw = get_compiled_regex(
        &OL_DRAW_RE,
        r"(?i)\bnew\s+ol\.interaction\.Draw\s*\(",
        "ol_draw",
    );
    if let Some(re) = re_oldraw
        && re.is_match(source) {
            has_drawing = true;
        }

    // --- Coordinate system detection ---
    let re_ol_proj = get_compiled_regex(
        &OL_PROJ_RE,
        r"(?i)\bol\.proj\.(?:fromLonLat|toLonLat|transform)\s*\(",
        "ol_proj",
    );
    if let Some(re) = re_ol_proj
        && re.is_match(source) {
            coordinate_system = "EPSG:3857 (from EPSG:4326)";
        }
    let re_crs = get_compiled_regex(
        &LEAFLET_CRS_RE,
        r"(?i)\bL\.CRS\.(?P<crs>\w+)",
        "leaflet_crs",
    );
    if let Some(re) = re_crs
        && let Some(cap) = re.captures(source) {
            let crs = cap.name("crs").map_or("", |m| m.as_str());
            coordinate_system = match crs.to_lowercase().as_str() {
                "epsg3857" => "EPSG:3857",
                "epsg4326" => "EPSG:4326",
                "simple" => "Simple (non-geographic)",
                _ => "Custom CRS",
            };
        }

    // --- Geocoding detection ---
    let re_geocoder = get_compiled_regex(
        &GMAPS_GEOCODER_RE,
        r"(?i)\b(?:google\.maps\.Geocoder|Geocoder|geocoder)\s*\(",
        "geocoder",
    );
    if let Some(re) = re_geocoder
        && re.is_match(source) {
            has_geocoding = true;
        }
    let re_geocode_url = get_compiled_regex(
        &GEOCODE_URL_RE,
        r"(?i)(?:geocode|geocoding|nominatim)",
        "geocode_url",
    );
    if let Some(re) = re_geocode_url
        && re.is_match(source) {
            has_geocoding = true;
        }

    // --- API key count (from existing GIS_API_KEY_RE) ---
    let re_apikey = get_compiled_regex(
        &GIS_API_KEY_RE,
        r#"(?i)(?:key|apiKey|api_key|apikey)\s*[:=]\s*['"](?P<key>[A-Za-z0-9_\-]{20,})['"]"#,
        "gis_api_key_count",
    );
    if let Some(re) = re_apikey {
        api_keys_detected = re.find_iter(source).count();
    }

    // Check for marker layers
    if (src_lower.contains("l.marker") || src_lower.contains("new google.maps.marker"))
        && !layers.contains(&"marker".to_string()) {
            layers.push("marker".to_string());
        }

    // Determine library and version hint
    let library = if has_leaflet {
        "leaflet"
    } else if has_gmaps {
        "google_maps"
    } else {
        "openlayers"
    };

    let version_hint = if has_leaflet && src_lower.contains("mapcontainer") {
        "1.9+ (react-leaflet v4)"
    } else if has_leaflet {
        "1.7+"
    } else if has_gmaps && src_lower.contains("@googlemaps") {
        "Google Maps JS API v3 (modern)"
    } else if has_gmaps {
        "Google Maps JS API v3"
    } else if src_lower.contains("ol/map") {
        "OpenLayers 6+"
    } else {
        "unknown"
    };

    // Deduplicate layers and tile sources
    layers.sort();
    layers.dedup();
    tile_sources.sort();
    tile_sources.dedup();

    // Modern target recommendations
    let modern_react = match library {
        "leaflet" => "react-leaflet + @react-leaflet/core",
        "google_maps" => "@react-google-maps/api",
        "openlayers" => "rlayers or ol + react wrapper",
        _ => "Manual migration analysis required",
    };
    let modern_blazor = match library {
        "leaflet" => "BlazorLeaflet or Leaflet.Blazor",
        "google_maps" => "BlazorGoogleMaps",
        "openlayers" => "Custom Blazor JS interop with OpenLayers",
        _ => "Manual migration analysis required",
    };
    let modern_angular = match library {
        "leaflet" => "ngx-leaflet",
        "google_maps" => "@angular/google-maps",
        "openlayers" => "ngx-openlayers",
        _ => "Manual migration analysis required",
    };

    // Emit gis_inventory insight symbol
    let mut meta = HashMap::with_capacity(12);
    meta.insert("library".into(), library.into());
    meta.insert("version_hint".into(), version_hint.into());
    meta.insert("tile_sources".into(), tile_sources.join(", "));
    meta.insert("layers".into(), layers.join(", "));
    meta.insert("has_drawing_tools".into(), has_drawing.to_string());
    meta.insert("has_geocoding".into(), has_geocoding.to_string());
    meta.insert("has_clustering".into(), has_clustering.to_string());
    meta.insert("has_geojson".into(), has_geojson.to_string());
    meta.insert("has_wms".into(), has_wms.to_string());
    meta.insert("coordinate_system".into(), coordinate_system.into());
    meta.insert("api_keys_detected".into(), api_keys_detected.to_string());
    meta.insert("modern_target_react".into(), modern_react.into());
    meta.insert("modern_target_blazor".into(), modern_blazor.into());
    meta.insert("modern_target_angular".into(), modern_angular.into());

    syms.push(ExtractedSymbol {
        name: format!("gis_inventory:{}", file_name),
        kind: "insight",
        start_line: 0,
        end_line: 0,
        metadata: Some(meta),
    });
}

/// Detect Esri/ArcGIS JavaScript API usage (AMD, ES modules, REST API, Dojo).
/// Emits `spatial_call` edges and `insight` symbols for ArcGIS patterns.
fn extract_esri_arcgis(
    source: &str,
    line_starts: &[usize],
    file_name: &str,
    edges: &mut Vec<ExtractedEdge>,
    syms: &mut Vec<ExtractedSymbol>,
) {
    let src_lower = source.to_lowercase();

    // Quick check: skip if no Esri/ArcGIS patterns
    if !src_lower.contains("esri")
        && !src_lower.contains("arcgis")
        && !src_lower.contains("dojo.require")
    {
        return;
    }

    let mut esri_classes: Vec<(String, u32)> = Vec::new();
    let mut has_rest_api = false;
    let mut has_feature_layer = false;
    let mut has_map_view = false;
    let mut has_3d = false;
    let mut has_widgets = false;
    let mut has_geoprocessing = false;
    let mut has_routing = false;
    let mut has_editing = false;
    let mut has_printing = false;
    let mut has_portal = false;
    let mut has_auth = false;
    let mut has_geometry_service = false;

    // --- AMD module loading: require(["esri/Map", "esri/views/MapView", ...]) ---
    let re_amd = get_compiled_regex(
        &ESRI_AMD_RE,
        r#"(?i)["']esri/(?P<module>[A-Za-z0-9_/]+)["']"#,
        "esri_amd",
    );
    if let Some(re) = re_amd {
        for cap in re.captures_iter(source) {
            let m = cap.get(0).expect("group 0 always exists");
            let module = cap.name("module").map_or("", |m| m.as_str());
            let line = line_of(line_starts, m.start());
            esri_classes.push((module.to_string(), line));

            let mod_lower = module.to_lowercase();
            if mod_lower.contains("featurelayer") {
                has_feature_layer = true;
            }
            if mod_lower.contains("mapview") || mod_lower.contains("sceneview") {
                has_map_view = true;
            }
            if mod_lower.contains("sceneview") || mod_lower.contains("webscene") {
                has_3d = true;
            }
            if mod_lower.contains("widgets/") {
                has_widgets = true;
            }
            if mod_lower.contains("geoprocessor") || mod_lower.contains("geoprocessing") {
                has_geoprocessing = true;
            }
            if mod_lower.contains("route")
                || mod_lower.contains("servicearea")
                || mod_lower.contains("closestfacility")
            {
                has_routing = true;
            }
            if mod_lower.contains("editor") || mod_lower.contains("sketch") {
                has_editing = true;
            }
            if mod_lower.contains("print") {
                has_printing = true;
            }
            if mod_lower.contains("portal") {
                has_portal = true;
            }
            if mod_lower.contains("identity") || mod_lower.contains("oauth") {
                has_auth = true;
            }
            if mod_lower.contains("geometryservice") || mod_lower.contains("geometryengine") {
                has_geometry_service = true;
            }

            emit_spatial_edge(file_name, line, "arcgis", module, edges);
        }
    }

    // --- ES module style: new Map(), new MapView(), new FeatureLayer() ---
    // Covers core classes, layers, widgets, tasks, geometry, and renderers
    let re_es = get_compiled_regex(
        &ESRI_ES_RE,
        r"(?i)\bnew\s+(?P<cls>Map|WebMap|WebScene|MapView|SceneView|FeatureLayer|GraphicsLayer|TileLayer|VectorTileLayer|ImageryLayer|ImageryTileLayer|ElevationLayer|CSVLayer|GeoJSONLayer|WMSLayer|WMTSLayer|MapImageLayer|StreamLayer|GroupLayer|Graphic|Point|Polyline|Polygon|Extent|SpatialReference|Multipoint|Circle|Mesh|Search|Legend|LayerList|BasemapGallery|BasemapToggle|Expand|Home|Locate|Compass|ScaleBar|Print|Sketch|Editor|FeatureForm|FeatureTable|Popup|PopupTemplate|Swipe|TimeSlider|Bookmarks|DirectLineMeasurement3D|AreaMeasurement3D|Measurement|CoordinateConversion|IdentifyTask|FindTask|QueryTask|Geoprocessor|RouteTask|ServiceAreaTask|ClosestFacilityTask|PrintTask|Locator|GeometryService|SimpleRenderer|UniqueValueRenderer|ClassBreaksRenderer|HeatmapRenderer|DotDensityRenderer|SimpleMarkerSymbol|SimpleLineSymbol|SimpleFillSymbol|PictureMarkerSymbol|TextSymbol|Query|FeatureEffect|FeatureFilter|Portal|PortalItem|PortalQueryParams|OAuthInfo|IdentityManager)\s*\(",
        "esri_es",
    );
    if let Some(re) = re_es {
        for cap in re.captures_iter(source) {
            let m = cap.get(0).expect("group 0 always exists");
            let cls = cap.name("cls").map_or("", |m| m.as_str());
            let line = line_of(line_starts, m.start());
            esri_classes.push((cls.to_string(), line));

            let cls_lower = cls.to_lowercase();
            if cls_lower == "featurelayer" {
                has_feature_layer = true;
            }
            if cls_lower == "mapview" || cls_lower == "sceneview" {
                has_map_view = true;
            }
            if cls_lower == "sceneview"
                || cls_lower == "webscene"
                || cls_lower.contains("3d")
                || cls_lower == "mesh"
            {
                has_3d = true;
            }
            if matches!(
                cls_lower.as_str(),
                "search"
                    | "legend"
                    | "layerlist"
                    | "basemapgallery"
                    | "basemaptoggle"
                    | "expand"
                    | "home"
                    | "locate"
                    | "compass"
                    | "scalebar"
                    | "print"
                    | "sketch"
                    | "editor"
                    | "featureform"
                    | "featuretable"
                    | "popup"
                    | "popuptemplate"
                    | "swipe"
                    | "timeslider"
                    | "bookmarks"
                    | "measurement"
                    | "coordinateconversion"
            ) {
                has_widgets = true;
            }
            if cls_lower == "geoprocessor" {
                has_geoprocessing = true;
            }
            if cls_lower == "routetask"
                || cls_lower == "serviceareatask"
                || cls_lower == "closestfacilitytask"
            {
                has_routing = true;
            }
            if cls_lower == "editor" || cls_lower == "sketch" || cls_lower == "featureform" {
                has_editing = true;
            }
            if cls_lower == "print" || cls_lower == "printtask" {
                has_printing = true;
            }
            if cls_lower == "portal"
                || cls_lower == "portalitem"
                || cls_lower == "portalqueryparams"
            {
                has_portal = true;
            }
            if cls_lower == "oauthinfo" || cls_lower == "identitymanager" {
                has_auth = true;
            }
            if cls_lower == "geometryservice"
                || cls_lower == "identifytask"
                || cls_lower == "findtask"
            {
                has_geometry_service = true;
            }

            emit_spatial_edge(file_name, line, "arcgis", cls, edges);
        }
    }

    // --- ArcGIS REST API endpoint detection ---
    let re_rest = get_compiled_regex(
        &ESRI_REST_RE,
        r#"(?i)/arcgis/rest/services/(?P<service>[A-Za-z0-9_/]+)"#,
        "esri_rest",
    );
    if let Some(re) = re_rest {
        for cap in re.captures_iter(source) {
            has_rest_api = true;
            let m = cap.get(0).expect("group 0 always exists");
            let service = cap.name("service").map_or("", |m| m.as_str());
            let line = line_of(line_starts, m.start());

            let mut meta = HashMap::with_capacity(3);
            meta.insert("gis_library".into(), "arcgis_rest".into());
            meta.insert("rest_service".into(), service.into());
            meta.insert(
                "modern_equivalent".into(),
                "ArcGIS REST JS (@esri/arcgis-rest-request)".into(),
            );

            edges.push(ExtractedEdge {
                source_name: file_name.to_string(),
                source_kind: "file",
                source_start_line: line,
                source_language: "javascript",
                target_name: format!("gis:arcgis_rest:{}", service),
                target_kind: Some("insight"),
                target_start_line: None,
                kind: "spatial_call",
                metadata: Some(meta),
            });
        }
    }

    // --- Dojo-style legacy ArcGIS API ---
    let re_dojo = get_compiled_regex(
        &ESRI_DOJO_RE,
        r#"(?i)dojo\.require\s*\(\s*['"]esri\.(?P<module>[A-Za-z0-9_.]+)['"]"#,
        "esri_dojo",
    );
    if let Some(re) = re_dojo {
        for cap in re.captures_iter(source) {
            let m = cap.get(0).expect("group 0 always exists");
            let module = cap.name("module").map_or("", |m| m.as_str());
            let line = line_of(line_starts, m.start());
            esri_classes.push((format!("dojo:{}", module), line));

            let mut meta = HashMap::with_capacity(3);
            meta.insert("gis_library".into(), "arcgis_dojo".into());
            meta.insert("dojo_module".into(), module.into());
            meta.insert(
                "modern_equivalent".into(),
                "Migrate to @arcgis/core ES modules".into(),
            );

            edges.push(ExtractedEdge {
                source_name: file_name.to_string(),
                source_kind: "file",
                source_start_line: line,
                source_language: "javascript",
                target_name: format!("gis:arcgis_dojo:{}", module),
                target_kind: Some("insight"),
                target_start_line: None,
                kind: "spatial_call",
                metadata: Some(meta),
            });
        }
    }

    // Emit summary insight if any ArcGIS patterns found
    if !esri_classes.is_empty() {
        let api_style = if re_dojo.and_then(|re| re.find(source)).is_some() {
            "dojo_legacy"
        } else if re_amd.and_then(|re| re.find(source)).is_some() {
            "amd"
        } else {
            "es_modules"
        };

        let modern_equiv = match api_style {
            "dojo_legacy" => "Migrate to @arcgis/core 4.x ES modules (major rewrite)",
            "amd" => "Migrate to @arcgis/core 4.x ES modules",
            _ => "Already modern - ensure @arcgis/core 4.x",
        };

        // Determine migration complexity
        let complexity = if has_geoprocessing || has_3d || has_routing || has_auth || has_portal {
            "high"
        } else if has_editing || has_printing || has_widgets || has_geometry_service {
            "medium"
        } else {
            "low"
        };

        let mut meta = HashMap::with_capacity(20);
        meta.insert("library".into(), "arcgis".into());
        meta.insert("api_style".into(), api_style.into());
        meta.insert("has_feature_layer".into(), has_feature_layer.to_string());
        meta.insert("has_map_view".into(), has_map_view.to_string());
        meta.insert("has_3d".into(), has_3d.to_string());
        meta.insert("has_rest_api".into(), has_rest_api.to_string());
        meta.insert("has_widgets".into(), has_widgets.to_string());
        meta.insert("has_geoprocessing".into(), has_geoprocessing.to_string());
        meta.insert("has_routing".into(), has_routing.to_string());
        meta.insert("has_editing".into(), has_editing.to_string());
        meta.insert("has_printing".into(), has_printing.to_string());
        meta.insert("has_portal".into(), has_portal.to_string());
        meta.insert("has_auth".into(), has_auth.to_string());
        meta.insert(
            "has_geometry_service".into(),
            has_geometry_service.to_string(),
        );
        meta.insert("esri_class_count".into(), esri_classes.len().to_string());
        meta.insert("migration_complexity".into(), complexity.into());
        meta.insert("modern_equivalent".into(), modern_equiv.into());
        meta.insert(
            "modern_target_react".into(),
            "@arcgis/core + react wrapper".into(),
        );
        meta.insert(
            "modern_target_blazor".into(),
            "Blazor JS interop with @arcgis/core".into(),
        );
        meta.insert(
            "modern_target_angular".into(),
            "@arcgis/core + angular-esri-components".into(),
        );

        syms.push(ExtractedSymbol {
            name: format!("esri_arcgis_inventory:{}", file_name),
            kind: "insight",
            start_line: esri_classes.first().map_or(0, |(_, l)| *l),
            end_line: esri_classes.last().map_or(0, |(_, l)| *l),
            metadata: Some(meta),
        });
    }
}

// ── Deduplication ───────────────────────────────────────────────────────────

/// Remove duplicate edges with the same `(source_name, target_name, kind)` triple.
/// Keeps the first occurrence (lowest line number).
fn dedup_edges(edges: &mut Vec<ExtractedEdge>) {
    let mut seen = HashSet::with_capacity(edges.len());
    edges.retain(|e| {
        let key = format!("{}|{}|{}", e.source_name, e.target_name, e.kind);
        seen.insert(key)
    });
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_path(name: &str) -> PathBuf {
        PathBuf::from(format!("Scripts/{name}"))
    }

    // ── Feature 3: jQuery ends-with selector ────────────────────────────

    #[test]
    fn jquery_ends_with_single_quotes() {
        let js = r#"var el = $("[id$='txtSearch']");"#;
        let (_, edges) = extract_js(&test_path("search.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "txtSearch");
        assert_eq!(edges[0].kind, "manipulates_dom");
        assert_eq!(edges[0].source_language, "javascript");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("selector_type").map(|s| s.as_str()),
            Some("jquery_ends_with")
        );
    }

    #[test]
    fn jquery_ends_with_double_quotes() {
        let js = r#"var el = $('[id$="btnSubmit"]');"#;
        let (_, edges) = extract_js(&test_path("form.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "btnSubmit");
        assert_eq!(edges[0].kind, "manipulates_dom");
    }

    #[test]
    fn jquery_ends_with_no_quotes_around_value() {
        let js = r#"var el = $("[id$=ddlCountry]");"#;
        let (_, edges) = extract_js(&test_path("form.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "ddlCountry");
    }

    // ── Feature 3: ASP.NET ClientID ─────────────────────────────────────

    #[test]
    fn asp_client_id_expression() {
        let js = r#"var id = '<%= txtFirstName.ClientID %>';"#;
        let (_, edges) = extract_js(&test_path("inline.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "txtFirstName");
        assert_eq!(edges[0].kind, "manipulates_dom");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("selector_type").map(|s| s.as_str()),
            Some("asp_client_id")
        );
    }

    // ── Feature 3: __doPostBack ─────────────────────────────────────────

    #[test]
    fn postback_simple_id() {
        let js = r#"__doPostBack('btnRefresh', '');"#;
        let (_, edges) = extract_js(&test_path("postback.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "btnRefresh");
        assert_eq!(edges[0].kind, "triggers_postback");
    }

    #[test]
    fn postback_unique_id_extracts_short_name() {
        let js = r#"__doPostBack('ctl00$MainContent$gvPatients', '');"#;
        let (_, edges) = extract_js(&test_path("grid.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "gvPatients");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("unique_id").map(|s| s.as_str()),
            Some("ctl00$MainContent$gvPatients")
        );
    }

    // ── Feature 3: document.getElementById ──────────────────────────────

    #[test]
    fn getelementbyid_simple() {
        let js = r#"var el = document.getElementById('pnlDetails');"#;
        let (_, edges) = extract_js(&test_path("utils.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "pnlDetails");
        assert_eq!(edges[0].kind, "manipulates_dom");
    }

    // ── Feature 4: $.ajax ───────────────────────────────────────────────

    #[test]
    fn ajax_call_with_method() {
        let js = r#"
            $.ajax({
                url: 'Services/MapData.asmx/GetPolygons',
                type: 'POST',
                success: function(data) {}
            });
        "#;
        let (_, edges) = extract_js(&test_path("map_utils.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "Services/MapData.asmx");
        assert_eq!(edges[0].kind, "api_call");
        assert_eq!(edges[0].target_kind, Some("web_service"));
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("ajax_target_method").map(|s| s.as_str()),
            Some("GetPolygons")
        );
        assert_eq!(
            meta.get("ajax_transport").map(|s| s.as_str()),
            Some("jquery_ajax")
        );
    }

    #[test]
    fn ajax_call_ashx_no_method() {
        let js = r#"$.ajax({ url: 'Handlers/Export.ashx', type: 'GET' });"#;
        let (_, edges) = extract_js(&test_path("export.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "Handlers/Export.ashx");
        assert_eq!(edges[0].target_kind, Some("http_handler"));
    }

    // ── Feature 4: $.get / $.post ───────────────────────────────────────

    #[test]
    fn ajax_shorthand_get() {
        let js = r#"$.get('Services/LookupData.asmx/GetCities', function(data) {});"#;
        let (_, edges) = extract_js(&test_path("lookup.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "Services/LookupData.asmx");
        assert_eq!(edges[0].kind, "api_call");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("ajax_target_method").map(|s| s.as_str()),
            Some("GetCities")
        );
    }

    #[test]
    fn ajax_shorthand_post() {
        let js = r#"$.post('api/Patients/Save', data, onSuccess);"#;
        let (_, edges) = extract_js(&test_path("patient.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "api/Patients");
        assert_eq!(edges[0].kind, "api_call");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("ajax_target_method").map(|s| s.as_str()),
            Some("Save")
        );
    }

    #[test]
    fn ajax_shorthand_getjson() {
        let js = r#"$.getJSON('Services/Config.svc/GetSettings', callback);"#;
        let (_, edges) = extract_js(&test_path("config.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "Services/Config.svc");
        assert_eq!(edges[0].target_kind, Some("wcf_service"));
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("ajax_target_method").map(|s| s.as_str()),
            Some("GetSettings")
        );
    }

    // ── Feature 4: fetch() ──────────────────────────────────────────────

    #[test]
    fn fetch_call_asmx() {
        let js = r#"fetch('Services/UserData.asmx/GetProfile').then(r => r.json());"#;
        let (_, edges) = extract_js(&test_path("profile.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "Services/UserData.asmx");
        assert_eq!(edges[0].kind, "api_call");
    }

    // ── Feature 4: XMLHttpRequest.open ──────────────────────────────────

    #[test]
    fn xhr_open_call() {
        let js = r#"
            var xhr = new XMLHttpRequest();
            xhr.open('POST', 'Services/Report.asmx/Generate');
            xhr.send(data);
        "#;
        let (_, edges) = extract_js(&test_path("report.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "Services/Report.asmx");
        assert_eq!(edges[0].kind, "api_call");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("ajax_target_method").map(|s| s.as_str()),
            Some("Generate")
        );
    }

    // ── Feature 4: PageMethods ──────────────────────────────────────────

    #[test]
    fn page_methods_call() {
        let js = r#"PageMethods.GetPatientList(searchTerm, onSuccess, onFailure);"#;
        let (_, edges) = extract_js(&test_path("patients.js"), js);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "GetPatientList");
        assert_eq!(edges[0].target_kind, Some("function"));
        assert_eq!(edges[0].kind, "api_call");
        let meta = edges[0].metadata.as_ref().expect("metadata");
        assert_eq!(
            meta.get("ajax_transport").map(|s| s.as_str()),
            Some("page_methods")
        );
    }

    // ── URL splitting ───────────────────────────────────────────────────

    #[test]
    fn split_url_asmx_with_method() {
        let (path, method) = split_service_url("Services/MapData.asmx/GetPolygons");
        assert_eq!(path, "Services/MapData.asmx");
        assert_eq!(method.as_deref(), Some("GetPolygons"));
    }

    #[test]
    fn split_url_ashx_no_method() {
        let (path, method) = split_service_url("Handlers/Export.ashx");
        assert_eq!(path, "Handlers/Export.ashx");
        assert!(method.is_none());
    }

    #[test]
    fn split_url_with_query_string() {
        let (path, method) = split_service_url("Services/Data.asmx/GetRows?filter=active&page=1");
        assert_eq!(path, "Services/Data.asmx");
        assert_eq!(method.as_deref(), Some("GetRows"));
    }

    #[test]
    fn split_url_relative_dot_dot() {
        let (path, method) = split_service_url("../Services/Auth.asmx/Login");
        assert_eq!(path, "Services/Auth.asmx");
        assert_eq!(method.as_deref(), Some("Login"));
    }

    #[test]
    fn split_url_tilde_prefix() {
        let (path, method) = split_service_url("~/Services/Data.svc/GetItems");
        assert_eq!(path, "Services/Data.svc");
        assert_eq!(method.as_deref(), Some("GetItems"));
    }

    #[test]
    fn split_url_api_route() {
        let (path, method) = split_service_url("api/Patients/Search");
        assert_eq!(path, "api/Patients");
        assert_eq!(method.as_deref(), Some("Search"));
    }

    // ── Edge case: skipped files ────────────────────────────────────────

    #[test]
    fn oversized_file_returns_empty() {
        let huge = "x".repeat(MAX_JS_SOURCE_BYTES + 1);
        let (syms, edges) = extract_js(&test_path("huge.js"), &huge);
        assert!(syms.is_empty());
        assert!(edges.is_empty());
    }

    // ── Deduplication ───────────────────────────────────────────────────

    #[test]
    fn dedup_same_target_multiple_lines() {
        let js = r#"
            var a = $("[id$='txtName']");
            var b = $("[id$='txtName']");
        "#;
        let (_, edges) = extract_js(&test_path("dup.js"), js);
        // Same (source, target, kind) → deduplicated to 1
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target_name, "txtName");
    }

    // ── Mixed scenario ──────────────────────────────────────────────────

    #[test]
    fn mixed_dom_and_ajax_edges() {
        let js = r#"
            $(document).ready(function() {
                var grid = $("[id$='gvResults']");
                __doPostBack('btnSearch', '');
                $.ajax({
                    url: 'Services/Search.asmx/FindRecords',
                    type: 'POST',
                    data: JSON.stringify({ q: query }),
                    success: function(data) {
                        PageMethods.FormatResults(data, onDone, onFail);
                    }
                });
            });
        "#;
        let (_, edges) = extract_js(&test_path("search_page.js"), js);
        // Should have: gvResults (manipulates_dom), btnSearch (triggers_postback),
        // Search.asmx (api_call), FormatResults (api_call)
        assert_eq!(edges.len(), 4);

        let dom_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "manipulates_dom")
            .collect();
        assert_eq!(dom_edges.len(), 1);
        assert_eq!(dom_edges[0].target_name, "gvResults");

        let postback_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "triggers_postback")
            .collect();
        assert_eq!(postback_edges.len(), 1);
        assert_eq!(postback_edges[0].target_name, "btnSearch");

        let api_edges: Vec<_> = edges.iter().filter(|e| e.kind == "api_call").collect();
        assert_eq!(api_edges.len(), 2);

        let asmx_edge = api_edges
            .iter()
            .find(|e| e.target_name == "Services/Search.asmx");
        assert!(asmx_edge.is_some());

        let pm_edge = api_edges.iter().find(|e| e.target_name == "FormatResults");
        assert!(pm_edge.is_some());
    }

    // ── Data URIs and skip patterns ─────────────────────────────────────

    #[test]
    fn skip_data_uri_in_ajax() {
        let js = r#"$.ajax({ url: 'data:text/plain;base64,abc' });"#;
        let (_, edges) = extract_js(&test_path("skip.js"), js);
        assert!(edges.is_empty());
    }

    #[test]
    fn skip_javascript_uri_in_fetch() {
        let js = r#"fetch('javascript:void(0)');"#;
        let (_, edges) = extract_js(&test_path("skip.js"), js);
        assert!(edges.is_empty());
    }

    // ── Feature 5: GIS / Spatial Logic ───────────────────────────────────

    #[test]
    fn google_maps_detection() {
        let js = r#"
            var map = new google.maps.Map(document.getElementById('map'), {
                center: new google.maps.LatLng(40.7128, -74.0060),
                zoom: 12
            });
            var marker = new google.maps.Marker({ position: center, map: map });
        "#;
        let (_, edges) = extract_js(&test_path("gmap.js"), js);
        let spatial: Vec<_> = edges.iter().filter(|e| e.kind == "spatial_call").collect();
        assert!(
            spatial.len() >= 3,
            "expected >=3 spatial_call edges, got {}",
            spatial.len()
        );
        assert!(
            spatial
                .iter()
                .any(|e| e.metadata.as_ref().unwrap().get("map_class").unwrap() == "Map")
        );
        assert!(
            spatial
                .iter()
                .any(|e| e.metadata.as_ref().unwrap().get("gis_library").unwrap() == "google_maps")
        );
    }

    #[test]
    fn leaflet_detection() {
        let js = r#"
            var map = L.map('mapid').setView([51.505, -0.09], 13);
            L.tileLayer('https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png').addTo(map);
            L.marker([51.5, -0.09]).addTo(map).bindPopup('Hello');
        "#;
        let (_, edges) = extract_js(&test_path("leaflet.js"), js);
        let spatial: Vec<_> = edges.iter().filter(|e| e.kind == "spatial_call").collect();
        assert!(
            spatial.len() >= 3,
            "expected >=3 spatial_call edges, got {}",
            spatial.len()
        );
        assert!(
            spatial
                .iter()
                .any(|e| e.metadata.as_ref().unwrap().get("gis_library").unwrap() == "leaflet")
        );
    }

    #[test]
    fn openlayers_detection() {
        let js = r#"
            var map = new ol.Map({ target: 'map' });
            var view = new ol.View({ center: [0, 0], zoom: 2 });
        "#;
        let (_, edges) = extract_js(&test_path("ol.js"), js);
        let spatial: Vec<_> = edges.iter().filter(|e| e.kind == "spatial_call").collect();
        assert!(
            spatial.len() >= 2,
            "expected >=2 spatial_call edges, got {}",
            spatial.len()
        );
        assert!(
            spatial
                .iter()
                .any(|e| e.metadata.as_ref().unwrap().get("gis_library").unwrap() == "openlayers")
        );
    }

    #[test]
    fn gis_api_key_extraction() {
        let js = r#"
            var config = { apiKey: 'AIzaSyD1234567890abcdef1234' };
        "#;
        let (syms, edges) = extract_js(&test_path("config.js"), js);
        let gis_syms: Vec<_> = syms.iter().filter(|s| s.kind == "gis_config").collect();
        assert!(
            !gis_syms.is_empty(),
            "expected gis_config symbols for API key"
        );
        assert!(
            gis_syms
                .iter()
                .any(|s| s.metadata.as_ref().unwrap().get("config_type").unwrap() == "api_key")
        );
        // Key should be masked
        assert!(gis_syms.iter().any(|s| {
            s.metadata
                .as_ref()
                .unwrap()
                .get("masked_value")
                .unwrap()
                .contains("...")
        }));
        // Should also emit a spatial_call edge
        assert!(
            edges
                .iter()
                .any(|e| e.kind == "spatial_call" && e.target_name.contains("api_key"))
        );
    }

    #[test]
    fn gis_zoom_and_center_extraction() {
        let js = r#"
            var options = { zoom: 15, center: [40.7128, -74.0060] };
        "#;
        let (syms, _) = extract_js(&test_path("opts.js"), js);
        let zoom: Vec<_> = syms
            .iter()
            .filter(|s| {
                s.kind == "gis_config"
                    && s.metadata.as_ref().unwrap().get("config_type").unwrap() == "zoom"
            })
            .collect();
        assert!(!zoom.is_empty(), "expected zoom config symbol");

        let center: Vec<_> = syms
            .iter()
            .filter(|s| {
                s.kind == "gis_config"
                    && s.metadata.as_ref().unwrap().get("config_type").unwrap() == "center"
            })
            .collect();
        assert!(!center.is_empty(), "expected center config symbol");
        let meta = center[0].metadata.as_ref().unwrap();
        assert_eq!(meta.get("latitude").unwrap(), "40.7128");
        assert_eq!(meta.get("longitude").unwrap(), "-74.0060");
    }

    #[test]
    fn ctl00_reverse_mapping() {
        let js = r#"
            document.getElementById('ctl00_MainContent_txtName').value = 'test';
            $('#ctl00_ContentPlaceHolder1_btnSubmit').click();
        "#;
        let (_, edges) = extract_js(&test_path("ctl.js"), js);
        let dom: Vec<_> = edges
            .iter()
            .filter(|e| {
                e.kind == "manipulates_dom"
                    && e.metadata.as_ref().unwrap().get("selector_type").unwrap()
                        == "ctl00_reverse_map"
            })
            .collect();
        assert!(
            dom.len() >= 2,
            "expected >=2 ctl00 reverse-mapped edges, got {}",
            dom.len()
        );
        assert!(dom.iter().any(|e| e.target_name == "txtName"));
        assert!(dom.iter().any(|e| e.target_name == "btnSubmit"));
    }

    #[test]
    fn modern_gis_equivalent_lookup() {
        assert!(modern_gis_equivalent("google_maps", "Map").contains("GoogleMap"));
        assert!(modern_gis_equivalent("leaflet", "marker").contains("react-leaflet"));
        assert!(modern_gis_equivalent("openlayers", "Map").contains("rlayers"));
        assert!(modern_gis_equivalent("unknown", "thing").contains("Manual"));
    }
}
