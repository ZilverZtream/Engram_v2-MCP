//! Markdown serialization for memory sections — the portable, human-readable
//! format used by `export_capture_pack` (backup) and `import_memory_bank`
//! (restore / cross-project copy). A memory that can only live in
//! `registry.redb` dies with it; this makes the store portable.
//!
//! Format: a `---`-delimited key/value front-matter block, then the body.
//! Round-trips losslessly for the fields that matter.

use engram_core::MemorySection;

/// Serialize a section to portable markdown.
pub fn to_markdown(sec: &MemorySection) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("section_id: {}\n", sec.section_id));
    out.push_str(&format!("title: {}\n", sec.title));
    if let Some(k) = &sec.kind {
        out.push_str(&format!("kind: {k}\n"));
    }
    if let Some(a) = &sec.author {
        out.push_str(&format!("author: {a}\n"));
    }
    if !sec.tags.is_empty() {
        out.push_str(&format!("tags: {}\n", sec.tags.join(", ")));
    }
    if !sec.related_files.is_empty() {
        out.push_str(&format!(
            "related_files: {}\n",
            sec.related_files.join(", ")
        ));
    }
    out.push_str(&format!("created_at_ms: {}\n", sec.created_at_ms));
    out.push_str(&format!("updated_at_ms: {}\n", sec.updated_at_ms));
    if let Some(r) = sec.review_after_ms {
        out.push_str(&format!("review_after_ms: {r}\n"));
    }
    out.push_str("---\n");
    out.push_str(&sec.content);
    out
}

/// A section parsed from portable markdown. `section_id` and `title` fall
/// back to sensible defaults when the front-matter omits them.
#[derive(Debug, Clone)]
pub struct ParsedMemory {
    pub section_id: Option<String>,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub author: Option<String>,
    pub tags: Vec<String>,
    pub related_files: Vec<String>,
    pub created_at_ms: Option<u64>,
    pub review_after_ms: Option<u64>,
    pub content: String,
}

/// Parse portable markdown back into fields. Tolerant: a document without a
/// front-matter block is treated as pure content.
pub fn from_markdown(text: &str) -> ParsedMemory {
    let mut p = ParsedMemory {
        section_id: None,
        title: None,
        kind: None,
        author: None,
        tags: Vec::new(),
        related_files: Vec::new(),
        created_at_ms: None,
        review_after_ms: None,
        content: text.to_string(),
    };

    // Front-matter only if the doc opens with a `---` line.
    let trimmed = text.trim_start_matches(['\u{feff}']);
    if !trimmed.starts_with("---") {
        return p;
    }
    // Split off the block between the first two `---` lines.
    let mut lines = trimmed.lines();
    let _open = lines.next(); // the opening ---
    let mut fm = String::new();
    let mut body_started = false;
    let mut body = String::new();
    for line in lines {
        if !body_started && line.trim_end() == "---" {
            body_started = true;
            continue;
        }
        if body_started {
            body.push_str(line);
            body.push('\n');
        } else {
            fm.push_str(line);
            fm.push('\n');
        }
    }
    // No closing delimiter → not real front-matter; keep the whole doc.
    if !body_started {
        return p;
    }

    let csv = |v: &str| -> Vec<String> {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    for line in fm.lines() {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let (key, val) = (key.trim(), val.trim());
        match key {
            "section_id" => p.section_id = Some(val.to_string()),
            "title" => p.title = Some(val.to_string()),
            "kind" => p.kind = Some(val.to_string()),
            "author" => p.author = Some(val.to_string()),
            "tags" => p.tags = csv(val),
            "related_files" => p.related_files = csv(val),
            "created_at_ms" => p.created_at_ms = val.parse().ok(),
            "review_after_ms" => p.review_after_ms = val.parse().ok(),
            _ => {}
        }
    }
    // Trailing newline from reconstruction — keep the body as-is minus a
    // single trailing newline the writer added.
    if body.ends_with('\n') {
        body.pop();
    }
    p.content = body;
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sec() -> MemorySection {
        MemorySection {
            section_id: "deploy".into(),
            title: "Deploy note".into(),
            content: "line one\nline two".into(),
            updated_at_ms: 2_000,
            created_at_ms: 1_000,
            author: Some("session-4".into()),
            kind: Some("gotcha".into()),
            review_after_ms: Some(9_999),
            tags: vec!["ops".into(), "deploy".into()],
            related_files: vec!["src/a.rs".into()],
        }
    }

    #[test]
    fn round_trips() {
        let s = sec();
        let md = to_markdown(&s);
        let p = from_markdown(&md);
        assert_eq!(p.section_id.as_deref(), Some("deploy"));
        assert_eq!(p.title.as_deref(), Some("Deploy note"));
        assert_eq!(p.kind.as_deref(), Some("gotcha"));
        assert_eq!(p.author.as_deref(), Some("session-4"));
        assert_eq!(p.tags, vec!["ops".to_string(), "deploy".to_string()]);
        assert_eq!(p.related_files, vec!["src/a.rs".to_string()]);
        assert_eq!(p.created_at_ms, Some(1_000));
        assert_eq!(p.review_after_ms, Some(9_999));
        assert_eq!(p.content, "line one\nline two");
    }

    #[test]
    fn plain_text_without_frontmatter_is_all_content() {
        let p = from_markdown("just a note, no header");
        assert!(p.section_id.is_none());
        assert_eq!(p.content, "just a note, no header");
    }

    #[test]
    fn body_may_contain_triple_dashes() {
        let md = "---\nsection_id: x\ntitle: X\ncreated_at_ms: 0\nupdated_at_ms: 0\n---\nbody\n---\nmore body";
        let p = from_markdown(md);
        assert_eq!(p.section_id.as_deref(), Some("x"));
        assert_eq!(p.content, "body\n---\nmore body");
    }
}
