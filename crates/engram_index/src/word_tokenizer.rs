//! Word-level analysis for BM25 over code and prose.
//!
//! The `content` field is a case-preserving character-trigram index built
//! for substring grep; ranking natural-language queries on it means "Missing"
//! never meets "missing" and every query word must survive as a trigram
//! phrase. `content_words` indexes the same text as words: runs of letters
//! and digits, lower-cased and ASCII-folded, with identifiers also split at
//! camelCase, acronym and letter/digit boundaries so `GetAllowedProjectIds`
//! is found by "allowed project" as well as by its full name. The whole
//! identifier is emitted after its parts, so the same analysis on both sides
//! keeps phrase positions aligned.

use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, RemoveLongFilter, TextAnalyzer, Token, TokenStream, Tokenizer,
};

/// Name the `content_words` field's analyzer is registered under.
pub const WORDS_TOKENIZER: &str = "engram_words";

/// Longest token kept; longer runs are hashes, base64 or minified noise.
const MAX_TOKEN_BYTES: usize = 64;

pub fn words_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(CodeWordTokenizer)
        .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}

#[derive(Clone, Default)]
pub struct CodeWordTokenizer;

pub struct CodeWordTokenStream {
    tokens: Vec<Token>,
    next: usize,
}

impl Tokenizer for CodeWordTokenizer {
    type TokenStream<'a> = CodeWordTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> CodeWordTokenStream {
        let mut tokens = Vec::new();
        let mut run_start = None;
        for (i, c) in text
            .char_indices()
            .chain(std::iter::once((text.len(), ' ')))
        {
            match (c.is_alphanumeric(), run_start) {
                (true, None) => run_start = Some(i),
                (false, Some(start)) => {
                    push_run(text, start, i, &mut tokens);
                    run_start = None;
                }
                _ => {}
            }
        }
        CodeWordTokenStream { tokens, next: 0 }
    }
}

/// Emit the sub-words of `text[start..end]`, then the whole run when it split.
fn push_run(text: &str, start: usize, end: usize, tokens: &mut Vec<Token>) {
    let run = &text[start..end];
    let chars: Vec<(usize, char)> = run.char_indices().collect();
    let mut part_start = 0;
    let mut parts = 0;
    for k in 1..chars.len() {
        let (prev, cur) = (chars[k - 1].1, chars[k].1);
        let next_lower = chars.get(k + 1).is_some_and(|(_, n)| n.is_lowercase());
        let boundary = (prev.is_lowercase() && cur.is_uppercase())
            || (prev.is_uppercase() && cur.is_uppercase() && next_lower)
            || (prev.is_alphabetic() && cur.is_numeric())
            || (prev.is_numeric() && cur.is_alphabetic());
        if boundary {
            push(tokens, text, start + part_start, start + chars[k].0);
            part_start = chars[k].0;
            parts += 1;
        }
    }
    push(tokens, text, start + part_start, end);
    if parts > 0 {
        push(tokens, text, start, end);
    }
}

fn push(tokens: &mut Vec<Token>, text: &str, from: usize, to: usize) {
    tokens.push(Token {
        offset_from: from,
        offset_to: to,
        position: tokens.len(),
        text: text[from..to].to_string(),
        position_length: 1,
    });
}

impl TokenStream for CodeWordTokenStream {
    fn advance(&mut self) -> bool {
        self.next += 1;
        self.next <= self.tokens.len()
    }

    fn token(&self) -> &Token {
        &self.tokens[self.next - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.next - 1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        let mut analyzer = words_analyzer();
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        out
    }

    #[test]
    fn prose_is_lowercased_words_without_punctuation() {
        assert_eq!(
            words("Missing photos, NOT in `list`!"),
            ["missing", "photos", "not", "in", "list"]
        );
    }

    #[test]
    fn identifiers_yield_parts_then_the_whole_name() {
        assert_eq!(
            words("GetAllowedProjectIds"),
            ["get", "allowed", "project", "ids", "getallowedprojectids"]
        );
        assert_eq!(words("HTMLParser"), ["html", "parser", "htmlparser"]);
        assert_eq!(words("tbl_user2fa"), ["tbl", "user", "2", "fa", "user2fa"]);
        assert_eq!(words("plain"), ["plain"]);
    }

    #[test]
    fn swedish_is_folded_so_either_spelling_matches() {
        assert_eq!(words("Användare åtgärd"), ["anvandare", "atgard"]);
    }

    #[test]
    fn positions_are_consecutive_for_phrase_matching() {
        let mut analyzer = words_analyzer();
        let mut stream = analyzer.token_stream("a GetAll b");
        let mut positions = Vec::new();
        while stream.advance() {
            positions.push(stream.token().position);
        }
        assert_eq!(positions, [0, 1, 2, 3, 4]);
    }
}
