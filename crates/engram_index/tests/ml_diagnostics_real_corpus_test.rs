//! Real-corpus validation for the three MiniLang diagnostics added in this
//! task: `strong_ref_self_cycle` (MLC6013), `send_on_closed_channel`, and
//! `match_missing_case_else`.
//!
//! Companion to `ml_real_corpus_test.rs` (which validates the extractor
//! itself). This one runs `language_diagnostics::minilang::detect` over
//! every real `.ml`/`.mlinc` file and prints what each new diagnostic
//! flags, so a human can eyeball every finding against the real source
//! rather than trusting a synthetic-snippet unit test alone.
//!
//! It is `#[ignore]`d because it needs a checkout that is not part of this
//! repository. Point `ENGRAM_MINILANG_CORPUS` at one and run:
//!
//! ```text
//! ENGRAM_MINILANG_CORPUS=/path/to/MiniLangCompiler \
//!   cargo test -p engram_index --test ml_diagnostics_real_corpus_test -- --ignored --nocapture
//! ```
//!
//! Worktree and scratch directories are excluded: a MiniLang checkout can
//! carry ~22x duplicates under `.claude/worktrees`, which silently inflates
//! every count (see `ml_real_corpus_test.rs`'s doc comment for the history).

use engram_index::language_diagnostics::{LanguageFamily, detect_language_diagnostics};
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
                if matches!(
                    name.as_ref(),
                    ".git" | ".claude" | ".tmp" | "target" | "artifacts"
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
fn real_corpus_new_diagnostics_are_all_explainable() {
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

    // The diagnostics are file-scoped (see minilang.rs's doc comments on
    // `strong_ref_cycle_diagnostics` / `match_missing_case_else_diagnostics`
    // for why), so running them one file at a time is both faithful to how
    // a real caller would invoke `detect_language_diagnostics` per-file
    // (`access_layer_tools.rs`) and avoids the ~5,300-file corpus getting
    // merged into one giant cross-file batch for no benefit.
    let mut by_category: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut findings: Vec<(String, String, String, String)> = Vec::new(); // (category, location, evidence, guidance)

    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let files_arg: Vec<(&str, &str)> = vec![(rel.as_str(), text.as_str())];
        let report = detect_language_diagnostics(LanguageFamily::MiniLang, &files_arg);
        for d in report.diagnostics {
            if matches!(
                d.category.as_str(),
                "strong_ref_self_cycle" | "send_on_closed_channel" | "match_missing_case_else"
            ) {
                *by_category.entry(d.category.clone()).or_insert(0) += 1;
                findings.push((d.category, d.location, d.evidence, d.guidance));
            }
        }
    }

    println!("\n=== MiniLang new-diagnostics real-corpus validation ===");
    println!("files scanned: {}", files.len());
    for (cat, n) in &by_category {
        println!("  {cat:<26} {n}");
    }
    println!();
    for (cat, loc, evidence, _guidance) in &findings {
        println!("[{cat}] {loc} :: {evidence}");
    }
    println!();
}
