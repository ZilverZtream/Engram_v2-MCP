/// Deep Layout Extractor (DLE) for WebForms and WinForms Designer files.
///
/// Captures hierarchical layout and logical grouping of UI controls to enable
/// AI-driven UI migration from legacy markup to modern grid systems (Tailwind,
/// MudBlazor, etc.) while preserving the original data-entry flow.
///
/// ## WebForms Extraction
///
/// Parses container tags (`<asp:Panel>`, `<table>`, `<div>`, `<asp:PlaceHolder>`)
/// to build a parent→child containment tree. Emits:
///   - `ui_container` symbols with metadata: `container_type`, `layout_style`, `logical_grouping`
///   - `contains_ui` edges: container → child control
///   - `ui_layout_neighbor` edges: sibling controls in tab/visual order
///
/// Heuristics applied:
///   1. **Label proximity**: `<asp:Label>` or `<asp:Literal>` immediately before
///      an input → `metadata["ui_label"]` on the input symbol.
///   2. **Grid detection**: `<table>` structures mapped with `row`/`col` metadata.
///   3. **Naming convention inference**: Suffixes `_OD`, `_OS`, `_R`, `_L` → logical
///      grouping metadata (e.g. `"RightEye"`, `"LeftEye"`).
///
/// ## WinForms Designer Extraction
///
/// Parses `.Designer.vb` and `.Designer.cs` files for:
///   - `Me.pnlFoo.Controls.Add(Me.txtBar)` / `this.pnlFoo.Controls.Add(this.txtBar)`
///   - `Me.txtBar.Location = New System.Drawing.Point(x, y)` / `this.txtBar.Location = new ...`
///   - `Me.txtBar.Size = New System.Drawing.Size(w, h)` / `this.txtBar.Size = new ...`
///   - `Me.txtBar.TabIndex = N` / `this.txtBar.TabIndex = N`
///   - `Me.txtBar.Text = "Label"` / `this.txtBar.Text = "Label"`
///
/// Groups controls by spatial proximity and emits the same edge types.
use crate::parsing::{ExtractedEdge, ExtractedSymbol};
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

// ── Static Regex Definitions ────────────────────────────────────────────────

/// Matches opening container tags: `<asp:Panel ...>`, `<asp:Table ...>`,
/// `<asp:PlaceHolder ...>`, `<div ...>`, `<table ...>`, `<fieldset ...>`,
/// `<asp:MultiView ...>`, `<asp:View ...>`.
/// Captures: (1) tag name, (2) attributes.
static CONTAINER_OPEN_RE: OnceLock<Regex> = OnceLock::new();

/// Matches closing container tags.
static CONTAINER_CLOSE_RE: OnceLock<Regex> = OnceLock::new();

/// Matches self-closing container tags.
static CONTAINER_SELF_CLOSE_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `<asp:Label ...>` or `<asp:Literal ...>` with Text attribute.
static LABEL_RE: OnceLock<Regex> = OnceLock::new();

/// Matches input-like controls with an ID: `<asp:TextBox`, `<asp:DropDownList`, etc.
static INPUT_CONTROL_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `<tr>` / `<tr ...>` opening tags.
static TR_OPEN_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `</tr>` closing tags.
static TR_CLOSE_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `<td>` / `<td ...>` opening tags.
static TD_OPEN_RE: OnceLock<Regex> = OnceLock::new();

/// Matches `</td>` closing tags.
static TD_CLOSE_RE: OnceLock<Regex> = OnceLock::new();

/// ID attribute extraction.
static ID_ATTR_RE: OnceLock<Regex> = OnceLock::new();

/// Text attribute extraction.
static TEXT_ATTR_RE: OnceLock<Regex> = OnceLock::new();

/// CssClass attribute extraction.
static CSS_CLASS_RE: OnceLock<Regex> = OnceLock::new();

/// GroupingField attribute extraction.
static GROUPING_FIELD_RE: OnceLock<Regex> = OnceLock::new();

/// WinForms: `Me.pnlFoo.Controls.Add(Me.txtBar)` or `this.pnlFoo.Controls.Add(this.txtBar)`
static WINFORMS_CONTROLS_ADD_RE: OnceLock<Regex> = OnceLock::new();

/// WinForms: `.Location = New System.Drawing.Point(x, y)` or `new Point(x, y)`
static WINFORMS_LOCATION_RE: OnceLock<Regex> = OnceLock::new();

/// WinForms: `.Size = New System.Drawing.Size(w, h)` or `new Size(w, h)`
static WINFORMS_SIZE_RE: OnceLock<Regex> = OnceLock::new();

/// WinForms: `.TabIndex = N`
static WINFORMS_TABINDEX_RE: OnceLock<Regex> = OnceLock::new();

/// WinForms: `.Text = "..."`
static WINFORMS_TEXT_RE: OnceLock<Regex> = OnceLock::new();

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

// ── Data Structures ─────────────────────────────────────────────────────────

/// A container found during parsing (Panel, Table, div, GroupBox, etc.)
#[derive(Debug, Clone)]
struct ContainerInfo {
    /// The control ID (e.g. "pnlRightEye"). Synthesized for anonymous containers.
    id: String,
    /// The tag type (e.g. "Panel", "Table", "div").
    tag_type: String,
    /// Line number where this container opens.
    start_line: u32,
    /// Character offset where the opening tag starts.
    #[allow(dead_code)]
    start_offset: usize,
    /// Optional CssClass for additional context.
    css_class: Option<String>,
}

/// A child control found inside a container.
#[derive(Debug, Clone)]
struct ChildControl {
    /// The control ID.
    id: String,
    /// The tag type (e.g. "TextBox", "DropDownList").
    tag_type: String,
    /// Line number.
    line: u32,
    /// Character offset.
    offset: usize,
    /// Label text found by proximity heuristic.
    ui_label: Option<String>,
    /// Table row index (if inside a <table>).
    table_row: Option<u32>,
    /// Table column index (if inside a <table>).
    table_col: Option<u32>,
    /// The ID of the nearest enclosing container.
    parent_container_id: Option<String>,
}

/// WinForms Designer control with spatial information.
#[derive(Debug, Clone)]
struct WinFormsControl {
    name: String,
    #[allow(dead_code)]
    parent: Option<String>,
    x: Option<i32>,
    y: Option<i32>,
    width: Option<i32>,
    height: Option<i32>,
    tab_index: Option<u32>,
    text: Option<String>,
    line: u32,
}

// ── WebForms Deep Layout Extraction ─────────────────────────────────────────

/// Extract UI layout hierarchy from WebForms markup.
///
/// Call this from `extract_webforms` after the standard control/edge extraction.
/// It returns additional symbols (ui_container) and edges (contains_ui, ui_layout_neighbor)
/// that describe the visual structure.
pub fn extract_webforms_layout(
    _rel_path_str: &str,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut edges: Vec<ExtractedEdge> = Vec::new();

    // ── Build line offset table ─────────────────────────────────────────────
    let line_offsets: Vec<usize> = {
        let mut offsets = vec![0usize];
        for (i, c) in source.char_indices() {
            if c == '\n' {
                offsets.push(i + 1);
            }
        }
        offsets
    };

    let char_to_line = |char_pos: usize| -> u32 {
        match line_offsets.binary_search(&char_pos) {
            Ok(line) => line as u32,
            Err(line) => line.saturating_sub(1) as u32,
        }
    };

    // ── Compile regexes ─────────────────────────────────────────────────────
    let Some(container_open_re) = get_compiled_regex(
        &CONTAINER_OPEN_RE,
        r#"(?i)<(asp:Panel|asp:Table|asp:PlaceHolder|asp:MultiView|asp:View|div|table|fieldset|asp:Wizard)\b([^>]*)>"#,
        "dle_container_open",
    ) else {
        return (symbols, edges);
    };
    let Some(container_close_re) = get_compiled_regex(
        &CONTAINER_CLOSE_RE,
        r#"(?i)</(asp:Panel|asp:Table|asp:PlaceHolder|asp:MultiView|asp:View|div|table|fieldset|asp:Wizard)\s*>"#,
        "dle_container_close",
    ) else {
        return (symbols, edges);
    };
    let Some(container_self_close_re) = get_compiled_regex(
        &CONTAINER_SELF_CLOSE_RE,
        r#"(?i)<(asp:Panel|asp:Table|asp:PlaceHolder|asp:MultiView|asp:View|div|table|fieldset|asp:Wizard)\b([^>]*)/>"#,
        "dle_container_self_close",
    ) else {
        return (symbols, edges);
    };
    let Some(label_re) = get_compiled_regex(
        &LABEL_RE,
        r#"(?i)<asp:(Label|Literal)\b[^>]*Text\s*=\s*"([^"]*)"[^>]*/?\s*>"#,
        "dle_label",
    ) else {
        return (symbols, edges);
    };
    let Some(input_re) = get_compiled_regex(
        &INPUT_CONTROL_RE,
        r#"(?i)<(asp:TextBox|asp:DropDownList|asp:CheckBox|asp:RadioButton|asp:RadioButtonList|asp:CheckBoxList|asp:ListBox|asp:HiddenField|asp:FileUpload|asp:Calendar|asp:ImageButton|asp:Button|asp:LinkButton|input|select|textarea)\b([^>]*(?:runat\s*=\s*"server"|ID\s*=\s*")[^>]*)/?>"#,
        "dle_input_control",
    ) else {
        return (symbols, edges);
    };
    let Some(id_re) = get_compiled_regex(&ID_ATTR_RE, r#"(?i)\bID\s*=\s*"([^"]+)""#, "dle_id_attr")
    else {
        return (symbols, edges);
    };
    let Some(_text_re) = get_compiled_regex(
        &TEXT_ATTR_RE,
        r#"(?i)\bText\s*=\s*"([^"]*?)""#,
        "dle_text_attr",
    ) else {
        return (symbols, edges);
    };
    let Some(css_class_re) = get_compiled_regex(
        &CSS_CLASS_RE,
        r#"(?i)\bCssClass\s*=\s*"([^"]+)""#,
        "dle_css_class",
    ) else {
        return (symbols, edges);
    };
    let Some(_grouping_field_re) = get_compiled_regex(
        &GROUPING_FIELD_RE,
        r#"(?i)\bGroupingText\s*=\s*"([^"]+)""#,
        "dle_grouping_field",
    ) else {
        return (symbols, edges);
    };
    let Some(tr_open_re) = get_compiled_regex(&TR_OPEN_RE, r"(?i)<tr\b[^>]*>", "dle_tr_open")
    else {
        return (symbols, edges);
    };
    let Some(tr_close_re) = get_compiled_regex(&TR_CLOSE_RE, r"(?i)</tr\s*>", "dle_tr_close")
    else {
        return (symbols, edges);
    };
    let Some(td_open_re) = get_compiled_regex(&TD_OPEN_RE, r"(?i)<td\b[^>]*>", "dle_td_open")
    else {
        return (symbols, edges);
    };
    let Some(td_close_re) = get_compiled_regex(&TD_CLOSE_RE, r"(?i)</td\s*>", "dle_td_close")
    else {
        return (symbols, edges);
    };

    // ── Phase 1: Build label position map ───────────────────────────────────
    // Maps byte offset → label text for proximity matching.
    let mut label_positions: Vec<(usize, String)> = Vec::new();
    for m in label_re.find_iter(source) {
        if let Some(cap) = label_re.captures(m.as_str()) {
            let text = cap[2].trim().to_string();
            if !text.is_empty() {
                label_positions.push((m.end(), text));
            }
        }
    }

    // ── Phase 2: Build table grid map ───────────────────────────────────────
    // For each offset, compute (row, col) if inside a <table>.
    // We track row transitions via <tr>/<td> tags.
    let mut table_grid_ranges: Vec<(usize, usize, u32, u32)> = Vec::new(); // (start, end, row, col)
    {
        let mut row_idx: i32 = -1;
        let mut col_idx: u32 = 0;
        let mut in_td = false;
        let mut td_start: usize = 0;

        // Collect all tr/td events and sort by offset
        let mut events: Vec<(usize, &str)> = Vec::new();
        for m in tr_open_re.find_iter(source) {
            events.push((m.start(), "tr_open"));
        }
        for m in tr_close_re.find_iter(source) {
            events.push((m.start(), "tr_close"));
        }
        for m in td_open_re.find_iter(source) {
            events.push((m.end(), "td_open"));
        }
        for m in td_close_re.find_iter(source) {
            events.push((m.start(), "td_close"));
        }
        events.sort_by_key(|e| e.0);

        for (offset, kind) in &events {
            match *kind {
                "tr_open" => {
                    row_idx += 1;
                    col_idx = 0;
                }
                "tr_close" => {}
                "td_open" => {
                    td_start = *offset;
                    in_td = true;
                }
                "td_close" => {
                    if in_td && row_idx >= 0 {
                        table_grid_ranges.push((td_start, *offset, row_idx as u32, col_idx));
                    }
                    col_idx += 1;
                    in_td = false;
                }
                _ => {}
            }
        }
    }

    let find_grid_pos = |offset: usize| -> (Option<u32>, Option<u32>) {
        for &(start, end, row, col) in &table_grid_ranges {
            if offset >= start && offset < end {
                return (Some(row), Some(col));
            }
        }
        (None, None)
    };

    // ── Phase 3: Parse container hierarchy ──────────────────────────────────
    // Use a stack to track open containers. For each opening tag, push onto stack.
    // For each input control, the top of stack is the parent container.
    let mut container_stack: Vec<ContainerInfo> = Vec::new();
    let mut all_containers: Vec<ContainerInfo> = Vec::new();
    let mut all_children: Vec<ChildControl> = Vec::new();
    let mut anon_counter: u32 = 0;

    // Collect all events (container open, container close, self-closing containers, inputs)
    #[derive(Debug)]
    enum LayoutEvent {
        ContainerOpen {
            offset: usize,
            tag_type: String,
            attrs: String,
        },
        ContainerClose {
            offset: usize,
            tag_type: String,
        },
        ContainerSelfClose {
            offset: usize,
            tag_type: String,
            attrs: String,
        },
        InputControl {
            offset: usize,
            tag_type: String,
            attrs: String,
        },
    }

    let mut events: Vec<LayoutEvent> = Vec::new();

    for cap in container_open_re.captures_iter(source) {
        let m = cap.get(0).expect("full match");
        events.push(LayoutEvent::ContainerOpen {
            offset: m.start(),
            tag_type: cap[1].to_string(),
            attrs: cap[2].to_string(),
        });
    }
    for cap in container_close_re.captures_iter(source) {
        let m = cap.get(0).expect("full match");
        events.push(LayoutEvent::ContainerClose {
            offset: m.start(),
            tag_type: cap[1].to_string(),
        });
    }
    for cap in container_self_close_re.captures_iter(source) {
        let m = cap.get(0).expect("full match");
        events.push(LayoutEvent::ContainerSelfClose {
            offset: m.start(),
            tag_type: cap[1].to_string(),
            attrs: cap[2].to_string(),
        });
    }
    for cap in input_re.captures_iter(source) {
        let m = cap.get(0).expect("full match");
        events.push(LayoutEvent::InputControl {
            offset: m.start(),
            tag_type: cap[1].to_string(),
            attrs: cap[2].to_string(),
        });
    }

    // Sort by offset
    events.sort_by(|a, b| {
        let (ao, ap) = match a {
            LayoutEvent::ContainerOpen { offset, .. } => (*offset, 0u8),
            LayoutEvent::ContainerSelfClose { offset, .. } => (*offset, 1u8),
            LayoutEvent::InputControl { offset, .. } => (*offset, 2u8),
            LayoutEvent::ContainerClose { offset, .. } => (*offset, 3u8),
        };
        let (bo, bp) = match b {
            LayoutEvent::ContainerOpen { offset, .. } => (*offset, 0u8),
            LayoutEvent::ContainerSelfClose { offset, .. } => (*offset, 1u8),
            LayoutEvent::InputControl { offset, .. } => (*offset, 2u8),
            LayoutEvent::ContainerClose { offset, .. } => (*offset, 3u8),
        };
        ao.cmp(&bo).then_with(|| ap.cmp(&bp))
    });

    for event in &events {
        match event {
            LayoutEvent::ContainerOpen {
                offset,
                tag_type,
                attrs,
            } => {
                // Skip if attrs ends with "/" — this is actually a self-closing tag
                // that was also matched by CONTAINER_OPEN_RE. The dedicated
                // ContainerSelfClose event handles it (no stack push).
                if attrs.trim_end().ends_with('/') {
                    continue;
                }

                let container_id = id_re
                    .captures(attrs)
                    .map(|c| c[1].trim().to_string())
                    .unwrap_or_else(|| {
                        anon_counter += 1;
                        format!(
                            "__anon_{}_{}",
                            tag_type.replace("asp:", "").to_lowercase(),
                            anon_counter
                        )
                    });
                let css_class = css_class_re
                    .captures(attrs)
                    .map(|c| c[1].trim().to_string());

                let info = ContainerInfo {
                    id: container_id,
                    tag_type: tag_type.clone(),
                    start_line: char_to_line(*offset),
                    start_offset: *offset,
                    css_class,
                };
                all_containers.push(info.clone());
                container_stack.push(info);
            }
            LayoutEvent::ContainerClose { tag_type, .. } => {
                // Pop matching container from stack (case-insensitive tag match).
                let tag_lower = tag_type.to_lowercase();
                if let Some(pos) = container_stack
                    .iter()
                    .rposition(|c| c.tag_type.to_lowercase() == tag_lower)
                {
                    container_stack.truncate(pos);
                }
            }
            LayoutEvent::ContainerSelfClose {
                offset,
                tag_type,
                attrs,
            } => {
                let container_id = id_re
                    .captures(attrs)
                    .map(|c| c[1].trim().to_string())
                    .unwrap_or_else(|| {
                        anon_counter += 1;
                        format!(
                            "__anon_{}_{}",
                            tag_type.replace("asp:", "").to_lowercase(),
                            anon_counter
                        )
                    });
                let css_class = css_class_re
                    .captures(attrs)
                    .map(|c| c[1].trim().to_string());

                let info = ContainerInfo {
                    id: container_id,
                    tag_type: tag_type.clone(),
                    start_line: char_to_line(*offset),
                    start_offset: *offset,
                    css_class,
                };
                all_containers.push(info);
                // Self-closing containers don't push onto stack (no children).
            }
            LayoutEvent::InputControl {
                offset,
                tag_type,
                attrs,
            } => {
                let Some(id_cap) = id_re.captures(attrs) else {
                    continue;
                };
                let ctrl_id = id_cap[1].trim().to_string();
                let line = char_to_line(*offset);

                // ── Label proximity heuristic ───────────────────────────
                // Find the closest label that ended before this control's offset,
                // within 500 chars (roughly 5 lines).
                let ui_label = label_positions
                    .iter()
                    .rev()
                    .find(|(end_offset, _)| *end_offset <= *offset && (*offset - *end_offset) < 500)
                    .map(|(_, text)| text.clone());

                // ── Table grid position ─────────────────────────────────
                let (table_row, table_col) = find_grid_pos(*offset);

                // ── Parent container ────────────────────────────────────
                let parent_container_id = container_stack.last().map(|c| c.id.clone());

                all_children.push(ChildControl {
                    id: ctrl_id,
                    tag_type: tag_type.clone(),
                    line,
                    offset: *offset,
                    ui_label,
                    table_row,
                    table_col,
                    parent_container_id,
                });
            }
        }
    }

    // ── Phase 4: Emit container symbols ─────────────────────────────────────
    let mut seen_container_ids = std::collections::HashSet::new();
    for container in &all_containers {
        if !seen_container_ids.insert(container.id.clone()) {
            continue;
        }
        let mut meta = HashMap::new();
        let clean_type = container.tag_type.replace("asp:", "");
        meta.insert("container_type".into(), clean_type.clone());

        // Determine layout style heuristic
        let layout_style = match clean_type.to_lowercase().as_str() {
            "table" => "Grid",
            _ => "Flow",
        };
        meta.insert("layout_style".into(), layout_style.into());

        // Check for grouping text attribute or infer from ID
        let logical_grouping = infer_logical_grouping(&container.id);
        if let Some(ref grouping) = logical_grouping {
            meta.insert("logical_grouping".into(), grouping.clone());
        }
        if let Some(ref css) = container.css_class {
            meta.insert("css_class".into(), css.clone());
        }

        symbols.push(ExtractedSymbol {
            name: container.id.clone(),
            kind: "ui_container".into(),
            start_line: container.start_line,
            end_line: container.start_line,
            metadata: Some(meta),
        });
    }

    // ── Phase 5: Emit contains_ui edges ─────────────────────────────────────
    for child in &all_children {
        if let Some(ref parent_id) = child.parent_container_id {
            let mut meta = HashMap::new();
            meta.insert("child_type".into(), child.tag_type.clone());
            if let Some(ref label) = child.ui_label {
                meta.insert("ui_label".into(), label.clone());
            }
            if let Some(row) = child.table_row {
                meta.insert("row".into(), row.to_string());
            }
            if let Some(col) = child.table_col {
                meta.insert("col".into(), col.to_string());
            }

            // Naming convention inference on the child
            if let Some(ref grouping) = infer_logical_grouping(&child.id) {
                meta.insert("logical_grouping".into(), grouping.clone());
            }

            edges.push(ExtractedEdge {
                source_name: parent_id.clone(),
                source_kind: "ui_container".into(),
                source_start_line: child.line,
                source_language: "aspx".into(),
                target_name: child.id.clone(),
                target_kind: Some("control".into()),
                target_start_line: Some(child.line),
                kind: "contains_ui".into(),
                metadata: Some(meta),
            });
        }
    }

    // ── Phase 6: Emit ui_layout_neighbor edges ──────────────────────────────
    // Group children by parent container, then emit neighbor edges in order.
    let mut children_by_parent: HashMap<String, Vec<&ChildControl>> = HashMap::new();
    for child in &all_children {
        if let Some(ref pid) = child.parent_container_id {
            children_by_parent
                .entry(pid.clone())
                .or_default()
                .push(child);
        }
    }

    for (_parent_id, children) in &children_by_parent {
        // Children are already sorted by offset (events were sorted).
        let mut sorted: Vec<&&ChildControl> = children.iter().collect();
        sorted.sort_by_key(|c| c.offset);

        for window in sorted.windows(2) {
            let prev = window[0];
            let next = window[1];
            let mut meta = HashMap::new();
            meta.insert("direction".into(), "next_tab".into());
            if let (Some(pr), Some(pc)) = (prev.table_row, prev.table_col) {
                meta.insert("from_row".into(), pr.to_string());
                meta.insert("from_col".into(), pc.to_string());
            }
            if let (Some(nr), Some(nc)) = (next.table_row, next.table_col) {
                meta.insert("to_row".into(), nr.to_string());
                meta.insert("to_col".into(), nc.to_string());
            }

            edges.push(ExtractedEdge {
                source_name: prev.id.clone(),
                source_kind: "control".into(),
                source_start_line: prev.line,
                source_language: "aspx".into(),
                target_name: next.id.clone(),
                target_kind: Some("control".into()),
                target_start_line: Some(next.line),
                kind: "ui_layout_neighbor".into(),
                metadata: Some(meta),
            });
        }
    }

    // ── Phase 7: Enrich existing control symbols with ui_label ──────────────
    // Instead of creating duplicate symbols, we return additional symbols that
    // the caller can merge. We only emit these for controls that got a ui_label
    // but weren't already emitted as ui_container.
    for child in &all_children {
        if child.ui_label.is_some() || child.table_row.is_some() {
            let mut meta = HashMap::new();
            meta.insert("control_type".into(), child.tag_type.clone());
            if let Some(ref label) = child.ui_label {
                meta.insert("ui_label".into(), label.clone());
            }
            if let Some(row) = child.table_row {
                meta.insert("row".into(), row.to_string());
            }
            if let Some(col) = child.table_col {
                meta.insert("col".into(), col.to_string());
            }
            if let Some(ref grouping) = infer_logical_grouping(&child.id) {
                meta.insert("logical_grouping".into(), grouping.clone());
            }

            // Emit as "control_layout" so it doesn't clash with the existing "control" symbol.
            symbols.push(ExtractedSymbol {
                name: child.id.clone(),
                kind: "control_layout".into(),
                start_line: child.line,
                end_line: child.line,
                metadata: Some(meta),
            });
        }
    }

    (symbols, edges)
}

// ── WinForms Designer Layout Extraction ─────────────────────────────────────

/// Returns true if the file path looks like a WinForms Designer file.
pub fn is_winforms_designer(path: &std::path::Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();
    name.ends_with(".designer.vb") || name.ends_with(".designer.cs")
}

/// Extract UI layout hierarchy from a WinForms Designer file.
///
/// Parses `Controls.Add(...)`, `Location`, `Size`, `TabIndex`, and `Text`
/// assignments to build container→child relationships and spatial grouping.
pub fn extract_winforms_layout(
    _rel_path_str: &str,
    source: &str,
) -> (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) {
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut edges: Vec<ExtractedEdge> = Vec::new();

    // ── Build line offset table ─────────────────────────────────────────────
    let line_offsets: Vec<usize> = {
        let mut offsets = vec![0usize];
        for (i, c) in source.char_indices() {
            if c == '\n' {
                offsets.push(i + 1);
            }
        }
        offsets
    };

    let char_to_line = |char_pos: usize| -> u32 {
        match line_offsets.binary_search(&char_pos) {
            Ok(line) => line as u32,
            Err(line) => line.saturating_sub(1) as u32,
        }
    };

    // ── Compile regexes ─────────────────────────────────────────────────────
    let Some(controls_add_re) = get_compiled_regex(
        &WINFORMS_CONTROLS_ADD_RE,
        r#"(?i)(?:Me|this)\s*\.\s*(\w+)\s*\.\s*Controls\s*\.\s*Add\s*\(\s*(?:Me|this)\s*\.\s*(\w+)\s*\)"#,
        "dle_winforms_controls_add",
    ) else {
        return (symbols, edges);
    };
    let Some(location_re) = get_compiled_regex(
        &WINFORMS_LOCATION_RE,
        r#"(?i)(?:Me|this)\s*\.\s*(\w+)\s*\.\s*Location\s*=\s*[Nn]ew\s+(?:System\.Drawing\.)?Point\s*\(\s*(-?\d+)\s*,\s*(-?\d+)\s*\)"#,
        "dle_winforms_location",
    ) else {
        return (symbols, edges);
    };
    let Some(size_re) = get_compiled_regex(
        &WINFORMS_SIZE_RE,
        r#"(?i)(?:Me|this)\s*\.\s*(\w+)\s*\.\s*Size\s*=\s*[Nn]ew\s+(?:System\.Drawing\.)?Size\s*\(\s*(-?\d+)\s*,\s*(-?\d+)\s*\)"#,
        "dle_winforms_size",
    ) else {
        return (symbols, edges);
    };
    let Some(tabindex_re) = get_compiled_regex(
        &WINFORMS_TABINDEX_RE,
        r#"(?i)(?:Me|this)\s*\.\s*(\w+)\s*\.\s*TabIndex\s*=\s*(\d+)"#,
        "dle_winforms_tabindex",
    ) else {
        return (symbols, edges);
    };
    let Some(text_re) = get_compiled_regex(
        &WINFORMS_TEXT_RE,
        r#"(?i)(?:Me|this)\s*\.\s*(\w+)\s*\.\s*Text\s*=\s*"([^"]*)""#,
        "dle_winforms_text",
    ) else {
        return (symbols, edges);
    };

    // ── Phase 1: Collect control properties ─────────────────────────────────
    let mut controls: HashMap<String, WinFormsControl> = HashMap::new();

    // Controls.Add relationships
    let mut parent_child: Vec<(String, String, u32)> = Vec::new();
    for cap in controls_add_re.captures_iter(source) {
        let parent = cap[1].to_string();
        let child = cap[2].to_string();
        let m = cap.get(0).expect("full match");
        let line = char_to_line(m.start());
        parent_child.push((parent.clone(), child.clone(), line));

        // Ensure both parent and child are in the controls map.
        controls
            .entry(parent.clone())
            .or_insert_with(|| WinFormsControl {
                name: parent.clone(),
                parent: None,
                x: None,
                y: None,
                width: None,
                height: None,
                tab_index: None,
                text: None,
                line,
            });
        controls
            .entry(child.clone())
            .or_insert_with(|| WinFormsControl {
                name: child,
                parent: Some(parent),
                x: None,
                y: None,
                width: None,
                height: None,
                tab_index: None,
                text: None,
                line,
            });
    }

    // Location properties
    for cap in location_re.captures_iter(source) {
        let name = cap[1].to_string();
        let x: i32 = cap[2].parse().unwrap_or(0);
        let y: i32 = cap[3].parse().unwrap_or(0);
        let m = cap.get(0).expect("full match");
        let line = char_to_line(m.start());
        let ctrl = controls
            .entry(name.clone())
            .or_insert_with(|| WinFormsControl {
                name: name.clone(),
                parent: None,
                x: None,
                y: None,
                width: None,
                height: None,
                tab_index: None,
                text: None,
                line,
            });
        ctrl.x = Some(x);
        ctrl.y = Some(y);
    }

    // Size properties
    for cap in size_re.captures_iter(source) {
        let name = cap[1].to_string();
        let w: i32 = cap[2].parse().unwrap_or(0);
        let h: i32 = cap[3].parse().unwrap_or(0);
        let ctrl = controls
            .entry(name.clone())
            .or_insert_with(|| WinFormsControl {
                name: name.clone(),
                parent: None,
                x: None,
                y: None,
                width: None,
                height: None,
                tab_index: None,
                text: None,
                line: 0,
            });
        ctrl.width = Some(w);
        ctrl.height = Some(h);
    }

    // TabIndex properties
    for cap in tabindex_re.captures_iter(source) {
        let name = cap[1].to_string();
        let idx: u32 = cap[2].parse().unwrap_or(0);
        let ctrl = controls
            .entry(name.clone())
            .or_insert_with(|| WinFormsControl {
                name: name.clone(),
                parent: None,
                x: None,
                y: None,
                width: None,
                height: None,
                tab_index: None,
                text: None,
                line: 0,
            });
        ctrl.tab_index = Some(idx);
    }

    // Text properties
    for cap in text_re.captures_iter(source) {
        let name = cap[1].to_string();
        let text = cap[2].to_string();
        let ctrl = controls
            .entry(name.clone())
            .or_insert_with(|| WinFormsControl {
                name: name.clone(),
                parent: None,
                x: None,
                y: None,
                width: None,
                height: None,
                tab_index: None,
                text: None,
                line: 0,
            });
        if !text.is_empty() {
            ctrl.text = Some(text);
        }
    }

    // ── Phase 2: Identify containers vs leaf controls ───────────────────────
    let parent_names: std::collections::HashSet<String> = parent_child
        .iter()
        .filter(|(p, _, _)| controls.contains_key(p))
        .map(|(p, _, _)| p.clone())
        .collect();

    // Emit container symbols
    for name in &parent_names {
        if let Some(ctrl) = controls.get(name) {
            let mut meta = HashMap::new();
            meta.insert("container_type".into(), "GroupBox".into());
            meta.insert("layout_style".into(), "Absolute".into());
            if let Some(ref text) = ctrl.text {
                meta.insert("ui_label".into(), text.clone());
            }
            if let Some(ref grouping) = infer_logical_grouping(name) {
                meta.insert("logical_grouping".into(), grouping.clone());
            }
            if let (Some(x), Some(y)) = (ctrl.x, ctrl.y) {
                meta.insert("x".into(), x.to_string());
                meta.insert("y".into(), y.to_string());
            }
            if let (Some(w), Some(h)) = (ctrl.width, ctrl.height) {
                meta.insert("width".into(), w.to_string());
                meta.insert("height".into(), h.to_string());
            }

            symbols.push(ExtractedSymbol {
                name: name.clone(),
                kind: "ui_container".into(),
                start_line: ctrl.line,
                end_line: ctrl.line,
                metadata: Some(meta),
            });
        }
    }

    // ── Phase 3: Emit contains_ui edges ─────────────────────────────────────
    for (parent, child, line) in &parent_child {
        let mut meta = HashMap::new();
        if let Some(ctrl) = controls.get(child) {
            if let Some(ref text) = ctrl.text {
                meta.insert("ui_label".into(), text.clone());
            }
            if let (Some(x), Some(y)) = (ctrl.x, ctrl.y) {
                meta.insert("x".into(), x.to_string());
                meta.insert("y".into(), y.to_string());
            }
            if let Some(ref grouping) = infer_logical_grouping(child) {
                meta.insert("logical_grouping".into(), grouping.clone());
            }
        }

        edges.push(ExtractedEdge {
            source_name: parent.clone(),
            source_kind: "ui_container".into(),
            source_start_line: *line,
            source_language: "designer".into(),
            target_name: child.clone(),
            target_kind: Some("control".into()),
            target_start_line: None,
            kind: "contains_ui".into(),
            metadata: if meta.is_empty() { None } else { Some(meta) },
        });
    }

    // ── Phase 4: Emit ui_layout_neighbor edges ──────────────────────────────
    // Within each container, sort children by TabIndex (if available) or by Y,X position.
    let mut children_by_parent: HashMap<String, Vec<(u32, &WinFormsControl)>> = HashMap::new();
    for (parent, child, line) in &parent_child {
        if let Some(ctrl) = controls.get(child) {
            children_by_parent
                .entry(parent.clone())
                .or_default()
                .push((*line, ctrl));
        }
    }

    for (_parent, children) in &mut children_by_parent {
        // Sort by tab_index first, then by (y, x) position and declaration line for stability.
        children.sort_by(|a, b| {
            let (a_line, a_ctrl) = *a;
            let (b_line, b_ctrl) = *b;
            match (a_ctrl.tab_index, b_ctrl.tab_index) {
                (Some(ai), Some(bi)) => ai.cmp(&bi).then_with(|| a_line.cmp(&b_line)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => {
                    let ay = a_ctrl.y.unwrap_or(0);
                    let by = b_ctrl.y.unwrap_or(0);
                    ay.cmp(&by)
                        .then_with(|| {
                            let ax = a_ctrl.x.unwrap_or(0);
                            let bx = b_ctrl.x.unwrap_or(0);
                            ax.cmp(&bx)
                        })
                        .then_with(|| a_line.cmp(&b_line))
                }
            }
        });

        for window in children.windows(2) {
            let (_prev_add_line, prev) = window[0];
            let (_next_add_line, next) = window[1];
            let mut meta = HashMap::new();
            meta.insert("direction".into(), "next_tab".into());
            if let Some(ti) = prev.tab_index {
                meta.insert("from_tab_index".into(), ti.to_string());
            }
            if let Some(ti) = next.tab_index {
                meta.insert("to_tab_index".into(), ti.to_string());
            }

            edges.push(ExtractedEdge {
                source_name: prev.name.clone(),
                source_kind: "control".into(),
                source_start_line: prev.line,
                source_language: "designer".into(),
                target_name: next.name.clone(),
                target_kind: Some("control".into()),
                target_start_line: Some(next.line),
                kind: "ui_layout_neighbor".into(),
                metadata: Some(meta),
            });
        }
    }

    // ── Phase 5: Emit control_layout symbols for leaf controls ──────────────
    for (name, ctrl) in &controls {
        if parent_names.contains(name) {
            continue; // Already emitted as ui_container
        }
        let mut meta = HashMap::new();
        meta.insert("control_type".into(), "winforms_control".into());
        if let Some(ref text) = ctrl.text {
            meta.insert("ui_label".into(), text.clone());
        }
        if let (Some(x), Some(y)) = (ctrl.x, ctrl.y) {
            meta.insert("x".into(), x.to_string());
            meta.insert("y".into(), y.to_string());
        }
        if let (Some(w), Some(h)) = (ctrl.width, ctrl.height) {
            meta.insert("width".into(), w.to_string());
            meta.insert("height".into(), h.to_string());
        }
        if let Some(ti) = ctrl.tab_index {
            meta.insert("tab_index".into(), ti.to_string());
        }
        if let Some(ref grouping) = infer_logical_grouping(name) {
            meta.insert("logical_grouping".into(), grouping.clone());
        }

        symbols.push(ExtractedSymbol {
            name: name.clone(),
            kind: "control_layout".into(),
            start_line: ctrl.line,
            end_line: ctrl.line,
            metadata: Some(meta),
        });
    }

    (symbols, edges)
}

// ── Naming Convention Inference ──────────────────────────────────────────────

/// Infer logical grouping from a control ID based on common naming conventions.
///
/// Detects medical/ophthalmic suffixes:
///   - `_OD`, `OD`  → "RightEye"
///   - `_OS`, `OS`  → "LeftEye"
///   - `_R`, `R`    → "Right"
///   - `_L`, `L`    → "Left"
///   - `_OU`        → "BothEyes"
///
/// Also detects panel-based groupings from the ID prefix:
///   - `pnlRightEye...` → "RightEye"
///   - `grpAddress...`  → "Address"
fn infer_logical_grouping(control_id: &str) -> Option<String> {
    // Check suffixes first (most specific)
    let id = control_id.trim();
    if id.is_empty() {
        return None;
    }

    // Suffix-based patterns (case-insensitive)
    let id_lower = id.to_lowercase();
    if id_lower.ends_with("_od")
        || (id_lower.len() > 2
            && id_lower.ends_with("od")
            && !id_lower.ends_with("method")
            && !id_lower.ends_with("period"))
    {
        return Some("RightEye".into());
    }
    if id_lower.ends_with("_os")
        || (id_lower.len() > 2
            && id_lower.ends_with("os")
            && !id_lower.ends_with("photos")
            && !id_lower.ends_with("videos"))
    {
        return Some("LeftEye".into());
    }
    if id_lower.ends_with("_ou") {
        return Some("BothEyes".into());
    }
    if id_lower.ends_with("_r") {
        return Some("Right".into());
    }
    if id_lower.ends_with("_l") {
        return Some("Left".into());
    }

    // Panel prefix patterns (pnlXxx → Xxx, grpXxx → Xxx)
    static GROUP_PREFIX_RE: OnceLock<Regex> = OnceLock::new();
    if let Some(re) = get_compiled_regex(
        &GROUP_PREFIX_RE,
        r"^(?:pnl|grp|grb|panel|group)([A-Z]\w+)",
        "dle_group_prefix",
    ) {
        if let Some(cap) = re.captures(id) {
            return Some(cap[1].to_string());
        }
    }

    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_webforms_container_hierarchy() {
        let markup = r#"
<asp:Panel ID="pnlRightEye" runat="server">
    <asp:Label ID="lblSphere" runat="server" Text="Sphere:" />
    <asp:TextBox ID="txtSphere_OD" runat="server" />
    <asp:Label ID="lblCylinder" runat="server" Text="Cylinder:" />
    <asp:TextBox ID="txtCylinder_OD" runat="server" />
    <asp:Label ID="lblAxis" runat="server" Text="Axis:" />
    <asp:TextBox ID="txtAxis_OD" runat="server" />
</asp:Panel>
"#;
        let (syms, edges) = extract_webforms_layout("ExaminationData.ascx", markup);

        // Should have a ui_container for pnlRightEye
        let container = syms
            .iter()
            .find(|s| s.kind == "ui_container" && s.name == "pnlRightEye");
        assert!(container.is_some(), "missing pnlRightEye ui_container");
        let meta = container.unwrap().metadata.as_ref().unwrap();
        assert_eq!(
            meta.get("container_type").map(|s| s.as_str()),
            Some("Panel")
        );
        assert_eq!(
            meta.get("logical_grouping").map(|s| s.as_str()),
            Some("RightEye")
        );

        // Should have contains_ui edges from pnlRightEye to each textbox
        let contains_edges: Vec<_> = edges.iter().filter(|e| e.kind == "contains_ui").collect();
        assert!(
            contains_edges.len() >= 3,
            "should have at least 3 contains_ui edges, got {}",
            contains_edges.len()
        );

        // Verify label proximity: txtSphere_OD should have ui_label = "Sphere:"
        let sphere_edge = contains_edges
            .iter()
            .find(|e| e.target_name == "txtSphere_OD");
        assert!(
            sphere_edge.is_some(),
            "missing contains_ui edge for txtSphere_OD"
        );
        let meta = sphere_edge.unwrap().metadata.as_ref().unwrap();
        assert_eq!(meta.get("ui_label").map(|s| s.as_str()), Some("Sphere:"));
        assert_eq!(
            meta.get("logical_grouping").map(|s| s.as_str()),
            Some("RightEye")
        );

        // Should have ui_layout_neighbor edges (tab order)
        let neighbor_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "ui_layout_neighbor")
            .collect();
        assert!(
            neighbor_edges.len() >= 2,
            "should have neighbor edges, got {}",
            neighbor_edges.len()
        );

        // First neighbor: txtSphere_OD → txtCylinder_OD
        let first_neighbor = neighbor_edges
            .iter()
            .find(|e| e.source_name == "txtSphere_OD");
        assert!(
            first_neighbor.is_some(),
            "missing neighbor from txtSphere_OD"
        );
        assert_eq!(first_neighbor.unwrap().target_name, "txtCylinder_OD");
    }

    #[test]
    fn test_table_grid_detection() {
        let markup = r#"
<table>
    <tr>
        <td><asp:Label ID="lblName" runat="server" Text="Name:" /></td>
        <td><asp:TextBox ID="txtName" runat="server" /></td>
    </tr>
    <tr>
        <td><asp:Label ID="lblAge" runat="server" Text="Age:" /></td>
        <td><asp:TextBox ID="txtAge" runat="server" /></td>
    </tr>
</table>
"#;
        let (syms, _edges) = extract_webforms_layout("Form.aspx", markup);

        // Should detect table as ui_container
        let table_container = syms.iter().find(|s| {
            s.kind == "ui_container"
                && s.metadata.as_ref().map_or(false, |m| {
                    m.get("container_type").map(|s| s.as_str()) == Some("table")
                })
        });
        assert!(table_container.is_some(), "missing table ui_container");

        // txtName should have grid position row=0, col=1
        let name_layout = syms
            .iter()
            .find(|s| s.kind == "control_layout" && s.name == "txtName");
        assert!(name_layout.is_some(), "missing txtName control_layout");
        let meta = name_layout.unwrap().metadata.as_ref().unwrap();
        assert_eq!(meta.get("row").map(|s| s.as_str()), Some("0"));
        assert_eq!(meta.get("col").map(|s| s.as_str()), Some("1"));
        assert_eq!(meta.get("ui_label").map(|s| s.as_str()), Some("Name:"));

        // txtAge should have grid position row=1, col=1
        let age_layout = syms
            .iter()
            .find(|s| s.kind == "control_layout" && s.name == "txtAge");
        assert!(age_layout.is_some(), "missing txtAge control_layout");
        let meta = age_layout.unwrap().metadata.as_ref().unwrap();
        assert_eq!(meta.get("row").map(|s| s.as_str()), Some("1"));
        assert_eq!(meta.get("col").map(|s| s.as_str()), Some("1"));
    }

    #[test]
    fn test_naming_convention_inference() {
        assert_eq!(
            infer_logical_grouping("txtSphere_OD"),
            Some("RightEye".into())
        );
        assert_eq!(
            infer_logical_grouping("txtCylinder_OS"),
            Some("LeftEye".into())
        );
        assert_eq!(
            infer_logical_grouping("txtValue_OU"),
            Some("BothEyes".into())
        );
        assert_eq!(infer_logical_grouping("chkActive_R"), Some("Right".into()));
        assert_eq!(infer_logical_grouping("chkActive_L"), Some("Left".into()));
        assert_eq!(
            infer_logical_grouping("pnlRightEye"),
            Some("RightEye".into())
        );
        assert_eq!(infer_logical_grouping("grpAddress"), Some("Address".into()));
        assert_eq!(infer_logical_grouping("txtPlain"), None);
    }

    #[test]
    fn test_winforms_designer_extraction() {
        let source = r#"
        Me.pnlRightEye.Controls.Add(Me.txtSphere)
        Me.pnlRightEye.Controls.Add(Me.txtCylinder)
        Me.pnlRightEye.Controls.Add(Me.txtAxis)
        Me.txtSphere.Location = New System.Drawing.Point(10, 30)
        Me.txtSphere.Size = New System.Drawing.Size(100, 20)
        Me.txtSphere.TabIndex = 0
        Me.txtSphere.Text = "Sphere"
        Me.txtCylinder.Location = New System.Drawing.Point(120, 30)
        Me.txtCylinder.Size = New System.Drawing.Size(100, 20)
        Me.txtCylinder.TabIndex = 1
        Me.txtAxis.Location = New System.Drawing.Point(230, 30)
        Me.txtAxis.Size = New System.Drawing.Size(100, 20)
        Me.txtAxis.TabIndex = 2
        "#;

        let (syms, edges) = extract_winforms_layout("ExaminationData.Designer.vb", source);

        // Should have container symbol for pnlRightEye
        let container = syms
            .iter()
            .find(|s| s.kind == "ui_container" && s.name == "pnlRightEye");
        assert!(container.is_some(), "missing pnlRightEye ui_container");

        // Should have 3 contains_ui edges
        let contains: Vec<_> = edges.iter().filter(|e| e.kind == "contains_ui").collect();
        assert_eq!(contains.len(), 3, "expected 3 contains_ui edges");

        // Should have 2 neighbor edges (Sphere→Cylinder, Cylinder→Axis)
        let neighbors: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "ui_layout_neighbor")
            .collect();
        assert_eq!(neighbors.len(), 2, "expected 2 neighbor edges");

        // Verify tab order: txtSphere → txtCylinder → txtAxis
        let first = neighbors.iter().find(|e| e.source_name == "txtSphere");
        assert!(first.is_some());
        assert_eq!(first.unwrap().target_name, "txtCylinder");

        let second = neighbors.iter().find(|e| e.source_name == "txtCylinder");
        assert!(second.is_some());
        assert_eq!(second.unwrap().target_name, "txtAxis");

        // Verify spatial metadata on contains_ui edge for txtSphere
        let sphere_edge = contains.iter().find(|e| e.target_name == "txtSphere");
        assert!(sphere_edge.is_some());
        let meta = sphere_edge.unwrap().metadata.as_ref().unwrap();
        assert_eq!(meta.get("x").map(|s| s.as_str()), Some("10"));
        assert_eq!(meta.get("y").map(|s| s.as_str()), Some("30"));
    }

    #[test]
    fn test_nested_containers() {
        let markup = r#"
<asp:Panel ID="pnlOuter" runat="server">
    <asp:Panel ID="pnlInner_OD" runat="server">
        <asp:TextBox ID="txtField_OD" runat="server" />
    </asp:Panel>
</asp:Panel>
"#;
        let (syms, edges) = extract_webforms_layout("Nested.aspx", markup);

        // Both panels should be ui_containers
        assert!(
            syms.iter()
                .any(|s| s.name == "pnlOuter" && s.kind == "ui_container")
        );
        assert!(
            syms.iter()
                .any(|s| s.name == "pnlInner_OD" && s.kind == "ui_container")
        );

        // txtField_OD should be contained by pnlInner_OD (innermost)
        let inner_edge = edges
            .iter()
            .find(|e| e.kind == "contains_ui" && e.target_name == "txtField_OD");
        assert!(inner_edge.is_some());
        assert_eq!(inner_edge.unwrap().source_name, "pnlInner_OD");
    }

    #[test]
    fn test_winforms_csharp_designer() {
        let source = r#"
        this.groupBox1.Controls.Add(this.textBox1);
        this.groupBox1.Controls.Add(this.textBox2);
        this.textBox1.Location = new System.Drawing.Point(15, 25);
        this.textBox1.Size = new System.Drawing.Size(200, 20);
        this.textBox1.TabIndex = 0;
        this.textBox2.Location = new System.Drawing.Point(15, 55);
        this.textBox2.Size = new System.Drawing.Size(200, 20);
        this.textBox2.TabIndex = 1;
        "#;

        let (syms, edges) = extract_winforms_layout("Form1.Designer.cs", source);

        // Container
        assert!(
            syms.iter()
                .any(|s| s.name == "groupBox1" && s.kind == "ui_container")
        );

        // Contains edges
        let contains: Vec<_> = edges.iter().filter(|e| e.kind == "contains_ui").collect();
        assert_eq!(contains.len(), 2);

        // Neighbor: textBox1 → textBox2 (by tab index)
        let neighbors: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "ui_layout_neighbor")
            .collect();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].source_name, "textBox1");
        assert_eq!(neighbors[0].target_name, "textBox2");
    }

    #[test]
    fn test_empty_markup() {
        let (syms, edges) = extract_webforms_layout("Empty.aspx", "");
        assert!(syms.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn test_no_containers_just_controls() {
        // Controls without any container should not produce contains_ui edges
        let markup = r#"
<asp:TextBox ID="txtOrphan" runat="server" />
<asp:Button ID="btnOrphan" runat="server" />
"#;
        let (_syms, edges) = extract_webforms_layout("Orphan.aspx", markup);
        let contains_edges: Vec<_> = edges.iter().filter(|e| e.kind == "contains_ui").collect();
        assert!(
            contains_edges.is_empty(),
            "orphan controls should not have contains_ui edges"
        );
        // But they should still have ui_layout_neighbor edges (NO — they have no parent)
        let neighbor_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "ui_layout_neighbor")
            .collect();
        assert!(
            neighbor_edges.is_empty(),
            "orphan controls have no parent, so no neighbor edges"
        );
    }

    #[test]
    fn test_anonymous_container() {
        // Container without an ID should get a synthetic ID
        let markup = r#"
<div>
    <asp:TextBox ID="txtInside" runat="server" />
</div>
"#;
        let (syms, edges) = extract_webforms_layout("Anon.aspx", markup);
        let container = syms.iter().find(|s| s.kind == "ui_container");
        assert!(container.is_some(), "should have an anonymous container");
        assert!(
            container.unwrap().name.starts_with("__anon_"),
            "anonymous container should have synthetic ID"
        );

        let contains: Vec<_> = edges.iter().filter(|e| e.kind == "contains_ui").collect();
        assert_eq!(contains.len(), 1, "should have 1 contains_ui edge");
        assert_eq!(contains[0].target_name, "txtInside");
    }

    #[test]
    fn test_label_not_too_far() {
        // A label that's more than 500 chars away should NOT be matched
        let filler = " ".repeat(600);
        let markup = format!(
            r#"<asp:Label ID="lblFar" runat="server" Text="FarLabel:" />{}<asp:TextBox ID="txtFarAway" runat="server" />"#,
            filler
        );
        // Wrap in a Panel so we can check the contains_ui metadata
        let full_markup = format!(
            r#"<asp:Panel ID="pnlTest" runat="server">{}</asp:Panel>"#,
            markup
        );
        let (_syms, edges) = extract_webforms_layout("Far.aspx", &full_markup);
        let contains = edges
            .iter()
            .find(|e| e.kind == "contains_ui" && e.target_name == "txtFarAway");
        if let Some(edge) = contains {
            let label = edge.metadata.as_ref().and_then(|m| m.get("ui_label"));
            assert!(
                label.is_none(),
                "label should NOT be matched when >500 chars away"
            );
        }
    }

    #[test]
    fn test_mixed_case_tags() {
        let markup = r#"
<ASP:PANEL ID="PNLUPPER" RUNAT="SERVER">
    <Asp:TextBox ID="txtMixed" Runat="Server" />
</ASP:PANEL>
"#;
        let (syms, edges) = extract_webforms_layout("MixedCase.aspx", markup);
        assert!(
            syms.iter()
                .any(|s| s.name == "PNLUPPER" && s.kind == "ui_container")
        );
        let contains: Vec<_> = edges.iter().filter(|e| e.kind == "contains_ui").collect();
        assert_eq!(contains.len(), 1);
        assert_eq!(contains[0].target_name, "txtMixed");
    }

    #[test]
    fn test_self_closing_panel() {
        // A self-closing Panel can't contain children
        let markup = r#"
<asp:Panel ID="pnlEmpty" runat="server" />
<asp:TextBox ID="txtOutside" runat="server" />
"#;
        let (syms, edges) = extract_webforms_layout("SelfClose.aspx", markup);
        assert!(
            syms.iter()
                .any(|s| s.name == "pnlEmpty" && s.kind == "ui_container")
        );
        let contains: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "contains_ui" && e.source_name == "pnlEmpty")
            .collect();
        assert!(
            contains.is_empty(),
            "self-closing container should have no children"
        );
    }

    #[test]
    fn test_mismatched_close_tag() {
        // Mismatched closing tag should not corrupt the container stack
        let markup = r#"
<asp:Panel ID="pnlOuter" runat="server">
    <asp:TextBox ID="txtInside" runat="server" />
</div>
<asp:TextBox ID="txtAfter" runat="server" />
</asp:Panel>
"#;
        let (_syms, edges) = extract_webforms_layout("Mismatch.aspx", markup);
        // txtInside should be parented to pnlOuter (the close </div> doesn't match)
        let inside: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "contains_ui" && e.target_name == "txtInside")
            .collect();
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].source_name, "pnlOuter");
        // txtAfter should also be parented to pnlOuter since the </div> didn't pop it
        let after: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "contains_ui" && e.target_name == "txtAfter")
            .collect();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].source_name, "pnlOuter");
    }

    #[test]
    fn test_deeply_nested_parent_tracking() {
        // Controls at different nesting levels should have the correct parent
        let markup = r#"
<asp:Panel ID="pnlL1" runat="server">
    <asp:TextBox ID="txtL1" runat="server" />
    <asp:Panel ID="pnlL2" runat="server">
        <asp:TextBox ID="txtL2" runat="server" />
        <div id="divL3">
            <asp:TextBox ID="txtL3" runat="server" />
        </div>
    </asp:Panel>
    <asp:TextBox ID="txtL1b" runat="server" />
</asp:Panel>
"#;
        let (_syms, edges) = extract_webforms_layout("Deep.aspx", markup);
        let parent_of = |ctrl: &str| -> String {
            edges
                .iter()
                .find(|e| e.kind == "contains_ui" && e.target_name == ctrl)
                .map(|e| e.source_name.clone())
                .unwrap_or_default()
        };
        assert_eq!(parent_of("txtL1"), "pnlL1");
        assert_eq!(parent_of("txtL2"), "pnlL2");
        assert_eq!(parent_of("txtL3"), "divL3");
        assert_eq!(parent_of("txtL1b"), "pnlL1");
    }

    #[test]
    fn test_winforms_no_spatial_data() {
        // Controls.Add with no Location/Size/TabIndex
        let source = r#"
        Me.pnlSimple.Controls.Add(Me.txtA)
        Me.pnlSimple.Controls.Add(Me.txtB)
        "#;
        let (syms, edges) = extract_winforms_layout("Simple.Designer.vb", source);
        assert!(
            syms.iter()
                .any(|s| s.name == "pnlSimple" && s.kind == "ui_container")
        );
        let contains: Vec<_> = edges.iter().filter(|e| e.kind == "contains_ui").collect();
        assert_eq!(contains.len(), 2);
        // Still should have neighbor edge (sorted by document order / line)
        let neighbors: Vec<_> = edges
            .iter()
            .filter(|e| e.kind == "ui_layout_neighbor")
            .collect();
        assert_eq!(neighbors.len(), 1);
    }
}
