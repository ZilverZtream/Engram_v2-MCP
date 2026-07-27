//! Support-KB generator — documentation for SUPPORT audiences (human
//! agents or a support AI), as opposed to `produce_claude_md`'s
//! developer audience.
//!
//! v1 design (see the support-kb-program memory):
//! - Feature taxonomy = the ingested end-user docs corpus: memory_bank
//!   sections whose ids look like `docs/**/<feature>/index`. Any project
//!   that ingests its user docs gets cards for free; the taxonomy is
//!   NOT hardcoded to any product.
//! - One support card per feature: front-matter (feature id, roles
//!   extracted from the docs' "Available to:" line), the docs sections
//!   themselves (purpose + workflows as the USER experiences them),
//!   and code-derived business rules from the business_logic namespace
//!   (the WHAT-IT-CONTROLS / enabled-vs-disabled tables).
//! - `write_to_disk` writes a `support-kb/` tree in the project root
//!   (engram-owned directory; cards are regenerated wholesale) plus an
//!   `index.md`. The tool ALWAYS returns a summary + the index inline.

use std::collections::BTreeMap;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::handlers::validate_project_id;
use crate::tools::Engram;

/// Extract the roles from a docs section's "Available to:" line —
/// the ground-truth visibility statement the docs carry per feature.
pub(crate) fn extract_roles_line(content: &str) -> Option<String> {
    for line in content.lines() {
        let t = line.trim().trim_start_matches('>').trim();
        if let Some(rest) = t
            .strip_prefix("**Available to:**")
            .or_else(|| t.strip_prefix("Available to:"))
        {
            let roles = rest.trim();
            if !roles.is_empty() {
                return Some(roles.to_string());
            }
        }
    }
    None
}

/// Feature slug from a docs index section id:
/// `docs/docs/change-requests/index` → `change-requests`.
/// A top-level `docs/index` is the docs ROOT, not a feature (live: it
/// produced a roleless "docs" card) — the feature index must live under
/// a docs subtree.
pub(crate) fn feature_slug(section_id: &str) -> Option<&str> {
    let stripped = section_id.strip_suffix("/index")?;
    if !stripped.contains('/') {
        return None;
    }
    stripped.rsplit('/').next().filter(|s| !s.is_empty())
}

/// The docs' own title for a section: frontmatter `title:` first, then the
/// first `# ` heading (first 30 lines).
pub(crate) fn extract_doc_title(content: &str) -> Option<String> {
    let mut in_fm = false;
    for (i, line) in content.lines().take(30).enumerate() {
        let t = line.trim();
        if i == 0 && t == "---" {
            in_fm = true;
            continue;
        }
        if in_fm {
            if t == "---" {
                in_fm = false;
                continue;
            }
            if let Some(v) = t.strip_prefix("title:") {
                let v = v.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
            continue;
        }
        if let Some(h) = t.strip_prefix("# ")
            && !h.trim().is_empty()
        {
            return Some(h.trim().to_string());
        }
    }
    None
}

/// `Resources.<family>.<Key>` and `GetGlobalResourceObject("<family>","<Key>")`
/// references in code — the exact user-visible strings a feature renders.
pub(crate) fn extract_resx_refs(code: &str) -> Vec<(String, String)> {
    use std::sync::LazyLock;
    static DOT: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\bResources\.(\w+)\.(\w+)").expect("valid regex"));
    static OBJ: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"GetGlobalResourceObject\(\s*"(\w+)"\s*,\s*"(\w+)""#)
            .expect("valid regex")
    });
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for re in [&*DOT, &*OBJ] {
        for c in re.captures_iter(code) {
            let pair = (c[1].to_string(), c[2].to_string());
            if seen.insert(pair.clone()) {
                out.push(pair);
            }
        }
    }
    out
}

/// Values for `keys` from resx XML. Machine-generated resx is shape-stable,
/// so a targeted regex beats an XML dependency for this read-only scan.
pub(crate) fn parse_resx_values(
    xml: &str,
    keys: &std::collections::HashSet<String>,
) -> std::collections::HashMap<String, String> {
    use std::sync::LazyLock;
    static DATA: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?s)<data\s+name="([^"]+)"[^>]*>.*?<value>([^<]*)</value>"#)
            .expect("valid regex")
    });
    let mut out = std::collections::HashMap::new();
    for c in DATA.captures_iter(xml) {
        if keys.contains(&c[1]) {
            out.insert(c[1].to_string(), c[2].trim().to_string());
        }
    }
    out
}

/// `change-requests` → `Change Requests`.
pub(crate) fn humanize_slug(slug: &str) -> String {
    slug.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Engram {
    pub async fn handle_produce_support_kb(
        &self,
        req: crate::models::ProduceSupportKbRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let max_features = req.max_features.clamp(1, 200);

        // 1. Feature taxonomy from the docs corpus.
        let reg = self.state.registry.clone();
        let pid = req.project_id.clone();
        let sections = tokio::task::spawn_blocking(move || reg.list_memory_sections(&pid))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // feature slug → (index section, sibling sections)
        let mut features: BTreeMap<String, (Option<usize>, Vec<usize>)> = BTreeMap::new();
        for (i, s) in sections.iter().enumerate() {
            if !s.section_id.starts_with("docs/") {
                continue;
            }
            if let Some(slug) = feature_slug(&s.section_id) {
                features.entry(slug.to_string()).or_default().0 = Some(i);
            } else if let Some(parent) = s.section_id.rsplit_once('/').map(|(p, _)| p) {
                if let Some(slug) = parent.rsplit('/').next() {
                    features.entry(slug.to_string()).or_default().1.push(i);
                }
            }
        }
        // Only features that HAVE an index section count (siblings alone
        // are sub-pages of something we didn't recognise).
        features.retain(|_, (idx, _)| idx.is_some());
        // A feature index that CONTAINS other feature indexes is a taxonomy
        // LEVEL (the docs root — live: docs/docs/index produced a roleless
        // "docs" card at any nesting depth), not a feature.
        let dirs: Vec<(String, String)> = features
            .iter()
            .filter_map(|(slug, (idx, _))| {
                idx.map(|i| {
                    (
                        slug.clone(),
                        sections[i]
                            .section_id
                            .trim_end_matches("/index")
                            .to_string(),
                    )
                })
            })
            .collect();
        let levels: std::collections::HashSet<String> = dirs
            .iter()
            .filter(|(_, d)| {
                dirs.iter()
                    .any(|(_, other)| other != d && other.starts_with(&format!("{d}/")))
            })
            .map(|(s, _)| s.clone())
            .collect();
        features.retain(|slug, _| !levels.contains(slug));

        if features.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "result: no docs-derived features found.\n\
                 hints: the support KB derives its feature taxonomy from ingested \
                 end-user docs (memory_bank sections named `docs/**/<feature>/index`). \
                 Ingest the product's user documentation into the memory bank first \
                 (update_memory_bank per section), then re-run."
                    .to_string(),
            )]));
        }

        // 2. Assemble one card per feature.
        let mut cards: Vec<(String, String)> = Vec::new(); // (slug, markdown)
        for (slug, (idx, siblings)) in features.iter().take(max_features) {
            let index_sec = &sections[idx.expect("retained above")];
            let roles = extract_roles_line(&index_sec.content)
                .unwrap_or_else(|| "(not stated in docs)".to_string());
            // Human title: the docs' own frontmatter/heading first — the
            // memory-bank section title is often the raw ingest path
            // ("docs/docs/change-requests/index"), useless on a card.
            let title = extract_doc_title(&index_sec.content)
                .or_else(|| {
                    let t = index_sec.title.trim();
                    (!t.is_empty() && !t.contains('/')).then(|| t.to_string())
                })
                .unwrap_or_else(|| humanize_slug(slug));

            // Code-derived rules: hybrid search over the business_logic
            // namespace with the feature title (GlobalMutable — no
            // generation filter applies).
            let hq = engram_index::hybrid::HybridQuery {
                project_id: req.project_id.to_string(),
                namespace: "business_logic".into(),
                generation: gen_,
                text: format!("{title} {slug}"),
                top_k: 2,
                fts_mode: "loose".into(),
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                use_mmr: true,
            };
            let cancel = tokio_util::sync::CancellationToken::new();
            let hits = ps
                .search
                .search(&hq, None, &cancel)
                .await
                .unwrap_or_default();
            let mut rules_md = String::new();
            for h in hits.into_iter().take(2) {
                if let Ok(Some((_, _, content, _, _))) = ps.search.get_doc_by_pk(&h.pk) {
                    let excerpt: String = content.chars().take(1500).collect();
                    rules_md.push_str(&excerpt);
                    rules_md.push_str("\n\n");
                }
            }
            if rules_md.trim().is_empty() {
                rules_md = "(no code-derived business rules indexed for this feature — \
                            run analyze_business_logic to populate)"
                    .to_string();
            }

            let mut card = String::with_capacity(8_000);
            card.push_str(&format!(
                "---\nfeature: {slug}\ntitle: {title}\navailable_to: {roles}\n\
                 generated_by: engram produce_support_kb\n---\n\n"
            ));
            card.push_str(&format!("# Support Card — {title}\n\n"));
            card.push_str(&format!("**Visible to:** {roles}\n\n"));
            card.push_str("## What it is (from the product docs)\n\n");
            let index_excerpt: String = index_sec.content.chars().take(4000).collect();
            card.push_str(&index_excerpt);
            card.push('\n');
            // Up to 3 sibling docs pages (workflows, how-tos).
            for &si in siblings.iter().take(3) {
                let sub = &sections[si];
                card.push_str(&format!("\n## {}\n\n", sub.title));
                let sub_excerpt: String = sub.content.chars().take(2500).collect();
                card.push_str(&sub_excerpt);
                card.push('\n');
            }
            card.push_str("\n## Business rules (derived from the code)\n\n");
            card.push_str(&rules_md);
            card.push('\n');

            // Exact UI strings: the feature's code files → the resx keys
            // they render → per-locale values. Engram's unique edge for
            // email support: the answer can quote the EXACT button/label
            // text the user sees, in the user's language. Fail-soft: no
            // code hits / no refs / no resx families → section omitted.
            let code_q = engram_index::hybrid::HybridQuery {
                project_id: req.project_id.to_string(),
                namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                generation: gen_,
                text: format!("{title} {slug}"),
                top_k: 8,
                fts_mode: "loose".into(),
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                use_mmr: true,
            };
            let code_hits = ps
                .search
                .search(&code_q, None, &cancel)
                .await
                .unwrap_or_default();
            let mut code_files: Vec<String> = Vec::new();
            for h in &code_hits {
                let p = h.path.as_str().replace('\\', "/");
                let pl = p.to_lowercase();
                if [".aspx", ".ascx", ".vb", ".cs", ".ts", ".js", ".vbhtml"]
                    .iter()
                    .any(|e| pl.ends_with(e))
                    && !code_files.contains(&p)
                {
                    code_files.push(p);
                }
                if code_files.len() >= 5 {
                    break;
                }
            }
            let project_dir = std::path::PathBuf::from(&rec.directory);
            let mut refs: Vec<(String, String)> = Vec::new();
            for f in &code_files {
                if let Ok(full) = engram_core::safe_join(&project_dir, f)
                    && let Ok(src) = std::fs::read_to_string(&full)
                {
                    for r in extract_resx_refs(&src) {
                        if !refs.contains(&r) {
                            refs.push(r);
                        }
                    }
                }
                if refs.len() >= 12 {
                    break;
                }
            }
            refs.truncate(12);
            if !refs.is_empty() {
                // Locate each family's resx set via the graph's file list
                // (generic — no hardcoded resources directory).
                let all_files: Vec<String> = self
                    .state
                    .graph
                    .list_file_node_metadata(&req.project_id)
                    .map(|m| {
                        m.into_iter()
                            .map(|(rp, _)| rp.as_str().replace('\\', "/"))
                            .collect()
                    })
                    .unwrap_or_default();
                let families: std::collections::BTreeSet<&String> =
                    refs.iter().map(|(f, _)| f).collect();
                let mut table = String::new();
                for family in families {
                    let fam_lower = family.to_lowercase();
                    let keys: std::collections::HashSet<String> = refs
                        .iter()
                        .filter(|(f, _)| f == family)
                        .map(|(_, k)| k.clone())
                        .collect();
                    // default + locale files: <family>.resx, <family>.<loc>.resx
                    let mut fam_files: Vec<&String> = all_files
                        .iter()
                        .filter(|p| {
                            let pl = p.to_lowercase();
                            pl.ends_with(&format!("/{fam_lower}.resx"))
                                || (pl.contains(&format!("/{fam_lower}.")) && pl.ends_with(".resx"))
                        })
                        .collect();
                    fam_files.sort();
                    fam_files.truncate(8);
                    if fam_files.is_empty() {
                        continue;
                    }
                    // locale label from the filename: text.resx → default,
                    // text.en.resx → en.
                    let mut cols: Vec<(String, std::collections::HashMap<String, String>)> =
                        Vec::new();
                    for rf in fam_files {
                        let Ok(full) = engram_core::safe_join(&project_dir, rf) else {
                            continue;
                        };
                        let Ok(xml) = std::fs::read_to_string(&full) else {
                            continue;
                        };
                        let base = rf.rsplit('/').next().unwrap_or(rf);
                        let parts: Vec<&str> = base.split('.').collect();
                        let loc = if parts.len() >= 3 {
                            parts[parts.len() - 2].to_string()
                        } else {
                            "default".to_string()
                        };
                        cols.push((loc, parse_resx_values(&xml, &keys)));
                    }
                    if cols.is_empty() {
                        continue;
                    }
                    let mut sorted_keys: Vec<&String> = keys.iter().collect();
                    sorted_keys.sort();
                    table.push_str(&format!(
                        "\n**{family} family** | Key | {} |\n|---|{}\n",
                        cols.iter()
                            .map(|(l, _)| l.as_str())
                            .collect::<Vec<_>>()
                            .join(" | "),
                        "---|".repeat(cols.len() + 1)
                    ));
                    for k in sorted_keys {
                        let vals: Vec<String> = cols
                            .iter()
                            .map(|(_, m)| m.get(k).cloned().unwrap_or_default())
                            .collect();
                        if vals.iter().all(|v| v.is_empty()) {
                            continue;
                        }
                        table.push_str(&format!("| {k} | {} |\n", vals.join(" | ")));
                    }
                }
                if !table.trim().is_empty() {
                    card.push_str(
                        "\n## Exact UI strings (from the resx resources)\n\
                         Quote these EXACTLY when guiding a user — they are the \
                         button/label texts the user sees on screen, per language:\n",
                    );
                    card.push_str(&table);
                    card.push('\n');
                }
            }
            cards.push((slug.clone(), card));
        }

        // 3. Index document.
        let mut index_md = String::from(
            "# Support KB (generated)\n\nOne card per product feature; taxonomy \
             derived from the ingested end-user docs, rules derived from code.\n\n",
        );
        for (slug, card) in &cards {
            let roles = card
                .lines()
                .find_map(|l| l.strip_prefix("available_to: "))
                .unwrap_or("");
            index_md.push_str(&format!("- [{slug}]({slug}.md) — {roles}\n"));
        }

        // 4. Optional disk tree (engram-owned directory).
        let mut written = 0usize;
        if req.write_to_disk {
            let project_dir = std::path::PathBuf::from(&rec.directory);
            // Engram-owned output in a customer tree: keep it out of the
            // team's git status / pushes via the local-only exclude file.
            crate::utils::files::ensure_git_excluded(&project_dir, "support-kb/");
            let kb_dir = engram_core::safe_join(&project_dir, "support-kb")
                .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
            std::fs::create_dir_all(&kb_dir)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            for (slug, card) in &cards {
                let path = engram_core::safe_join(&kb_dir, &format!("{slug}.md"))
                    .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
                std::fs::write(&path, card)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                written += 1;
            }
            let idx_path = engram_core::safe_join(&kb_dir, "index.md")
                .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
            std::fs::write(&idx_path, &index_md)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            written += 1;
        }

        let summary = format!(
            "# Support KB generated\n\nfeatures: {} (from docs taxonomy)\n\
             cards written to disk: {}\n\n{}\n",
            cards.len(),
            if req.write_to_disk {
                format!("{written} (support-kb/)")
            } else {
                "0 (write_to_disk=false — inline index below)".to_string()
            },
            index_md
        );
        Ok(CallToolResult::success(vec![Content::text(summary)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_roles_line_handles_docs_form() {
        let c = "---\ntitle: X\n---\n\n# X\n\n> **Available to:** Administrator, Foreman, User\n\n## Overview\n";
        assert_eq!(
            extract_roles_line(c).as_deref(),
            Some("Administrator, Foreman, User")
        );
        assert_eq!(extract_roles_line("no roles here"), None);
    }

    #[test]
    fn feature_slug_from_index_ids() {
        assert_eq!(
            feature_slug("docs/docs/change-requests/index"),
            Some("change-requests")
        );
        assert_eq!(feature_slug("docs/docs/change-requests/creating"), None);
        // The docs ROOT index is not a feature (it produced a roleless
        // "docs" card on the live KB).
        assert_eq!(feature_slug("docs/index"), None);
    }

    #[test]
    fn resx_refs_and_values_round_trip() {
        let code = r#"
            lblStatus.Text = Resources.text.The_file_already_exists
            Dim s = Resources.label.Codes_in_category
            Dim t = GetGlobalResourceObject("text", "Upload_selected_files")
            ' duplicate ref must dedup:
            x = Resources.text.The_file_already_exists
        "#;
        let refs = extract_resx_refs(code);
        assert_eq!(refs.len(), 3, "{refs:?}");
        assert!(refs.contains(&("text".into(), "The_file_already_exists".into())));
        assert!(refs.contains(&("label".into(), "Codes_in_category".into())));
        assert!(refs.contains(&("text".into(), "Upload_selected_files".into())));

        let xml = r#"<?xml version="1.0"?><root>
            <data name="The_file_already_exists" xml:space="preserve">
              <value>Filen finns redan</value>
            </data>
            <data name="Unrelated_key" xml:space="preserve">
              <value>nope</value>
            </data>
        </root>"#;
        let keys: std::collections::HashSet<String> = ["The_file_already_exists".to_string()]
            .into_iter()
            .collect();
        let vals = parse_resx_values(xml, &keys);
        assert_eq!(
            vals.get("The_file_already_exists").map(String::as_str),
            Some("Filen finns redan")
        );
        assert!(!vals.contains_key("Unrelated_key"));
    }

    #[test]
    fn doc_title_from_frontmatter_heading_or_slug() {
        // Frontmatter title wins (the live Change-Requests docs shape).
        let fm = "---\ntitle: Change Requests\ndescription: Track modifications\n---\n\n# Something Else\n";
        assert_eq!(extract_doc_title(fm).as_deref(), Some("Change Requests"));
        // No frontmatter → first heading.
        let h = "\n# As-Built Notes\n\ncontent";
        assert_eq!(extract_doc_title(h).as_deref(), Some("As-Built Notes"));
        // Neither → caller falls back to the humanized slug.
        assert_eq!(extract_doc_title("plain text only"), None);
        assert_eq!(humanize_slug("change-requests"), "Change Requests");
        assert_eq!(humanize_slug("as_built_notes"), "As Built Notes");
    }
}
