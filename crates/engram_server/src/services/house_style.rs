//! Bounded neighboring-page idioms shared by page context and review advice.
//! Sampled markup conventions are examples, not universal UI requirements.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Sibling files read per territory (bounded, honest cap).
pub const SIBLING_SCAN: usize = 12;
/// Siblings shown in the page context (most similar first).
pub const SIBLINGS_SHOWN: usize = 3;
pub const CLASS_CAP: usize = 12;

#[derive(Debug, Clone, Serialize)]
pub struct HouseStyle {
    /// The directory the page lives in (project-relative).
    pub territory: String,
    /// Nearest siblings, most similar first (server-control overlap).
    pub siblings: Vec<SiblingExemplar>,
    /// User controls (`uc:files` …) the shown siblings reuse, with counts.
    pub user_controls: Vec<CountedIdiom>,
    /// Resource families (`text`, `label`, `control` …) the siblings read.
    pub resource_families: Vec<CountedIdiom>,
    /// CSS classes the shown siblings share.
    pub common_classes: Vec<CountedIdiom>,
    /// Idioms EVERY shown sibling has and this page lacks.
    pub missing_in_page: Vec<String>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SiblingExemplar {
    pub path: String,
    /// Jaccard overlap of server-control types with the page (0–1).
    pub similarity: f32,
    pub shared_controls: Vec<String>,
    pub user_controls: Vec<String>,
    /// `CssClass` of the sibling's `asp:Panel` message boxes (alert …).
    pub message_panels: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CountedIdiom {
    pub name: String,
    pub siblings: usize,
}

/// The markup idioms of one file (or of a set of added lines).
#[derive(Debug, Clone, Default)]
pub struct MarkupIdioms {
    pub controls: BTreeSet<String>,
    pub user_controls: BTreeSet<String>,
    pub resource_families: BTreeSet<String>,
    pub classes: BTreeSet<String>,
    /// False means malformed/over-budget markup, not an empty class inventory.
    pub class_inventory_known: bool,
    pub message_panels: Vec<String>,
}

pub fn markup_idioms(content: &str) -> MarkupIdioms {
    let tag_re = regex::Regex::new(r"<([A-Za-z][A-Za-z0-9]*):([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    let res_re = regex::Regex::new(r"Resources\s*[.:]\s*([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    let panel_re =
        regex::Regex::new(r#"(?i)<asp:Panel[^>]*CssClass\s*=\s*"([^"]*alert[^"]*)""#).unwrap();
    let mut m = MarkupIdioms::default();
    for c in tag_re.captures_iter(content) {
        let prefix = c[1].to_lowercase();
        let tag = c[2].to_lowercase();
        if prefix == "asp" {
            m.controls.insert(tag);
        } else {
            m.user_controls.insert(format!("{prefix}:{tag}"));
        }
    }
    for c in res_re.captures_iter(content) {
        m.resource_families.insert(c[1].to_lowercase());
    }
    if let Ok(occurrences) = static_class_occurrences(content) {
        m.class_inventory_known = true;
        for occurrence in occurrences {
            m.classes.extend(occurrence.classes);
        }
    }
    for c in panel_re.captures_iter(content) {
        m.message_panels.push(c[1].trim().to_string());
    }
    m
}

/// The territory's sibling files (same directory, `.aspx`/`.ascx`, the page
/// itself excluded), each with its idioms; at most `SIBLING_SCAN` read.
pub fn scan_siblings(project_dir: &Path, aspx_file: &str) -> (String, Vec<(String, MarkupIdioms)>) {
    let rel = aspx_file.replace('\\', "/");
    let territory = rel
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();
    let dir = if territory.is_empty() {
        project_dir.to_path_buf()
    } else {
        project_dir.join(&territory)
    };
    let mut out: Vec<(String, MarkupIdioms)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut names: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| {
                let l = n.to_lowercase();
                l.ends_with(".aspx") || l.ends_with(".ascx")
            })
            .collect();
        names.sort();
        for n in names {
            let p = if territory.is_empty() {
                n.clone()
            } else {
                format!("{territory}/{n}")
            };
            if p.eq_ignore_ascii_case(&rel) {
                continue;
            }
            if out.len() >= SIBLING_SCAN {
                break;
            }
            if let Ok(c) = std::fs::read_to_string(dir.join(&n)) {
                out.push((p, markup_idioms(&c)));
            }
        }
    }
    (territory, out)
}

fn counted(m: BTreeMap<String, usize>, cap: usize) -> Vec<CountedIdiom> {
    let mut v: Vec<CountedIdiom> = m
        .into_iter()
        .map(|(name, siblings)| CountedIdiom { name, siblings })
        .collect();
    v.sort_by(|x, y| {
        y.siblings
            .cmp(&x.siblings)
            .then_with(|| x.name.cmp(&y.name))
    });
    v.truncate(cap);
    v
}

/// The house style of `aspx_file`'s territory: siblings ranked by
/// server-control overlap, the idioms the top siblings share, and what this
/// page lacks. Never fails — an empty territory is reported as such.
pub fn house_style_for(project_dir: &Path, aspx_file: &str, page_content: &str) -> HouseStyle {
    let page = markup_idioms(page_content);
    let (territory, cands) = scan_siblings(project_dir, aspx_file);
    let scanned = cands.len();
    let unknown_class_siblings = cands
        .iter()
        .filter(|(_, m)| !m.class_inventory_known)
        .count();
    let mut scored: Vec<(f32, String, MarkupIdioms)> = cands
        .into_iter()
        .map(|(p, m)| {
            let inter = page.controls.intersection(&m.controls).count();
            let uni = page.controls.union(&m.controls).count();
            let sim = if uni == 0 {
                0.0
            } else {
                inter as f32 / uni as f32
            };
            (sim, p, m)
        })
        .collect();
    scored.sort_by(|x, y| {
        y.0.partial_cmp(&x.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| x.1.cmp(&y.1))
    });
    scored.truncate(SIBLINGS_SHOWN);
    let n = scored.len();
    let mut uc: BTreeMap<String, usize> = BTreeMap::new();
    let mut rf: BTreeMap<String, usize> = BTreeMap::new();
    let mut cls: BTreeMap<String, usize> = BTreeMap::new();
    for (_, _, m) in &scored {
        for x in &m.user_controls {
            *uc.entry(x.clone()).or_default() += 1;
        }
        for x in &m.resource_families {
            *rf.entry(x.clone()).or_default() += 1;
        }
        for x in &m.classes {
            *cls.entry(x.clone()).or_default() += 1;
        }
    }
    let mut missing: Vec<String> = Vec::new();
    if n > 0 {
        for (name, c) in &uc {
            if *c == n && !page.user_controls.contains(name) {
                missing.push(name.clone());
            }
        }
        for (name, c) in &cls {
            if page.class_inventory_known
                && unknown_class_siblings == 0
                && *c == n
                && !page.classes.contains(name)
            {
                missing.push(name.clone());
            }
        }
        for (name, c) in &rf {
            if *c == n && !page.resource_families.contains(name) {
                missing.push(format!("Resources.{name}"));
            }
        }
    }
    let mut note = if n == 0 {
        format!(
            "no sibling page in `{territory}` — nothing to copy from next door; use find_implementation_pattern for the idiom you need"
        )
    } else {
        format!(
            "{n} nearest sibling(s) of {scanned} scanned in `{territory}`; copy their containers, classes and resource keys when you add markup here"
        )
    };
    if !page.class_inventory_known || unknown_class_siblings > 0 {
        note.push_str(&format!(
            "; static class coverage: current page {}; {unknown_class_siblings} of {scanned} sampled sibling inventories unknown. Missing-class advice skipped; observed class exemplars are partial. Resource/user-control checks are independent.",
            if page.class_inventory_known { "known" } else { "unknown" }
        ));
    }
    HouseStyle {
        territory,
        siblings: scored
            .iter()
            .map(|(sim, p, m)| SiblingExemplar {
                path: p.clone(),
                similarity: *sim,
                shared_controls: page.controls.intersection(&m.controls).cloned().collect(),
                user_controls: m.user_controls.iter().cloned().collect(),
                message_panels: m.message_panels.clone(),
            })
            .collect(),
        user_controls: counted(uc, 8),
        resource_families: counted(rf, 6),
        common_classes: counted(cls, CLASS_CAP),
        missing_in_page: missing,
        note,
    }
}

/// Markdown for the page context.
pub fn render_house_style(hs: &HouseStyle) -> String {
    let mut md = String::from("## House style (nearest siblings in this territory)\n\n");
    md.push_str(&format!(
        "- **Territory**: `{}` — {}\n",
        hs.territory, hs.note
    ));
    for s in &hs.siblings {
        let ucs = if s.user_controls.is_empty() {
            String::new()
        } else {
            format!("; user controls: {}", s.user_controls.join(", "))
        };
        let panels = if s.message_panels.is_empty() {
            String::new()
        } else {
            format!("; message panels: {}", s.message_panels.join(" | "))
        };
        md.push_str(&format!(
            "- **Sibling** `{}` (control overlap {:.0}%; shared: {}){}{}\n",
            s.path,
            s.similarity * 100.0,
            if s.shared_controls.is_empty() {
                "none".to_string()
            } else {
                s.shared_controls.join(", ")
            },
            ucs,
            panels
        ));
    }
    let list = |v: &[CountedIdiom]| {
        v.iter()
            .map(|c| format!("`{}` ({})", c.name, c.siblings))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !hs.user_controls.is_empty() {
        md.push_str(&format!(
            "- **User controls reused next door**: {}\n",
            list(&hs.user_controls)
        ));
    }
    if !hs.resource_families.is_empty() {
        md.push_str(&format!(
            "- **Resource families**: {}\n",
            list(&hs.resource_families)
        ));
    }
    if !hs.common_classes.is_empty() {
        md.push_str(&format!(
            "- **Classes the siblings share**: {}\n",
            list(&hs.common_classes)
        ));
    }
    if !hs.missing_in_page.is_empty() {
        md.push_str(&format!(
            "- **Every sibling has it, this page lacks it**: {}\n",
            hs.missing_in_page.join(", ")
        ));
    }
    md.push('\n');
    md
}

/// Source-spanned static class values only; not DOM rendering or CSS resolution.
#[derive(Debug)]
pub struct ClassOccurrence {
    pub classes: BTreeSet<String>,
    pub attribute_lines: std::ops::RangeInclusive<usize>,
    pub tag_lines: std::ops::RangeInclusive<usize>,
}

/// Bounded lexical markup inventory. Server expressions, entity-dependent and
/// dynamic values are omitted; malformed ownership makes the inventory unknown.
/// Raw-text elements and comments never supply fake tags or attributes.
pub fn static_class_occurrences(source: &str) -> Result<Vec<ClassOccurrence>, &'static str> {
    if source.len() > 2 * 1024 * 1024 {
        return Err("markup exceeds 2 MiB class inventory budget");
    }
    let b = source.as_bytes();
    let lower = source.to_ascii_lowercase();
    let mut i = 0;
    let mut out = Vec::new();
    let starts: Vec<usize> = std::iter::once(0)
        .chain(
            b.iter()
                .enumerate()
                .filter_map(|(n, c)| (*c == b'\n').then_some(n + 1)),
        )
        .collect();
    let line = |offset| starts.partition_point(|n| *n <= offset);
    let name_byte = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b':' | b'_' | b'-' | b'.');
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let skip = if source[i..].starts_with("<!--") {
            Some((4, "-->"))
        } else if source[i..].starts_with("<%--") {
            Some((4, "--%>"))
        } else if source[i..].starts_with("<%") {
            Some((2, "%>"))
        } else if source[i..].starts_with("<![CDATA[") {
            Some((9, "]]>"))
        } else {
            None
        };
        if let Some((n, end)) = skip {
            i += n;
            i += source[i..]
                .find(end)
                .ok_or("unterminated comment/server block")?
                + end.len();
            continue;
        }
        let tag_start = i;
        i += 1;
        let closing = b.get(i) == Some(&b'/');
        if closing {
            i += 1;
        }
        if b.get(i) == Some(&b'!') {
            // Only the common simple doctype form; internal subsets are unknown.
            let end = source[i..].find('>').ok_or("unterminated declaration")? + i;
            if !lower[i..end].starts_with("!doctype ")
                || source[i..end].contains(['<', '[', '\'', '"'])
            {
                return Err("unsupported markup declaration");
            }
            i = end + 1;
            continue;
        }
        if !b.get(i).is_some_and(u8::is_ascii_alphabetic) {
            return Err("ambiguous tag opening");
        }
        let ns = i;
        while i < b.len() && name_byte(b[i]) {
            i += 1;
        }
        let tag = &lower[ns..i];
        let mut classes = Vec::new();
        let mut dynamic_attributes = false;
        let mut seen = BTreeSet::new();
        loop {
            let before_space = i;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if b.get(i) == Some(&b'>') {
                i += 1;
                break;
            }
            if b.get(i..i + 2) == Some(b"/>") {
                i += 2;
                break;
            }
            if closing || i == before_space {
                return Err("ambiguous attribute boundary");
            }
            if b.get(i..i + 2) == Some(b"<%") {
                dynamic_attributes = true;
                i += 2;
                i += source[i..]
                    .find("%>")
                    .ok_or("unterminated tag server expression")?
                    + 2;
                continue;
            }
            let attr_start = i;
            if !b
                .get(i)
                .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, b'_' | b':'))
            {
                return Err("unsupported attribute syntax");
            }
            while i < b.len() && name_byte(b[i]) {
                i += 1;
            }
            let attr = &lower[attr_start..i];
            if !seen.insert(attr) {
                return Err("duplicate attribute ownership");
            }
            let name_end = i;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if b.get(i) != Some(&b'=') {
                i = name_end;
                continue;
            }
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            let quote = *b.get(i).ok_or("missing attribute value")?;
            if !matches!(quote, b'\'' | b'"') {
                // Unquoted values aren't used as exact class evidence.
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                    if matches!(b[i], b'<' | b'\'' | b'"' | b'=' | b'`') {
                        return Err("ambiguous unquoted attribute");
                    }
                    i += 1;
                }
                continue;
            }
            i += 1;
            let value_start = i;
            while i < b.len() && b[i] != quote {
                if b.get(i..i + 2) == Some(b"<%") {
                    i += 2;
                    i += source[i..]
                        .find("%>")
                        .ok_or("unterminated attribute server expression")?
                        + 2;
                } else {
                    i += 1;
                }
            }
            if i == b.len() {
                return Err("unterminated quoted attribute");
            }
            let value = &source[value_start..i];
            i += 1;
            if matches!(attr, "class" | "cssclass")
                && !value.contains(['<', '>', '&', '{', '}', '%', '\\'])
            {
                let tokens: BTreeSet<_> =
                    value.split_ascii_whitespace().map(str::to_string).collect();
                if tokens.iter().all(|t| {
                    t.chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-'))
                }) {
                    classes.push((tokens, attr_start, i - 1));
                }
            }
        }
        if dynamic_attributes {
            classes.clear();
        }
        for (classes, start, end) in classes {
            out.push(ClassOccurrence {
                classes,
                attribute_lines: line(start)..=line(end),
                tag_lines: line(tag_start)..=line(i - 1),
            });
            if out.len() > 50_000 {
                return Err("class inventory exceeds 50000 attributes");
            }
        }
        if !closing
            && matches!(
                tag,
                "script"
                    | "style"
                    | "textarea"
                    | "title"
                    | "xmp"
                    | "iframe"
                    | "noembed"
                    | "noframes"
                    | "plaintext"
            )
        {
            if tag == "plaintext" {
                break;
            }
            let marker = format!("</{tag}");
            let mut from = i;
            loop {
                let at = lower[from..]
                    .find(&marker)
                    .ok_or("unterminated raw-text element")?
                    + from;
                let end = at + marker.len();
                if b.get(end)
                    .is_some_and(|c| c.is_ascii_whitespace() || *c == b'>')
                {
                    i = at;
                    break;
                }
                from = end;
            }
        }
    }
    Ok(out)
}

/// Check every supplied new-side hunk line, not merely added lines. Unknown or
/// inconsistent hunk counts cannot prove which current attributes are unchanged.
pub fn source_bound_classes(
    source: &str,
    diff: &super::pre_commit_review_service::DiffFile,
) -> Result<(BTreeSet<String>, BTreeSet<String>), &'static str> {
    if source.len() > 2 * 1024 * 1024 {
        return Err("markup exceeds 2 MiB class inventory budget");
    }
    let lines: Vec<_> = source.lines().collect();
    let mut added = BTreeSet::new();
    if diff.hunks.is_empty() {
        return Err("no source-bound diff hunks");
    }
    for h in &diff.hunks {
        let mut new = h.new_start;
        let mut old_count = 0;
        let mut new_count = 0;
        for raw in &h.body {
            let Some(prefix) = raw.as_bytes().first() else {
                return Err("invalid hunk body");
            };
            match prefix {
                b' ' | b'+' => {
                    if new == 0 || lines.get(new - 1).copied() != Some(&raw[1..]) {
                        return Err("diff new-side context does not match current source");
                    }
                    if *prefix == b'+' {
                        added.insert(new);
                    }
                    if *prefix == b' ' {
                        old_count += 1;
                    }
                    new = new.checked_add(1).ok_or("hunk line overflow")?;
                    new_count += 1;
                }
                b'-' => old_count += 1,
                b'\\' => {}
                _ => return Err("invalid hunk prefix"),
            }
        }
        if old_count != h.old_count || new_count != h.new_count {
            return Err("hunk counts do not establish source coverage");
        }
    }
    if added.len() != diff.added_lines.len()
        || diff.added_lines.iter().any(|(n, s)| {
            !added.contains(n) || lines.get(n.saturating_sub(1)).copied() != Some(s.as_str())
        })
    {
        return Err("added-line inventory mismatch");
    }
    let mut new_classes = BTreeSet::new();
    let mut existing = BTreeSet::new();
    for occurrence in static_class_occurrences(source)? {
        if occurrence
            .attribute_lines
            .clone()
            .any(|n| added.contains(&n))
        {
            new_classes.extend(occurrence.classes);
        } else if !occurrence.tag_lines.clone().any(|n| added.contains(&n)) {
            existing.extend(occurrence.classes);
        }
    }
    Ok((new_classes, existing))
}
