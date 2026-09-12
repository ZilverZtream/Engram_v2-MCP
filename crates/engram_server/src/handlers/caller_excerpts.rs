//! Bounded lexical leads within fingerprint-verified caller spans.
//! These are source excerpts, not resolved argument bindings or execution proof.
use std::io::Read;

use engram_graph::GraphStore;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct CallerExcerpt {
    pub file_path: String,
    pub caller: String,
    pub status: String,
    pub detail: String,
    pub numbered_source: String,
}

pub fn collect(
    graph: &GraphStore,
    project: &str,
    root: &str,
    caller: &super::access_layer_tools::CallerLocation,
    method: &str,
    budget: &mut usize,
) -> CallerExcerpt {
    let mut result = CallerExcerpt {
        file_path: caller.file_path.clone(),
        caller: caller.fqn.clone(),
        status: "withheld".into(),
        detail: String::new(),
        numbered_source: String::new(),
    };
    let read = || -> Result<String, String> {
        let node = graph
            .get_node(
                project,
                &format!("file:{}", caller.file_path.replace('\\', "/")),
            )
            .map_err(|e| e.to_string())?;
        let hash = node
            .as_ref()
            .and_then(|n| n.metadata.as_ref())
            .and_then(|m| m.get("file_hash"))
            .and_then(|v| v.as_str())
            .ok_or("No indexed fingerprint; refresh the index to obtain verified excerpts")?;
        let path = engram_core::safe_join(std::path::Path::new(root), &caller.file_path)
            .map_err(|e| e.to_string())?;
        let limit = (*budget).min(8 * 1024 * 1024);
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|e| e.to_string())?
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        *budget = budget.saturating_sub(bytes.len());
        if bytes.len() > limit {
            return Err("Source read budget exceeded".into());
        }
        if blake3::hash(&bytes).to_hex().as_str() != hash {
            return Err(
                "Caller changed since indexing; refresh before using its line spans".into(),
            );
        }
        String::from_utf8(bytes).map_err(|_| "Source is not UTF-8".into())
    };
    let mut read = read;
    match read() {
        Err(e) => result.detail = e,
        Ok(source) => {
            if caller.line == 0
                || caller.line_end < caller.line
                || caller.line_end as usize > source.lines().count()
            {
                result.detail =
                    "Indexed caller span is invalid for the verified source; refresh the index"
                        .into();
                return result;
            }
            result.numbered_source = select(&source, caller.line, caller.line_end, method);
            result.status = if result.numbered_source.is_empty() {
                "no_lexical_match"
            } else {
                "verified_source"
            }
            .into();
            result.detail = "Lexical name matches in the indexed caller span; comments, strings and other receivers may match. At most 3 windows, 8 lines per window, 240 characters per line, including up to 6 lines after the call to expose nearby result handling; windows may end mid-call or mid-branch. Inspect full caller for argument binding, defaults and complete output handling.".into();
        }
    }
    result
}

fn select(source: &str, start: u32, end: u32, method: &str) -> String {
    let name = method.rsplit('.').next().unwrap_or(method);
    if name.is_empty() || start == 0 || end < start {
        return String::new();
    }
    let Ok(pattern) = regex::RegexBuilder::new(&format!(r"\b{}\s*\(", regex::escape(name)))
        .case_insensitive(true)
        .build()
    else {
        return String::new();
    };
    let lines: Vec<_> = source.lines().collect();
    let Some(span) = lines.get(start as usize - 1..end as usize) else {
        return String::new();
    };
    let mut output = String::new();
    let mut next = 0;
    for i in (0..span.len())
        .filter(|i| pattern.is_match(span[*i]))
        .take(3)
    {
        for j in i.saturating_sub(1).max(next)..(i + 7).min(span.len()) {
            output.push_str(&format!(
                "{}: {}{}\n",
                start as usize + j,
                span[j].chars().take(240).collect::<String>(),
                if span[j].chars().count() > 240 {
                    " …"
                } else {
                    ""
                }
            ));
            next = j + 1;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_distinct_consumer_arguments_and_numbering() {
        let source = "Sub Export()\n rows = sheet.Render(images)\n rows = sheet.Render(images,\n     renderHtml := True)\nEnd Sub\n";
        let excerpt = select(source, 1, 5, "Sheet.Render");
        assert!(excerpt.contains("2:  rows = sheet.Render(images)"));
        assert!(excerpt.contains("4:      renderHtml := True)"));
        assert_eq!(excerpt.matches("3: ").count(), 1);
    }

    #[test]
    fn includes_nearby_comparison_of_converted_results() {
        let source = "Sub Audit()\n current = Value.Format(incoming)\n previous = Value.Format(saved)\n\n ' Notify only when the converted values differ\n If current <> previous Then\n     WriteLog(current)\n End If\nEnd Sub\n";
        let excerpt = select(source, 1, 9, "Value.Format");
        assert!(excerpt.contains("6:  If current <> previous Then"));
        assert!(excerpt.contains("7:      WriteLog(current)"));
        assert_eq!(excerpt.matches("6: ").count(), 1);
    }

    #[test]
    fn bounds_spans_windows_and_unicode_lines() {
        assert!(select("Render()\nOtherRender()", 2, 2, "Render").is_empty());
        assert!(select("Render()", 1, 20, "Render").is_empty());
        let source = format!("Render({})\n", "ä".repeat(2000)).repeat(20);
        let excerpt = select(&source, 1, 20, "Render");
        assert!(excerpt.lines().count() <= 24);
        assert!(excerpt.len() < 12000);
        assert!(excerpt.contains('…'));
    }

    #[test]
    fn read_budget_is_enforced_before_excerpting() {
        let temp = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&temp.path().join("graph")).unwrap();
        let source = "Render()\n";
        std::fs::write(temp.path().join("caller.cs"), source).unwrap();
        let node = engram_graph::Node {
            node_id: "file:caller.cs".into(),
            node_type: "file".into(),
            name: "caller.cs".into(),
            namespace: String::new(),
            language: "csharp".into(),
            file_path: engram_core::RelPath::new("caller.cs"),
            start_line: 1,
            end_line: 1,
            generation: 1,
            metadata: Some(
                serde_json::json!({"file_hash": blake3::hash(source.as_bytes()).to_hex().to_string()}),
            ),
        };
        graph.upsert_nodes("budget-test", &[node]).unwrap();
        let caller = super::super::access_layer_tools::CallerLocation {
            fqn: "Export".into(),
            file_path: "caller.cs".into(),
            line: 1,
            line_end: 1,
            line_kind: "declaration",
            edge_kind: "calls".into(),
        };
        let mut budget = 3;
        let result = collect(
            &graph,
            "budget-test",
            temp.path().to_str().unwrap(),
            &caller,
            "Render",
            &mut budget,
        );
        assert_eq!(result.status, "withheld");
        assert!(result.detail.contains("budget"));
        assert!(result.numbered_source.is_empty());
        assert_eq!(budget, 0);
    }
}
