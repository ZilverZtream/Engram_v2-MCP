//! Real-corpus verification for the MiniLang extractor.
//!
//! Every other MiniLang test in this crate runs the extractor over synthetic
//! snippets. This one runs it over an actual MiniLang repository, which is the
//! only way to catch a construct family that never reaches the graph despite
//! its unit test passing — and the only way to notice that the corpus contains
//! a shape nobody wrote a fixture for.
//!
//! It is `#[ignore]`d because it needs a checkout that is not part of this
//! repository. Point `ENGRAM_MINILANG_CORPUS` at one and run:
//!
//! ```text
//! ENGRAM_MINILANG_CORPUS=/path/to/MiniLangCompiler \
//!   cargo test -p engram_index --test ml_real_corpus_test -- --ignored --nocapture
//! ```
//!
//! Worktree and scratch directories are excluded: a MiniLang checkout can carry
//! ~22x duplicates under `.claude/worktrees`, which silently inflates every
//! count (this bit an earlier measurement in this project's history — a "284
//! affected files" claim that was really 58).

use engram_index::ml_extractor::extract_ml;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn collect_ml_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                // Skip duplicate/scratch trees that would inflate counts.
                // `fuzz`/`negative` are also skipped: their fixtures are
                // deliberately malformed (mismatched blocks, constructs
                // the real compiler rejects), so counting them would mix
                // adversarial noise into a census meant to characterize
                // real, valid MiniLang, and scanning them logs a
                // `tracing::warn!` per mismatch for a shape that is never
                // going to balance.
                if matches!(
                    name.as_ref(),
                    ".git" | ".claude" | ".tmp" | "target" | "artifacts" | "fuzz" | "negative"
                ) {
                    continue;
                }
                walk(&path, out);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("ml") | Some("mlinc")
            ) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

#[test]
#[ignore = "needs ENGRAM_MINILANG_CORPUS pointing at a MiniLang checkout"]
fn real_corpus_yields_every_construct_family() {
    let Ok(root) = std::env::var("ENGRAM_MINILANG_CORPUS") else {
        panic!("set ENGRAM_MINILANG_CORPUS to a MiniLang checkout path");
    };
    let root = PathBuf::from(root);
    assert!(
        root.is_dir(),
        "corpus root {} is not a directory",
        root.display()
    );

    let files = collect_ml_files(&root);
    assert!(
        files.len() > 1000,
        "expected a substantial corpus, found {} files under {}",
        files.len(),
        root.display()
    );

    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut edge_kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut parsed = 0usize;
    let mut unreadable = 0usize;

    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            unreadable += 1;
            continue;
        };
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let (syms, edges) = extract_ml(path, &rel, &text);
        parsed += 1;
        for s in syms {
            *kinds.entry(s.kind).or_insert(0) += 1;
        }
        for e in edges {
            *edge_kinds.entry(e.kind).or_insert(0) += 1;
        }
    }

    println!("\n=== MiniLang real-corpus extraction ===");
    println!("files found:     {}", files.len());
    println!("files parsed:    {parsed}");
    println!("files unreadable:{unreadable}");
    println!("--- symbol kinds ---");
    for (k, n) in &kinds {
        println!("  {k:<18} {n}");
    }
    println!("--- edge kinds ---");
    for (k, n) in &edge_kinds {
        println!("  {k:<22} {n}");
    }
    println!();

    // Every construct family the extractor claims to support must actually
    // appear when run over real code. A zero here means a family is reachable
    // in a unit test but never in practice.
    for required in [
        "namespace",
        "function",
        "struct",
        "union",
        "enum",
        "interface",
        "constant",
        "extern_function",
        "ui_container",
        "control",
        "inline_asm",
    ] {
        assert!(
            kinds.get(required).copied().unwrap_or(0) > 0,
            "no `{required}` symbols extracted from the real corpus — \
             the family is unit-tested but never reaches the graph. counts: {kinds:?}"
        );
    }

    for required in [
        "calls",
        "includes_file",
        "implements_interface",
        "contains_ui",
    ] {
        assert!(
            edge_kinds.get(required).copied().unwrap_or(0) > 0,
            "no `{required}` edges from the real corpus. counts: {edge_kinds:?}"
        );
    }
}
