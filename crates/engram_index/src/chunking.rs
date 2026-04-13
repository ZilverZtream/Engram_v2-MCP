use crate::parsing::ExtractedSymbol;
use engram_core::{ContentHash, DocIdStr};

/// Number of lines to overlap between adjacent chunks for agentic context.
const OVERLAP_LINES: usize = 5;

#[derive(Debug, Clone)]
pub struct Chunk {
    pub start_line: u32,
    pub end_line: u32,
    pub content: String,
    /// blake3 hash of raw content bytes (hex string).
    pub content_hash: ContentHash,
    /// Instance identity = blake3(rel_path + NUL + start_line + NUL + end_line + NUL + content_hash).
    /// Populated by `set_doc_id`; empty string until then.
    pub doc_id: DocIdStr,
    /// Name of the innermost enclosing symbol (function, class, etc.) if any.
    pub enclosing_symbol: Option<String>,
}

impl Chunk {
    /// Populate `doc_id` from path and line range.
    pub fn set_doc_id(&mut self, rel_path: &str) {
        self.doc_id =
            DocIdStr::compute(rel_path, self.start_line, self.end_line, &self.content_hash);
    }
}

pub fn chunk_lines(text: &str, max_chars: usize) -> Vec<Chunk> {
    semantic_chunk_lines(text, max_chars, &[])
}

/// Chunks text while attempting to respect symbol boundaries (functions, classes).
pub fn semantic_chunk_lines(
    text: &str,
    max_chars: usize,
    symbols: &[ExtractedSymbol],
) -> Vec<Chunk> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return out;
    }

    let mut current_start: usize = 0; // 0-indexed line index
    let mut current_len: usize = 0;

    // Map line index -> symbol that starts here (if any)
    let mut symbol_starts = std::collections::HashMap::new();
    for s in symbols {
        // start_line is 1-based
        if s.start_line > 0 {
            symbol_starts.insert(s.start_line as usize - 1, s);
        }
    }

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let line_len = line.len() + 1;

        // If this line starts a symbol, check if adding the WHOLE symbol would exceed max_chars
        if let Some(sym) = symbol_starts.get(&i) {
            let sym_end = (sym.end_line as usize).min(lines.len());
            let mut sym_total_len = 0;
            for line in lines.iter().take(sym_end).skip(i) {
                sym_total_len += line.len() + 1;
            }

            // If we have a current chunk and adding this symbol exceeds limit, flush first
            if current_len > 0 && (current_len + sym_total_len > max_chars) {
                out.push(build_chunk(&lines, current_start, i, symbols));
                current_start = i;
                current_len = 0;
            }

            // If the symbol ITSELF is larger than max_chars, we have to process it line by line
            if sym_total_len > max_chars {
                // FALLTHROUGH to line-by-line processing for this large symbol
            } else {
                // Add the whole symbol at once
                i = sym_end;
                current_len += sym_total_len;
                continue;
            }
        }

        // Line-by-line fallback
        if current_len > 0 && (current_len + line_len > max_chars) {
            out.push(build_chunk(&lines, current_start, i, symbols));
            current_start = i;
            current_len = 0;
        }

        current_len += line_len;
        i += 1;
    }

    if current_len > 0 {
        out.push(build_chunk(&lines, current_start, lines.len(), symbols));
    }

    // Add overlap context: prepend last OVERLAP_LINES lines of previous chunk.
    // Pre-allocate the combined buffer to exact capacity to avoid repeated
    // reallocations when concatenating prefix + existing content.
    if out.len() > 1 {
        for chunk in out.iter_mut().skip(1) {
            let curr_start_0 = chunk.start_line as usize - 1; // 0-based start index
            // Chunks are always contiguous; overlap is relative to curr_start_0 only.
            if curr_start_0 > 0 {
                let overlap_start = curr_start_0.saturating_sub(OVERLAP_LINES);
                if overlap_start < curr_start_0 {
                    // Calculate exact prefix size for a single allocation.
                    let prefix_len: usize = lines[overlap_start..curr_start_0]
                        .iter()
                        .map(|l| l.len() + 1)
                        .sum();
                    if prefix_len > 0 {
                        let total = prefix_len + chunk.content.len();
                        let mut combined = String::with_capacity(total);
                        for line in lines.iter().take(curr_start_0).skip(overlap_start) {
                            combined.push_str(line);
                            combined.push('\n');
                        }
                        combined.push_str(&chunk.content);
                        chunk.content = combined;
                        // Recompute hash for the overlapped content.
                        chunk.content_hash = ContentHash::compute(chunk.content.as_bytes());
                    }
                }
            }
        }
    }

    out
}

fn build_chunk(lines: &[&str], start: usize, end: usize, symbols: &[ExtractedSymbol]) -> Chunk {
    let cap: usize = lines[start..end].iter().map(|l| l.len() + 1).sum();
    let mut buf = String::with_capacity(cap);
    for line in lines.iter().take(end).skip(start) {
        buf.push_str(line);
        buf.push('\n');
    }

    let content_hash = ContentHash::compute(buf.as_bytes());

    // Find the innermost enclosing symbol for the chunk's start line
    let start_line_1based = (start + 1) as u32;
    let enclosing = symbols
        .iter()
        .filter(|s| s.start_line <= start_line_1based && s.end_line >= start_line_1based)
        .max_by_key(|s| s.start_line) // innermost = latest start
        .and_then(|s| {
            s.metadata
                .as_ref()
                .and_then(|m| m.get("fqn").cloned())
                .or_else(|| Some(s.name.clone()))
        });

    Chunk {
        start_line: (start + 1) as u32,
        end_line: end as u32,
        content: buf,
        content_hash,
        doc_id: DocIdStr(String::new()),
        enclosing_symbol: enclosing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsing::ExtractedSymbol;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn make_symbol(name: &str, start_line: u32, end_line: u32) -> ExtractedSymbol {
        ExtractedSymbol {
            name: name.to_string(),
            kind: "function",
            start_line,
            end_line,
            metadata: None,
        }
    }

    fn make_symbol_with_fqn(
        name: &str,
        fqn: &str,
        start_line: u32,
        end_line: u32,
    ) -> ExtractedSymbol {
        let mut meta = std::collections::HashMap::new();
        meta.insert("fqn".to_string(), fqn.to_string());
        ExtractedSymbol {
            name: name.to_string(),
            kind: "function",
            start_line,
            end_line,
            metadata: Some(meta),
        }
    }

    // ── 1. empty_text_returns_no_chunks ──────────────────────────────────────

    #[test]
    fn empty_text_returns_no_chunks() {
        let chunks = chunk_lines("", 500);
        assert!(chunks.is_empty(), "empty input must yield no chunks");
    }

    // ── 2. single_line_returns_one_chunk ─────────────────────────────────────

    #[test]
    fn single_line_returns_one_chunk() {
        let chunks = chunk_lines("hello world", 500);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("hello world"));
    }

    // ── 3. text_within_max_chars_not_split ───────────────────────────────────

    #[test]
    fn text_within_max_chars_not_split() {
        let text = "line one\nline two\nline three";
        // text is well under 500 chars
        let chunks = chunk_lines(text, 500);
        assert_eq!(chunks.len(), 1, "short text must not be split");
    }

    // ── 4. text_over_max_chars_splits_into_two ───────────────────────────────

    #[test]
    fn text_over_max_chars_splits_into_two() {
        // 20 lines of 10 chars each = 220 chars; limit = 100 → must split
        let text: String = (0..20).map(|i| format!("line_{:04}\n", i)).collect();
        let chunks = chunk_lines(&text, 100);
        assert!(
            chunks.len() >= 2,
            "text exceeding max_chars must split: got {} chunks",
            chunks.len()
        );
    }

    // ── 5. each_chunk_under_max_chars ────────────────────────────────────────

    #[test]
    fn each_chunk_under_max_chars() {
        // 50 lines of 20 chars each; limit = 80
        let text: String = (0..50)
            .map(|i| format!("abcdefghij_{:04}xxx\n", i))
            .collect();
        let max = 80usize;
        let chunks = chunk_lines(&text, max);
        // Each *original* (pre-overlap) chunk must be ≤ max_chars.
        // Because overlap prepends up to OVERLAP_LINES lines to chunks[1..],
        // we verify the *start_line→end_line* span fits.
        for c in &chunks {
            let span_chars: usize = text
                .lines()
                .skip(c.start_line as usize - 1)
                .take((c.end_line - c.start_line + 1) as usize)
                .map(|l| l.len() + 1)
                .sum();
            assert!(
                span_chars <= max,
                "chunk span [{},{}] has {} bytes > max {}",
                c.start_line,
                c.end_line,
                span_chars,
                max
            );
        }
    }

    // ── 6. second_chunk_has_overlap_prefix ───────────────────────────────────

    #[test]
    fn second_chunk_has_overlap_prefix() {
        // All lines are unique (no accidental content collisions).
        // Each line is ~10 chars; max_chars = 50 → ~5 raw lines per chunk.
        let text: String = (1..=30).map(|i| format!("LINE{:03}\n", i)).collect();
        let chunks = chunk_lines(&text, 50);
        assert!(chunks.len() >= 2, "need at least 2 chunks for overlap test");

        // The overlap is implemented by prepending the last ≤OVERLAP_LINES lines of
        // the previous chunk's original range into the next chunk's content.
        // chunk[0] is the first chunk → no prefix, content = original lines.
        // chunk[1] should begin with some suffix of chunk[0]'s original content.
        let chunk0_lines: Vec<&str> = chunks[0].content.lines().collect();
        let chunk1_first_line = chunks[1].content.lines().next().unwrap_or("");

        // The first line of chunk[1] must be a line that appears in chunk[0].
        assert!(
            chunk0_lines.contains(&chunk1_first_line),
            "second chunk must start with a line from the first chunk (overlap prefix), \
             but starts with: {:?}. chunk[0].content:\n{}",
            chunk1_first_line,
            chunks[0].content
        );

        // More specifically, the first OVERLAP_LINES lines of chunk[1].content must
        // exactly match the last OVERLAP_LINES lines of chunk[0].content.
        let overlap_count = OVERLAP_LINES.min(chunk0_lines.len());
        let chunk0_tail: Vec<&str> = chunk0_lines
            .iter()
            .rev()
            .take(overlap_count)
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let chunk1_head: Vec<&str> = chunks[1].content.lines().take(overlap_count).collect();
        assert_eq!(
            chunk1_head, chunk0_tail,
            "first {} lines of chunk[1] must match last {} lines of chunk[0]",
            overlap_count, overlap_count
        );
    }

    // ── 7. overlap_is_capped_at_five_lines ───────────────────────────────────

    #[test]
    fn overlap_is_capped_at_five_lines() {
        // Each line is ~10 chars; max_chars = 60 → roughly 6 raw lines per chunk.
        // The overlap is prepended content: chunk[i+1].content starts with the last
        // N lines of chunk[i]'s original range, where N ≤ OVERLAP_LINES.
        let text: String = (1u32..=100)
            .map(|i| format!("L{:03}xxxxxxx\n", i))
            .collect();
        let chunks = chunk_lines(&text, 60);
        assert!(chunks.len() >= 3, "need ≥3 chunks to check overlap cap");

        for (idx, _chunk) in chunks.iter().enumerate().skip(1) {
            let curr_start_0 = chunks[idx].start_line as usize - 1; // 0-indexed original start
            // The overlap prefix is lines[overlap_start..curr_start_0].
            // Number of prepended lines = curr_start_0 - overlap_start ≤ OVERLAP_LINES.
            let expected_overlap = OVERLAP_LINES.min(curr_start_0);

            // Count how many leading lines of chunk[idx].content are BEFORE the original start.
            // Each prepended overlap line comes from the text before curr_start_0.
            let all_lines: Vec<&str> = text.lines().collect();
            let chunk_lines: Vec<&str> = chunks[idx].content.lines().collect();
            let prepended_count = chunk_lines
                .iter()
                .take_while(|&&l| {
                    // A line is part of the prepended prefix if it comes from before curr_start_0.
                    all_lines[..curr_start_0].contains(&l)
                })
                .count();

            assert_eq!(
                prepended_count, expected_overlap,
                "chunk[{}] (start_line={}) should have exactly {} prepended overlap lines, got {}",
                idx, chunks[idx].start_line, expected_overlap, prepended_count
            );
        }
    }

    // ── 8. no_content_lost_across_chunks ─────────────────────────────────────

    #[test]
    fn no_content_lost_across_chunks() {
        let text: String = (1u32..=40)
            .map(|i| format!("unique_line_{}\n", i))
            .collect();
        let chunks = chunk_lines(&text, 80);
        assert!(chunks.len() >= 2, "need multiple chunks for this test");

        // Every original line must appear in at least one chunk.
        for line in text.lines() {
            let found = chunks.iter().any(|c| c.content.contains(line));
            assert!(found, "line {:?} missing from all chunks", line);
        }
    }

    // ── 9. content_hash_is_deterministic ─────────────────────────────────────

    #[test]
    fn content_hash_is_deterministic() {
        let text = "fn foo() {\n    let x = 1;\n}\n";
        let chunks_a = chunk_lines(text, 500);
        let chunks_b = chunk_lines(text, 500);
        assert_eq!(chunks_a.len(), chunks_b.len());
        for (a, b) in chunks_a.iter().zip(chunks_b.iter()) {
            assert_eq!(
                a.content_hash, b.content_hash,
                "content_hash must be deterministic"
            );
        }
    }

    // ── 10. doc_id_set_correctly ─────────────────────────────────────────────

    #[test]
    fn doc_id_set_correctly() {
        let text = "fn bar() {}\n";
        let mut chunks = chunk_lines(text, 500);
        assert_eq!(chunks.len(), 1);
        // doc_id starts empty
        assert!(
            chunks[0].doc_id.0.is_empty(),
            "doc_id must be empty before set_doc_id"
        );
        chunks[0].set_doc_id("src/lib.rs");
        assert!(
            !chunks[0].doc_id.0.is_empty(),
            "doc_id must be non-empty after set_doc_id"
        );
    }

    // ── 11. doc_id_includes_path_and_lines ───────────────────────────────────

    #[test]
    fn doc_id_includes_path_and_lines() {
        let text = "fn baz() {}\n";
        let mut c1 = chunk_lines(text, 500);
        let mut c2 = chunk_lines(text, 500);
        c1[0].set_doc_id("src/alpha.rs");
        c2[0].set_doc_id("src/beta.rs");
        assert_ne!(
            c1[0].doc_id, c2[0].doc_id,
            "doc_ids for different paths must differ"
        );
    }

    // ── 12. line_numbers_are_one_based ───────────────────────────────────────

    #[test]
    fn line_numbers_are_one_based() {
        let text = "first\nsecond\nthird\n";
        let chunks = chunk_lines(text, 500);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1, "start_line must be 1-based");
    }

    // ── 13. end_line_correct_for_last_chunk ──────────────────────────────────

    #[test]
    fn end_line_correct_for_last_chunk() {
        // 10 lines; limit is large so we get one chunk covering all 10 lines
        let text: String = (1..=10).map(|i| format!("line {}\n", i)).collect();
        let line_count = text.lines().count() as u32;
        let chunks = chunk_lines(&text, 5000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].end_line, line_count,
            "end_line must equal the last line number"
        );
    }

    // ── 14. symbol_aware_keeps_function_together ─────────────────────────────

    #[test]
    fn symbol_aware_keeps_function_together() {
        // 10-line function that fits within a 300-char max_chars
        let func_lines: String = std::iter::once("fn small_fn() {\n".to_string())
            .chain((0..8).map(|i| format!("    let x{} = {};\n", i, i)))
            .chain(std::iter::once("}\n".to_string()))
            .collect();
        let sym = make_symbol("small_fn", 1, 10);
        let chunks = semantic_chunk_lines(&func_lines, 300, &[sym]);

        // All lines of the function must be in the same chunk.
        let func_chunk = chunks
            .iter()
            .find(|c| c.start_line == 1)
            .expect("chunk at line 1");
        assert_eq!(
            func_chunk.end_line, 10,
            "entire function must be in one chunk"
        );
    }

    // ── 15. large_symbol_falls_back_to_line_by_line ──────────────────────────

    #[test]
    fn large_symbol_falls_back_to_line_by_line() {
        // 20-line function with max_chars = 50 → function won't fit in one chunk.
        let func_lines: String = std::iter::once("fn big_fn() {\n".to_string())
            .chain((0..18).map(|i| format!("    let var{} = {};\n", i, i)))
            .chain(std::iter::once("}\n".to_string()))
            .collect();
        let sym = make_symbol("big_fn", 1, 20);
        let chunks = semantic_chunk_lines(&func_lines, 50, &[sym]);

        // Must produce multiple chunks since the function body won't fit in one.
        assert!(
            chunks.len() > 1,
            "large symbol must fall back to line-by-line, got {} chunks",
            chunks.len()
        );

        // Every line in the original function must appear in at least one chunk.
        for (lineno, line) in func_lines.lines().enumerate() {
            let found = chunks.iter().any(|c| c.content.contains(line));
            assert!(
                found,
                "line {} ({:?}) missing from all fallback chunks",
                lineno + 1,
                line
            );
        }

        // All fallback chunks must have enclosing_symbol set to "big_fn".
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(
                chunk.enclosing_symbol.as_deref(),
                Some("big_fn"),
                "chunk[{}] missing enclosing_symbol after line-by-line fallback",
                i
            );
        }
    }

    // ── 16. enclosing_symbol_set_for_chunk_in_function ───────────────────────

    #[test]
    fn enclosing_symbol_set_for_chunk_in_function() {
        // A function spanning lines 1–20; limit forces it to be split.
        let func_lines: String = std::iter::once("fn my_fn() {\n".to_string())
            .chain((0..18).map(|i| format!("    let v{} = {};\n", i, i)))
            .chain(std::iter::once("}\n".to_string()))
            .collect();
        let sym = make_symbol("my_fn", 1, 20);
        let chunks = semantic_chunk_lines(&func_lines, 50, &[sym]);
        // The first chunk starts at line 1 which is covered by the symbol.
        let first = &chunks[0];
        assert_eq!(
            first.enclosing_symbol.as_deref(),
            Some("my_fn"),
            "chunk inside function must have enclosing_symbol = \"my_fn\""
        );
    }

    // ── 16b. enclosing_symbol_uses_fqn_when_present ──────────────────────────

    #[test]
    fn enclosing_symbol_uses_fqn_when_present() {
        let func_lines: String = std::iter::once("fn fqn_fn() {\n".to_string())
            .chain((0..18).map(|i| format!("    let v{} = {};\n", i, i)))
            .chain(std::iter::once("}\n".to_string()))
            .collect();
        let sym = make_symbol_with_fqn("fqn_fn", "MyModule::fqn_fn", 1, 20);
        let chunks = semantic_chunk_lines(&func_lines, 50, &[sym]);
        let first = &chunks[0];
        assert_eq!(
            first.enclosing_symbol.as_deref(),
            Some("MyModule::fqn_fn"),
            "enclosing_symbol must use fqn from metadata when present"
        );
    }

    // ── 17. chunk_lines_delegates_to_semantic ────────────────────────────────

    #[test]
    fn chunk_lines_delegates_to_semantic() {
        let text = "alpha\nbeta\ngamma\n";
        // chunk_lines calls semantic_chunk_lines with empty symbols — results must match
        let a = chunk_lines(text, 500);
        let b = semantic_chunk_lines(text, 500, &[]);
        assert_eq!(a.len(), b.len());
        for (ca, cb) in a.iter().zip(b.iter()) {
            assert_eq!(ca.content, cb.content);
            assert_eq!(ca.start_line, cb.start_line);
            assert_eq!(ca.end_line, cb.end_line);
        }
    }

    // ── 18. single_very_long_line_gets_its_own_chunk ─────────────────────────

    #[test]
    fn single_very_long_line_gets_its_own_chunk() {
        // One line that is longer than max_chars; should still produce exactly 1 chunk
        let long_line = "x".repeat(200);
        let chunks = chunk_lines(&long_line, 50);
        assert_eq!(
            chunks.len(),
            1,
            "a single line, however long, must produce exactly one chunk"
        );
        assert!(chunks[0].content.contains(&long_line));
    }

    // ── 19. multiple_symbols_chunked_in_order ────────────────────────────────

    #[test]
    fn multiple_symbols_chunked_in_order() {
        // Two small functions on lines 1-3 and 4-6, limit = 500 → both fit in one chunk
        let text = "fn a() {}\nlet a = 1;\n}\nfn b() {}\nlet b = 2;\n}\n";
        let sym_a = make_symbol("fn_a", 1, 3);
        let sym_b = make_symbol("fn_b", 4, 6);
        let chunks = semantic_chunk_lines(text, 500, &[sym_a, sym_b]);
        // All lines are present
        let combined: String = chunks.iter().map(|c| c.content.as_str()).collect();
        assert!(combined.contains("fn a()"), "fn_a must be present");
        assert!(combined.contains("fn b()"), "fn_b must be present");
        // start_line of first chunk must be 1
        assert_eq!(chunks[0].start_line, 1);
    }

    // ── 20. whitespace_only_text_handled ─────────────────────────────────────

    #[test]
    fn whitespace_only_text_handled() {
        // A string with only spaces and newlines — lines() yields empty strings
        let text = "   \n   \n   \n";
        // Must not panic and must handle gracefully (0 or 1 chunks)
        let chunks = chunk_lines(text, 500);
        // All produced chunks should have valid line numbers
        for c in &chunks {
            assert!(c.start_line >= 1);
            assert!(c.end_line >= c.start_line);
        }
    }
}
