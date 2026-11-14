//! Observe links in supplied guidance without adopting or reading their targets.
use super::{PolicyContext, PolicyReference};
use crate::services::instruction_maintenance::manifest::{markdown_links, normalize};

const REFERENCE_LIMIT: usize = 32;

fn markdown_path(target: &str) -> bool {
    target.split(['#', '?']).next().unwrap_or_default().rsplit_once('.')
        .is_some_and(|(_, extension)| matches!(extension.to_ascii_lowercase().as_str(), "md" | "mdc" | "markdown"))
}

fn same_path(left: &str, right: &str) -> bool {
    if cfg!(windows) { left.eq_ignore_ascii_case(right) } else { left == right }
}

pub(super) fn observe(context: &mut PolicyContext) {
    context.reference_coverage = if context.sources.is_empty() { "not_evaluated" } else { "complete" }.into();
    if context.sources.is_empty() { return; }
    context.instruction.push_str(" Direct-source coverage does not establish linked-document coverage. References observe supported Markdown links and inline Markdown paths in supplied complete sources only. A link does not establish authority, applicability or a valid fragment. Assess the source conditions and target's own scope; resolve missing guidance before claiming complete applicable policy coverage. For historical contexts, references belong to source_revision; never substitute current checkout content.");
    for source in &context.sources {
        let parsed = markdown_links::parse(&source.content, 256);
        for (status, (_, count)) in parsed.issues {
            if status.ends_with("omitted") {
                context.references_omitted += count;
            }
        }
        for link in parsed.links {
            if link.kind == "image" || (!markdown_path(&link.target) && link.unresolved_status.is_none()) { continue; }
            if context.references.len() >= REFERENCE_LIMIT {
                context.references_omitted += 1;
                continue;
            }
            let (path, fragment) = if link.unresolved_status.is_none() && link.kind == "link" {
                normalize(&source.path, &link.target).map_or((None, None), |(path, fragment)| (Some(path), fragment))
            } else { (None, None) };
            let supplied = path.as_deref().and_then(|path| context.sources.iter().find(|row| same_path(&row.path, path)));
            let omitted = path.as_deref().and_then(|path| context.omissions.iter().find(|row| same_path(&row.path, path)));
            let status = if let Some(status) = link.unresolved_status { status }
                else if link.kind == "inline_code_path" { "inline_path_basis_unverified" }
                else if path.is_none() { "unsupported_or_nonlocal_target" }
                else if supplied.is_some() { "supplied" }
                else if omitted.is_some() { "omitted_source" }
                else { "not_supplied_applicability_unverified" };
            if status != "supplied" { context.reference_coverage = "incomplete".into(); }
            context.references.push(PolicyReference {
                source_path: source.path.clone(), source_blake3: source.blake3.clone(),
                byte_range: [link.offset, link.end], target: link.target, path, fragment, status: status.into(),
                target_blake3: supplied.map(|row| row.blake3.clone()).or_else(|| omitted.and_then(|row| row.blake3.clone())),
            });
        }
    }
    if context.references_omitted > 0 { context.reference_coverage = "incomplete".into(); }
}

#[cfg(test)]
mod tests {
    use super::super::{resolve, resolve_with_budget, PolicyBudget};

    fn write(root: &std::path::Path, name: &str, text: &str) {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn returned_guidance_discloses_unreturned_link_without_adopting_its_scope() {
        let temp = tempfile::tempdir().unwrap();
        let root = "Use [shared guidance](docs/required.md#permissions) when applicable.\nRead [VB guidance](.github/instructions/vb.instructions.md).\nException: preserve manual approval.\n";
        write(temp.path(), "AGENTS.md", root);
        write(temp.path(), "docs/required.md", "This linked source must not become repository-wide policy merely because it was linked.\n");
        write(temp.path(), ".github/instructions/vb.instructions.md", "---\napplyTo: 'src/*.vb'\n---\nPreserve event wiring.\n");
        let context = resolve(temp.path(), &["src/Page.vb".into()], &[]);
        assert_eq!(context.sources.len(), 2);
        assert_eq!(context.references.len(), 2);
        let missing = context.references.iter().find(|row| row.target.starts_with("docs/")).unwrap();
        assert_eq!(missing.path.as_deref(), Some("docs/required.md"));
        assert_eq!(missing.fragment.as_deref(), Some("permissions"));
        assert_eq!(missing.status, "not_supplied_applicability_unverified");
        assert_eq!(missing.source_blake3, blake3::hash(root.as_bytes()).to_hex().to_string());
        assert!(root[missing.byte_range[0]..missing.byte_range[1]].contains("docs/required.md#permissions"));
        assert_eq!(context.references.iter().find(|row| row.status == "supplied").unwrap().path.as_deref(), Some(".github/instructions/vb.instructions.md"));
        assert_eq!(context.reference_coverage, "incomplete");
        assert_eq!(context.sources.iter().find(|row| row.path == "AGENTS.md").unwrap().content, root);
        assert!(context.render_markdown().contains("not_supplied_applicability_unverified"));
    }

    #[test]
    fn omitted_guidance_and_ambiguous_references_remain_explicit() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), "AGENTS.md", "Read [shared](.github/copilot-instructions.md). Also assess `docs/manual.md` and [missing][definition].\n");
        let large = "Preserve qualifications.\n".repeat(500);
        write(temp.path(), ".github/copilot-instructions.md", &large);
        let context = resolve(temp.path(), &["src/Page.vb".into()], &[]);
        assert!(context.references.iter().any(|row| row.status == "omitted_source" && row.target_blake3.as_deref() == Some(blake3::hash(large.as_bytes()).to_hex().as_str())));
        assert!(context.references.iter().any(|row| row.status == "inline_path_basis_unverified" && row.path.is_none()));
        assert!(context.references.iter().any(|row| row.status == "unresolved_reference_label" && row.path.is_none()));
        assert_eq!(context.reference_coverage, "incomplete");
    }

    #[test]
    fn reference_metadata_is_bounded_without_hiding_source_text_or_tail_omissions() {
        let temp = tempfile::tempdir().unwrap();
        let root = (0..35).map(|i| format!("[Guide {i}](docs/guide{i}.md)\n")).collect::<String>();
        write(temp.path(), "AGENTS.md", &root);
        let context = resolve_with_budget(temp.path(), &["src/Page.vb".into()], &[], PolicyBudget::default());
        assert_eq!(context.references.len(), 32);
        assert_eq!(context.references_omitted, 3);
        assert_eq!(context.reference_coverage, "incomplete");
        assert_eq!(context.sources[0].content, root);
    }
}
