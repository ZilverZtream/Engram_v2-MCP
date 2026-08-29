//! External audit 2026-08-29 row 5 v3 (owner: "metric → exemplar → enforce").
//! The house style of a page's TERRITORY — what an engineer opens next door
//! before writing markup. On a Bootstrap WebForms app without a component
//! layer (OciusX: 211 pages, 36 user controls, `.row` on 81 % of pages) the
//! convention lives in the nearest sibling pages, the user controls they
//! reuse and the idioms they share; a catalog of container/class families
//! was measured negative twice (story-invariant Bootstrap universals).
//! Shared by `get_page_context` (slice 2) and the pre_push_audit gate
//! (slice 3).

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
    pub message_panels: Vec<String>,
}

pub fn markup_idioms(content: &str) -> MarkupIdioms {
    let tag_re = regex::Regex::new(r"<([A-Za-z][A-Za-z0-9]*):([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    let res_re = regex::Regex::new(r"Resources\s*[.:]\s*([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    let class_re = regex::Regex::new(r#"(?i)(?:class|CssClass)\s*=\s*"([^"]*)""#).unwrap();
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
    for c in class_re.captures_iter(content) {
        for t in c[1].split_whitespace() {
            if !t.contains(['<', '%', '(', ')']) {
                m.classes.insert(t.to_lowercase());
            }
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
            if *c == n && !page.classes.contains(name) {
                missing.push(name.clone());
            }
        }
        for (name, c) in &rf {
            if *c == n && !page.resource_families.contains(name) {
                missing.push(format!("Resources.{name}"));
            }
        }
    }
    let note = if n == 0 {
        format!(
            "no sibling page in `{territory}` — nothing to copy from next door; use find_implementation_pattern for the idiom you need"
        )
    } else {
        format!(
            "{n} nearest sibling(s) of {scanned} scanned in `{territory}`; copy their containers, classes and resource keys when you add markup here"
        )
    };
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
