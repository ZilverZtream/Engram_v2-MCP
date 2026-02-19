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

    // Add overlap context: prepend last OVERLAP_LINES lines of previous chunk
    if out.len() > 1 {
        for idx in 1..out.len() {
            let prev_end = out[idx - 1].end_line as usize; // 1-based end line
            let curr_start_0 = out[idx].start_line as usize - 1; // 0-based start index
            if curr_start_0 > 0 && prev_end > 0 {
                let overlap_start = curr_start_0.saturating_sub(OVERLAP_LINES);
                if overlap_start < curr_start_0 {
                    let mut prefix = String::new();
                    for line in lines.iter().take(curr_start_0).skip(overlap_start) {
                        prefix.push_str(line);
                        prefix.push('\n');
                    }
                    if !prefix.is_empty() {
                        prefix.push_str(&out[idx].content);
                        out[idx].content = prefix;
                        // Recompute hash for the overlapped content
                        out[idx].content_hash = ContentHash::compute(out[idx].content.as_bytes());
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
