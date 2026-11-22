use crate::services::pre_commit_review_service::DiffFile;
use engram_index::{HybridQuery, HybridSearchEngine};
use std::{collections::BTreeSet, fmt::Write, path::Path, sync::LazyLock};

/// Use the kind of change to query guidance that may use entirely different
/// words from the added code. These sources supply reading, not line verdicts.
pub(super) fn related(
    root: &Path,
    files: &[DiffFile],
    engine: &HybridSearchEngine,
    base: &HybridQuery,
) -> String {
    let mut topics = BTreeSet::new();
    for f in files {
        let path = f.path.to_lowercase();
        if path.ends_with(".resx") {
            topics.insert((
                "Resource keys",
                "multilingual resource files controls buttons dropdowns headers error messages",
                "resource|multilingual|error messages",
            ));
        }
        for (_, s) in &f.added_lines {
            let l = s.to_ascii_lowercase();
            if l.contains("keydown") || l.contains("keyup") || l.contains("data-enter") {
                topics.insert((
                    "Keyboard actions",
                    "keyboard activatable enter key button keyboard handlers",
                    "keyboard|data-enter",
                ));
            }
            if ["alert", "bg-warning", "text-warning", "background-color"]
                .iter()
                .any(|v| l.contains(v))
            {
                topics.insert((
                    "Feedback banners",
                    "alert banners success warning danger feedback bootstrap",
                    "alert banner|alert-success|alert-warning",
                ));
            }
            if l.contains("glyphicon") || l.contains("fa-") {
                topics.insert((
                    "Icons",
                    "icons icon library font awesome",
                    "font awesome|icon library",
                ));
            }
            if s.chars().count() > 120 {
                topics.insert((
                    "Long lines",
                    "line length characters maximum coding style",
                    "line length|characters per line",
                ));
            }
        }
    }
    if topics.is_empty() {
        return String::new();
    }
    let mut output = String::new();
    let mut seen = BTreeSet::new();
    let mut docs = Vec::new();
    static DOC: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?:`|\()([\w./-]+\.md)(?:#[\w-]+)?(?:`|\))").unwrap());
    for dir in [".claude/skills", ".agents/skills"] {
        let Ok(path) = engram_core::safe_join(root, dir) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(path) else {
            continue;
        };
        let mut entries = entries.flatten().map(|e| e.file_name()).collect::<Vec<_>>();
        entries.sort();
        for name in entries.iter().take(64) {
            let sub = format!("{dir}/{}", name.to_string_lossy());
            let Ok(guide) = engram_core::safe_read_to_string(root, &format!("{sub}/SKILL.md"))
            else {
                continue;
            };
            for cap in DOC.captures_iter(&guide).take(24) {
                let rel = format!("{sub}/{}", &cap[1]);
                if !seen.insert(rel.clone()) || docs.len() >= 64 {
                    continue;
                }
                if let Ok(text) = engram_core::safe_read_to_string(root, &rel) {
                    docs.push((rel, text));
                }
            }
        }
    }
    for (topic, query, anchors) in topics {
        for (path, doc) in &docs {
            let lines: Vec<_> = doc.lines().collect();
            if let Some((i, _)) = lines.iter().enumerate().find(|(_, l)| {
                anchors.split('|').any(|a| l.to_lowercase().contains(a))
                    && l.trim_start().starts_with("- ")
            }) {
                let mut clause = vec![lines[i].trim()];
                clause.extend(
                    lines[i + 1..]
                        .iter()
                        .take_while(|s| s.starts_with("  "))
                        .take(5)
                        .map(|s| s.trim()),
                );
                let _ = writeln!(
                    output,
                    "- {topic}: {}:{} — {}",
                    path,
                    i + 1,
                    clause.join(" ").chars().take(650).collect::<String>()
                );
                break;
            }
        }
        let mut q = base.clone();
        q.namespace = "memory_bank".into();
        q.generation = 0;
        q.text = query.into();
        q.top_k = 6;
        q.use_mmr = false;
        let hits = match engine.lexical_search(&q) {
            Ok(h) => h,
            Err(_) => {
                let _ = writeln!(output, "- {topic}: memory-bank lookup unavailable.");
                continue;
            }
        };
        for hit in hits {
            let Ok(Some((_, _, text, _, _))) = engine.get_doc_by_pk(&hit.pk) else {
                continue;
            };
            let rows = text.lines().collect::<Vec<_>>();
            let Some(i) = rows
                .iter()
                .position(|l| anchors.split('|').any(|a| l.to_lowercase().contains(a)))
            else {
                continue;
            };
            let excerpt = rows[i..]
                .iter()
                .take(4)
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
            let _ = writeln!(
                output,
                "- {topic}: {} (memory_bank, document {}) — {}",
                hit.path,
                hit.doc_id,
                excerpt.chars().take(390).collect::<String>()
            );
            break;
        }
    }
    if output.is_empty() {
        output
    } else {
        format!("## Related repository guidance (verify applicability)\n{output}\n")
    }
}
