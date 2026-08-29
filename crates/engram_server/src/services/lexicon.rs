//! EN↔SV domain lexicon mined from the project's OWN localization resources
//! (external audit 2026-08-29 row 1, owner decision 10:58).
//!
//! A `.resx` in the default culture (or `*.sv.resx`) paired with its
//! `*.en.resx` sibling is a deterministic bilingual dictionary the team
//! itself maintains: `Mängdredovisning` ↔ `Reporting of Quantities`. An
//! English story that names a domain entity in English can be translated —
//! longest match over the resource values — into the Swedish terms the code
//! is written in, without a parenthesized gloss. Working-tree parse, cached
//! per project by a signature of the resx files (path, mtime, len).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// (english phrase, lowercase) → swedish resource value (original spelling).
#[derive(Debug, Default, Clone)]
pub struct Lexicon {
    pub pairs: Vec<(String, String)>,
    /// Index: first English word → pair indices.
    by_first: HashMap<String, Vec<usize>>,
    pub signature: u64,
    pub resx_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexiconHit {
    pub en: String,
    pub sv: String,
}

const SKIP_DIRS: [&str; 7] = [
    ".git",
    "node_modules",
    "bin",
    "obj",
    "packages",
    "target",
    ".vs",
];
const MAX_ENTRIES: usize = 60_000;

fn walk_resx(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                return out;
            }
            let p = e.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !SKIP_DIRS.contains(&name) {
                    stack.push(p);
                }
            } else if p
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("resx"))
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// `name.resx` → (base "name", culture None); `name.en.resx` → ("name", Some("en")).
fn split_culture(p: &Path) -> Option<(String, Option<String>)> {
    let stem = p.file_stem()?.to_str()?.to_string();
    let mut parts = stem.rsplitn(2, '.');
    let last = parts.next()?;
    let Some(base) = parts.next() else {
        return Some((stem, None));
    };
    let is_culture = (2..=5).contains(&last.len())
        && last.chars().take(2).all(|c| c.is_ascii_lowercase())
        && (last.len() == 2 || last.as_bytes()[2] == b'-');
    if is_culture {
        Some((base.to_string(), Some(last[..2].to_string())))
    } else {
        Some((stem, None))
    }
}

/// (swedish-side file, english file) per base name in a directory.
pub fn find_resx_pairs(root: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut groups: HashMap<
        (PathBuf, String),
        (Option<PathBuf>, Option<PathBuf>, Option<PathBuf>),
    > = HashMap::new();
    for p in walk_resx(root) {
        let Some((base, culture)) = split_culture(&p) else {
            continue;
        };
        let dir = p.parent().map(Path::to_path_buf).unwrap_or_default();
        let g = groups.entry((dir, base)).or_default();
        match culture.as_deref() {
            None => g.0 = Some(p),
            Some("sv") => g.1 = Some(p),
            Some("en") => g.2 = Some(p),
            _ => {}
        }
    }
    let mut out: Vec<(PathBuf, PathBuf)> = groups
        .into_values()
        .filter_map(|(default, sv, en)| Some((sv.or(default)?, en?)))
        .collect();
    out.sort();
    out
}

fn parse_resx(p: &Path) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(p) else {
        return HashMap::new();
    };
    let re = regex::Regex::new(r#"(?s)<data\s+name="([^"]+)"[^>]*>\s*<value>([^<]*)</value>"#)
        .expect("static regex");
    re.captures_iter(&text)
        .map(|c| (c[1].to_string(), c[2].trim().to_string()))
        .collect()
}

fn clean_phrase(v: &str) -> Option<String> {
    let v = v.trim();
    if v.is_empty() || v.contains('{') || v.contains('<') || v.contains(':') {
        return None;
    }
    let words: Vec<&str> = v.split_whitespace().collect();
    if words.len() > 6 || !v.chars().any(|c| c.is_alphabetic()) {
        return None;
    }
    Some(words.join(" "))
}

fn file_signature(root: &Path) -> (u64, usize) {
    let mut sig: u64 = 0xcbf2_9ce4_8422_2325;
    let files = walk_resx(root);
    for p in &files {
        for b in p.to_string_lossy().bytes() {
            sig = (sig ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
        if let Ok(m) = std::fs::metadata(p) {
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            sig = (sig ^ t).wrapping_mul(0x0100_0000_01b3);
            sig = (sig ^ m.len()).wrapping_mul(0x0100_0000_01b3);
        }
    }
    (sig, files.len())
}

/// Signature of the project's resx files — changes when any of them does.
pub fn resx_signature(root: &Path) -> u64 {
    file_signature(root).0
}

/// Mine every (default|sv, en) resx pair under `root` into a lexicon.
pub fn build_lexicon(root: &Path) -> Lexicon {
    let (signature, resx_files) = file_signature(root);
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (sv_file, en_file) in find_resx_pairs(root) {
        let sv = parse_resx(&sv_file);
        let en = parse_resx(&en_file);
        for (key, en_val) in &en {
            let Some(sv_val) = sv.get(key) else { continue };
            let (Some(e), Some(s)) = (clean_phrase(en_val), clean_phrase(sv_val)) else {
                continue;
            };
            if e.eq_ignore_ascii_case(&s) {
                continue;
            }
            let e_lower = e.to_lowercase();
            let en_words = e_lower.split_whitespace().count();
            // A single generic English word ("Save", "Back", "Map") is not a
            // domain entity; one long word ("Dashboard", "Inspection") can be.
            if en_words == 1 && e_lower.chars().filter(|c| c.is_alphabetic()).count() < 6 {
                continue;
            }
            pairs.push((e_lower, s));
        }
    }
    pairs.sort();
    pairs.dedup();
    let mut by_first: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, (e, _)) in pairs.iter().enumerate() {
        if let Some(w) = e.split_whitespace().next() {
            by_first.entry(w.to_string()).or_default().push(i);
        }
    }
    Lexicon {
        pairs,
        by_first,
        signature,
        resx_files,
    }
}

fn story_tokens(story: &str) -> Vec<String> {
    story
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect()
}

/// Longest-match translation of the story's English n-grams (≤ 6 words).
pub fn translate(story: &str, lex: &Lexicon) -> Vec<LexiconHit> {
    let toks = story_tokens(story);
    let mut out: Vec<LexiconHit> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let mut best: Option<(usize, usize)> = None; // (pair idx, words)
        if let Some(cands) = lex.by_first.get(&toks[i]) {
            for &pi in cands {
                let words: Vec<&str> = lex.pairs[pi].0.split_whitespace().collect();
                if i + words.len() <= toks.len()
                    && words.iter().zip(&toks[i..]).all(|(a, b)| *a == b.as_str())
                {
                    if best.is_none_or(|(_, n)| words.len() > n) {
                        best = Some((pi, words.len()));
                    }
                }
            }
        }
        if let Some((pi, n)) = best {
            let hit = LexiconHit {
                en: lex.pairs[pi].0.clone(),
                sv: lex.pairs[pi].1.clone(),
            };
            if !out.iter().any(|h| h.sv.eq_ignore_ascii_case(&hit.sv)) {
                out.push(hit);
            }
            i += n;
        } else {
            i += 1;
        }
    }
    out
}

/// Identifier-friendly fold of a Swedish term: `Mängdredovisning` → `mangdredovisning`.
pub fn ascii_fold(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'å' | 'ä' => 'a',
            'ö' => 'o',
            'Å' | 'Ä' => 'A',
            'Ö' => 'O',
            'é' | 'è' => 'e',
            _ => c,
        })
        .collect()
}

/// The concept terms a story's lexicon hits contribute: each Swedish token
/// ≥ 5 letters, lowercase, plus its ASCII-folded identifier form. Ordered by
/// the length of the English phrase that produced them (most specific first).
pub fn concept_terms(hits: &[LexiconHit]) -> Vec<String> {
    let mut ranked: Vec<&LexiconHit> = hits.iter().collect();
    ranked.sort_by(|a, b| b.en.len().cmp(&a.en.len()).then_with(|| a.sv.cmp(&b.sv)));
    let mut out: Vec<String> = Vec::new();
    for h in ranked {
        for tok in h.sv.split(|c: char| !c.is_alphanumeric()) {
            if tok.chars().filter(|c| c.is_alphabetic()).count() < 5 {
                continue;
            }
            let lower = tok.to_lowercase();
            for t in [lower.clone(), ascii_fold(&lower)] {
                if !out.contains(&t) {
                    out.push(t);
                }
            }
        }
        if out.len() >= 8 {
            break;
        }
    }
    out
}

/// Cached lexicon for a project directory (rebuilt when the resx files change).
pub fn lexicon_for(
    state: &crate::state::AppState,
    project_id: &str,
    project_dir: &Path,
) -> std::sync::Arc<Lexicon> {
    let sig = resx_signature(project_dir);
    if let Some(l) = state.lexicon_cache.get(project_id)
        && l.signature == sig
    {
        return l.clone();
    }
    let l = std::sync::Arc::new(build_lexicon(project_dir));
    state
        .lexicon_cache
        .insert(project_id.to_string(), l.clone());
    l
}

/// Story → lexicon-derived concept terms (empty when the project has no
/// bilingual resources or the story names nothing the lexicon knows).
pub fn story_lexicon_concepts(
    state: &crate::state::AppState,
    project_id: &str,
    project_dir: &Path,
    story: &str,
) -> (Vec<LexiconHit>, Vec<String>) {
    let lex = lexicon_for(state, project_id, project_dir);
    if lex.pairs.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let hits = translate(story, &lex);
    let terms = concept_terms(&hits);
    (hits, terms)
}
